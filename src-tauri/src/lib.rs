mod activity;
mod browser_stream;
mod commands;
mod doctor;
pub mod gateway;
pub mod gateways;
mod hub;
pub mod localfs;
mod media;
pub mod openclaw;
pub mod runtime;
mod tray;
mod updater;
mod windows;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, RunEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use crate::activity::Activity;
use crate::hub::Hub;
use crate::tray::TrayAnimator;
use crate::updater::Updates;

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=info")).init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let open_shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyL);

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            windows::open_main(app, None);
        }))
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if shortcut == &open_shortcut && event.state() == ShortcutState::Pressed {
                        windows::open_main(app, None);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            commands::gw_call,
            commands::gw_statuses,
            commands::gw_reconnect,
            commands::gw_set_credential,
            commands::gateway_add,
            commands::gateway_update,
            commands::gateway_remove,
            commands::gateways_discover,
            commands::bot_add,
            commands::claude_code_detect,
            commands::claude_code_install_plugin,
            commands::claude_code_configure,
            commands::gateway_setup_code,
            commands::acp_registry,
            commands::hermes_detect,
            commands::get_roster,
            commands::refresh_roster,
            commands::set_focused_session,
            commands::set_guy_hidden,
            commands::open_main,
            commands::hide_popover,
            commands::fit_popover,
            commands::reveal_path,
            browser_stream::browser_stream_start,
            browser_stream::browser_stream_stop,
            browser_stream::browser_screenshot,
            media::gw_media,
            media::save_media,
            commands::quit,
            commands::update_state,
            commands::update_check,
            commands::update_restart,
            doctor::doctor_check,
            doctor::doctor_install_openclaw,
            doctor::doctor_update_openclaw,
            doctor::doctor_start_gateway,
            doctor::doctor_onboard,
            doctor::doctor_gateway_log,
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;

            // Menu-bar guy
            let animator = TrayAnimator::new();
            let menu = tray::menu(&handle, None)?;
            TrayIconBuilder::with_id(tray::TRAY_ID)
                .icon(tray::first_frame())
                .icon_as_template(true)
                .tooltip("littleguys")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => windows::open_main(app, None),
                    "quit" => app.exit(0),
                    "restart-update" => app.restart(),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        windows::toggle_popover(tray.app_handle());
                    }
                })
                .build(app)?;
            animator.start(handle.clone());

            windows::setup(&handle);

            // Self-update from GitHub releases
            let updates = Updates::new(app.package_info().version.to_string());
            updater::start(handle.clone(), updates.clone(), animator.clone());
            app.manage(updates);

            // Gateway connections + roster
            let activity = Activity::new(handle.clone(), animator);
            let hub = Hub::new(&handle, data_dir, app.package_info().version.to_string(), activity.clone())?;
            hub.start();
            app.manage(hub);
            app.manage(activity);
            app.manage(std::sync::Arc::new(browser_stream::Streams::default()));

            if let Err(e) = app.global_shortcut().register(open_shortcut) {
                log::warn!("could not register ⌘⇧L: {e}");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building littleguys")
        .run(|app, event| {
            if let RunEvent::Reopen { has_visible_windows, .. } = event {
                if !has_visible_windows {
                    windows::open_main(app, None);
                }
            }
        });
}
