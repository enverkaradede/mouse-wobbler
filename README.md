# Mouse Wobbler

A tiny, native desktop app that keeps your session alive by nudging the mouse cursor in a small circular orbit — so your status stays "active", your screen never sleeps, and long-running tasks aren't interrupted by idle timeouts.

Built with [Tauri v2](https://v2.tauri.app/) (Rust core, system WebView UI). The whole app is a single small binary that lives in your menu bar / system tray.

---

## Features

- **Two modes**
  - **Manual** — toggle wobbling on/off instantly from the tray, the window button, or a global shortcut.
  - **Auto** — starts wobbling only after you've been idle for a configurable threshold, and stops the moment you touch the mouse again.
- **Configurable behavior**
  - Idle threshold (when auto-mode kicks in)
  - Wobble interval (how often the cursor nudges)
  - Wobble radius (how far it moves)
- **Stays out of your way** — the cursor orbits an anchor point in a smooth 8-point circle. In manual mode it re-anchors to wherever you leave the cursor; in auto mode it instantly cedes control when you move.
- **Global shortcut** — bind a hotkey to toggle wobbling from anywhere.
- **Lives in the tray** — closing the window hides it to the tray instead of quitting.
- **Lightweight** — single native binary, no Electron, no background browser.

---

## Install

Grab the latest installer from the [**Releases**](https://github.com/enverkaradede/mouse-wobbler/releases) page.

| Platform | File |
|---|---|
| **macOS (Apple Silicon / M1+)** | `Mouse Wobbler_*_aarch64.dmg` |
| **Windows (x64)** | `Mouse Wobbler_*_x64-setup.exe` or `.msi` |

### macOS first-launch note

The macOS build is currently **unsigned**, so Gatekeeper will show a misleading *"the application is damaged / corrupted"* message on first open. The app is fine — this is just the unsigned-app warning. Clear the quarantine flag once after installing:

```bash
xattr -dr com.apple.quarantine "/Applications/Mouse Wobbler.app"
```

Then open it normally.

### Accessibility permission (macOS)

Moving the cursor requires Accessibility access. On first wobble, macOS will prompt — or grant it manually under:

**System Settings → Privacy & Security → Accessibility → enable Mouse Wobbler**

Without this permission the app shows an error and won't be able to move the cursor.

---

## Usage

1. Launch the app — it appears in the menu bar / system tray.
2. **Manual mode:** click the tray icon → *Start Wobbling*, use the in-window toggle, or press your configured shortcut.
3. **Auto mode:** enable it in settings and set an idle threshold; the app wobbles only after you've been inactive that long, and backs off instantly when you return.
4. Adjust idle threshold, interval, and radius in the settings window.
5. Closing the window hides it to the tray; choose **Quit** from the tray menu to exit fully.

---

## Build from source

### Prerequisites

| Tool | Notes |
|---|---|
| **Rust** (stable) | via [rustup](https://rustup.rs) |
| **Tauri CLI v2** | `cargo install tauri-cli --version "^2"` |
| **macOS** | Xcode Command Line Tools (`xcode-select --install`) — provides the linker and `ApplicationServices` framework |
| **Windows** | MSVC Build Tools (C++ workload + Windows SDK) and the WebView2 runtime (preinstalled on Win 10/11) |

The frontend is plain static HTML/CSS/JS in [`ui/`](ui/) — there's **no Node/npm build step**.

### Commands

```bash
# Run in development (hot-reloads the Rust app)
cargo tauri dev

# Produce a release installer for the current platform
cargo tauri build

# Run the core state-machine unit tests
cargo test -p mouse-wobbler-core
```

Build output lands in `src-tauri/target/release/bundle/`.

> **Cross-compiling note:** Tauri can't practically cross-compile to Windows from macOS (it needs MSVC + WebView2). Build each platform on its own OS, or use the included CI (below).

---

## Project structure

```
.
├── core/            # Pure Rust state machine (wobble math + tick logic), fully unit-tested, no I/O
├── src-tauri/       # Tauri shell: tray, window, global shortcut, mouse I/O via enigo
│   ├── src/lib.rs   # App wiring, commands, background wobbler thread
│   └── tauri.conf.json
├── ui/              # Static frontend (index.html, main.js, styles.css)
└── .github/workflows/release.yml   # CI: builds M1 macOS + Windows, publishes a Release
```

The design deliberately separates **pure logic** (`core`) from **side effects** (`src-tauri`): all the decision-making lives in `core::tick()`, which takes the current cursor position and returns whether/where to move — making it testable without touching real hardware.

---

## Releases & CI

Pushing a `v*` tag (e.g. `v0.1.0`) triggers the [release workflow](.github/workflows/release.yml), which builds installers for Apple Silicon macOS and Windows in parallel and publishes them to a GitHub Release. It can also be run manually from the **Actions** tab.

```bash
git tag v0.1.0
git push origin v0.1.0
```

---

## License

No license specified yet.
