mod browser;
mod download;
mod engine;
mod extract;
#[cfg(debug_assertions)]
mod probe;
mod settings;
mod util;
mod webview;

use engine::{AddResult, Engine, Snapshot};
use serde::Serialize;
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

type Eng<'a> = State<'a, Arc<Engine>>;

struct Platform {
    mica: bool,
    native_frame: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformInfo {
    mica: bool,
    native_frame: bool,
    version: &'static str,
    browser: Option<String>,
}

#[tauri::command]
fn snapshot(eng: Eng, log_after: u64) -> Snapshot {
    eng.snapshot(log_after)
}

#[tauri::command]
fn add_links(eng: Eng, text: String, out_dir: Option<String>, start: bool) -> AddResult {
    eng.add_links(&text, out_dir, start)
}

#[tauri::command]
fn preview_links(text: String) -> engine::Plan {
    engine::plan_links(&text)
}

// ── files: the UI never names a path it didn't get from a dialog or a drop ──

const LINK_FILE_EXTS: &[&str] = &["txt", "text", "html", "htm", "md", "csv", "log"];
const LINK_FILE_MAX: u64 = 8 * 1024 * 1024;

/// Text of link-list files. Only text-like extensions, capped in size, so the
/// command can't be bent into a general file reader.
fn read_link_files(paths: &[PathBuf]) -> Result<String, String> {
    let mut text = String::new();
    for p in paths {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !LINK_FILE_EXTS.contains(&ext.as_str()) {
            return Err(format!("{} isn't a link list (.txt, .html, .md or .csv)", display_name(p)));
        }
        let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > LINK_FILE_MAX {
            return Err(format!("{} is too large to be a link list", display_name(p)));
        }
        let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
        text.push_str(&String::from_utf8_lossy(&bytes));
        text.push('\n');
    }
    Ok(text)
}

fn display_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Files dropped on the window (paths come from Windows' drop event).
#[tauri::command]
fn read_dropped_files(paths: Vec<PathBuf>) -> Result<String, String> {
    read_link_files(&paths)
}

#[tauri::command]
async fn pick_link_files(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter("Link lists", LINK_FILE_EXTS)
        .blocking_pick_files();
    let Some(files) = picked else { return Ok(None) };
    let paths: Vec<PathBuf> = files.into_iter().filter_map(|f| f.into_path().ok()).collect();
    read_link_files(&paths).map(Some)
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle, current: Option<String>) -> Option<String> {
    let mut d = app.dialog().file();
    if let Some(c) = current.filter(|c| Path::new(c).is_dir()) {
        d = d.set_directory(c);
    }
    d.blocking_pick_folder()
        .and_then(|f| f.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Save resolved direct links. The file is chosen in the save dialog here,
/// never passed in from the page.
#[tauri::command]
async fn export_links(app: tauri::AppHandle, eng: State<'_, Arc<Engine>>) -> Result<Option<usize>, String> {
    let links = eng.direct_links(None);
    if links.is_empty() {
        return Ok(Some(0));
    }
    let Some(path) = app
        .dialog()
        .file()
        .set_file_name("direct_links.txt")
        .add_filter("Text", &["txt"])
        .blocking_save_file()
        .and_then(|f| f.into_path().ok())
    else {
        return Ok(None);
    };
    std::fs::write(&path, links.join("\n") + "\n").map_err(|e| e.to_string())?;
    Ok(Some(links.len()))
}

#[tauri::command]
async fn confirm(app: tauri::AppHandle, message: String, ok_label: String) -> bool {
    app.dialog()
        .message(message)
        .title("BetterMoonDownloader")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(ok_label, "Cancel".into()))
        .blocking_show()
}

#[tauri::command]
fn pause(eng: Eng, ids: Vec<u64>) {
    eng.pause(&ids)
}

#[tauri::command]
fn resume(eng: Eng, ids: Vec<u64>) {
    eng.resume(&ids)
}

#[tauri::command]
fn pause_all(eng: Eng) {
    eng.pause_all()
}

#[tauri::command]
fn resume_all(eng: Eng) {
    eng.resume_all()
}

#[tauri::command]
fn retry_failed(eng: Eng) {
    eng.retry_failed()
}

#[tauri::command]
fn remove(eng: Eng, ids: Vec<u64>, delete_partial: bool) {
    eng.remove(&ids, delete_partial)
}

#[tauri::command]
fn clear_finished(eng: Eng) {
    eng.clear_finished()
}

#[tauri::command]
fn dismiss_notice(eng: Eng) {
    eng.dismiss_notice()
}

#[tauri::command]
fn get_settings(eng: Eng) -> Settings {
    eng.settings()
}

#[tauri::command]
fn set_settings(app: tauri::AppHandle, eng: Eng, settings: Settings) -> Settings {
    let s = eng.set_settings(settings);
    apply_theme(&app, &s.theme);
    s
}

#[tauri::command]
fn reveal(app: tauri::AppHandle, eng: Eng, id: u64) -> Result<(), String> {
    let (dest, done) = eng.job_paths(id).ok_or("no such download")?;
    let target = if done && dest.exists() {
        dest
    } else {
        let part = download::part_path(&dest);
        if part.exists() { part } else { dest.parent().map(|p| p.to_path_buf()).unwrap_or(dest) }
    };
    if target.is_dir() {
        app.opener().open_path(target.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
    } else {
        // Select the file in Explorer; never open (run) it.
        app.opener().reveal_item_in_dir(target).map_err(|e| e.to_string())
    }
}

/// Open a folder in Explorer. Folders only: `open_path` on a file would run it.
#[tauri::command]
fn open_folder(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.exists() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    if !p.is_dir() {
        return Err("That isn't a folder".into());
    }
    app.opener().open_path(path, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn direct_links(eng: Eng, ids: Option<Vec<u64>>) -> Vec<String> {
    eng.direct_links(ids.as_deref())
}

#[tauri::command]
fn source_links(eng: Eng, ids: Vec<u64>) -> Vec<String> {
    eng.source_links(&ids)
}

#[tauri::command]
async fn show_browser(eng: State<'_, Arc<Engine>>) -> Result<(), String> {
    eng.gate.show().await;
    Ok(())
}

#[tauri::command]
fn platform_info(eng: Eng, platform: State<'_, Platform>) -> PlatformInfo {
    PlatformInfo {
        mica: platform.mica,
        native_frame: platform.native_frame,
        version: env!("CARGO_PKG_VERSION"),
        browser: browser::find_browser(&eng.settings().browser_path).map(|p| p.display().to_string()),
    }
}

// ── window controls (the window is undecorated) ─────────────────────────────

fn main_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, String> {
    app.get_webview_window("main").ok_or_else(|| "no main window".to_string())
}

#[tauri::command]
fn win_minimize(app: tauri::AppHandle) -> Result<(), String> {
    main_window(&app)?.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
fn win_toggle_maximize(app: tauri::AppHandle) -> Result<bool, String> {
    let w = main_window(&app)?;
    if w.is_maximized().unwrap_or(false) {
        w.unmaximize().map_err(|e| e.to_string())?;
        Ok(false)
    } else {
        w.maximize().map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[tauri::command]
fn win_is_maximized(app: tauri::AppHandle) -> bool {
    main_window(&app).and_then(|w| w.is_maximized().map_err(|e| e.to_string())).unwrap_or(false)
}

#[tauri::command]
fn win_close(app: tauri::AppHandle) -> Result<(), String> {
    main_window(&app)?.close().map_err(|e| e.to_string())
}

fn apply_theme(app: &tauri::AppHandle, theme: &str) {
    if let Some(w) = app.get_webview_window("main") {
        let t = match theme {
            "light" => Some(tauri::Theme::Light),
            "dark" => Some(tauri::Theme::Dark),
            _ => None,
        };
        let _ = w.set_theme(t);
    }
}

/// The app's own pages only. A link dropped on the window, or anything else
/// that tries to navigate the main window to the web, is refused.
fn is_app_url(u: &url::Url) -> bool {
    matches!(u.scheme(), "tauri")
        || (matches!(u.scheme(), "http" | "https") && u.host_str() == Some("tauri.localhost"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Engine::new();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(engine.clone())
        .setup(move |app| {
            // Built here rather than in tauri.conf.json so it can carry the
            // navigation guard. Linux keeps the system title bar: borderless,
            // transparent windows lose their resize edges on GNOME/Wayland and
            // render unreliably on WebKitGTK.
            let native_frame = cfg!(target_os = "linux");
            let w = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("BetterMoonDownloader")
                .inner_size(1180.0, 760.0)
                .min_inner_size(760.0, 480.0)
                .decorations(native_frame)
                .transparent(!native_frame)
                .shadow(true)
                .visible(false)
                .center()
                .on_navigation(is_app_url)
                .build()?;
            #[cfg(windows)]
            let mica = window_vibrancy::apply_mica(&w, None).is_ok();
            #[cfg(not(windows))]
            let mica = false;
            let _ = w.show();
            app.manage(Platform { mica, native_frame });
            apply_theme(app.handle(), &engine.settings().theme);
            engine.set_app(app.handle().clone());
            engine.spawn_scheduler();
            // Developer test hook; not compiled into release builds.
            #[cfg(debug_assertions)]
            if let Ok(links) = std::env::var("BMD_PROBE") {
                probe::run(app.handle().clone(), engine.clone(), links);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            add_links,
            preview_links,
            read_dropped_files,
            pick_link_files,
            pick_folder,
            export_links,
            confirm,
            pause,
            resume,
            pause_all,
            resume_all,
            retry_failed,
            remove,
            clear_finished,
            dismiss_notice,
            get_settings,
            set_settings,
            reveal,
            open_folder,
            direct_links,
            source_links,
            show_browser,
            platform_info,
            win_minimize,
            win_toggle_maximize,
            win_is_maximized,
            win_close,
        ])
        .build(tauri::generate_context!())
        .expect("error while building BetterMoonDownloader")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let eng = app.state::<Arc<Engine>>().inner().clone();
                tauri::async_runtime::block_on(async move { eng.shutdown().await });
            }
        });
}

#[cfg(test)]
mod nav_tests {
    #[test]
    fn main_window_stays_on_app_pages() {
        let ok = |s: &str| super::is_app_url(&s.parse().unwrap());
        assert!(ok("http://tauri.localhost/index.html"));
        assert!(ok("tauri://localhost/index.html"));
        assert!(!ok("https://datanodes.to/abc"));
        assert!(!ok("https://tauri.localhost.evil.com/"));
        assert!(!ok("file:///C:/Windows/win.ini"));
    }
}

#[cfg(test)]
mod live {
    //! Live checks against the real hosts. Opens a browser window.
    //! `BMD_LINKS="url1 url2" cargo test --lib live_resume -- --ignored --nocapture`
    use super::*;

    async fn range(client: &reqwest::Client, url: &str, cookies: &Option<String>, r: &str) -> (u16, String, usize) {
        let mut b = client.get(url).header("Range", r).header("Referer", util::referer_for(url));
        if let Some(c) = cookies {
            b = b.header("Cookie", c);
        }
        match b.send().await {
            Ok(resp) => {
                let st = resp.status().as_u16();
                let cr = resp
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let n = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
                (st, cr, n)
            }
            Err(e) => (0, e.to_string(), 0),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn live_resume() {
        let links = std::env::var("BMD_LINKS").expect("BMD_LINKS");
        let attention = std::sync::Arc::new(engine::Attention::default());
        let gate = std::sync::Arc::new(browser::BrowserGate::new(attention));
        let ctx = extract::ExtractCtx { http: extract::extraction_client(), gate: gate.clone(), settings: settings::Settings::default(), app: None };
        let client = reqwest::Client::builder().user_agent(util::USER_AGENT).http1_only().build().unwrap();
        for url in links.split_whitespace() {
            println!("\n== {url}");
            let t = std::time::Instant::now();
            let r = match extract::resolve(&ctx, url, "test").await {
                Ok(r) => r,
                Err(e) => {
                    println!("RESOLVE FAILED after {:?}: {e:?}", t.elapsed());
                    continue;
                }
            };
            println!("resolved in {:.1}s -> {}", t.elapsed().as_secs_f32(), &r.url[..r.url.len().min(90)]);
            println!("cookies: {}", r.cookies.as_deref().map(|c| c.len()).unwrap_or(0));
            println!("probe 0-0       : {:?}", range(&client, &r.url, &r.cookies, "bytes=0-0").await);
            println!("mid 1000000-1999999: {:?}", range(&client, &r.url, &r.cookies, "bytes=1000000-1999999").await);
            // Parallel connections, as the segmented downloader does.
            let (a, b, c, d) = tokio::join!(
                range(&client, &r.url, &r.cookies, "bytes=0-1048575"),
                range(&client, &r.url, &r.cookies, "bytes=50000000-51048575"),
                range(&client, &r.url, &r.cookies, "bytes=100000000-101048575"),
                range(&client, &r.url, &r.cookies, "bytes=150000000-151048575"),
            );
            println!("4 parallel      : {:?} | {:?} | {:?} | {:?}", a, b, c, d);
            println!("again (reuse)   : {:?}", range(&client, &r.url, &r.cookies, "bytes=200000000-200999999").await);
        }
        gate.shutdown().await;
    }
}
