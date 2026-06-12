// Privacy-curtain unlock page. Loaded into one borderless full-screen window per
// monitor. Its only job: take a password, ask the backend to verify it, and let
// the backend tear the window down on success.

const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;

const form = document.getElementById("unlock-form");
const input = document.getElementById("password");
const errorEl = document.getElementById("error");

// Keep the password field focused: this window is meant to be the only thing the
// user can interact with, so any stray click should land back on the input.
function refocus() {
  input.focus();
}
window.addEventListener("DOMContentLoaded", refocus);
document.addEventListener("click", refocus);

function showError(message) {
  errorEl.textContent = message;
  // Restart the shake animation by forcing a reflow between toggles.
  form.classList.remove("shake");
  void form.offsetWidth;
  form.classList.add("shake");
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!invoke) {
    showError("Backend unavailable — force-quit the app to regain access.");
    return;
  }

  const password = input.value;
  // Clear immediately so the plaintext does not linger in the field.
  input.value = "";

  try {
    const unlocked = await invoke("unlock_curtain", { password });
    if (!unlocked) {
      showError("Incorrect password");
      refocus();
    }
    // On success the backend destroys this window; nothing more to do here.
  } catch (err) {
    showError(String(err));
    refocus();
  }
});
