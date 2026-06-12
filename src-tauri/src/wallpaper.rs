//! Privacy-curtain wallpaper cache.
//!
//! The curtain shows live NASA space imagery. The frontend can't do this itself:
//! the NASA asset host sends no CORS header, so the webview can display an image
//! but cannot read its bytes — making offline caching impossible client-side.
//! So the fetching lives here, server-side, where CORS does not apply. A
//! background thread searches NASA's keyless Image Library, filters to actual
//! space photos, downloads a handful of full-size stills, and caches them on
//! disk (app-data dir) so they survive restarts and work offline. The curtain
//! asks for one via the `get_curtain_wallpaper` command — instant, even offline.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime, State};

const CACHE_SUBDIR: &str = "wallpapers";
const INDEX_FILE: &str = "index.json";
/// How many images to keep cached for variety and offline use.
const CACHE_SIZE: usize = 6;
/// Guard against a pathologically large original blowing up memory.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
const RETRY_WHEN_EMPTY: Duration = Duration::from_secs(2 * 60);

/// Astronomy-focused search terms — broad enough for variety, specific enough
/// that results are dominated by deep-space imagery rather than NASA press media.
const TOPICS: &[&str] = &[
    "nebula", "galaxy", "supernova remnant", "star cluster", "deep field",
    "spiral galaxy", "carina nebula", "orion nebula", "planetary nebula",
    "galaxy cluster", "hubble deep space", "james webb",
];

/// Keyword search still drags in people/events/PR; drop those by title.
const TITLE_BLOCKLIST: &[&str] = &[
    "administrator", "press", "conference", "briefing", "ceremony", "portrait",
    "interview", "meeting", "building", "employee", "headquarters", "award",
    "signing", "town hall", "expedition", "crew", "astronaut", "training",
    "congress", "senator", "budget", "anniversary", "logo", "patch",
    "groundbreaking", "official",
];

/// One cached image: a file on disk plus the metadata needed to serve it.
#[derive(Clone, Serialize, Deserialize)]
struct CacheEntry {
    file: String,
    title: String,
    content_type: String,
}

/// Managed state: the current cache index, mirrored from `index.json`.
#[derive(Default)]
pub struct WallpaperCache {
    entries: Mutex<Vec<CacheEntry>>,
    /// Monotonic round-robin cursor so consecutive arms cycle through the whole
    /// pool in order — no immediate repeats, every image shown before any repeat.
    cursor: AtomicUsize,
}

/// What the curtain receives: a ready-to-display data URL and a caption.
#[derive(Serialize)]
pub struct WallpaperPayload {
    data_url: String,
    title: String,
}

// ─── Public entry points ──────────────────────────────────────────────────────

/// Load any persisted cache immediately (so offline-after-restart works), then
/// refresh in the background on a loop. Best-effort throughout: a failed refresh
/// leaves the existing cache untouched.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    let persisted = load_index(app);
    if let Ok(mut entries) = app.state::<WallpaperCache>().entries.lock() {
        *entries = persisted;
    }

    let app = app.clone();
    std::thread::spawn(move || loop {
        refresh_cache(&app);
        let empty = app
            .state::<WallpaperCache>()
            .entries
            .lock()
            .map(|e| e.is_empty())
            .unwrap_or(true);
        // Retry sooner while we have nothing to show (e.g. launched offline).
        std::thread::sleep(if empty { RETRY_WHEN_EMPTY } else { REFRESH_INTERVAL });
    });
}

/// Return a random cached image as a data URL, or `None` if the cache is empty
/// (the curtain then keeps its gradient).
#[tauri::command]
pub fn get_curtain_wallpaper(app: AppHandle, cache: State<WallpaperCache>) -> Option<WallpaperPayload> {
    let entries = cache.entries.lock().ok()?.clone();
    if entries.is_empty() {
        return None;
    }
    // Round-robin in order rather than random-with-replacement, so the same image
    // never shows twice running. `% len` adapts when the cache is refreshed.
    let index = cache.cursor.fetch_add(1, Ordering::Relaxed) % entries.len();
    let entry = entries[index].clone();
    let bytes = fs::read(cache_dir(&app)?.join(&entry.file)).ok()?;
    Some(WallpaperPayload {
        data_url: format!("data:{};base64,{}", entry.content_type, STANDARD.encode(&bytes)),
        title: entry.title,
    })
}

// ─── Refresh pipeline ─────────────────────────────────────────────────────────

fn refresh_cache<R: Runtime>(app: &AppHandle<R>) {
    let Some(dir) = cache_dir(app) else { return };

    let mut fresh: Vec<CacheEntry> = Vec::new();
    // Bounded attempts so a run of misses (filtered out, video, network) can't spin.
    let mut attempts = 0;
    while fresh.len() < CACHE_SIZE && attempts < CACHE_SIZE * 4 {
        attempts += 1;
        if let Some(entry) = fetch_one(&dir, fresh.len()) {
            fresh.push(entry);
        }
    }

    // Offline / everything failed → keep whatever we already had.
    if fresh.is_empty() {
        return;
    }

    prune_files_not_in(&dir, &fresh);
    if let Ok(mut entries) = app.state::<WallpaperCache>().entries.lock() {
        *entries = fresh.clone();
    }
    save_index(app, &fresh);
}

