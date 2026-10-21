mod browser;
mod download;
mod engine;
mod extract;
mod settings;
mod util;
mod webview;
mod probe;

use engine::{AddResult, Engine, Snapshot};
use serde::Serialize;
use settings::Settings;
use std::sync::Arc;
use tauri::{Manager, State};
use tauri_plugin_opener::OpenerExt;

type Eng<'a> = State<'a, Arc<Engine>>;

struct Platform {
    mica: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformInfo {
    mica: bool,
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

/// Text of a dropped or picked link file. Capped so a stray ISO dropped on the
/// window doesn't get read into memory.
#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > 8 * 1024 * 1024 {
        return Err("That file is too large to be a link list".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        app.opener().reveal_item_in_dir(target).map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn open_folder(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let _ = std::fs::create_dir_all(&path);
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
fn write_text_file(path: String, text: String) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| e.to_string())
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
        version: env!("CARGO_PKG_VERSION"),
        browser: browser::find_browser(&eng.settings().browser_path).map(|p| p.display().to_string()),
    }
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Engine::new();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(engine.clone())
        .setup(move |app| {
            let mut mica = false;
            if let Some(w) = app.get_webview_window("main") {
                #[cfg(windows)]
                {
                    mica = window_vibrancy::apply_mica(&w, None).is_ok();
                }
                let _ = w.show();
            }
            app.manage(Platform { mica });
            apply_theme(app.handle(), &engine.settings().theme);
            engine.set_app(app.handle().clone());
            engine.spawn_scheduler();
            if let Ok(links) = std::env::var("BMD_PROBE") {
                probe::run(app.handle().clone(), engine.clone(), links);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            add_links,
            preview_links,
            read_text_file,
            pause,
            resume,
            pause_all,
            resume_all,
            retry_failed,
            remove,
            clear_finished,
            get_settings,
            set_settings,
            reveal,
            open_folder,
            direct_links,
            source_links,
            write_text_file,
            show_browser,
            platform_info,
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
