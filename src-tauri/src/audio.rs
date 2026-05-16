use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use parking_lot::Mutex;
use rustfft::{num_complex::Complex32, FftPlanner};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use wasapi::{
    initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, SessionState,
    StreamMode, WaveFormat,
};
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

const FFT_SIZE: usize = 1024;
const HOP_SIZE: usize = 512;
const SAMPLE_RATE: usize = 48_000;
const N_BINS_EMITTED: usize = 256;
const N_WAVE_EMITTED: usize = 256;

// Hardcoded for process loopback because the API's device-period query is broken in that mode.
// 10 ms is a safe middle ground — large enough to avoid xruns, small enough for ~90 Hz visual updates.
const PROC_LOOPBACK_PERIOD_HNS: i64 = 100_000;

#[derive(Clone, Serialize)]
pub struct SpectrumFrame {
    pub bins: Vec<f32>,
    pub waveform: Vec<f32>,
    pub rms: f32,
}

#[derive(Clone, Serialize)]
pub struct AudioSession {
    pub pid: u32,
    pub exe: String,        // e.g. "Spotify.exe"
    pub display: String,    // e.g. "Spotify"
    pub is_active: bool,    // currently producing audio
}

#[derive(Default)]
pub struct CaptureState {
    stop_flag: Mutex<Option<Arc<AtomicBool>>>,
}

// ─── Commands ─────────────────────────────────────────────────────────────

#[tauri::command]
pub fn start_capture(
    app: AppHandle,
    state: State<'_, CaptureState>,
    pid: Option<u32>,
) -> Result<(), String> {
    stop_internal(&state);
    let stop = Arc::new(AtomicBool::new(false));
    *state.stop_flag.lock() = Some(stop.clone());
    thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(app, stop, pid) {
                eprintln!("audio capture loop ended: {e:?}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn stop_capture(state: State<'_, CaptureState>) {
    stop_internal(&state);
}

#[tauri::command]
pub fn list_audio_sessions() -> Result<Vec<AudioSession>, String> {
    enumerate_sessions().map_err(|e| e.to_string())
}

// ─── Internals ────────────────────────────────────────────────────────────

fn stop_internal(state: &CaptureState) {
    if let Some(flag) = state.stop_flag.lock().take() {
        flag.store(true, Ordering::SeqCst);
    }
}

// Processes that hold audio sessions but are not what a user means by "an app playing music".
// These are Windows internals or shell components that surface ambient system sounds.
const SYSTEM_PROCESS_DENYLIST: &[&str] = &[
    // Our own app — don't show ourselves as a source.
    "wmp-modern.exe",
    // Windows shell + system processes that surface ambient sounds.
    "explorer.exe",
    "audiodg.exe",
    "dwm.exe",
    "svchost.exe",
    "taskhostw.exe",
    "RuntimeBroker.exe",
    "SearchHost.exe",
    "StartMenuExperienceHost.exe",
    "ShellExperienceHost.exe",
    "TextInputHost.exe",
    "ApplicationFrameHost.exe",
    "winlogon.exe",
    "csrss.exe",
    "fontdrvhost.exe",
    "smss.exe",
];

fn is_system_process(exe: &str) -> bool {
    SYSTEM_PROCESS_DENYLIST
        .iter()
        .any(|d| d.eq_ignore_ascii_case(exe))
}

fn enumerate_sessions() -> Result<Vec<AudioSession>> {
    let _ = initialize_mta();
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&Direction::Render)?;
    let session_manager = device.get_iaudiosessionmanager()?;
    let session_enumerator = session_manager.get_audiosessionenumerator()?;
    let count: i32 = session_enumerator.get_count()?;

    // PIDs can appear multiple times (e.g., one session per output stream). Dedupe by PID,
    // taking the "most active" state we see across duplicates.
    let mut by_pid: HashMap<u32, AudioSession> = HashMap::new();
    for i in 0..count {
        let session = match session_enumerator.get_session(i) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let pid = match session.get_process_id() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == 0 {
            continue; // system sounds
        }
        let state = session.get_state().unwrap_or(SessionState::Expired);
        let is_active = matches!(state, SessionState::Active);

        let exe = match process_exe_name(pid) {
            Some(name) => name,
            None => continue, // can't identify process, skip
        };
        if is_system_process(&exe) {
            continue;
        }
        let display = exe
            .strip_suffix(".exe")
            .or_else(|| exe.strip_suffix(".EXE"))
            .unwrap_or(&exe)
            .to_string();

        // If we've seen this PID, only overwrite if the new entry is active and the old one wasn't.
        by_pid
            .entry(pid)
            .and_modify(|existing| {
                if is_active && !existing.is_active {
                    existing.is_active = true;
                }
            })
            .or_insert(AudioSession {
                pid,
                exe,
                display,
                is_active,
            });
    }

    let mut out: Vec<AudioSession> = by_pid.into_values().collect();
    // Active first, then alphabetical by display name.
    out.sort_by(|a, b| {
        b.is_active
            .cmp(&a.is_active)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
    });
    Ok(out)
}

fn process_exe_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0), // Win32 path
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if res.is_err() || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
    }
}

