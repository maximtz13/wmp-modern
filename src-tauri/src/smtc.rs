// Now Playing via SMTC (System Media Transport Controls).
//
// Architecture: a single background thread initializes COM for itself (MTA), then
// polls SMTC once a second and writes the result to a shared cache. The Tauri
// command just reads the cache — never blocks on WinRT — so the main thread
// can't freeze even if SMTC hangs.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference, InputStreamOptions};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

#[derive(Serialize, Default, Clone)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub status: String,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub playback_rate: f64,
    pub last_updated_unix_ms: i64,
    pub source_app: String,
    pub thumbnail: Option<String>,
}

#[derive(Serialize, Default, Clone)]
pub struct NowPlayingSnapshot {
    pub current: Option<NowPlaying>,
    pub error: Option<String>,
    /// Wall-clock ms (process-monotonic — `Instant::elapsed` since startup).
    /// Frontend uses this to know if polling is alive.
    pub poll_age_ms: u64,
}

struct CacheData {
    current: Option<NowPlaying>,
    error: Option<String>,
    last_poll: std::time::Instant,
}

static CACHE: OnceLock<Mutex<CacheData>> = OnceLock::new();

fn cache() -> &'static Mutex<CacheData> {
    CACHE.get_or_init(|| {
        Mutex::new(CacheData {
            current: None,
            error: Some("poller has not started yet".into()),
            last_poll: std::time::Instant::now(),
        })
    })
}

/// Normalized exe basename (lowercase, no .exe) of the app the user wants to track.
/// None = use SMTC's "current session" (system-chosen).
static TARGET_EXE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn target_exe() -> &'static Mutex<Option<String>> {
    TARGET_EXE.get_or_init(|| Mutex::new(None))
}

#[tauri::command]
pub fn set_smtc_target_exe(exe: Option<String>) {
    let normalized = exe.map(|s| {
        let s = s.trim().to_lowercase();
        s.strip_suffix(".exe").map(|s| s.to_string()).unwrap_or(s)
    });
    *target_exe().lock().unwrap() = normalized;
}

