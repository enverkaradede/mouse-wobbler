use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use enigo::{Coordinate, Enigo, Mouse, Settings as EnigoSettings};
use mouse_wobbler_core::{tick, AppStatus, SharedCore, WobblerCore, WobblerSettings};
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};

// ─── Background wobbler thread ────────────────────────────────────────────────

pub fn start_wobbler_thread<R: tauri::Runtime>(core: SharedCore, app: AppHandle<R>) {
    thread::spawn(move || {
        let mut enigo = match Enigo::new(&EnigoSettings::default()) {
            Ok(e) => e,
            Err(err) => {
                let mut c = core.lock().unwrap();
                c.last_error = Some(format!(
                    "Cannot control mouse: {err}. \
                     On macOS grant Accessibility in System Settings → Privacy & Security."
                ));
                drop(c);
                let _ = app.emit("status-update", build_status(&core));
                return;
            }
        };

        // Poll on a small fixed quantum so the configured wobble interval (which
        // may be as low as 200ms) is honoured precisely; a coarse sleep would
        // silently floor any sub-quantum interval. Status events are throttled
        // separately so the finer polling doesn't flood the UI.
        const POLL_QUANTUM: Duration = Duration::from_millis(100);
        const STATUS_EMIT_INTERVAL: Duration = Duration::from_millis(500);

        let mut prev_wobbling = false;
        let mut last_emit = Instant::now() - STATUS_EMIT_INTERVAL;

        loop {
            thread::sleep(POLL_QUANTUM);

            let current = match enigo.location() {
                Ok(p) => p,
                Err(_) => continue,
            };

            let (move_to, is_wobbling) = {
                let mut c = core.lock().unwrap();
                let r = tick(&mut c, current);
                (r.move_to, c.is_wobbling)
            };

            if let Some((tx, ty)) = move_to {
                match enigo.move_mouse(tx, ty, Coordinate::Abs) {
                    Ok(()) => {
                        let mut c = core.lock().unwrap();
                        c.last_error = None;
                        c.expected_pos = Some((tx, ty));
                        c.wobble_step = (c.wobble_step + 1) % 8;
                        c.last_wobble = Instant::now();
                    }
                    Err(err) => {
                        let mut c = core.lock().unwrap();
                        c.last_error = Some(format!(
                            "Mouse move failed: {err}. \
                             On macOS grant Accessibility permission."
                        ));
                    }
                }
            }

            // Emit on a wobble-state change, or at most every STATUS_EMIT_INTERVAL,
            // so the 100ms poll loop doesn't spam the frontend.
            let now = Instant::now();
            if is_wobbling != prev_wobbling
                || now.duration_since(last_emit) >= STATUS_EMIT_INTERVAL
            {
                prev_wobbling = is_wobbling;
                let _ = app.emit("status-update", build_status(&core));
                last_emit = now;
            }
        }
    });
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn build_status(core: &SharedCore) -> AppStatus {
    let c = core.lock().unwrap();
    AppStatus {
        is_wobbling: c.is_wobbling,
        is_manual: c.is_manual,
        auto_mode: c.settings.auto_mode,
        idle_seconds: c.last_user_activity.elapsed().as_secs(),
        settings: c.settings.clone(),
        error: c.last_error.clone(),
    }
}

fn update_tray_label<R: tauri::Runtime>(app: &AppHandle<R>, wobbling: bool) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(if wobbling {
            "Mouse Wobbler – Active"
        } else {
            "Mouse Wobbler"
        }));
    }
}

// ─── Tauri commands ───────────────────────────────────────────────────────────

#[tauri::command]
fn get_status(core: tauri::State<SharedCore>) -> AppStatus {
    build_status(&core)
}

#[tauri::command]
fn start_wobble(core: tauri::State<SharedCore>, app: AppHandle) {
    {
        let mut c = core.lock().unwrap();
        c.is_manual = true;
    }
    update_tray_label(&app, true);
}

#[tauri::command]
fn stop_wobble(core: tauri::State<SharedCore>, app: AppHandle) {
    {
        let mut c = core.lock().unwrap();
        c.is_manual = false;
        c.is_wobbling = false;
        c.anchor_pos = None;
        c.expected_pos = None;
    }
    update_tray_label(&app, false);
}

