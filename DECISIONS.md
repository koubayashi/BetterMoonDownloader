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
| G8 | ~~Inside a group, rows show `part1.rar`~~ **Replaced:** rows always show the **original filename**, trimmed in the middle when it's too long (`ExampleGame_--_fitgirl-re….part1.rar`) so the part number stays visible. Files on disk keep their exact original names (only Windows-illegal characters `<>:"/\|?*` are replaced). | User: "keep the original file names". | Very long names show an ellipsis in the middle. The full name is on hover. |

## 6. Build and distribution

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| B1 | Ship both a portable `.exe` (~19 MB) and an NSIS installer (~4.6 MB). | Portable for "just run it", installer for a Start-menu entry and uninstall. | — |
| B2 | Release profile: LTO, `codegen-units = 1`, stripped. | Smallest and fastest binary. | Release builds take about 3–6 minutes. |
| B3 | The UI has a **built-in mock engine** that only runs when opened outside Tauri (in a browser), with synthetic data. | Lets the interface be designed and previewed without the desktop shell. | It's dead code inside the app (a few KB). |

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
| Release grouping and mirror merge for the two ExampleGame lists | ✅ unit test: 8 links → 1 release, 4 files, 4 + 4 sources |
| Full multi-GB download of a real part through the app | ⚠️ not run (2 GB per part). The range checks above cover the mechanism. |

## 5. Design

| # | Decision | Why | Trade-off |
|---|----------|-----|-----------|
| V1 | The base is an **Apple-standard download manager** (Finder/Transmission-like): translucent sidebar with filters, toolbar, list. Two light touches are mixed in from the other directions. **Tide:** the batch summary shows a live throughput curve and finish times as clock times. **Moon phase:** each row has a small moon that waxes with progress, and the header moon shows the whole batch. | User: "A mix of three… don't make the UI too complicated". | Each metaphor is held to a single small element so it never becomes a gimmick. |
| V2 | Finished files **stay in place**, stamped with their finish time. Failures stay visible with the reason. | You can always see what is done and what isn't (borrowed from the "ticket wallet" challenger). | The list grows. The sidebar filters handle that. |
| V3 | State is shown by **form, not only colour**: dashed bar = queued, solid = live, hatched = paused, full bar plus a stamp = done, red outline = failed. | Readable for colour-blind users and in either theme. | — |
| V4 | One accent colour, used only for live progress and things you can act on. | Restraint: colour means "this is live or clickable". | — |
| V5 | System font (Segoe UI Variable) with tabular numbers for every metric. | Apple-design rule: the platform font first. Numbers must not jiggle as they update. | — |
| V6 | Light and dark modes follow Windows by default, with a System/Light/Dark override. Windows 11 Mica under the sidebar, with a solid fallback. | Requested. Mica is the Windows equivalent of Apple's translucent materials. | Mica needs Windows 11 build 22000 or later. Windows 10 gets a solid sidebar. |
| V7 | Custom title bar (undecorated window) with native-looking Windows caption buttons. | Unifies the toolbar and title bar the way macOS apps do, so no vertical space is wasted. | We own drag, maximize and snap behaviour (Tauri provides drag regions, and Win11 snap still works). |
| V9 | The sidebar wordmark is two lines, **BetterMoon** / *Downloader*. The folder button sits under a "Save to" label and shows `Parent › Folder` (e.g. `Downloads › BetterMoon`). | The user picked option C of three previewed. A one-line "BetterMoon" at the top plus a folder button also reading "BetterMoon" looked like the app's name twice. | Uses about 4 px more height at the top of the sidebar. |
| V8 | Motion: critically damped, short transitions (sheets 240 ms, rows 160 ms). `prefers-reduced-motion` switches to plain fades. | Apple-design defaults: no bounce unless a gesture carried momentum (none here). | — |
