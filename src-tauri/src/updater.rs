//! Self-update from GitHub releases (tauri-plugin-updater; the endpoint and
//! public key are in tauri.conf.json). Checks shortly after launch and every
//! few hours, downloads and installs in the background, then restarts the app
//! the next time it's idle with every window closed. The tray menu and
//! Settings offer "Restart to update" in the meantime.
//!
//! On Windows installing means running the new installer, which quits the app
//! at once; so there it only downloads in the background, and runs the
//! installer (which starts the new version) at that same idle moment.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

use crate::tray::{Mood, TrayAnimator};

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
const IDLE_POLL: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    pub current: String,
    /// "idle" | "checking" | "downloading" | "ready" | "error" | "disabled"
    pub phase: &'static str,
    /// The new version, while downloading or once ready.
    pub version: Option<String>,
    pub error: Option<String>,
    /// Unix ms of the last finished check.
    pub checked_at: Option<i64>,
}

pub struct Updates {
    state: Mutex<UpdateState>,
    running: tokio::sync::Mutex<()>,
    /// Windows: the downloaded update, until it's installed.
    #[cfg(windows)]
    downloaded: Mutex<Option<(tauri_plugin_updater::Update, Vec<u8>)>>,
}

impl Updates {
    pub fn new(current: String) -> Arc<Self> {
        // Debug builds aren't an installed .app, so there's nothing to replace.
        let phase = if cfg!(debug_assertions) { "disabled" } else { "idle" };
        Arc::new(Self {
            state: Mutex::new(UpdateState { current, phase, version: None, error: None, checked_at: None }),
            running: tokio::sync::Mutex::new(()),
            #[cfg(windows)]
            downloaded: Mutex::new(None),
        })
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().unwrap().clone()
    }

    fn set(&self, app: &AppHandle, f: impl FnOnce(&mut UpdateState)) {
        let state = {
            let mut s = self.state.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        let _ = app.emit("lg:update", &state);
    }

    /// Check now; if there's a newer release, download and install it.
    pub async fn check(&self, app: &AppHandle) -> UpdateState {
        if matches!(self.state().phase, "disabled" | "ready") {
            return self.state();
        }
        let Ok(_guard) = self.running.try_lock() else { return self.state() };
        self.set(app, |s| (s.phase, s.error) = ("checking", None));
        match self.check_and_install(app).await {
            Ok(Some(version)) => {
                self.set(app, |s| {
                    s.phase = "ready";
                    s.version = Some(version.clone());
                    s.checked_at = Some(chrono::Utc::now().timestamp_millis());
                });
                crate::tray::set_update_ready(app, &version);
                let _ = app
                    .notification()
                    .builder()
                    .title(format!("littleguys {version} is ready"))
                    .body(format!(
                        "It'll restart itself next time you're away, or restart now from the {}.",
                        crate::platform::TRAY
                    ))
                    .show();
            }
            Ok(None) => self.set(app, |s| {
                s.phase = "idle";
                s.version = None;
                s.checked_at = Some(chrono::Utc::now().timestamp_millis());
            }),
            Err(e) => {
                log::warn!("update check failed: {e:#}");
                self.set(app, |s| {
                    s.phase = "error";
                    s.error = Some(e.to_string());
                    s.checked_at = Some(chrono::Utc::now().timestamp_millis());
                });
            }
        }
        self.state()
    }

    async fn check_and_install(&self, app: &AppHandle) -> anyhow::Result<Option<String>> {
        let Some(update) = app.updater()?.check().await? else { return Ok(None) };
        let version = update.version.clone();
        log::info!("downloading littleguys {version}");
        self.set(app, |s| (s.phase, s.version) = ("downloading", Some(version.clone())));
        #[cfg(windows)]
        {
            let bytes = update.download(|_, _| {}, || {}).await?;
            *self.downloaded.lock().unwrap() = Some((update, bytes));
            log::info!("downloaded littleguys {version}; installs when idle");
        }
        #[cfg(not(windows))]
        {
            update.download_and_install(|_, _| {}, || {}).await?;
            log::info!("installed littleguys {version}; restarts when idle");
        }
        Ok(Some(version))
    }
}

/// Start the installed update: relaunch (macOS), or run the downloaded
/// installer, which quits littleguys and opens the new version (Windows).
pub fn restart(app: &AppHandle) {
    #[cfg(windows)]
    {
        let updates = app.state::<Arc<Updates>>();
        let downloaded = updates.downloaded.lock().unwrap().take();
        if let Some((update, bytes)) = downloaded {
            // Only returns if the installer didn't start.
            let error = update.install(&bytes).map_err(|e| e.to_string()).err().unwrap_or_default();
            log::warn!("could not start the littleguys {} installer: {error}", update.version);
            updates.set(app, |s| (s.phase, s.version, s.error) = ("error", None, Some(error)));
            return;
        }
    }
    app.restart();
}

/// Background loop: periodic checks, then a quiet restart once an update is
/// installed and nobody's using the app.
pub fn start(app: AppHandle, updates: Arc<Updates>, tray: TrayAnimator) {
    if updates.state().phase == "disabled" {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        let mut next_check = tokio::time::Instant::now();
        loop {
            if updates.state().phase == "ready" {
                if is_idle(&app, &tray) {
                    log::info!("restarting into the installed update");
                    restart(&app);
                }
            } else if tokio::time::Instant::now() >= next_check {
                updates.check(&app).await;
                next_check = tokio::time::Instant::now() + CHECK_EVERY;
            }
            tokio::time::sleep(IDLE_POLL).await;
        }
    });
}

/// No window on screen, and no guy working or waiting on an approval: ACP
/// agents and their routines run inside littleguys, so a restart would cut
/// them off.
fn is_idle(app: &AppHandle, tray: &TrayAnimator) -> bool {
    let any_visible = app.webview_windows().values().any(|w| w.is_visible().unwrap_or(false));
    !any_visible && !matches!(tray.mood(), Mood::Working | Mood::NeedsYou)
}
