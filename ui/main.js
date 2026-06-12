// ── Tauri bridge (guarded) ──────────────────────────────────────────────────
// withGlobalTauri must be true in tauri.conf.json or window.__TAURI__ is
// undefined and this whole file would die on load.
const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;
const listen = TAURI ? TAURI.event.listen : null;

if (!TAURI) {
  // Defensive: surface a missing bridge as a banner instead of a silent dead UI.
  console.error("window.__TAURI__ is undefined — withGlobalTauri not enabled");
  document.addEventListener("DOMContentLoaded", () => {
    const banner = document.getElementById("error-banner");
    const text = document.getElementById("error-text");
    if (banner && text) {
      text.textContent =
        "Tauri bridge unavailable (withGlobalTauri off). UI cannot talk to the backend.";
      banner.classList.remove("hidden");
    }
  });
}

// ── State ─────────────────────────────────────────────────────────────────────
let currentShortcut = "";
let isRecording = false;
let settingsDebounce = null;

// ── DOM refs ──────────────────────────────────────────────────────────────────
const statusDot    = document.getElementById("status-dot");
const statusLabel  = document.getElementById("status-label");
const idleDisplay  = document.getElementById("idle-display");
const mainBtn      = document.getElementById("main-btn");
const errorBanner  = document.getElementById("error-banner");
const errorText    = document.getElementById("error-text");
const autoToggle   = document.getElementById("auto-mode-toggle");
const idleSlider   = document.getElementById("idle-threshold");
const idleInput    = document.getElementById("idle-threshold-input");
const intervalSlider = document.getElementById("wobble-interval");
const intervalInput  = document.getElementById("wobble-interval-input");
const radiusSlider = document.getElementById("wobble-radius");
const radiusInput  = document.getElementById("wobble-radius-input");
const shortcutDisp = document.getElementById("shortcut-display");
const shortcutHint = document.getElementById("shortcut-hint");

// ── Render ────────────────────────────────────────────────────────────────────
function renderStatus(status) {
  const { is_wobbling, is_manual, auto_mode, idle_seconds, settings, error } = status;

  if (error) {
    errorText.textContent = error;
    errorBanner.classList.remove("hidden");
  } else {
    errorBanner.classList.add("hidden");
  }

  statusDot.className = "dot";
  if (is_wobbling && is_manual) {
    statusDot.classList.add("dot-wobbling");
    statusLabel.textContent = "Wobbling";
  } else if (is_wobbling && auto_mode) {
    statusDot.classList.add("dot-auto");
    statusLabel.textContent = "Auto Wobbling";
  } else {
    statusDot.classList.add("dot-inactive");
    statusLabel.textContent = auto_mode ? "Watching (idle)" : "Inactive";
  }

  idleDisplay.textContent = `Idle: ${formatDuration(idle_seconds)}`;

  if (is_manual) {
    mainBtn.textContent = "Stop Wobbling";
    mainBtn.classList.add("active");
  } else {
    mainBtn.textContent = "Start Wobbling";
    mainBtn.classList.remove("active");
  }

  // NB: settings controls (sliders/inputs/auto toggle) are intentionally NOT
  // synced here. They are user-owned; the frontend is the source of truth and
  // pushes changes down. Echoing backend settings back into them on every
  // status broadcast would fight live edits. They are seeded once in init().
}

// Seed the settings controls from the backend a single time at startup.
function seedControls(settings) {
  autoToggle.checked = settings.auto_mode;
  idleSlider.value = settings.idle_threshold_secs;
  idleInput.value = settings.idle_threshold_secs;
  intervalSlider.value = settings.wobble_interval_ms;
  intervalInput.value = settings.wobble_interval_ms;
  radiusSlider.value = settings.wobble_radius;
  radiusInput.value = settings.wobble_radius;
  curtainAutoToggle.checked = settings.curtain_auto_arm;
}

// Clamp a typed value to its slider's min/max and snap to the slider's step.
function clampToSlider(slider, raw) {
  const min = Number(slider.min);
  const max = Number(slider.max);
  const step = Number(slider.step) || 1;
  let v = Number(raw);
  if (!Number.isFinite(v)) v = Number(slider.value);
  v = Math.min(max, Math.max(min, v));
  return Math.round((v - min) / step) * step + min;
}

function formatDuration(secs) {
  if (secs < 60) return secs + "s";
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? m + "m" : `${m}m ${s}s`;
}

// ── Event handlers ────────────────────────────────────────────────────────────
async function handleMainBtn() {
  try {
    const wobbling = await invoke("toggle_wobble");
    mainBtn.textContent = wobbling ? "Stop Wobbling" : "Start Wobbling";
    wobbling ? mainBtn.classList.add("active") : mainBtn.classList.remove("active");
  } catch (e) {
    console.error("toggle_wobble failed:", e);
  }
}

