use clap::Parser;
use directories::ProjectDirs;
use lofty::file::AudioFile;
use lofty::file::TaggedFileExt;
use lofty::read_from_path;
use lofty::tag::Accessor;
use reqwest::Client;
use serde::Deserialize;
use std::error::Error;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = ".")]
    path: PathBuf,

    #[arg(short, long, default_value_t = false)]
    jellyfin: bool,

    #[arg(short = 'u', long, default_value = "http://localhost:8096")]
    jellyfin_url: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Track {
    id: i32,
    track_name: String,
    artist_name: String,
    album_name: String,
    duration: f64,
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

pub fn get_api_key() -> Result<String, Box<dyn Error>> {
    let local_path = PathBuf::from("api.txt");
    if local_path.exists() {
        let key = std::fs::read_to_string(local_path)?;
        return Ok(key.trim().to_string());
    }

    if let Some(proj_dirs) = ProjectDirs::from("com", "quix", "lrc_downloader") {
        let config_dir = proj_dirs.config_dir();
        let global_path = config_dir.join("api.txt");

        if global_path.exists() {
            let key = std::fs::read_to_string(global_path)?;
            return Ok(key.trim().to_string());
        }
    }

    Err("API not found in api.txt and in dotfiles direcrtory".into())
}

async fn trigger_jellyfin_scan(client: &Client, base_url: &str) -> Result<(), Box<dyn Error>> {
    let jellyfin_url = format!("{}/Library/Refresh", base_url.trim_end_matches('/'));
    let key = get_api_key()?;

    let response = client
        .post(&jellyfin_url)
        .header("X-Emby-Token", key)
        .send()
        .await?;

    if response.status().is_success() {
        println!("[+] Jellyfin rescan started!");
    } else {
        eprintln!(
            "[-] Failed to rescan Jellyfin library: {}",
            response.status()
        );
    }

    Ok(())
}

async fn ask_lrclib(
    client: &Client,
    tag: (PathBuf, String, String, String, u64),
    marker: &mut bool,
) -> Result<Option<String>, reqwest::Error> {
    let url = "https://lrclib.net/api/get";

    let track_name = tag.1;
    let artist_name = tag.2;
    let album_name = tag.3;
    let duration_str = tag.4.to_string();

    let mut params = vec![("track_name", track_name), ("artist_name", artist_name)];
    if !album_name.is_empty() {
        params.push(("album_name", album_name));
    }
    if tag.4 > 0 {
        params.push(("duration", duration_str));
    }

    let response = client
        .get(url)
        .header("User-Agent", "lrc_downloader/0.1.5")
        .query(&params)
        .send()
        .await?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let response = response.error_for_status()?;
    let track_info = response.json::<Track>().await?;
    if track_info.synced_lyrics.is_some() {
        *marker = true;
    }

    Ok(track_info.synced_lyrics.or(track_info.plain_lyrics))
}

async fn save_lrc(
    lyrics_to_save: Option<String>,
    path: PathBuf,
    is_synced: bool,
    artist: String,
    track: String,
) {
    if let Some(text) = lyrics_to_save {
        let lrc_path = path.with_extension("lrc");

        match fs::write(&lrc_path, text).await {
            Ok(_) => {
                let tag = if is_synced { "[+ SYNC]" } else { "[+ PLAIN]" };
                println!("{} Saved LRC {} - {}", tag, artist, track);
            }
            Err(e) => eprintln!("[-] Error while writing {:?}: {}", lrc_path, e),
        }
    } else {
        println!("[!] No LRC for {} - {}", artist, track);
    }
}

async fn get_music(path: PathBuf) -> Vec<PathBuf> {
    tokio::task::spawn_blocking(move || {
        let mut data: Vec<PathBuf> = Vec::new();

        for entry in WalkDir::new(path) {
            match entry {
                Ok(e) => {
                    let path = e.into_path();
                    if path.is_file() {
                        let is_audio = path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext_str| {
                                let lower = ext_str.to_lowercase();
                                matches!(lower.as_str(), "flac" | "mp3" | "m4a" | "ogg" | "wav")
                            })
                            .unwrap_or(false);

                        if is_audio {
                            data.push(path);
                        }
                    }
                }
                Err(err) => eprintln!("Ignored cause of error: {}", err),
            }
        }

        data
    })
    .await
    .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let client = Client::new();
    let semaphore = Arc::new(Semaphore::new(4));
    let mut set = JoinSet::new();

    let data: Vec<PathBuf> = get_music(args.path).await;

    let mut apis: Vec<(PathBuf, String, String, String, u64)> = Vec::new();

    for path in data {
        let file = match read_from_path(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let lrc_path = path.with_extension("lrc");

        if lrc_path.exists() {
            println!("Skipped {:?}, LRC already exists", path);
            continue;
        }
        let duration = file.properties().duration();
        let duration_secs = duration.as_secs();

        let mut tl = String::new();
        let mut art = String::new();
        let mut alb = String::new();

        if let Some(tag) = file.primary_tag() {
            if let Some(title) = tag.title() {
                tl = title.into_owned();
            }
            if let Some(artist) = tag.artist() {
                art = artist.into_owned();
            }
            if let Some(album) = tag.album() {
                alb = album.into_owned();
            }
        }

        if tl.is_empty() || art.is_empty() {
            continue;
        }

        apis.push((path, tl, art, alb, duration_secs));
    }

    for tag in apis {
        let client = client.clone();
        let permit = semaphore.clone();

        set.spawn(async move {
            let _permit = permit.acquire().await.unwrap();

            let path = tag.0.clone();
            let track_name = tag.1.clone();
            let artist_name = tag.2.clone();
            let mut is_synced = false;

            match ask_lrclib(&client, tag, &mut is_synced).await {
                Ok(lyrics) => {
                    save_lrc(lyrics, path, is_synced, artist_name, track_name).await;
                }
                Err(e) => {
                    eprintln!(
                        "[-] Error fetching LRC for {} - {}: {}",
                        artist_name, track_name, e
                    );
                }
            }
            sleep(Duration::from_millis(150)).await;
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("[+] Task panicked: {:?}", e);
        }
    }

    if args.jellyfin {
        if let Err(e) = trigger_jellyfin_scan(&client, &args.jellyfin_url).await {
            eprintln!("[-] Jellyfin scan failed: {}", e);
        }
    }
    Ok(())
}
