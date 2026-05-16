use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use parking_lot::Mutex;
use rustfft::{num_complex::Complex32, FftPlanner};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use wasapi::{
    initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

const FFT_SIZE: usize = 1024;
const HOP_SIZE: usize = 512;
const SAMPLE_RATE: usize = 48_000;
const N_BINS_EMITTED: usize = 256;
const N_WAVE_EMITTED: usize = 256; // 4x decimation of the 1024-sample window.

#[derive(Clone, Serialize)]
pub struct SpectrumFrame {
    pub bins: Vec<f32>,
    pub waveform: Vec<f32>,
    pub rms: f32,
}

#[derive(Default)]
pub struct CaptureState {
    stop_flag: Mutex<Option<Arc<AtomicBool>>>,
}

#[tauri::command]
pub fn start_capture(app: AppHandle, state: State<'_, CaptureState>) -> Result<(), String> {
    stop_internal(&state);
    let stop = Arc::new(AtomicBool::new(false));
    *state.stop_flag.lock() = Some(stop.clone());
    thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(app, stop) {
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

fn stop_internal(state: &CaptureState) {
    if let Some(flag) = state.stop_flag.lock().take() {
        flag.store(true, Ordering::SeqCst);
    }
}

fn capture_loop(app: AppHandle, stop: Arc<AtomicBool>) -> Result<()> {
    // MTA init: safe to call repeatedly; ignore "already initialized".
    let _ = initialize_mta();

    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&Direction::Render)?;
    let mut audio_client = device.get_iaudioclient()?;

    let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, 2, None);
    let blockalign = format.get_blockalign() as usize;

    let (_def_t, min_t) = audio_client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_t,
    };
    // Render device + Capture direction = loopback (wasapi sets AUDCLNT_STREAMFLAGS_LOOPBACK).
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
                scratch[i] = Complex32 {
                    re: mono[i] * hann[i],
                    im: 0.0,
                };
            }
            let rms = {
                let sumsq: f32 = mono[..FFT_SIZE].iter().map(|x| x * x).sum();
                (sumsq / FFT_SIZE as f32).sqrt()
            };
            // Decimated raw waveform for the waveform ring.
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