/// Search → filter to space → read the asset manifest → download the best size.
fn fetch_one(dir: &Path, slot: usize) -> Option<CacheEntry> {
    let topic = TOPICS[rand_below(TOPICS.len())];
    let search_url = format!(
        "https://images-api.nasa.gov/search?media_type=image&q={}",
        topic.replace(' ', "%20")
    );
    let search: Value = serde_json::from_str(&http_get_string(&search_url)?).ok()?;
    let items = search["collection"]["items"].as_array()?;

    let space: Vec<&Value> = items.iter().filter(|it| is_space_item(it)).collect();
    if space.is_empty() {
        return None;
    }
    let item = space[rand_below(space.len())];

    // The manifest (collection.json) lists the actual size variants for this
    // asset — reachable here because server-side requests aren't bound by CORS.
    let manifest_url = item["href"].as_str()?;
    let assets: Vec<String> = serde_json::from_str(&http_get_string(manifest_url)?).ok()?;
    let image_url = pick_best_asset(&assets)?;

    let (bytes, content_type) = http_get_bytes(&image_url)?;
    let ext = if image_url.to_lowercase().ends_with(".png") { "png" } else { "jpg" };
    let file = format!("wp_{}_{}.{}", now_millis(), slot, ext);
    fs::write(dir.join(&file), &bytes).ok()?;

    let title = item["data"][0]["title"].as_str().unwrap_or("").to_string();
    Some(CacheEntry { file, title, content_type })
}

/// Keep only items with a usable thumbnail and a title free of PR/people noise.
fn is_space_item(item: &Value) -> bool {
    if item["links"][0]["href"].as_str().is_none() {
        return false;
    }
    let title = item["data"][0]["title"].as_str().unwrap_or("").to_lowercase();
    !title.is_empty() && !TITLE_BLOCKLIST.iter().any(|word| title.contains(word))
}

/// Prefer `large` (the wallpaper sweet spot), then the full `orig`, then smaller.
fn pick_best_asset(assets: &[String]) -> Option<String> {
    const ORDER: &[&str] = &["~large.", "~orig.", "~medium.", "~small."];
    let images: Vec<&String> = assets
        .iter()
        .filter(|u| {
            let lower = u.to_lowercase();
            lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".png")
        })
        .collect();
    for key in ORDER {
        if let Some(url) = images.iter().find(|u| u.to_lowercase().contains(key)) {
            return Some((*url).clone());
        }
    }
    images.first().map(|u| (*u).clone())
}

// ─── HTTP (blocking, on the refresh thread) ───────────────────────────────────

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build()
}

fn http_get_string(url: &str) -> Option<String> {
    agent().get(url).call().ok()?.into_string().ok()
}

fn http_get_bytes(url: &str) -> Option<(Vec<u8>, String)> {
    let response = agent().get(url).call().ok()?;
    let content_type = response.content_type().to_string();
    let mut buffer = Vec::new();
    response
        .into_reader()
        .take(MAX_IMAGE_BYTES)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.is_empty() {
        return None;
    }
    Some((buffer, content_type))
}

// ─── Disk cache helpers ───────────────────────────────────────────────────────

fn cache_dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?.join(CACHE_SUBDIR);
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn load_index<R: Runtime>(app: &AppHandle<R>) -> Vec<CacheEntry> {
    let Some(dir) = cache_dir(app) else { return Vec::new() };
    let Ok(text) = fs::read_to_string(dir.join(INDEX_FILE)) else { return Vec::new() };
    let entries: Vec<CacheEntry> = serde_json::from_str(&text).unwrap_or_default();
    // Drop entries whose backing file has gone missing.
    entries.into_iter().filter(|e| dir.join(&e.file).exists()).collect()
}

fn save_index<R: Runtime>(app: &AppHandle<R>, entries: &[CacheEntry]) {
    if let Some(dir) = cache_dir(app) {
        if let Ok(text) = serde_json::to_string_pretty(entries) {
            let _ = fs::write(dir.join(INDEX_FILE), text);
        }
    }
}

/// Delete cached image files no longer referenced by the new index.
fn prune_files_not_in(dir: &Path, keep: &[CacheEntry]) {
    let Ok(read) = fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == INDEX_FILE || !name.starts_with("wp_") {
            continue;
        }
        if !keep.iter().any(|e| e.file == name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ─── Misc ─────────────────────────────────────────────────────────────────────

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Cheap, non-crypto index picker — fine for choosing a topic/image at random.
fn rand_below(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    nanos % n
}
