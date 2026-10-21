# BetterMoonDownloader: Security and Bug Review

**Build reviewed:** 1.1.0, branch `feat/builtin-browser` (commit `685b3ea`) · **Date:** 21 Oct 2026 · **Approach:** adversarial. I tried to break it, then confirmed each finding with a live test or a test run on this machine where possible.

## Verdict

**No exploitable remote vulnerability found.** The most important boundary held under live attack: a hostile web page loaded in the hidden resolver window **could not call any app command**, read or write files, or launch programs.

There are **4 issues worth fixing before merging** (1 performance, 3 integrity/data) and 11 low-severity hardening items.

| ID | Severity | Finding | Proven by |
|---|---|---|---|
| [H1](#h1) | **High** (performance) | Preallocated files make Windows zero-fill on the first write deep into the file: a **2.4 s stall per segment**, the disk writes the file twice, and app worker threads block | Measured on this PC |
| [M1](#m1) | **Medium** (integrity) | The file-link detector trusts **any host**, so an ad pop-up or redirect could be downloaded under your real filename | Unit test |
| [M2](#m2) | **Medium** (data loss) | A corrupt `queue.json` is silently discarded and then overwritten | Code read |
| [M3](#m3) | **Medium** (correctness) | Any existing file with the same name is marked "Done", even if it's partial or unrelated | Code read |
| [L1](#l1) | Low (privacy) | datanodes session cookies are sent to the third-party CDN host | Code read |
| [L2](#l2) | Low (spoofing) | Unicode right-to-left override characters survive in filenames (`rar.exe` displays as `exe.rar`) | Unit test |
| [L3](#l3) | Low | Reserved device names (`CON`, `NUL.rar`, `COM1…`) are created as files most Windows tools can't open or delete | Created on NTFS |
| [L4](#l4) | Low (bug) | Filenames over 255 characters fail the job | Created on NTFS |
| [L5](#l5) | Low (fingerprint) | The "hide app globals" script doesn't work; `__TAURI_INTERNALS__` and plugin globals are visible to remote pages | Live test |
| [L6](#l6) | Low (defense in depth) | The UI's commands can read or write any file and open (run) any path | Code read |
| [L7](#l7) | Low | The `BMD_PROBE` test hook is compiled into release builds | Code read |
| [L8](#l8) | Low (bug) | "Remove and delete partial" on an active download leaves an orphan `.bmdpart.json` | Code read |
| [L9](#l9) | Low (at rest) | API key, proxy passwords and session cookies are stored in plain JSON | Code read |
| [L10](#l10) | Low | Downloads follow redirects to any host, including `http://` and LAN addresses | Code read |
| [L11](#l11) | Info | External-browser mode exposes a DevTools debugging port on localhost while it runs | Design |

---

## What held up (attacks that failed)

| Attack | Result |
|---|---|
| **Hostile page → app commands.** Loaded a remote page in a resolver window and called `read_text_file(C:\Windows\win.ini)`, `write_text_file(C:\Users\Public\bmd-pwned.txt)`, `open_folder(calc.exe)`, `get_settings`, opener `open_url`, dialog `open` through the leaked `__TAURI_INTERNALS__.invoke` | **All 6 denied** by Tauri's permission list ("not allowed on window resolver-1 … URL: https"). No file written, no calculator. |
| **Hostile page → raw IPC.** Hand-crafted `POST http://ipc.localhost/<cmd>` | **Rejected:** `500 missing Tauri-Invoke-Key header` |
| **Path traversal in filenames.** `#../../Windows/evil.dll`, `#..`, NUL bytes, `file.rar:hidden` (alternate data stream) | **Neutralised:** `_.._Windows_evil.dll`, `download`, `a_b.rar`, `file.rar_hidden`. Nothing escapes the download folder. |
| **XSS in the UI.** Reviewed all 18 `innerHTML` writes in `ui/app.js` | Every untrusted value (filenames, errors, log lines, release names, search text) is escaped. CSP `script-src 'self'` also blocks inline script and `on*=` handlers as a second wall. |
| **Drive-by downloads or pop-ups in the resolver** | Every browser download is cancelled (`on_download` → false), every pop-up is denied, and the resolver uses its own profile and browser process, separate from the app UI. |
| **Resume integrity** | Interrupted then resumed downloads matched byte for byte (10 MB, 34 MB), and real direct links answer `206` to ranges. |
| **Known-vulnerable dependencies** | `cargo audit`: **0 vulnerabilities** in 535 crates. The 5 "unmaintained/unsound" warnings are in build-time or Linux-only crates (`glib`, `zvariant`, `zbus_names`, `proc-macro-error`). None are compiled into the Windows exe. |

---

## Findings

### H1. NTFS zero-fill stall on preallocated files {#h1}
**Where:** `download.rs`. `set_len(total)` and then `seek_write` at each segment's offset.
**Proof:** a 4 GiB file preallocated instantly (165 µs), but **the first 1 MiB write at the 3 GiB offset took 2.38 s**. The next write took 0.26 ms. NTFS must zero everything before a write that lands past the file's "valid data length".
**Impact:**
- Every segment that starts deep in a file pays this cost: up to 4 connections × 4 files at once.
- It's far worse on HDDs and USB drives.
- The disk writes about **twice the file size**: the zeros, then the real data.
- The write runs inside an async task, so it **blocks the app's worker threads**. Snapshots and the UI can stutter during these stalls.
**Fix:**
- Mark the file sparse (`FSCTL_SET_SPARSE`) before `set_len`. No zero-fill, and resume still works.
- Sparse files don't reserve space, so add an explicit free-space check (`GetDiskFreeSpaceExW`) up front to keep the early "disk full" detection.
- Move disk writes to `spawn_blocking` / a writer thread.

### M1. Link capture trusts any host {#m1}
**Where:**
- `extract/datanodes.rs::is_file_url`, used by the external browser's request interception and by the built-in browser's `on_navigation` / `on_new_window` / `on_download`.
- `webview.rs::resolve_fuckingfast` (a substring check).

**Proof (unit test):**

| Candidate URL | Accepted as "the file"? |
|---|---|
| `https://evil.example.com/totally/legit/setup.exe` | **yes** |
| `https://evil.example.com/ExampleGame.part1.rar.html` | **yes** |
| `file:///C:/Windows/System32/config/SAM.rar` | **yes** (download then fails; reqwest refuses `file:`) |
| fuckingfast: `https://evil.example.com/?r=fuckingfast.co/dl/AAAA` | **yes** |

**Impact:** datanodes pages run ad scripts that open pop-ups and redirects. If an ad (or a compromised page) pops a URL ending in `.exe` / `.rar` / `.zip`, the app records it as the file and **downloads attacker-controlled bytes under your trusted name** (`ExampleGame…part1.rar`). Nothing runs automatically, but extracting the archive could hand you a booby-trapped file, or at best a corrupt part that fails extraction.
**Fix:**
- Parse the host and accept only `https://`.
- Allowlist known handoff hosts: `*.datanodes.to`, `*.dlproxy.uk`, `dl.fuckingfast.co`. Log and reject anything else, so a CDN change shows up as a clear error, not a silent switch.
- Never accept `.exe` from pop-ups.
- After the probe, compare the server's size with the size the host page shows.

### M2. A corrupt queue is silently discarded {#m2}
**Where:** `engine.rs::Engine::new`. A `queue.json` that fails to parse becomes `unwrap_or_default()`, an empty queue. The next save overwrites the file.
**Impact:** one bad write (a crash mid-rename, antivirus interference, a manual edit) wipes your whole queue with no warning. The partial files stay on disk but the app forgets them.
**Fix:** on a parse failure, rename the file to `queue.corrupt-<timestamp>.json`, log it, and show a banner. Keep `queue.json.bak` from the previous good save.

### M3. "Already on disk" trusts the filename alone {#m3}
**Where:** `engine.rs::start_download`. `dest.exists() && !part.exists()` → Done.
**Impact:** a leftover from another downloader, an earlier failed attempt, or an unrelated file with the same name is reported as completed. You only find out when extraction fails.
**Fix:** probe first and mark Done only if the size on disk equals the server's size. Otherwise keep the old file aside (`name.old`) or ask the user.

### L1. Session cookies sent to the CDN {#l1}
**Where:** `webview.rs::resolve_datanodes` and the CDP path both merge cookies for `https://datanodes.to/` **and** the direct link, then send them all to the direct-link host (`tunnel5.dlproxy.uk`).
**Impact:** your datanodes session and `cf_clearance` cookies go to a third-party host that doesn't need them.
**Fix:** send only cookies that match the direct URL's host. Re-test that range requests still return `206` without the rest.

### L2. Bidi control characters in filenames {#l2}
**Proof:** `#Game%E2%80%AErar.exe` is saved as `Game\u202Erar.exe`, which Explorer displays as **`Gameexe.rar`**. It's an executable posing as an archive.
**Fix:** strip U+200E/U+200F, U+202A–U+202E and U+2066–U+2069 in `sanitize_filename`.

### L3. Reserved device names {#l3}
**Proof:** `CON`, `nul.rar` and `COM1.part1.rar` were all created successfully. Rust uses `\\?\` paths, so it gets past the usual block, but Explorer and most tools can't open or delete such files.
**Fix:** if the stem matches `CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9]`, prefix it with `_`.

### L4. Over-long filenames fail the job {#l4}
**Proof:** a 300-character name failed with `os error 123 (InvalidFilename)`. The job would end as "Write error".
**Fix:** truncate the stem to fit 255 characters minus the `.bmdpart.json` suffix, keeping the extension.

### L5. App globals are visible to remote pages {#l5}
**Proof:** in the resolver window, a remote page sees `__TAURI_INTERNALS__` (object), `__TAURI_IIFE__`, `__TAURI_PLUGIN_DIALOG__` and `__TAURI_PLUGIN_OPENER__`. The cleanup script runs before Tauri defines them, so it has no effect.
**Impact:** no access (see "What held up"), but it's a fingerprint no real Edge has, which Cloudflare could start scoring.
**Fix:** turn `withGlobalTauri` off and call the few APIs the UI needs through `invoke`. That removes the plugin and IIFE globals. Separately, delete `__TAURI_INTERNALS__` at `DOMContentLoaded`, after Tauri has set it up.

### L6. Over-powered UI commands {#l6}
**Where:** `lib.rs`.
- `read_text_file(path)` reads any file up to 8 MB.
- `write_text_file(path, text)` writes anywhere.
- `open_folder(path)` calls `open_path`, which **opens (runs) a file** if the path isn't a folder. `open_folder("C:\\Windows\\System32\\calc.exe")` would launch Calculator.

**Impact:** none today. Only the local UI can call these, and no XSS was found. But if an XSS bug ever slipped in, this would turn it into code execution.
**Fix:**
- `open_folder`: refuse anything that isn't an existing directory.
- `write_text_file`: accept only a path the save dialog returned in this session.
- `read_text_file`: limit it to `.txt/.html/.md/.csv`.

### L7. Test hook in release builds {#l7}
`BMD_PROBE` is checked unconditionally in `setup`. Anything that can launch the app with that environment variable can make it open pages and write `probe.log`.
**Fix:** wrap it in `#[cfg(debug_assertions)]`.

### L8. Orphan sidecar after "Remove and delete partial" {#l8}
`Engine::remove` deletes `.bmdpart` and `.bmdpart.json` right away. The download task then hits its cancel path and **rewrites** the `.json` checkpoint, leaving an orphan file.
**Fix:** delete the files after the task has stopped: record "delete on finish" and do it in the task's exit path.

### L9. Secrets in plain JSON {#l9}
`settings.json` holds the datanodes API key and proxy credentials, and `queue.json` holds datanodes session cookies, all readable by any program running as you.
**Fix:** encrypt the API key and proxy passwords with Windows DPAPI (`CryptProtectData`), and don't save cookies; re-resolve instead.

### L10. Unrestricted redirects on downloads {#l10}
The download client uses reqwest's default policy: up to 10 hops, to any host, including `https`→`http` and private IPs. Cookies are dropped across hosts (good), but a redirect can still silently downgrade the transfer or aim it at your LAN.
**Fix:** a custom redirect policy: `https` only, no private or loopback addresses, at most 5 hops.

### L11. DevTools port in external-browser mode {#l11}
With the built-in browser turned off, Chrome/Edge runs with `--remote-debugging-port` on a random 127.0.0.1 port. Any local process can drive that browser profile while it's open. It's the original project's design, and it's no longer the default.

---

## Not tested (gaps)

- **Dropping a web link (not a file) onto the main window.** It could make the UI navigate away to the website. The ACL would still block that site from calling commands (verified above for remote origins), but the UI would be gone until restart. **Fix:** block non-local navigation in the main window.
- A real "Cloudflare demands a click" case in the built-in browser. It never happened in testing.
- Very large lists: adding is O(n²) on duplicate checks, and the UI re-renders every row about 4×/s. Expect slowdowns past roughly 5–10k files.
- Behaviour on HDDs and network drives (H1 will be much worse there).

## Suggested fix order

1. **H1** (sparse files + free-space check + blocking writer). Biggest real-world effect.
2. **M1** (host allowlist + size check). The only path to "wrong bytes under the right name".
3. **M2, M3** (protect the queue; size-check "already on disk").
4. **L2, L3, L4** (filename hardening; small, one function).
5. **L1, L6, L7, L8, L10** (small, contained changes).
6. **L5, L9** (larger: UI API rework, DPAPI).

## How to re-run the checks

```bash
cd src-tauri && cargo test --lib && cargo audit
```

Hostile-page isolation test (debug build): start the app with `BMD_PROBE=ipc` set, then read `%APPDATA%\BetterMoonDownloader\probe.log`. Every `inv:*` line must say `denied`.
