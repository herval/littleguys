//! The two windows: the dropdown popover anchored under the menu-bar guy (on
//! Windows, above the taskbar's), and the main chat window that folds back into
//! the menu bar when closed.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow, WindowEvent};
use tauri_plugin_positioner::{Position, WindowExt};

use crate::gateway::identity::now_ms;

pub const MAIN: &str = "main";
pub const POPOVER: &str = "popover";

/// When the popover last hid because it lost focus. A click on the tray icon
/// blurs the popover *before* the tray click arrives; without this the click
/// would immediately re-open it.
static POPOVER_BLURRED_AT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Navigate {
    pub gateway_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_key: Option<String>,
    pub view: Option<String>,
}

pub fn setup(app: &AppHandle) {
    if let Some(popover) = app.get_webview_window(POPOVER) {
        #[cfg(target_os = "macos")]
        {
            use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
            let _ = apply_vibrancy(&popover, NSVisualEffectMaterial::Popover, Some(NSVisualEffectState::Active), Some(12.0));
        }
        // Windows 11 flyouts use Acrylic; Windows 10 gets the older acrylic blur, tinted.
        #[cfg(windows)]
        let _ = window_vibrancy::apply_acrylic(&popover, Some(acrylic_tint(&popover)));
        let _ = popover.set_visible_on_all_workspaces(true);
        let handle = popover.clone();
        popover.on_window_event(move |event| {
            if let WindowEvent::Focused(false) = event {
                POPOVER_BLURRED_AT.store(now_ms(), Ordering::SeqCst);
                hide(&handle);
            }
        });
    }
    if let Some(main) = app.get_webview_window(MAIN) {
        #[cfg(target_os = "macos")]
        {
            use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
            let _ = apply_vibrancy(&main, NSVisualEffectMaterial::Sidebar, Some(NSVisualEffectState::FollowsWindowActiveState), None);
        }
        // Mica (Windows 11), else acrylic (Windows 10): the page is see-through.
        #[cfg(windows)]
        if window_vibrancy::apply_mica(&main, None).is_err() {
            let _ = window_vibrancy::apply_acrylic(&main, Some(acrylic_tint(&main)));
        }
        let app_handle = app.clone();
        let handle = main.clone();
        main.on_window_event(move |event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = handle.hide();
                set_dock_visible(&app_handle, false);
            }
            WindowEvent::Focused(focused) => {
                let _ = handle.emit("lg:main-focus", *focused);
            }
            WindowEvent::ThemeChanged(_) => crate::tray::read_taskbar_theme(),
            _ => {}
        });
    }
}

pub fn toggle_popover(app: &AppHandle) {
    let Some(popover) = app.get_webview_window(POPOVER) else { return };
    if popover.is_visible().unwrap_or(false) {
        hide(&popover);
        return;
    }
    if now_ms().saturating_sub(POPOVER_BLURRED_AT.load(Ordering::SeqCst)) < 250 {
        return;
    }
    show_popover(&popover);
}

/// Where the popover goes: under the menu-bar guy on macOS; above the tray icon
/// on Windows (the taskbar is usually at the bottom; below it when it's at the top).
fn place_popover(popover: &WebviewWindow) {
    #[cfg(target_os = "macos")]
    let _ = popover.move_window(Position::TrayBottomCenter);
    #[cfg(not(target_os = "macos"))]
    let _ = popover.move_window_constrained(Position::TrayCenter);
}

fn show_popover(popover: &WebviewWindow) {
    place_popover(popover);
    let _ = popover.show();
    let _ = popover.set_focus();
    let _ = popover.emit("lg:popover-shown", ());
}

fn hide(popover: &WebviewWindow) {
    let _ = popover.hide();
    let _ = popover.emit("lg:popover-hidden", ());
}

pub fn hide_popover(app: &AppHandle) {
    if let Some(popover) = app.get_webview_window(POPOVER) {
        hide(&popover);
    }
}

/// Resize the popover to fit its content, keeping it anchored to the tray.
pub fn fit_popover(app: &AppHandle, height: f64) {
    let Some(popover) = app.get_webview_window(POPOVER) else { return };
    let height = height.clamp(160.0, 640.0);
    let _ = popover.set_size(tauri::LogicalSize::new(340.0, height));
    if popover.is_visible().unwrap_or(false) {
        place_popover(&popover);
    }
}

pub fn open_main(app: &AppHandle, nav: Option<Navigate>) {
    hide_popover(app);
    let Some(main) = app.get_webview_window(MAIN) else { return };
    set_dock_visible(app, true);
    let _ = main.unminimize();
    let _ = main.show();
    let _ = main.set_focus();
    if let Some(nav) = nav {
        let _ = main.emit("lg:navigate", nav);
    }
}

/// The app lives in the menu bar; it only shows in the Dock / ⌘Tab while the
/// main window is open.
/// (Windows and Linux: the window's taskbar button already comes and goes with it.)
pub fn set_dock_visible(app: &AppHandle, visible: bool) {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let policy = if visible { ActivationPolicy::Regular } else { ActivationPolicy::Accessory };
        let _ = app.set_activation_policy(policy);
        if visible {
            // Switching to Regular doesn't activate the app by itself.
            let _ = app.run_on_main_thread(activate_app);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
}

/// Windows 10's acrylic takes a tint (Windows 11 picks its own): the theme's
/// window color, mostly opaque so text stays readable.
#[cfg(windows)]
fn acrylic_tint(window: &WebviewWindow) -> (u8, u8, u8, u8) {
    match window.theme() {
        Ok(tauri::Theme::Dark) => (32, 30, 29, 216),
        _ => (243, 241, 239, 216),
    }
}

#[cfg(target_os = "macos")]
fn activate_app() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}
