# BetterMoonDownloader

A Rust + Tauri rewrite of [moondownloader](https://github.com/LeyckerS/moondownloader).
Paste or drop a `.txt` full of **fuckingfast.co** / **datanodes.to** links. It resolves each one to the direct CDN link and downloads it like IDM/FDM would: a persistent queue, several connections per file, resume after restart, retries, and pause/resume per file.

## Run it

- **`BetterMoonDownloader.exe`**: portable, runs as is (Windows 10/11, which already ship WebView2).
- **`BetterMoonDownloader_1.0.0_x64-setup.exe`**: installer with Start-menu entry and uninstaller.

## Using it

1. **Add links**: `Ctrl+N`, paste anywhere (`Ctrl+V`), drop a `.txt` on the window, or use *Add links → Open .txt…*
   You can open or drop several `.txt` files at once. Parts are grouped by release (e.g. **ExampleGame**) and saved to `<Save to>\ExampleGame\`.
   The same part listed on both fuckingfast and datanodes is downloaded once. The other link is kept as a backup and used automatically if the first one fails, and a partial download carries over.
   Each release has a header with its own progress, speed, ETA and pause/resume. Right-click it for **Download only this release**. Once there are two or more releases, the sidebar's **Releases** list shows them one at a time.
2. Each row shows the file, a moon that fills as it downloads, %, done/total size, speed and time left.
   The header shows the whole batch: files done, GB done, total speed, time left, and the clock time it should finish.
3. **Right-click** a row (or select several with Ctrl/Shift) to pause, resume, retry, show in folder, copy links, or remove.
   `Space` pauses or resumes the selection, `Del` removes it, and `Shift+Del` also deletes the partial file.
4. **Captchas.** Both sites sit behind Cloudflare. Links are resolved in a **hidden browser window inside the app** (Windows' built-in Edge engine), so there's nothing to install and no browser pops up. If Cloudflare ever wants a click, a small "BetterMoonDownloader · verification" window appears with the checkbox, and the blue banner tells you so. Tick it and the download continues. You can switch back to an external Chrome/Edge window in Settings.
5. **Settings**: files at once, connections per file, speed limit, links-only mode (export direct links for another downloader), retries, browser tabs, captcha wait, browser path, datanodes premium API key (skips the browser), and proxies.

Data lives in `%APPDATA%\BetterMoonDownloader` (`queue.json`, `settings.json`). The browser profile is in `%LOCALAPPDATA%\BetterMoonDownloader\browser-profile`.
Partial files are `<name>.bmdpart`, with progress saved in `<name>.bmdpart.json`.

## Build from source

Needs Rust (stable) and Node (only for the Tauri CLI).

```bash
cd src-tauri
npx @tauri-apps/cli@2 build
```

Output: `src-tauri/target/release/better-moon-downloader.exe` and `src-tauri/target/release/bundle/nsis/*-setup.exe`.

Tests (the live download test is opt-in and needs network):

```bash
cd src-tauri
cargo test --lib
```

```bash
cargo test --lib live_segmented -- --ignored --nocapture
```

## Layout

| Path | What |
|---|---|
| `src-tauri/src/engine.rs` | Queue, scheduler, retries, stall killer, persistence, UI snapshot |
| `src-tauri/src/download.rs` | Segmented, resumable downloader (dynamic range splitting) |
| `src-tauri/src/extract/` | `fuckingfast.rs`, `datanodes.rs`: share link → direct link |
| `src-tauri/src/browser.rs` | Spawns and attaches to Chrome/Edge over CDP; Turnstile clicking |
| `ui/` | The whole interface: `index.html`, `styles.css`, `app.js` (no build step) |
| `DECISIONS.md` | Every meaningful decision and its trade-off |

## Credits

A rewrite of [moondownloader](https://github.com/LeyckerS/moondownloader) by LeyckerS (MIT). The Rust code is new; the in-page JavaScript and extraction rules for datanodes/fuckingfast are derived from the original. See [LICENSE](LICENSE).
