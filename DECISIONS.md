# BetterMoonDownloader: Decision Log

Each entry: what was decided, why, and what we gave up. Newest decisions are appended at the bottom of each section.

---

## 1. Product scope

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| P1 | Rewrite [LeyckerS/moondownloader](https://github.com/LeyckerS/moondownloader) (Python) in Rust from scratch. No Python code or runtime is kept. | The user wants a smooth, fast, compiled `.exe` with no Python install. | We re-implement roughly 4.5k lines of Python logic. Provider quirks are ported by reading the original, not shared code. |
| P2 | Support the same two hosts: `fuckingfast.co` and `datanodes.to`. | That's the whole point of the original. Generic managers (IDM, FDM, JDownloader) can't resolve these links. | Any other host is rejected with a clear "unsupported host" state. |
| P3 | Behave like a real download manager (IDM / FDM / JDownloader): persistent queue, per-file pause/resume/retry/remove, resume after restart, multi-connection downloads. | User answer: "should work like JDownloader, IDM, FDM". | More engine complexity than the original's run-once batch. |
| P4 | The main window is the **transfer list**, with a compact batch summary above it: files done/remaining, bytes done/total, live speed, ETA as a duration **and** as a clock time ("done by 02:14"). | User: "I should be able to see what files are downloaded… percentage… speed… estimated time". | The big live graph from the original is reduced to a small throughput curve. |
| P5 | Keep the original's useful extras: links-only export, datanodes premium API key, proxy rotation, `.tmp` resume, stall killer, disk-full stop. | These are features users of the original rely on. | — |
| P6 | Don't follow the original's UI. | User: "it need not follow original moondownloader design philosophy". | — |

## 2. Platform and stack

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| S1 | **Tauri v2** (stable 2.x), not egui/iced/Slint. | User picked it. It gives a native Rust core plus full design freedom in WebView2, which is built into Windows 11. The result is one small `.exe`. | Needs the WebView2 runtime (present on Win 10/11). The UI is HTML/CSS, not native widgets. |
| S2 | Tauri **2.x stable**, not the 3.0 alphas now on crates.io. | Alpha APIs churn, and plugins lag behind. A shipping app needs stable. | We miss any 3.x improvements. |
| S3 | Frontend is plain HTML/CSS/JS with **no bundler and no npm dependencies at runtime**. | Fewer moving parts, fast builds, nothing to audit. The UI is one screen plus two sheets. | No framework conveniences, so state rendering is hand-written. |
| S4 | The UI **pulls** a snapshot from Rust about 8× a second. Rust never pushes per-byte events. | Same idea as the original (V4 notes): DOM writes stay on the page's own timeline, and a slow frame drops a frame instead of stalling the engine. | A small polling cost while idle, so polling slows down when nothing is running. |
| S5 | `reqwest` 0.12 + `tokio` for HTTP, `chromiumoxide` for the browser (Chrome DevTools Protocol). | Mature crates. chromiumoxide gives us Fetch interception, Network events and trusted Input events, which is everything the Playwright code used. | chromiumoxide is a big compile. Its CDP types can lag behind new Chrome releases (unknown events are ignored). |

## 3. Extraction (porting the provider logic)

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| E1 | **fuckingfast:** try a plain HTTPS `POST /f/{id}/go` first and read the `hx-redirect` header. On any 403/challenge, fall back to the browser. | The original's `curl_cffi` Chrome-TLS impersonation has no light Rust equivalent (`wreq`/`rquest` need BoringSSL + CMake + NASM to build). Since Aug 2026 `/go` sits behind Turnstile anyway, so the browser path is what actually works. | Without TLS impersonation the HTTP fast path will usually 403. That costs about 1 s before the browser path takes over. |
| E2 | **datanodes:** the API key path first (single JSON GET), then the browser flow. The step1/step2 shape detection, single-POST latch, trigger chain, and overlay/popup sweeping are ported from the original JS. | Those details are hard-won and documented in the original (`moon_extract.py`). | The original's JS snippets are now our maintenance surface whenever datanodes changes. |
| E3 | Drive a **real Chrome, falling back to Edge**, spawned by us with `--remote-debugging-port` on a **dedicated persistent profile** (`%LOCALAPPDATA%\BetterMoonDownloader\browser-profile`). Not headless. | Turnstile rejects headless and automation-flagged browsers (measured in the original). A persistent profile keeps `cf_clearance` between links. Edge is always present on Windows 11. | A visible browser window appears whenever a link needs it, and the user may need to tick a captcha. |
| E4 | The browser launches **lazily**, only when a link needs it, and is respawned if it dies. | Same as the original's `BrowserGate`: no browser for batches that don't need one. | — |
| E5 | One shared browser context. Concurrency is capped by a "pages" semaphore (default 3), not by separate identities. | The original measured separate contexts as *worse* (Turnstile treats several identities from one IP as a bot farm). | Extraction throughput is bounded. |
| E6 | Captcha clicks use CDP `Input.dispatchMouseEvent` (trusted events) with a few pointer moves first. If that fails, the app shows a "Tick the captcha in the browser" banner with a **Show browser** button and waits (default 240 s). | A JS `.click()` is ignored by Turnstile, and a human can always finish it. | Sometimes needs the user. |

## 4. Download engine

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| D1 | **Multi-connection segmented downloads** (IDM-style), default 4 connections per file, with dynamic segment splitting: a finished connection steals half of the largest remaining segment. | This is what makes IDM/FDM fast and what the user asked for. Both CDNs answer `206` with correct `Content-Range`. | Some free CDN lanes may throttle per connection, so it's configurable from 1 to 16. A server that ignores Range gets a single stream. |
| D2 | Downloads go to `<name>.bmdpart` (preallocated to full size) plus a `<name>.bmdpart.json` sidecar with segment progress. On completion the file is renamed atomically. | Preallocation catches a **full disk up front**. The sidecar makes resume exact across restarts and crashes. | Two temp files per active download. |
| D3 | The download client is **HTTP/1.1 only**. | With HTTP/2, all segments would multiplex onto one TCP connection, which defeats multi-connection speed. | — |
| D4 | Persist the queue (`queue.json`) and settings (`settings.json`) in `%APPDATA%\BetterMoonDownloader`. Direct links are cached but re-resolved on 4xx or expiry. | Set-and-forget across restarts. Direct links from both hosts expire. | — |
| D5 | Stall killer as in the original (below 0.5 MB/s over a 60 s window after a 90 s grace, files of 50 MB or more, under 80 % done): re-resolve once to get a different CDN lane. | The original found that the CDN pins a session to a lane, so re-extracting is the fix. | — |
| D6 | When the destination file already exists, mark the job **Done (already on disk)** and don't re-download. | Same as the original. It also makes re-adding the same list safe. | No integrity check (the hosts publish no hashes). |
| D7 | On HTTP 429/503, a connection honours `Retry-After`. If the host keeps refusing, that connection **retires and hands its range back** to the ones still running, instead of failing the whole file. | Found in testing: a host that caps connections per IP made extra segments fail the file. | That file runs on fewer connections. |
| D8 | Files waiting to be probed are counted at the **average known file size** in the batch ETA, and the UI says "roughly" while that's the case. | Multi-part repacks have near-identical part sizes, so this is a good guess, and an honest ETA beats none. | The ETA is approximate until every file has been probed. |
| D9 | Links resolve only a little ahead of free download slots (free slots + 2). | Direct links expire. Resolving the whole list up front would leave most links stale by the time they download. | Slightly less pipelining. |

## 5b. Releases (groups), subfolders and mirrors (v1.1)

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| G1 | Files are grouped by **release name** taken from the filename: `.partN.rar` / `.r00` / `.7z.001` suffixes and uploader tags like `_--_fitgirl-repacks.site_--_` are stripped, and `_`/`.` become spaces (`ExampleGame_--_fitgirl-repacks.site_--_.part1.rar` → **ExampleGame**). | One `.txt` can hold several games, and several `.txt` files can hold one. The filename is the only reliable signal, and grouping by "which paste it came from" would split or merge wrongly. | An oddly named file can end up in a group of its own. That's harmless: it's shown as a plain row. |
| G2 | Each release downloads into **its own subfolder**: `<Save to>\ExampleGame\part1.rar …` (setting, on by default). | User request: all the parts together under the game's name. | Turn it off for the old flat layout. |
| G3 | The **same filename in the same release is one file with mirrors**, not two files. Your two lists (fuckingfast and datanodes, same 4 parts) become 4 files with 4 backup links. | Mirrors exist so one host can cover for the other. Downloading both would waste 8 GB and create `(2)` duplicates. | — |
| G4 | **fuckingfast is preferred** as the primary source when both exist and nothing has started yet. datanodes becomes the fallback. | Live test: fuckingfast resolved in **0.1 s over plain HTTP**, while datanodes took **31 s** in the browser with a captcha. | — |
| G5 | Fall back to the next mirror when a link is dead, when extraction keeps failing after all retries, or when the CDN keeps refusing the direct link. **Partial data carries over**, because it's the same file with the same size, so the sidecar still matches. | Turns host outages into a slowdown instead of a failure. | — |
| G6 | Group controls live **in the UI on top of the existing per-file commands** (pause, resume, remove with a list of ids). No group state is stored in the engine. | The group's state is derived from its files, so there's nothing to keep in sync, and the queue file format only gained optional fields. | — |
| G7 | Each group has a collapsible header (aggregate progress, speed, ETA, finish time), pause/resume/open-folder buttons, and a right-click menu that includes **"Download only this release"** (pause every other release and resume this one). A **Releases** list in the sidebar shows one release at a time with its own summary. | User: "pause one group, download another, resume it later". | Single-file releases show without a header, to avoid noise. |
| G9 | **Companion files join the release they were pasted with.** FitGirl's `fg-selective-*.bin`, `fg-optional-*.bin` and `setup-fitgirl-*.bin` carry no game name, so they take the release listed just before them in the same paste (or just after, if they come first). When a saved list has such a file alone in its own release with **nothing on disk yet**, it's moved into the release added before it at startup. | User pasted 75 parts plus 7 language files and got 8 releases (and 7 stray subfolders); an older release had split into 10. | A companion pasted on its own, with no named release in the same paste, still can't be placed: it goes into a "FitGirl setup files" release. Lone companions that already have data stay where they are, so nothing moves on disk. |
| G10 | **What's moving comes first.** Releases are ordered downloading → waiting → paused/failed → finished, newest paste first within each. Inside a release, rows go downloading → failed → waiting → paused; finished parts fold into one **"N finished · size"** row (closed by default). A fully finished release folds itself away. Waiting and finished rows are shorter, and a finished row drops its (always full) bar. After adding links, the list scrolls to the new release and highlights it once. | User: the files actually downloading were buried under dozens of finished parts, finished and active rows looked alike, and a new paste landed out of sight at the bottom. Replaces V2's "finished files stay in place". | Rows change position as their state changes. The "Completed" filter and search still list finished files one by one. |
| G8 | ~~Inside a group, rows show `part1.rar`~~ **Replaced:** rows always show the **original filename**, trimmed in the middle when it's too long (`ExampleGame_--_fitgirl-re….part1.rar`) so the part number stays visible. Files on disk keep their exact original names (only Windows-illegal characters `<>:"/\|?*` are replaced). | User: "keep the original file names". | Very long names show an ellipsis in the middle. The full name is on hover. |

## 5c. Built-in browser (v1.1, branch `feat/builtin-browser`)

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| W1 | Resolve links in a **hidden window of the app's own WebView2** (the Edge engine built into Windows 10/11) instead of spawning an external Chrome/Edge over CDP. This is now the default; the external browser stays available as a setting. | User: "the user never has to worry about some particular browser being installed". No extra download, and no browser window popping up. | No trusted mouse input from inside a WebView, so a Turnstile box that won't clear on its own must be ticked by the user. It appears in an in-app window at that moment. |
| W2 | **Don't bundle a browser.** | A light browser can't run Cloudflare Turnstile at all. Bundled Chromium adds 150–300 MB, and the original project measured unbranded/headless Chromium being rejected (Error 600010). WebView2 is already present and branded. | Depends on WebView2, which Windows ships and updates. |
| W3 | One provider flow drives both browser types through a `PageOps` trait (eval, goto, reload, captcha, show_to_user). | The datanodes step1/step2/latch/trigger-chain logic is the hard-won part, so it should exist once. | A small trait layer. |
| W4 | The file link is captured with Tauri's native hooks: `on_navigation` (top-level navigations), `on_download` (cancelled downloads) and `on_new_window` (all popups denied, a file link captured). Cookies come from `cookies_for_url` on a blocking thread, since it deadlocks on the WebView2 thread. | WebView2 has no per-request interception through Tauri, but every way the file link can leave the page goes through one of these hooks. | Images and fonts can't be blocked as the CDP path did, so a page uses slightly more bandwidth. |
| W5 | The resolver window uses a **separate persistent profile** (`%LOCALAPPDATA%\BetterMoonDownloader\webview-profile`) and deletes the app's own `__TAURI__` globals before page scripts run. Remote pages get no IPC: the capabilities cover only the `main` window. | Keeps `cf_clearance` between links, removes an obvious fingerprint, and a hostile page can't call app commands. | — |
| W6 | The window **stays hidden** and is shown only when Cloudflare needs a human: after 15 s on the interstitial, or after 10 s of Turnstile not clearing. It hides again afterwards. | In live tests it never had to appear. | — |

**Live results (a real 4-part release, Oct 21 2026, built-in browser):**

| Link | Path | Time | Range check |
|---|---|---|---|
| datanodes part 2 | WebView2, hidden | 21.2 s | `206` ✅ |
| datanodes part 3 | WebView2, hidden | 19.8 s | `206` ✅ |
| datanodes part 4 | normal app routing → WebView2 | 19.2 s | `206` ✅ (279 MB part) |
| fuckingfast part 3 | WebView2 fallback, forced | 8.2 s | `206` ✅ |
| *(v1 reference)* datanodes part 1 | external Edge over CDP | 31 s | `206` ✅ |

The window never needed to be shown, and no captcha click was required in any run.

Test hook: start the app with `BMD_PROBE="<links>"` set. It resolves those links through the built-in browser and writes `%APPDATA%\BetterMoonDownloader\probe.log`.

## 5d. Security hardening (from SECURITY_REVIEW.md)

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| X1 | Downloads go to **sparse** files, with an explicit free-space check. All disk writes run on the blocking pool. | Preallocation made NTFS zero-fill, measured at 2.4 s per deep write and twice the disk writes. | Sparse files don't reserve space, so a parallel copy elsewhere could still fill the disk mid-download. The existing disk-full stop handles that. |
| X2 | The file handoff is **allowlisted by host** (`*.datanodes.to`, `*.dlproxy.uk`, `dl.fuckingfast.co`) and must be HTTPS. | Ad pop-ups and redirects could otherwise put foreign bytes under your filename. | If a host moves its CDN, extraction fails until the list is updated. The error names the new host. |
| X3 | The UI no longer gets Tauri's JS API (`withGlobalTauri: false`) or any plugin permissions. Dialogs, file reads, exports and window controls are app commands with fixed scopes. | It shrinks what a UI bug could ever reach, and removes visible fingerprints from remote pages. | The UI reaches Tauri through its internal `invoke` / `transformCallback`. These are stable in practice but not public API. |
| X4 | The main window is created in Rust with a navigation guard. | A dropped web link could otherwise replace the UI with a website. | Window settings moved out of `tauri.conf.json` into `lib.rs`. |
| X5 | Secrets are sealed with DPAPI, and cookies are never persisted. datanodes links are re-resolved after a restart. | Plain-text secrets in `%APPDATA%`. | A restart costs about 20 s per unfinished datanodes file to get a fresh link. |
| X6 | A file "already on disk" counts only if its size matches. Anything else is moved aside as `.old`. | Stale partials and unrelated files were trusted. | The share link is resolved first, even for files that turn out complete. |

## 6. Build and distribution

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| B1 | Ship both a portable `.exe` (~19 MB) and an NSIS installer (~4.6 MB). | Portable for "just run it", installer for a Start-menu entry and uninstall. | — |
| B2 | Release profile: LTO, `codegen-units = 1`, stripped. | Smallest and fastest binary. | Release builds take about 3–6 minutes. |
| B3 | The UI has a **built-in mock engine** that only runs when opened outside Tauri (in a browser), with synthetic data. | Lets the interface be designed and previewed without the desktop shell. | It's dead code inside the app (a few KB). |
| B4 | From v1.1 the GitHub release carries **only the portable exe** for Windows. The installer is still built locally by `build-release.ps1`. | Portable is what users asked for; one file is less confusing. | No Start-menu entry or uninstaller on Windows. |

## 6b. Linux (v1.2, branch `feat/linux`)

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| L1 | Ship an **AppImage** (portable), plus a **.deb** and an **.rpm** for people who want an app-menu entry. All three are Tauri's own bundles. | The AppImage is the Linux equivalent of the portable exe; .deb and .rpm cover the big distro families with no extra tooling. | Arch users wrap the AppImage themselves (AUR). Flatpak/Flathub left for later: it needs a hand-written manifest, and its sandbox breaks the external-Chrome mode. |
| L2 | Build them in **GitHub Actions on Ubuntu 22.04**, never on a developer machine. | A Linux binary needs a glibc at least as old as the one it was built on, so the oldest supported Ubuntu gives the widest reach. CI paths are generic, so nothing personal is embedded. | Building on Linux requires a push. |
| L3 | Linux uses the **system title bar** and an opaque window. Windows keeps the custom, borderless, Mica window. | Borderless windows lose their resize edges on GNOME/Wayland, and transparent windows render unreliably on WebKitGTK. | Linux doesn't get the custom caption look. |
| L4 | Window buttons are **SVG**, not the Segoe Fluent icon font. | That font exists only on Windows; elsewhere the buttons would be blank. | — |
| L5 | On Linux, `settings.json` is written **owner-only (0600)** instead of DPAPI-encrypted. | DPAPI is Windows-only. 0600 is how Linux apps usually keep tokens (`~/.netrc`, `gh`, `docker`). The system keyring (Secret Service) is absent on many minimal setups and adds a D-Bus dependency. | Any program running as the same user can read the key, as on most Linux apps. |
| L6 | The free-space check uses `statvfs` (`f_bavail`) on Linux. | Same up-front "will it fit" check as Windows, counting only space usable without root. | — |
| L7 | The external browser falls back to Chromium paths on Linux. | Many distros ship Chromium, not Chrome. Flatpak/Snap browsers can't be driven over a debugging port reliably, so only the plain installs are searched (and `/snap/bin/chromium`). | A Flatpak-only Chrome needs the built-in browser instead, which is the default anyway. |

## 7. Verification status

| What | Status |
|---|---|
| Rust unit tests (filename parsing, link parsing, id extraction, datanodes file-URL detection) | ✅ pass |
| Segmented download, interrupted then resumed, compared byte for byte with a plain download (10 MB OVH, 34 MB nodejs.org) | ✅ pass |
| Release exe launches; Mica sidebar, custom title bar and empty state render | ✅ checked with a screenshot of the real window |
| UI in light and dark (browser preview with synthetic data) | ✅ checked |
| Live extraction, fuckingfast (ExampleGame part1) | ✅ resolved in 0.1 s over plain HTTPS (no browser) → `dl.fuckingfast.co/dl/…` |
| Live extraction, datanodes (ExampleGame part1) | ✅ resolved in 31 s through Edge; the captcha was solved automatically → `tunnelN.dlproxy.uk/download/…` plus cookies |
| **Resume support, fuckingfast** | ✅ `206 Partial Content` for every range. 4 parallel ranges (0 MB, 50 MB, 100 MB, 150 MB) all served. The link still worked on a later request. Quirk: `Content-Range` always claims "to end of file", but the body honours the requested range. The downloader caps each range itself, so this is harmless. |
| **Resume support, datanodes** | ✅ `206` with an exact `Content-Range` for every range, 4 parallel ranges served, the link reusable, total size reported (2,097,152,000 bytes). |
| Release grouping and mirror merge for two mirror lists of one 4-part release | ✅ unit test: 8 links → 1 release, 4 files, 4 + 4 sources |
| Full multi-GB download of a real part through the app | ⚠️ not run (2 GB per part). The range checks above cover the mechanism. |
| Linux build (v1.2): tests, AppImage/.deb/.rpm bundling, app starts under a virtual display | ✅ GitHub Actions on Ubuntu 22.04 |
| Linux AppImage on Fedora 44 (WSL, no WebKitGTK installed) | ✅ starts and stays up; no install needed |
| Linux .rpm dependencies on Fedora 44 | ✅ `dnf` dry run resolves `webkit2gtk4.1` from the Fedora repos |
| Linux live link resolution through WebKitGTK (Cloudflare) | ⚠️ not run yet; needs a real Linux desktop |

## 5. Design

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| V1 | The base is an **Apple-standard download manager** (Finder/Transmission-like): translucent sidebar with filters, toolbar, list. Two light touches are mixed in from the other directions. **Tide:** the batch summary shows a live throughput curve and finish times as clock times. **Moon phase:** each row has a small moon that waxes with progress, and the header moon shows the whole batch. | User: "A mix of three… don't make the UI too complicated". | Each metaphor is held to a single small element so it never becomes a gimmick. |
| V2 | **Replaced by G10** (finished parts fold under the live ones). Originally: finished files **stay in place**, stamped with their finish time. Failures stay visible with the reason. | You can always see what is done and what isn't (borrowed from the "ticket wallet" challenger). | The list grows. The sidebar filters handle that. |
| V3 | State is shown by **form, not only colour**: dashed bar = queued, solid = live, hatched = paused, full bar plus a stamp = done, red outline = failed. | Readable for colour-blind users and in either theme. | — |
| V4 | One accent colour, used only for live progress and things you can act on. | Restraint: colour means "this is live or clickable". | — |
| V5 | System font (Segoe UI Variable) with tabular numbers for every metric. | Apple-design rule: the platform font first. Numbers must not jiggle as they update. | — |
| V6 | Light and dark modes follow Windows by default, with a System/Light/Dark override. Windows 11 Mica under the sidebar, with a solid fallback. | Requested. Mica is the Windows equivalent of Apple's translucent materials. | Mica needs Windows 11 build 22000 or later. Windows 10 gets a solid sidebar. |
| V7 | Custom title bar (undecorated window) with native-looking Windows caption buttons. | Unifies the toolbar and title bar the way macOS apps do, so no vertical space is wasted. | We own drag, maximize and snap behaviour (Tauri provides drag regions, and Win11 snap still works). |
| V9 | The sidebar wordmark is two lines, **BetterMoon** / *Downloader*. The folder button sits under a "Save to" label and shows `Parent › Folder` (e.g. `Downloads › BetterMoon`). | The user picked option C of three previewed. A one-line "BetterMoon" at the top plus a folder button also reading "BetterMoon" looked like the app's name twice. | Uses about 4 px more height at the top of the sidebar. |
| V8 | Motion: critically damped, short transitions (sheets 240 ms, rows 160 ms). `prefers-reduced-motion` switches to plain fades. | Apple-design defaults: no bounce unless a gesture carried momentum (none here). | — |
