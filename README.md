<p align="center">
  <img src="assets/icon.svg" width="112" alt="BetterMoonDownloader logo">
</p>

<h1 align="center">BetterMoonDownloader</h1>

<p align="center">
  A fast, portable bulk downloader for <b>fuckingfast.co</b> and <b>datanodes.to</b> links, for Windows.<br>
  Written in Rust + Tauri. A rewrite of <a href="https://github.com/LeyckerS/moondownloader">moondownloader</a>.
</p>

<p align="center">
  <a href="https://github.com/koubayashi/BetterMoonDownloader/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/koubayashi/BetterMoonDownloader"></a>
  <img alt="Windows 10/11" src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4">
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-green"></a>
</p>

Paste or drop a `.txt` full of share links. BetterMoonDownloader turns each one into its direct download link and downloads it like IDM or FDM would: a persistent queue, several connections per file, resume after restart, retries, and pause/resume per file or per release.

## Download

Get **`BetterMoonDownloader.exe`** from the [latest release](https://github.com/koubayashi/BetterMoonDownloader/releases/latest).

It's portable: a single exe, no installer, no admin rights. Run it from anywhere. It needs Microsoft WebView2, which already ships with Windows 10 and 11.

## Features

- **Releases, not just files.** Parts are grouped by release (e.g. **ExampleGame**) and saved to `<Save to>\ExampleGame\`. Each release has its own progress, speed, ETA and pause/resume.
- **Mirrors.** The same part listed on both fuckingfast and datanodes is downloaded once. The other link is kept as a backup and used automatically if the first fails, and the partial download carries over.
- **Multi-connection, resumable downloads.** Up to 16 connections per file, with dynamic range splitting. Partial files survive restarts and crashes.
- **Built-in browser.** Cloudflare-protected pages are resolved in a hidden browser window inside the app, so there's nothing else to install.
- **Clear progress.** Files done, GB done, total speed, time left and the clock time the batch should finish.
- **Light and dark mode**, following Windows or set by hand.

## Using it

1. **Add links**: `Ctrl+N`, paste anywhere (`Ctrl+V`), drop one or more `.txt` files on the window, or use *Add links → Open .txt…*
2. Each row shows the file, a moon that fills as it downloads, %, size done, speed and time left.
3. **Right-click** a row (or select several with Ctrl/Shift) to pause, resume, retry, show in folder, copy links, or remove. Right-click a release header for **Download only this release**.
   `Space` pauses or resumes the selection, `Del` removes it, and `Shift+Del` also deletes the partial file.
4. **Captchas.** If Cloudflare ever wants a click, a small "BetterMoonDownloader · verification" window appears with the checkbox, and the blue banner tells you so. Tick it and the download continues. You can switch to an external Chrome/Edge window in Settings.
5. **Settings**: files at once, connections per file, speed limit, links-only mode (export direct links for another downloader), retries, browser tabs, captcha wait, browser path, datanodes premium API key (skips the browser), and proxies.

### Where things are kept

| What | Where |
|---|---|
| Queue and settings | `%APPDATA%\BetterMoonDownloader` (`queue.json`, `settings.json`) |
| Built-in browser data (Cloudflare cookies) | `%LOCALAPPDATA%\BetterMoonDownloader\webview-profile` |
| External browser profile (if used) | `%LOCALAPPDATA%\BetterMoonDownloader\browser-profile` |
| Partial downloads | `<name>.bmdpart`, with progress in `<name>.bmdpart.json`, next to the final file |

The datanodes API key is encrypted with Windows DPAPI, so it only works for the Windows account that saved it.

A file already in the folder counts as done only when its size matches the server's exactly. Otherwise it's renamed to `name.old.ext` and downloaded again. Neither host publishes checksums, so for `.rar` sets, use WinRAR's or 7-Zip's **Test** to check the archive before extracting.

## Build from source

Needs Rust (stable) and Node (only for the Tauri CLI).

Use the release script. It strips your local paths (Windows username, checkout location) from the exe, and fails if any remain:

```bash
powershell -ExecutionPolicy Bypass -File ./build-release.ps1
```

Output: `src-tauri/target/release/better-moon-downloader.exe`.

Tests (the live download tests are opt-in and need network):

```bash
cd src-tauri && cargo test --lib
```

## Layout

| Path | What |
|---|---|
| `src-tauri/src/engine.rs` | Queue, scheduler, retries, stall killer, persistence, UI snapshot |
| `src-tauri/src/download.rs` | Segmented, resumable downloader (dynamic range splitting) |
| `src-tauri/src/extract/` | `fuckingfast.rs`, `datanodes.rs`: share link → direct link |
| `src-tauri/src/webview.rs` | The built-in, hidden WebView2 browser |
| `src-tauri/src/browser.rs` | External Chrome/Edge over CDP; Turnstile clicking |
| `ui/` | The whole interface: `index.html`, `styles.css`, `app.js` (no build step) |
| `DECISIONS.md` | Every meaningful decision and its trade-off |
| `SECURITY_REVIEW.md` | Security review findings and how each was fixed |

## Credits

A rewrite of [moondownloader](https://github.com/LeyckerS/moondownloader) by LeyckerS (MIT). The Rust code is new; the in-page JavaScript and extraction rules for datanodes/fuckingfast are derived from the original. See [LICENSE](LICENSE).

This project isn't affiliated with fuckingfast.co or datanodes.to. Only download files you have the right to download.