async function handleAutoModeChange() {
  await pushSettings();
}

// Slider handlers: mirror the slider value into its number input.
function handleIdleThreshold(val) {
  idleInput.value = val;
  scheduleSettingsPush();
}

function handleInterval(val) {
  intervalInput.value = val;
  scheduleSettingsPush();
}

function handleRadius(val) {
  radiusInput.value = val;
  scheduleSettingsPush();
}

// Number-input handlers: clamp the typed value, mirror it back to the slider
// (and the input itself, so out-of-range entries snap visibly).
function handleIdleInput(raw) {
  const v = clampToSlider(idleSlider, raw);
  idleSlider.value = v;
  idleInput.value = v;
  scheduleSettingsPush();
}

function handleIntervalInput(raw) {
  const v = clampToSlider(intervalSlider, raw);
  intervalSlider.value = v;
  intervalInput.value = v;
  scheduleSettingsPush();
}

function handleRadiusInput(raw) {
  const v = clampToSlider(radiusSlider, raw);
  radiusSlider.value = v;
  radiusInput.value = v;
  scheduleSettingsPush();
}

function scheduleSettingsPush() {
  clearTimeout(settingsDebounce);
  settingsDebounce = setTimeout(pushSettings, 400);
}

async function pushSettings() {
  const settings = {
    auto_mode:            autoToggle.checked,
    idle_threshold_secs:  Number(idleSlider.value),
    wobble_interval_ms:   Number(intervalSlider.value),
    wobble_radius:        Number(radiusSlider.value),
    curtain_auto_arm:     curtainAutoToggle.checked,
  };
  try {
    await invoke("update_settings", { settings });
  } catch (e) {
    console.error("update_settings failed:", e);
  }
}

// ── Shortcut recording ────────────────────────────────────────────────────────
function startRecording() {
  isRecording = true;
  shortcutDisp.classList.add("recording");
  shortcutDisp.textContent = "Press your key combo…";
  shortcutHint.textContent = "Press Escape to cancel.";
  shortcutDisp.focus();
}

function stopRecording() {
  isRecording = false;
  shortcutDisp.classList.remove("recording");
  shortcutHint.textContent = "Press a key combo while the field is focused.";
  renderShortcutDisplay(currentShortcut);
}

function captureShortcut(event) {
  event.preventDefault();
  event.stopPropagation();

  if (event.key === "Escape") {
    stopRecording();
    return;
  }

  const mods = [];
  if (event.metaKey)  mods.push("Super");
  if (event.ctrlKey)  mods.push("Control");
  if (event.altKey)   mods.push("Alt");
  if (event.shiftKey) mods.push("Shift");

  const ignoredKeys = new Set([
    "Control", "Alt", "Shift", "Meta", "Super",
    "CapsLock", "NumLock", "ScrollLock",
  ]);
  if (ignoredKeys.has(event.key)) return;

  const key = mapKey(event);
  if (!key) return;

  if (mods.length === 0) {
    shortcutHint.textContent = "Add at least one modifier key (Ctrl, Alt, Shift, Cmd).";
    return;
  }

  const combo = [...mods, key].join("+");
  currentShortcut = combo;
  stopRecording();
  applyShortcut(combo);
}

function mapKey(event) {
  const code = event.code;
  if (/^F(\d+)$/.test(code)) return code;
  if (/^Digit(\d)$/.test(code)) return code.replace("Digit", "");
  if (/^Key([A-Z])$/.test(code)) return code.replace("Key", "");
  const specials = {
    Space: "Space", Enter: "Return", Tab: "Tab",
    Backspace: "Backspace", Delete: "Delete", Insert: "Insert",
    Home: "Home", End: "End", PageUp: "PageUp", PageDown: "PageDown",
    ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left", ArrowRight: "Right",
    Minus: "Minus", Equal: "Equal", BracketLeft: "BracketLeft",
    BracketRight: "BracketRight", Backslash: "Backslash",
    Semicolon: "Semicolon", Quote: "Quote", Comma: "Comma",
    Period: "Period", Slash: "Slash", Backquote: "Grave",
  };
  return specials[code] || null;
}

async function applyShortcut(shortcut) {
  try {
    await invoke("register_shortcut", { shortcut });
    shortcutHint.textContent = `Shortcut "${shortcut}" registered.`;
  } catch (e) {
    shortcutHint.textContent = `Error: ${e}`;
    currentShortcut = "";
    renderShortcutDisplay("");
    console.error("register_shortcut failed:", e);
  }
}