#[tauri::command]
fn toggle_wobble(core: tauri::State<SharedCore>, app: AppHandle) -> bool {
    let wobbling = {
        let mut c = core.lock().unwrap();
        if c.is_manual {
            c.is_manual = false;
            c.is_wobbling = false;
            c.anchor_pos = None;
            c.expected_pos = None;
            false
        } else {
            c.is_manual = true;
            true
        }
    };
    update_tray_label(&app, wobbling);
    wobbling
}

#[tauri::command]
fn update_settings(core: tauri::State<SharedCore>, settings: WobblerSettings) {
    core.lock().unwrap().settings = settings;
}

#[tauri::command]
fn register_shortcut(
    app: AppHandle,
    core: tauri::State<SharedCore>,
    shortcut: String,
) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

    app.global_shortcut()
        .unregister_all()
        .map_err(|e| e.to_string())?;

    if shortcut.is_empty() {
        return Ok(());
    }

    let core_inner: SharedCore = core.inner().clone();
    let app_handle = app.clone();

    app.global_shortcut()
        .on_shortcut(shortcut.as_str(), move |_app, _sc, event| {
            // The handler fires on both key-press and key-release. Acting on
            // both would make the shortcut a hold-to-wobble; gating on Pressed
            // makes each full press toggle exactly once.
            if !matches!(event.state, ShortcutState::Pressed) {
                return;
            }
            let wobbling = {
                let mut c = core_inner.lock().unwrap();
                if c.is_manual {
                    c.is_manual = false;
                    c.is_wobbling = false;
                    c.anchor_pos = None;
                    c.expected_pos = None;
                    false
                } else {
                    c.is_manual = true;
                    true
                }
            };
            update_tray_label(&app_handle, wobbling);
            let _ = app_handle.emit("shortcut-toggled", wobbling);
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

// ─── App entry point ──────────────────────────────────────────────────────────

pub fn run() {
    let core: SharedCore = Arc::new(Mutex::new(WobblerCore::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(core.clone())
        .setup(move |app| {
            let app_handle = app.handle().clone();
            start_wobbler_thread(core.clone(), app_handle);

            let menu = MenuBuilder::new(app)
                .text("toggle", "Start Wobbling")
                .text("show", "Open Settings")
                .separator()
                .text("quit", "Quit")
                .build()?;

            // Resolve the tray icon explicitly so a missing icon surfaces as an
            // error rather than a silent panic. On macOS we mark it as a template
            // image so the menu bar renders a crisp monochrome glyph that adapts
            // to light/dark.
            let tray_icon = match app.default_window_icon() {
                Some(icon) => icon.clone(),
                None => return Err("no default window icon available".into()),
            };

            let tray_builder = TrayIconBuilder::with_id("main")
                .tooltip("Mouse Wobbler")
                .icon(tray_icon)
                .menu(&menu)
                .show_menu_on_left_click(false);

            #[cfg(target_os = "macos")]
            let tray_builder = tray_builder.icon_as_template(true);

            let _tray = tray_builder
                .on_menu_event({
                    let app_handle = app.handle().clone();
                    let core_ref = app.state::<SharedCore>().inner().clone();
                    move |_app, event| match event.id.as_ref() {
                        "toggle" => {
                            let wobbling = {
                                let mut c = core_ref.lock().unwrap();
                                if c.is_manual {
                                    c.is_manual = false;
                                    c.is_wobbling = false;
                                    c.anchor_pos = None;
                                    c.expected_pos = None;
                                    false
                                } else {
                                    c.is_manual = true;
                                    true
                                }
                            };
                            update_tray_label(&app_handle, wobbling);
                            let _ = app_handle.emit("shortcut-toggled", wobbling);
                        }
                        "show" => {
                            if let Some(w) = app_handle.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "quit" => app_handle.exit(0),
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // Hide to tray instead of quitting when the window is closed.
            if let Some(window) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(w) = handle.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            start_wobble,
            stop_wobble,
            toggle_wobble,
            update_settings,
            register_shortcut,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
