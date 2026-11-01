# KDownloader

A light download manager and BitTorrent client for macOS, named as a nod to JDownloader.

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

- **Protocol encryption (MSE/PE) is *not supported*** — the `librqbit` engine currently has no
  message-stream-encryption support, so KDownloader can only talk to peers in **plaintext**. Peers that
  require/negotiate encryption fail the handshake. This mostly hits **private trackers** (which often
  mandate encryption): their swarms seed/leech poorly or not at all, while public torrents (with many
  plaintext peers) work fine. Tracked upstream in
  [rqbit PR #633 (MSE support)](https://github.com/ikatson/rqbit/pull/633) — once merged and released,
  bump the `librqbit` dependency to gain it.
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

- `macos/KDownloader.app` — the application
- `dmg/KDownloader_0.1.0_aarch64.dmg` — a drag-to-install disk image

> The build is ad-hoc signed. See **Install on macOS** below for why that
> matters for automatic UPnP, and how to fix it for free.

## Install on macOS

KDownloader is not Apple-notarized, so after you download or build it, do a one-time
local signing step — otherwise macOS silently blocks the app's **local network**
access, and automatic **UPnP** port-forwarding (so peers can reach you) won't work.

```sh
# signs /Applications/KDownloader.app by default; pass a path for another location
./scripts/kdownloader-macos-sign.command
```

The script creates a **stable, locally-trusted code-signing certificate** (its
private key is generated on your machine and never leaves it), clears the
download-quarantine flag, and signs the app. You'll be asked for your macOS login
password once, to trust the certificate. Then:

1. Launch KDownloader.
2. **System Settings → Privacy & Security → Local Network → enable KDownloader.**

That's it — UPnP then opens your BitTorrent port automatically, the way other
clients do, and the permission sticks.

- Undo at any time: `./scripts/kdownloader-macos-sign.command --remove`.
- It works with both the macOS system OpenSSL (LibreSSL) and Homebrew OpenSSL.
- **Distributing to non-technical users?** Proper **Developer ID signing +
  notarization** (an Apple Developer account) removes this step entirely — it's
  the paid, turnkey version of what this script does for free.
- **Linux / Windows:** no signing needed; UPnP works out of the box.

## Possible next steps

- A native folder picker (via `tauri-plugin-dialog`) for save paths and the watch folder
- Live per-torrent speed limits (would need an upstream librqbit addition)
- RSS "smart episode" filters and per-feed scheduling
- Search against a legitimate indexer API (opt-in)
- Global download-folder change without restart

## Direct Download

The **Direct Download** tab is a light download manager next to the torrent client:
link grabbing, multi-connection HTTP downloads with resume, packages, automatic
extraction, and premium accounts through the official APIs of Real-Debrid, AllDebrid,
Premiumize, Debrid-Link, Uploadedpremiumlink, TorBox, Mega-Debrid, Cocoleech and
Fakirdebrid. API keys are stored in the macOS Keychain.

Copied links are routed automatically: magnets and `.torrent` URLs prefill the Torrent
tab, and other file links start at once, or wait in the Link Grabber if you turn that off. The code lives in `src-tauri/src/direct/`
and `ui/direct.js`; the engine is independent of Tauri and tested with `cargo test direct::`.

Multi-volume RAR sets extract once their last volume arrives, and a failed extraction
can be retried from the right-click menu. Deleted files go to the Trash.

Link protectors open in a small window where you solve the captcha yourself. KDownloader also
listens for Click'n'Load on `127.0.0.1:9666`: links from a known protector start at once,
and links from any other page wait in the Link Grabber.

Out of scope: captcha solving, dodging hoster limits, container decryption, protected streams.