/// Try to pick the SMTC session matching the user-selected exe. Falls back to
/// SMTC's "current" session when no target is set or no session matches.
fn pick_session(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Option<GlobalSystemMediaTransportControlsSession> {
    let target = target_exe().lock().unwrap().clone();
    if let Some(target_base) = target {
        if let Ok(sessions) = manager.GetSessions() {
            let size = sessions.Size().unwrap_or(0);
            for i in 0..size {
                if let Ok(session) = sessions.GetAt(i) {
                    if let Ok(aumid) = session.SourceAppUserModelId() {
                        let aumid_str = aumid.to_string();
                        let aumid_base = normalize_aumid(&aumid_str);
                        if aumid_base == target_base {
                            return Some(session);
                        }
                    }
                }
            }
        }
    }
    manager.GetCurrentSession().ok()
}

fn normalize_aumid(aumid: &str) -> String {
    // "Spotify.exe"                                        → "spotify"
    // "Microsoft.Edge_8wekyb3d8bbwe!App"                   → "microsoft.edge_8wekyb3d8bbwe"
    // (UWP AUMIDs won't match a plain desktop exe name; that's a known gap.)
    let s = aumid.split('!').next().unwrap_or(aumid);
    let lower = s.to_lowercase();
    lower.strip_suffix(".exe").map(|s| s.to_string()).unwrap_or(lower)
}

/// Spawn the SMTC poller thread. Call this exactly once at app startup.
pub fn spawn_poller() {
    let _ = cache(); // ensure initialized

    std::thread::Builder::new()
        .name("smtc-poller".into())
        .spawn(poller_thread)
        .expect("failed to spawn smtc poller thread");
}

fn poller_thread() {
    // Initialize COM as multi-threaded apartment for the lifetime of this thread.
    // Safe to call repeatedly; subsequent calls return S_FALSE.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    loop {
        let now = std::time::Instant::now();
        let result = pollster::block_on(fetch_now_playing());

        {
            let mut data = cache().lock().unwrap();
            data.last_poll = now;
            match result {
                Ok(maybe_np) => {
                    data.current = maybe_np;
                    data.error = None;
                }
                Err(e) => {
                    data.current = None;
                    data.error = Some(format!(
                        "HRESULT=0x{:08X} {}",
                        e.code().0,
                        e.message()
                    ));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1000));
    }
}

#[tauri::command]
pub fn get_now_playing() -> NowPlayingSnapshot {
    let data = cache().lock().unwrap();
    NowPlayingSnapshot {
        current: data.current.clone(),
        error: data.error.clone(),
        poll_age_ms: data.last_poll.elapsed().as_millis() as u64,
    }
}

// ─── SMTC WinRT body ─────────────────────────────────────────────────

async fn fetch_now_playing() -> windows::core::Result<Option<NowPlaying>> {
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.await?;

    let session = match pick_session(&manager) {
        Some(s) => s,
        None => return Ok(None),
    };

    let props = match session.TryGetMediaPropertiesAsync() {
        Ok(op) => match op.await {
            Ok(p) => p,
            Err(_) => return Ok(None),
        },
        Err(_) => return Ok(None),
    };

    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    let album = props.AlbumTitle().map(|s| s.to_string()).unwrap_or_default();

    let playback = session.GetPlaybackInfo()?;
    let status = match playback.PlaybackStatus() {
        Ok(PlaybackStatus::Playing) => "playing",
        Ok(PlaybackStatus::Paused) => "paused",
        Ok(PlaybackStatus::Stopped) => "stopped",
        Ok(PlaybackStatus::Changing) => "changing",
        Ok(PlaybackStatus::Opened) => "opened",
        Ok(PlaybackStatus::Closed) => "closed",
        _ => "unknown",
    }
    .to_string();

    let playback_rate = playback
        .PlaybackRate()
        .ok()
        .and_then(|r| r.Value().ok())
        .unwrap_or(1.0);

    let timeline = session.GetTimelineProperties()?;
    let position_ticks = timeline.Position().map(|t| t.Duration).unwrap_or(0);
    let duration_ticks = timeline.EndTime().map(|t| t.Duration).unwrap_or(0);
    let last_updated_ticks = timeline
        .LastUpdatedTime()
        .map(|d| d.UniversalTime)
        .unwrap_or(0);

    // FILETIME 1601 epoch → Unix 1970 epoch.
    const FILETIME_TO_UNIX_TICKS: i64 = 116_444_736_000_000_000;
    let last_updated_unix_ms = (last_updated_ticks - FILETIME_TO_UNIX_TICKS) / 10_000;

    let source_app = session
        .SourceAppUserModelId()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let thumbnail = match props.Thumbnail() {
        Ok(thumb_ref) => read_thumbnail(&thumb_ref).await.ok(),
        Err(_) => None,
    };

    Ok(Some(NowPlaying {
        title,
        artist,
        album,
        status,
        position_ms: position_ticks / 10_000,
        duration_ms: duration_ticks / 10_000,
        playback_rate,
        last_updated_unix_ms,
        source_app,
        thumbnail,
    }))
}

async fn read_thumbnail(thumb: &IRandomAccessStreamReference) -> windows::core::Result<String> {
    let stream = thumb.OpenReadAsync()?.await?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(-1),
            "empty thumbnail",
        ));
    }
    let content_type = stream.ContentType().map(|s| s.to_string()).unwrap_or_default();

    let reader = DataReader::CreateDataReader(&stream)?;
    reader.SetInputStreamOptions(InputStreamOptions::None)?;
    reader.LoadAsync(size)?.await?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    let mime = if content_type.is_empty() {
        "image/jpeg".to_string()
    } else {
        content_type
    };
    Ok(format!("data:{mime};base64,{b64}"))
}
