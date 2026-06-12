use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use enigo::{Coordinate, Enigo, Mouse, Settings as EnigoSettings};
use mouse_wobbler_core::{tick, AppStatus, SharedCore, WobblerCore, WobblerSettings};
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Position, Runtime, Size,
    State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_store::StoreExt;

mod wallpaper;

// ─── Persistence ──────────────────────────────────────────────────────────────
// Settings are the only thing that needs to outlive a process restart today, so
// a flat key/value store (not a database) is the right altitude. The store file
// lives in the OS app-data dir; the plugin handles the platform-specific path.

const SETTINGS_STORE: &str = "settings.json";
const SETTINGS_KEY: &str = "settings";

/// Read persisted settings, or `None` if nothing is saved yet (first run) or the
/// stored value is unreadable. A corrupt entry must not abort startup — we log it
/// and fall back to defaults so the app always launches.
fn load_settings<R: Runtime>(app: &AppHandle<R>) -> Option<WobblerSettings> {
    let store = app.store(SETTINGS_STORE).ok()?;
    let value = store.get(SETTINGS_KEY)?;
    match serde_json::from_value::<WobblerSettings>(value) {
        Ok(settings) => Some(settings),
        Err(err) => {
            eprintln!("ignoring corrupt persisted settings: {err}");
            None
        }
    }
}

/// Persist settings best-effort. A failure to write must not break the live
/// session, so every fallible step is logged at the boundary rather than
/// propagated up into the command path.
fn save_settings<R: Runtime>(app: &AppHandle<R>, settings: &WobblerSettings) {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("cannot open settings store for write: {err}");
            return;
        }
    };
    let value = match serde_json::to_value(settings) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("cannot serialise settings: {err}");
            return;
        }
    };
    store.set(SETTINGS_KEY, value);
    if let Err(err) = store.save() {
        eprintln!("cannot persist settings: {err}");
    }
}

// ─── Privacy Curtain ──────────────────────────────────────────────────────────
// A full-screen, always-on-top cover with an app-set password to dismiss. It is
// a PRIVACY CURTAIN, not a system lock: a userspace window cannot block
// Ctrl+Alt+Del, a remote shell, or a force-quit, and it disappears if the
// process dies. It raises the bar against a passer-by while the wobbler keeps
// the session awake — nothing more. The honest framing lives in the UI too.

const CURTAIN_KEY: &str = "curtain_password_hash";
const CURTAIN_LABEL_PREFIX: &str = "curtain-";

/// Tracks whether the curtain is currently raised. Kept in the Tauri layer (not
/// the pure core) because it owns no wobble logic — it only gates window spawning
/// and unlock. Atomic so the tray thread and command handlers share it lock-free.
#[derive(Default)]
struct CurtainState {
    armed: AtomicBool,
    /// The `is_manual` value to restore when the curtain comes down, captured at
    /// arm time. Manual arming forces wobbling on so the session stays awake while
    /// covered; without this snapshot that forced state would persist after
    /// unlock and the user could no longer reclaim the cursor by moving it.
    restore_manual: AtomicBool,
}

/// Put the wobble state back to what it was before the curtain was raised. When
/// the prior state was non-manual we also clear the active wobble so the cursor
/// stops and auto/idle logic takes over cleanly — otherwise a forced manual
/// wobble would linger and refuse to cede control to the returning user.
fn restore_wobble_after_curtain(core: &SharedCore, curtain: &CurtainState) {
    let restore_manual = curtain.restore_manual.load(Ordering::SeqCst);
    let mut c = core.lock().unwrap();
    c.is_manual = restore_manual;
    if !restore_manual {
        c.is_wobbling = false;
        c.anchor_pos = None;
        c.expected_pos = None;
    }
}

/// Read the stored Argon2id PHC hash string, or `None` if no password is set.
fn load_password_hash<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let store = app.store(SETTINGS_STORE).ok()?;
    store.get(CURTAIN_KEY)?.as_str().map(str::to_string)
}

/// Write (or, with `None`, delete) the stored password hash. The PHC string
/// already embeds its own random salt and params, so no separate salt field is
/// needed. Plaintext never reaches the store.
fn store_password_hash<R: Runtime>(app: &AppHandle<R>, hash: Option<&str>) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    match hash {
        Some(h) => store.set(CURTAIN_KEY, serde_json::Value::String(h.to_string())),
        None => {
            store.delete(CURTAIN_KEY);
        }
    }
    store.save().map_err(|e| e.to_string())
}