async function clearShortcut() {
  currentShortcut = "";
  renderShortcutDisplay("");
  try {
    await invoke("register_shortcut", { shortcut: "" });
    shortcutHint.textContent = "Shortcut cleared.";
  } catch (e) {
    console.error("clear shortcut failed:", e);
  }
  shortcutDisp.classList.remove("has-value");
}

function renderShortcutDisplay(shortcut) {
  if (!shortcut) {
    shortcutDisp.textContent = "Click to set shortcut…";
    shortcutDisp.classList.remove("has-value");
    return;
  }
  shortcutDisp.classList.add("has-value");
  shortcutDisp.innerHTML = shortcut
    .split("+")
    .map(k => `<span class="shortcut-key">${k}</span>`)
    .join(" + ");
}

// ── Privacy curtain ─────────────────────────────────────────────────────────────
const curtainSetup     = document.getElementById("curtain-setup");
const curtainActive    = document.getElementById("curtain-active");
const curtainPw        = document.getElementById("curtain-pw");
const curtainPw2       = document.getElementById("curtain-pw2");
const curtainCancelBtn = document.getElementById("curtain-cancel-btn");
const curtainAutoToggle = document.getElementById("curtain-auto-toggle");
const curtainHint      = document.getElementById("curtain-hint");

function clearCurtainInputs() {
  curtainPw.value = "";
  curtainPw2.value = "";
}

// Switch between the "set a password" view and the "armed controls" view based
// on whether the backend already holds a password. Always resets the setup form
// to its first-run state (cleared inputs, no Cancel button).
async function refreshCurtainUI() {
  if (!invoke) return;
  try {
    const hasPassword = await invoke("has_curtain_password");
    curtainSetup.classList.toggle("hidden", hasPassword);
    curtainActive.classList.toggle("hidden", !hasPassword);
    curtainCancelBtn.classList.add("hidden");
    clearCurtainInputs();
  } catch (e) {
    console.error("has_curtain_password failed:", e);
  }
}

async function handleSetCurtainPassword() {
  const password = curtainPw.value;
  const confirm = curtainPw2.value;
  if (!password) {
    curtainHint.textContent = "Enter a password first.";
    return;
  }
  if (password !== confirm) {
    curtainHint.textContent = "Passwords do not match.";
    curtainPw2.value = "";
    curtainPw2.focus();
    return;
  }
  try {
    await invoke("set_curtain_password", { password });
    curtainHint.textContent = "Password saved.";
    await refreshCurtainUI();
  } catch (e) {
    curtainHint.textContent = `Error: ${e}`;
    console.error("set_curtain_password failed:", e);
  }
}

// Reveal the password fields again so the user can overwrite the stored one.
// A Cancel button appears here (but not on first-run setup) so the user can back
// out without changing anything.
function showChangeCurtainPassword() {
  curtainSetup.classList.remove("hidden");
  curtainActive.classList.add("hidden");
  curtainCancelBtn.classList.remove("hidden");
  clearCurtainInputs();
  curtainHint.textContent = "Enter a new password to replace the current one.";
  curtainPw.focus();
}

// Back out of the change-password flow, leaving the existing password intact.
function handleCancelChangePassword() {
  curtainHint.textContent = "";
  refreshCurtainUI();
}

async function handleClearCurtainPassword() {
  try {
    await invoke("clear_curtain_password");
    curtainHint.textContent = "Password removed.";
    await refreshCurtainUI();
  } catch (e) {
    curtainHint.textContent = `Error: ${e}`;
    console.error("clear_curtain_password failed:", e);
  }
}

async function handleArmCurtain() {
  try {
    await invoke("arm_curtain");
    curtainHint.textContent = "";
  } catch (e) {
    curtainHint.textContent = `Error: ${e}`;
    console.error("arm_curtain failed:", e);
  }
}

// The auto-cover preference rides along in WobblerSettings, so persist it through
// the same settings push as the sliders.
function handleCurtainAutoToggle() {
  pushSettings();
}

// ── Init ──────────────────────────────────────────────────────────────────────
async function init() {
  if (!TAURI) return;

  try {
    await listen("status-update", (event) => {
      renderStatus(event.payload);
    });

    await listen("shortcut-toggled", (event) => {
      const wobbling = event.payload;
      if (wobbling) {
        mainBtn.textContent = "Stop Wobbling";
        mainBtn.classList.add("active");
      } else {
        mainBtn.textContent = "Start Wobbling";
        mainBtn.classList.remove("active");
      }
    });
  } catch (e) {
    console.error("listen() failed:", e);
  }

  try {
    const status = await invoke("get_status");
    seedControls(status.settings);
    renderStatus(status);
  } catch (e) {
    console.error("get_status failed:", e);
  }

  await refreshCurtainUI();
}

init();
