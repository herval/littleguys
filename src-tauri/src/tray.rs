//! The little guy in the menu bar: a template-image sprite animated per mood.
//! Frames come from scripts/gen-icons.mjs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Wry};
use tokio::sync::Notify;

pub const TRAY_ID: &str = "littleguys";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mood {
    Idle,
    Working,
    News,
    NeedsYou,
    Offline,
    Setup,
}

macro_rules! frame {
    ($name:literal) => {
        ($name, include_bytes!(concat!("../assets/tray/", $name, ".png")).as_slice())
    };
}

const FRAMES: &[(&str, &[u8])] = &[
    frame!("idle-0"),
    frame!("idle-1"),
    frame!("idle-2"),
    frame!("work-0"),
    frame!("work-1"),
    frame!("work-2"),
    frame!("work-3"),
    frame!("news-0"),
    frame!("news-1"),
    frame!("needs-0"),
    frame!("needs-1"),
    frame!("off-0"),
    frame!("off-1"),
    frame!("off-2"),
    frame!("setup-0"),
    frame!("setup-1"),
    frame!("setup-2"),
];

/// (frame, how long to hold it). The sequence loops.
fn schedule(mood: Mood) -> &'static [(&'static str, u64)] {
    match mood {
        Mood::Idle => &[("idle-0", 3600), ("idle-1", 140), ("idle-0", 2400), ("idle-2", 900), ("idle-0", 1800), ("idle-1", 130)],
        // Calm on purpose: a busy guy can work for hours.
        Mood::Working => &[("work-0", 380), ("work-1", 380), ("work-2", 380), ("work-3", 380), ("work-0", 1400)],
        Mood::News => &[("news-0", 5000), ("news-1", 260), ("news-0", 260), ("news-1", 260)],
        Mood::NeedsYou => &[("needs-0", 520), ("needs-1", 520)],
        Mood::Offline => &[("off-0", 800), ("off-1", 800), ("off-2", 800)],
        Mood::Setup => &[("setup-0", 320), ("setup-1", 320), ("setup-2", 320)],
    }
}

/// The right-click menu. Once an update is installed, it offers the restart.
pub fn menu(app: &AppHandle, update: Option<&str>) -> tauri::Result<Menu<Wry>> {
    let open = MenuItem::with_id(app, "open", "Open littleguys", true, Some("CmdOrCtrl+Shift+L"))?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit littleguys", true, Some("CmdOrCtrl+Q"))?;
    match update {
        Some(version) => {
            let restart = MenuItem::with_id(app, "restart-update", format!("Restart to Update to {version}"), true, None::<&str>)?;
            Menu::with_items(app, &[&open, &restart, &separator, &quit])
        }
        None => Menu::with_items(app, &[&open, &separator, &quit]),
    }
}

pub fn set_update_ready(app: &AppHandle, version: &str) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY_ID), menu(app, Some(version))) {
        let _ = tray.set_menu(Some(menu));
    }
}

pub fn first_frame() -> Image<'static> {
    Image::from_bytes(FRAMES[0].1).expect("tray frame decodes")
}

#[derive(Clone)]
pub struct TrayAnimator {
    mood: Arc<Mutex<Mood>>,
    changed: Arc<Notify>,
}

impl TrayAnimator {
    pub fn new() -> Self {
        Self { mood: Arc::new(Mutex::new(Mood::Setup)), changed: Arc::new(Notify::new()) }
    }

    pub fn mood(&self) -> Mood {
        *self.mood.lock().unwrap()
    }

    pub fn set_mood(&self, mood: Mood) {
        let mut current = self.mood.lock().unwrap();
        if *current != mood {
            *current = mood;
            self.changed.notify_one();
        }
    }

    pub fn start(&self, app: AppHandle) {
        let images: HashMap<&'static str, Image<'static>> = FRAMES
            .iter()
            .filter_map(|(name, bytes)| Image::from_bytes(bytes).ok().map(|img| (*name, img)))
            .collect();
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut shown: Option<&'static str> = None;
            loop {
                let mood = *this.mood.lock().unwrap();
                let tooltip = tooltip(mood);
                if let Some(tray) = app.tray_by_id(TRAY_ID) {
                    let _ = tray.set_tooltip(Some(tooltip));
                }
                'mood: loop {
                    for (frame, hold) in schedule(mood) {
                        if shown != Some(*frame) {
                            if let (Some(tray), Some(image)) = (app.tray_by_id(TRAY_ID), images.get(frame)) {
                                // One call: set_icon alone draws the frame as a
                                // non-template image until the template flag lands.
                                let _ = tray.set_icon_with_as_template(Some(image.clone()), true);
                            }
                            shown = Some(*frame);
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(*hold)) => {}
                            _ = this.changed.notified() => break 'mood,
                        }
                    }
                }
            }
        });
    }
}

fn tooltip(mood: Mood) -> &'static str {
    match mood {
        Mood::Idle => "littleguys — all quiet",
        Mood::Working => "littleguys — working on it…",
        Mood::News => "littleguys — new replies",
        Mood::NeedsYou => "littleguys — needs you",
        Mood::Offline => "littleguys — gateway offline",
        Mood::Setup => "littleguys — connecting…",
    }
}
