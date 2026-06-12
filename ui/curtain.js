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
window.addEventListener("DOMContentLoaded", () => {
  refocus();
  loadWallpaper();
});
document.addEventListener("click", refocus);

// ── Live wallpaper (NASA APOD) ──────────────────────────────────────────────────
// Decoration only. Every failure path silently keeps the dark gradient so the
// unlock prompt is never blocked or delayed by the network. DEMO_KEY is NASA's
// public, rate-limited demo key (not a secret); for a higher limit, get a free
// key at api.nasa.gov and move this fetch to the Rust backend so the key isn't
// shipped in the page.
// count=N returns N random APODs in one request (one hit against the rate limit).
// A larger pool means better odds of a high-res, non-video pick and more variety.
const APOD_ENDPOINT = "https://api.nasa.gov/planetary/apod?api_key=DEMO_KEY&count=8";
const WALLPAPER_TIMEOUT_MS = 8000;

async function loadWallpaper() {
  try {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), WALLPAPER_TIMEOUT_MS);
    const response = await fetch(APOD_ENDPOINT, { signal: controller.signal });
    clearTimeout(timeout);
    if (!response.ok) return; // rate-limited / down → keep gradient

    const items = await response.json();
    // Keep only still images (skip APOD's occasional video days).
    const images = (Array.isArray(items) ? items : [items]).filter(
      (it) => it && it.media_type === "image" && (it.hdurl || it.url)
    );
    if (images.length === 0) return;

    // Prefer full-resolution originals: APOD's `hdurl` is the source image while
    // `url` is a downscaled display copy. Choose among hdurl-bearing items when
    // any exist so we never settle for a soft, low-res `url`.
    const hires = images.filter((it) => it.hdurl);
    const pool = hires.length > 0 ? hires : images;
    const pick = pool[Math.floor(Math.random() * pool.length)];
    applyWallpaper(pick.hdurl || pick.url, pick.title, pick.copyright);
  } catch {
    // Offline, aborted (timeout), CORS, or bad JSON → gradient stays. A backdrop
    // is never worth breaking the lock screen over.
  }
}

function applyWallpaper(src, title, copyright) {
  const img = document.getElementById("wallpaper");
  // Fade in only after the full image decodes, so we never flash a partial load.
  img.onload = () => {
    img.classList.add("loaded");
    showCredit(title, copyright);
  };
  // onerror: leave it hidden; the gradient remains.
  img.src = src;
}

function showCredit(title, copyright) {
  const credit = document.getElementById("wallpaper-credit");
  if (!credit) return;
  const author = copyright ? ` · © ${copyright.trim()}` : "";
  credit.textContent = `${title || "Astronomy Picture of the Day"} · NASA APOD${author}`;
  credit.classList.add("visible");
}

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
