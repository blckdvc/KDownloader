# Pulsar

A light, native BitTorrent client for macOS — Transmission-weight, built on the
[`librqbit`](https://crates.io/crates/librqbit) engine and [Tauri 2](https://tauri.app).

No bundled browser: the UI runs in the macOS system WebView, so the app is a
small native binary rather than a 150 MB Electron bundle.

## Features

- Add torrents by **magnet link**, **http(s) `.torrent` URL**, or a **local `.torrent` file**
- Start / pause / resume; remove with optional file deletion
- Live progress, download/upload speed, ETA, and total throughput
- **Resume across restarts** — session persistence + fastresume
- **Speed limits** — global (adjustable live) and per-torrent (set at add time)
- **Per-file selection** — choose which files in a torrent to download
- **Per-torrent save path** and **categories / tags** (persisted, with a category filter)
- **Watch folder** — auto-adds `.torrent` files dropped into a folder, then moves them to `.pulsar-added`
- **Scheduler** — apply alternate speed limits during a daily time window
- **RSS auto-download** — poll feeds and add items whose title matches your regex rules

### Not included / known limits

- **Sequential download** is *not available* — the `librqbit` engine has no sequential/streaming-order
  download mode, so this can't be offered honestly. (Per-file selection is the closest supported control.)
- **Search plugins** (qBittorrent-style site scrapers) are intentionally not bundled: that's a separate
  subsystem that scrapes torrent indexers, with real legal/ToS considerations. Can be added later against
  a legitimate indexer API if you want it.
- Per-torrent speed limits are set **at add time** (engine exposes them there); the global limit is the
  one adjustable live. Changing the **global download folder** (status bar → *Change…*) applies
  immediately to newly added torrents; already-added torrents keep their folder.

All app settings live in `~/Library/Application Support/com.pulsar.torrent/`.

## Project layout

```
torrent-client/
├── src-tauri/          Rust backend (Tauri app + librqbit engine)
│   ├── src/lib.rs      Session + Tauri commands
│   ├── src/main.rs     entry point
│   ├── Cargo.toml
│   └── tauri.conf.json
└── ui/                 Frontend (vanilla HTML/CSS/JS — no bundler, no npm)
    ├── index.html
    ├── styles.css
    └── main.js
```

## Develop

```sh
cd src-tauri
cargo tauri dev
```

## Build a distributable app + .dmg

```sh
cd src-tauri
cargo tauri build
```

Output lands in `src-tauri/target/release/bundle/`:

- `macos/Pulsar.app` — the application
- `dmg/Pulsar_0.1.0_aarch64.dmg` — a drag-to-install disk image

> The build is ad-hoc signed. To distribute to other Macs without Gatekeeper
> warnings you'd sign with a Developer ID certificate and notarize — not needed
> for running it yourself.

## Possible next steps

- A native folder picker (via `tauri-plugin-dialog`) for save paths and the watch folder
- Live per-torrent speed limits (would need an upstream librqbit addition)
- RSS "smart episode" filters and per-feed scheduling
- Search against a legitimate indexer API (opt-in)
- Global download-folder change without restart
