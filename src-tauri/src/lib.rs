mod activity;
mod agent_setup;
mod avatar_file;
pub mod bridge;
mod browser_stream;
pub mod code;
mod commands;
mod doctor;
pub mod gateway;
pub mod gateways;
mod hub;
mod limits;
pub mod localfs;
#[cfg(target_os = "macos")]
mod mac_notifications;
mod media;
pub mod openclaw;
mod outbox;
pub mod platform;
pub mod runtime;
pub mod terminal;
mod tray;
mod updater;
mod windows;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use crate::activity::Activity;
use crate::hub::Hub;
use crate::tray::TrayAnimator;
use crate::updater::Updates;

#[cfg(target_os = "macos")]
fn install_settings_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuItem, MenuItemKind};

    const SETTINGS: &str = "app-settings";
    if let Some(menu) = app.menu() {
        if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.first() {
            let settings = MenuItem::with_id(app, SETTINGS, "Settings…", true, Some("Cmd+,"))?;
            app_menu.insert(&settings, 1)?;
            app.on_menu_event(|app, event| {
                if event.id().as_ref() == SETTINGS {
                    windows::open_main(app, Some(windows::Navigate {
                        view: Some("settings".into()),
                        ..Default::default()
                    }));
                }
            });
        }
    }
    Ok(())
}

pub fn run() {
    // Started to build a message index, out of the app's own memory: that, and nothing else.
    if let Some(code) = runtime::search::child_main() {
        std::process::exit(code);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=info")).init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ⌘⇧L on macOS, Ctrl+Shift+L elsewhere (Win+Shift+L is too close to locking the screen).
    let open_key = if cfg!(target_os = "macos") { Modifiers::SUPER } else { Modifiers::CONTROL };
    let open_shortcut = Shortcut::new(Some(open_key | Modifiers::SHIFT), Code::KeyL);

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
            commands::codex_configure,
            commands::codex_detect,
            agent_setup::agent_install,
            agent_setup::agent_sign_in,
            agent_setup::agent_sign_in_cancel,
            commands::gateway_setup_code,
            commands::acp_registry,
            commands::hermes_detect,
            commands::get_roster,
            commands::refresh_roster,
            commands::set_focused_session,
            commands::set_guy_hidden,
            commands::set_guy_steps,
            commands::set_notifications,
            commands::set_coder_prefs,
            commands::coder_tools,
            commands::coder_terminals,
            commands::open_terminal,
            commands::in_git_repo,
            commands::set_guy_face,
            commands::set_workspace_avatar,
            commands::open_main,
            commands::hide_popover,
            commands::fit_popover,
            commands::reveal_path,
            browser_stream::browser_stream_start,
            browser_stream::browser_stream_stop,
            browser_stream::browser_screenshot,
            limits::limits_read,
            limits::claude_status_line,
            media::gw_media,
            media::gw_path,
            media::save_media,
            bridge::commands::bridge_state,
            bridge::commands::bridge_set_enabled,
            bridge::commands::bridge_pair_start,
            bridge::commands::bridge_pair_cancel,
            bridge::commands::bridge_phone_remove,
            bridge::commands::bridge_phone_rename,
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
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                install_settings_menu(app)?;
            }

            let handle = app.handle().clone();
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;

            // Menu-bar guy
            tray::read_taskbar_theme();
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
                    "restart-update" => updater::restart(app),
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
            #[cfg(target_os = "macos")]
            mac_notifications::setup(&handle);

            // Self-update from GitHub releases
            let updates = Updates::new(app.package_info().version.to_string());
            updater::start(handle.clone(), updates.clone(), animator.clone());
            app.manage(updates);

            // Gateway connections + roster
            let activity = Activity::new(handle.clone(), animator);
            let hub = Hub::new(&handle, data_dir.clone(), app.package_info().version.to_string(), activity.clone())?;
            hub.start();

            // Paired phones (Settings → Phones)
            let config = bridge::BridgeConfig::new(data_dir, bridge::computer_name(), app.package_info().version.to_string());
            let host = bridge::app_host::AppHost::new(hub.clone(), activity.clone());
            let phones = bridge::Bridge::new(config, host, tauri::async_runtime::handle().inner().clone())?;
            let emitter = handle.clone();
            phones.on_state(move |state| {
                let _ = tauri::Emitter::emit(&emitter, "bridge:state", state);
            });
            phones.start();
            app.manage(phones);

            app.manage(hub);
            app.manage(activity);
            app.manage(std::sync::Arc::new(browser_stream::Streams::default()));

            if let Err(e) = app.global_shortcut().register(open_shortcut) {
                log::warn!("could not register the open shortcut: {e}");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building littleguys")
        .run(|app, event| {
            // Dock/Finder reopen should always restore the chat, even if the
            // popover is visible or the main window is minimized.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                windows::open_main(app, None);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}
