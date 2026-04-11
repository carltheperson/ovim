//! Widget info Tauri commands

use crate::widgets::{battery, capslock, selection, shell};

#[tauri::command]
pub fn get_selection_info() -> selection::SelectionInfo {
    selection::get_selection_info()
}

#[tauri::command]
pub fn get_battery_info() -> Option<battery::BatteryInfo> {
    battery::get_battery_info()
}

#[tauri::command]
pub fn get_caps_lock_state() -> bool {
    capslock::is_caps_lock_on()
}

#[tauri::command]
pub async fn run_shell_widget(script: Option<String>, script_path: Option<String>) -> String {
    tokio::task::spawn_blocking(move || {
        shell::run_shell_script(script.as_deref(), script_path.as_deref())
    })
    .await
    .unwrap_or_else(|e| format!("err: {}", e))
}

/// Log message from webview to /tmp/ovim-webview.log
#[tauri::command]
pub fn webview_log(level: String, message: String) {
    use std::fs::OpenOptions;
    use std::io::Write;

    let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
    let line = format!(
        "[{}] {} - {}\n",
        timestamp,
        level.to_uppercase(),
        message
    );

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/ovim-webview.log")
    {
        let _ = file.write_all(line.as_bytes());
    }

    match level.to_lowercase().as_str() {
        "error" => log::error!("[webview] {}", message),
        "warn" => log::warn!("[webview] {}", message),
        "debug" => log::debug!("[webview] {}", message),
        _ => log::info!("[webview] {}", message),
    }
}