fn capture_loop(app: AppHandle, stop: Arc<AtomicBool>, pid: Option<u32>) -> Result<()> {
    let _ = initialize_mta();

    let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, 2, None);
    let blockalign = format.get_blockalign() as usize;

    let (mut audio_client, buffer_hns): (AudioClient, i64) = if let Some(pid) = pid {
        // Per-process loopback. The classic device-period query is broken here — use a fixed value.
        let client = AudioClient::new_application_loopback_client(pid, true)?;
        (client, PROC_LOOPBACK_PERIOD_HNS)
    } else {
        // System-wide loopback: open the default render device in capture direction.
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_default_device(&Direction::Render)?;
        let client = device.get_iaudioclient()?;
        let (_def_t, min_t) = client.get_device_period()?;
        (client, min_t)
    };

    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: buffer_hns,
    };
    audio_client.initialize_client(&format, &Direction::Capture, &mode)?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let hann: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            let x = i as f32 / (FFT_SIZE as f32 - 1.0);
            0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
        })
        .collect();

    let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(blockalign * 8192);
    let mut mono: Vec<f32> = Vec::with_capacity(FFT_SIZE * 4);
    let mut scratch = vec![Complex32::default(); FFT_SIZE];

    audio_client.start_stream()?;

    while !stop.load(Ordering::SeqCst) {
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        capture_client.read_from_device_to_deque(&mut byte_queue)?;

        let aligned = byte_queue.len() - (byte_queue.len() % blockalign);
        if aligned > 0 {
            let drained: Vec<u8> = byte_queue.drain(..aligned).collect();
            for chunk in drained.chunks_exact(blockalign) {
                let l = f32::from_le_bytes(chunk[0..4].try_into().unwrap());
                let r = f32::from_le_bytes(chunk[4..8].try_into().unwrap());
                mono.push((l + r) * 0.5);
            }
        }

        while mono.len() >= FFT_SIZE {
            for i in 0..FFT_SIZE {
                scratch[i] = Complex32 { re: mono[i] * hann[i], im: 0.0 };
            }
            let rms = {
                let sumsq: f32 = mono[..FFT_SIZE].iter().map(|x| x * x).sum();
                (sumsq / FFT_SIZE as f32).sqrt()
            };
            let waveform: Vec<f32> = mono[..FFT_SIZE]
                .iter()
                .step_by(FFT_SIZE / N_WAVE_EMITTED)
                .copied()
                .collect();
            fft.process(&mut scratch);
            let scale = 1.0 / FFT_SIZE as f32;
            let bins: Vec<f32> = scratch
                .iter()
                .take(N_BINS_EMITTED)
                .map(|c| (c.re * c.re + c.im * c.im).sqrt() * scale)
                .collect();
            let _ = app.emit("audio-spectrum", SpectrumFrame { bins, waveform, rms });
            mono.drain(..HOP_SIZE);
        }
    }

    audio_client.stop_stream()?;
    Ok(())
}
