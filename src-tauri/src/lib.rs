mod audio;
mod smtc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Background SMTC poller; runs for the lifetime of the process and
    // updates a shared cache. The Tauri command just reads the cache.
    smtc::spawn_poller();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(audio::CaptureState::default())
        .invoke_handler(tauri::generate_handler![
            audio::start_capture,
            audio::stop_capture,
            audio::list_audio_sessions,
            smtc::get_now_playing,
            smtc::set_smtc_target_exe,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
