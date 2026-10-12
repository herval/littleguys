//! IPC for Settings → Phones.

use tauri::State;

use super::{Bridge, BridgeState, PairStart};

#[tauri::command]
pub fn bridge_state(bridge: State<'_, Bridge>) -> BridgeState {
    bridge.state()
}

/// "Let paired phones connect".
#[tauri::command]
pub fn bridge_set_enabled(bridge: State<'_, Bridge>, on: bool) -> Result<BridgeState, String> {
    bridge.set_enabled(on).map_err(|e| e.to_string())?;
    Ok(bridge.state())
}

/// A fresh pairing link (show it as a QR code); valid for 10 minutes, once.
#[tauri::command]
pub async fn bridge_pair_start(bridge: State<'_, Bridge>) -> Result<PairStart, String> {
    bridge.pair_start().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn bridge_pair_cancel(bridge: State<'_, Bridge>) {
    bridge.pair_cancel();
}

#[tauri::command]
pub fn bridge_phone_remove(bridge: State<'_, Bridge>, key: String) -> Result<(), String> {
    bridge.remove_phone(&key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn bridge_phone_rename(bridge: State<'_, Bridge>, key: String, name: String) -> Result<(), String> {
    bridge.rename_phone(&key, &name).map_err(|e| e.to_string())
}