/// Cover every monitor with its own borderless, always-on-top window positioned
/// in physical pixels so mixed-DPI multi-monitor setups line up exactly. Each
/// window refuses every close request — only a verified unlock tears it down.
fn spawn_curtain_windows<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    // Monitor enumeration needs a window context; the main window always exists
    // (it only ever hides to tray, never gets destroyed).
    let anchor = app
        .get_webview_window("main")
        .ok_or_else(|| "main window unavailable".to_string())?;
    let monitors = anchor.available_monitors().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("no monitors detected".into());
    }

    for (index, monitor) in monitors.iter().enumerate() {
        let label = format!("{CURTAIN_LABEL_PREFIX}{index}");
        if app.get_webview_window(&label).is_some() {
            continue; // already covering this monitor
        }

        let window =
            WebviewWindowBuilder::new(app, &label, WebviewUrl::App("curtain.html".into()))
                .title("")
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                // A curtain the user can resize, drag, or minimise is no curtain.
                .resizable(false)
                .maximizable(false)
                .minimizable(false)
                .visible(false)
                .build()
                .map_err(|e| e.to_string())?;

        let pos = monitor.position();
        let size = monitor.size();
        window
            .set_position(Position::Physical(PhysicalPosition { x: pos.x, y: pos.y }))
            .map_err(|e| e.to_string())?;
        window
            .set_size(Size::Physical(PhysicalSize {
                width: size.width,
                height: size.height,
            }))
            .map_err(|e| e.to_string())?;
        window.set_always_on_top(true).map_err(|e| e.to_string())?;

        // Block every close path (Cmd+W, window manager, etc.); the curtain may
        // only come down via unlock_curtain → destroy().
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
            }
        });

        window.show().map_err(|e| e.to_string())?;

        // On Windows a borderless window is still an ordinary, movable/resizable
        // window that can be dragged aside to peek behind the curtain. Borderless
        // fullscreen locks it to the entire monitor — unmovable, unresizable, and
        // covering the taskbar too. macOS already covers fully via the sized
        // borderless window above; going fullscreen there would spawn a separate
        // Space and break the cross-Space behaviour we rely on.
        #[cfg(target_os = "windows")]
        window.set_fullscreen(true).map_err(|e| e.to_string())?;

        // Span every virtual desktop only after the window is realised so its
        // native handle exists (the Windows pin needs a valid HWND).
        cover_all_desktops(&window);
        if index == 0 {
            window.set_focus().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Make a curtain window span every virtual desktop / Space the OS offers, so
/// switching desktops can't expose the screen behind it.
///
/// macOS has a clean public API (CanJoinAllSpaces). Windows has none — pinning to
/// all virtual desktops goes through undocumented shell internals, wrapped here by
/// the `winvd` crate. Both calls are best-effort: a failure degrades to "covers
/// the current desktop only" rather than breaking the curtain, so a Windows
/// feature update that breaks the undocumented path can never leave the screen
/// uncovered on the active desktop.
fn cover_all_desktops<R: Runtime>(window: &tauri::WebviewWindow<R>) {
    // No-op on Windows in tao; real behaviour on macOS/Linux.
    if let Err(err) = window.set_visible_on_all_workspaces(true) {
        eprintln!("curtain: set_visible_on_all_workspaces failed: {err}");
    }

    #[cfg(target_os = "windows")]
    pin_to_all_virtual_desktops_windows(window);
}

/// Pin a window to every Windows virtual desktop via `winvd`. Tauri's `HWND`
/// comes from a different `windows`-crate version than `winvd`'s, so the handle is
/// bridged through its raw pointer. Undocumented and version-sensitive — failures
/// are logged, never propagated.
#[cfg(target_os = "windows")]
fn pin_to_all_virtual_desktops_windows<R: Runtime>(window: &tauri::WebviewWindow<R>) {
    use windows::Win32::Foundation::HWND;
    match window.hwnd() {
        Ok(handle) => {
            let hwnd = HWND(handle.0);
            if let Err(err) = winvd::pin_window(hwnd) {
                eprintln!("curtain: pin to all virtual desktops failed: {err:?}");
            }
        }
        Err(err) => eprintln!("curtain: could not resolve HWND for pinning: {err}"),
    }
}

/// Tear down all curtain windows. Uses `destroy()` — not `close()` — to bypass
/// the prevent-close guard installed above.
fn close_curtain_windows<R: Runtime>(app: &AppHandle<R>) {
    for (label, window) in app.webview_windows() {
        if label.starts_with(CURTAIN_LABEL_PREFIX) {
            if let Err(err) = window.destroy() {
                eprintln!("failed to destroy {label}: {err}");
            }
        }
    }
}

/// Raise the curtain. Shared by the command, the tray menu, and auto mode.
/// Requires a password to be set. `force_wobble` distinguishes the two entry
/// paths: a manual raise turns wobbling on so the session stays awake while
/// covered; an auto-mode raise leaves it alone because auto mode is already
/// wobbling. Either way the prior `is_manual` is snapshotted for restore.
fn arm_curtain_inner<R: Runtime>(
    app: &AppHandle<R>,
    core: &SharedCore,
    curtain: &CurtainState,
    force_wobble: bool,
) -> Result<(), String> {
    if load_password_hash(app).is_none() {
        return Err("Set a curtain password first".into());
    }
    if curtain.armed.swap(true, Ordering::SeqCst) {
        return Ok(()); // already armed — idempotent
    }

    {
        let mut c = core.lock().unwrap();
        curtain.restore_manual.store(c.is_manual, Ordering::SeqCst);
        if force_wobble {
            c.is_manual = true;
        }
    }
    update_tray_label(app, true);

    if let Err(err) = spawn_curtain_windows(app) {
        // Roll back fully so a partial failure never leaves us "armed" with no
        // cover and a forced wobble the user didn't ask for.
        curtain.armed.store(false, Ordering::SeqCst);
        restore_wobble_after_curtain(core, curtain);
        update_tray_label(app, core.lock().unwrap().is_manual);
        close_curtain_windows(app);
        return Err(err);
    }

    // Bring the app to the foreground so the curtain reliably captures the unlock
    // keystrokes — important for the auto-arm path, where it may rise while the
    // settings window (and the dock icon) is hidden. The dock icon sits behind the
    // full-screen cover, so making it visible here is invisible to the user.
    set_dock_icon_visible(app, true);
    Ok(())
}

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

            let (move_to, is_wobbling, is_manual, auto_arm_enabled) = {
                let mut c = core.lock().unwrap();
                let r = tick(&mut c, current);
                let auto_arm = c.settings.auto_mode && c.settings.curtain_auto_arm;
                (r.move_to, c.is_wobbling, c.is_manual, auto_arm)
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

            // Auto-arm: on the rising edge of an *auto* (non-manual) wobble, raise
            // the curtain too — but only if enabled and a password is set. Window
            // creation must happen on the main thread, so dispatch there. The
            // rising-edge guard (`!prev_wobbling`) means unlocking while still idle
            // does not immediately re-curtain.
            if is_wobbling
                && !prev_wobbling
                && !is_manual
                && auto_arm_enabled
                && load_password_hash(&app).is_some()
            {
                let app_main = app.clone();
                let core_main = core.clone();
                let _ = app.run_on_main_thread(move || {
                    let curtain = app_main.state::<CurtainState>();
                    if let Err(err) =
                        arm_curtain_inner(&app_main, &core_main, curtain.inner(), false)
                    {
                        eprintln!("auto-arm curtain failed: {err}");
                    }
                });
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

/// Show or hide the macOS dock icon so the app reads as a menu-bar utility: the
/// icon appears while the settings window is open and disappears once it hides to
/// the tray, instead of lingering in the dock. No-op off macOS, where there is no
/// dock and the tray/taskbar already conveys presence.
fn set_dock_icon_visible<R: tauri::Runtime>(app: &AppHandle<R>, visible: bool) {
    #[cfg(target_os = "macos")]
    if let Err(err) = app.set_dock_visibility(visible) {
        eprintln!("failed to set dock visibility: {err}");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
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
fn update_settings(core: tauri::State<SharedCore>, app: AppHandle, settings: WobblerSettings) {
    core.lock().unwrap().settings = settings.clone();
    save_settings(&app, &settings);
}

// ─── Curtain commands ─────────────────────────────────────────────────────────

#[tauri::command]
fn has_curtain_password(app: AppHandle) -> bool {
    load_password_hash(&app).is_some()
}

#[tauri::command]
fn set_curtain_password(app: AppHandle, password: String) -> Result<(), String> {
    if password.is_empty() {
        return Err("Password cannot be empty".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("hashing failed: {e}"))?
        .to_string();
    store_password_hash(&app, Some(&hash))
}

#[tauri::command]
fn clear_curtain_password(app: AppHandle, curtain: State<CurtainState>) -> Result<(), String> {
    // Clearing while armed would strip the only way back in — refuse it.
    if curtain.armed.load(Ordering::SeqCst) {
        return Err("Disarm the curtain before removing the password".into());
    }
    store_password_hash(&app, None)
}

#[tauri::command]
fn arm_curtain(
    app: AppHandle,
    core: State<SharedCore>,
    curtain: State<CurtainState>,
) -> Result<(), String> {
    // Manual raise from the UI button: force wobbling on to keep the session awake.
    arm_curtain_inner(&app, core.inner(), curtain.inner(), true)
}

/// Verify the supplied password against the stored Argon2id hash. Returns
/// `Ok(true)` and lowers the curtain on a match, `Ok(false)` on a wrong password
/// (the curtain stays up), and `Err` only on an internal/storage fault.
#[tauri::command]
fn unlock_curtain(
    app: AppHandle,
    core: State<SharedCore>,
    curtain: State<CurtainState>,
    password: String,
) -> Result<bool, String> {
    let stored = load_password_hash(&app).ok_or_else(|| "no curtain password set".to_string())?;
    let parsed = PasswordHash::new(&stored).map_err(|e| format!("stored hash invalid: {e}"))?;
    let unlocked = Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();

    if unlocked {
        curtain.armed.store(false, Ordering::SeqCst);
        restore_wobble_after_curtain(core.inner(), curtain.inner());
        update_tray_label(&app, core.lock().unwrap().is_manual);
        close_curtain_windows(&app);

        // Restore the dock icon to match the settings window: keep it if the
        // window is open, hide it again if the curtain was armed from the tray
        // with the window tucked away.
        let main_visible = app
            .get_webview_window("main")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        set_dock_icon_visible(&app, main_visible);
    }
    Ok(unlocked)
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
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(core.clone())
        .manage(CurtainState::default())
        .manage(wallpaper::WallpaperCache::default())
        .setup(move |app| {
            // Restore persisted settings before the wobbler thread reads them, so
            // the very first tick already honours the user's saved configuration.
            if let Some(persisted) = load_settings(app.handle()) {
                core.lock().unwrap().settings = persisted;
            }

            // Warm the curtain wallpaper cache (loads any on-disk images now,
            // refreshes from NASA in the background).
            wallpaper::init(app.handle());

            let app_handle = app.handle().clone();
            start_wobbler_thread(core.clone(), app_handle);

            let menu = MenuBuilder::new(app)
                .text("toggle", "Start Wobbling")
                .text("curtain", "Lock Screen (Curtain)")
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
                        "curtain" => {
                            let curtain = app_handle.state::<CurtainState>();
                            if let Err(err) =
                                arm_curtain_inner(&app_handle, &core_ref, curtain.inner(), true)
                            {
                                // Most likely "no password set" — open Settings so
                                // the user sees why nothing happened.
                                eprintln!("arm curtain from tray failed: {err}");
                                if let Some(w) = app_handle.get_webview_window("main") {
                                    set_dock_icon_visible(&app_handle, true);
                                    let _ = w.show();
                                    let _ = w.set_focus();
                                }
                            }
                        }
                        "show" => {
                            if let Some(w) = app_handle.get_webview_window("main") {
                                set_dock_icon_visible(&app_handle, true);
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
                                set_dock_icon_visible(app, false);
                            } else {
                                set_dock_icon_visible(app, true);
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
                            set_dock_icon_visible(&handle, false);
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
            has_curtain_password,
            set_curtain_password,
            clear_curtain_password,
            arm_curtain,
            unlock_curtain,
            wallpaper::get_curtain_wallpaper,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
