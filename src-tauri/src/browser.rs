//! The one real browser every extraction shares, opened on first demand.
//!
//! Turnstile hands no token to headless or automation-flagged Chromium, so we
//! spawn a real Chrome (or Edge) ourselves with only a debugging port and a
//! dedicated persistent profile, then attach over CDP. One profile means one
//! `cf_clearance` cookie: solve once and later links are challenged less.

use chromiumoxide::cdp::browser_protocol::browser::{
    Bounds, GetWindowForTargetParams, SetWindowBoundsParams, WindowState,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, Page};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::engine::Attention;

/// Hides the two or three signals a real branded build still leaks over CDP.
const STEALTH_JS: &str = r#"
Object.defineProperty(navigator, 'webdriver', {get: () => undefined});
Object.defineProperty(navigator, 'languages', {get: () => ['en-US', 'en']});
"#;

struct Live {
    browser: Arc<Browser>,
    child: Option<Child>,
    handler: tokio::task::JoinHandle<()>,
}

pub struct BrowserGate {
    live: Mutex<Option<Live>>,
    lanes: std::sync::Mutex<(usize, Arc<Semaphore>)>,
    pub attention: Arc<Attention>,
}

/// A checked-out tab slot on the shared window. Drop to free it.
pub struct Lane {
    pub browser: Arc<Browser>,
    _permit: OwnedSemaphorePermit,
}

impl BrowserGate {
    pub fn new(attention: Arc<Attention>) -> Self {
        Self {
            live: Mutex::new(None),
            lanes: std::sync::Mutex::new((3, Arc::new(Semaphore::new(3)))),
            attention,
        }
    }

    pub fn is_open(&self) -> bool {
        self.live.try_lock().map(|g| g.is_some()).unwrap_or(true)
    }

    /// Wait for one of the `pages` tab slots, launching the browser if needed.
    pub async fn acquire(&self, pages: usize, exe_override: &str) -> Result<Lane, String> {
        let sem = {
            let mut l = self.lanes.lock().unwrap();
            if l.0 != pages {
                *l = (pages, Arc::new(Semaphore::new(pages)));
            }
            l.1.clone()
        };
        let permit = sem.acquire_owned().await.map_err(|e| e.to_string())?;
        let browser = self.get(exe_override).await?;
        Ok(Lane { browser, _permit: permit })
    }

    /// The live shared browser; respawned if the previous one died.
    async fn get(&self, exe_override: &str) -> Result<Arc<Browser>, String> {
        let mut guard = self.live.lock().await;
        if let Some(live) = guard.as_ref() {
            let ok = tokio::time::timeout(Duration::from_secs(4), live.browser.version())
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
            if ok {
                return Ok(live.browser.clone());
            }
            log::warn!("shared browser stopped answering; respawning");
            if let Some(old) = guard.take() {
                shutdown_live(old);
            }
        }
        let live = launch(exe_override).await?;
        let b = live.browser.clone();
        *guard = Some(live);
        Ok(b)
    }

    pub async fn shutdown(&self) {
        if let Some(live) = self.live.lock().await.take() {
            shutdown_live(live);
        }
    }

    /// Put the shared window on screen, restored, so the user can tick a captcha.
    pub async fn show(&self) {
        let guard = self.live.lock().await;
        let Some(live) = guard.as_ref() else { return };
        if let Ok(pages) = live.browser.pages().await {
            if let Some(p) = pages.last() {
                let _ = p.bring_to_front().await;
                if let Ok(w) = p
                    .execute(GetWindowForTargetParams::builder().target_id(p.target_id().clone()).build())
                    .await
                {
                    let bounds = Bounds::builder().window_state(WindowState::Normal).build();
                    let _ = p
                        .execute(SetWindowBoundsParams::new(w.result.window_id, bounds))
                        .await;
                }
            }
        }
    }
}

fn shutdown_live(mut live: Live) {
    live.handler.abort();
    if let Some(mut c) = live.child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// A real Chrome first, Edge second (always present on Windows 11), then
/// Chromium on Linux.
pub fn find_browser(exe_override: &str) -> Option<PathBuf> {
    if !exe_override.is_empty() && Path::new(exe_override).exists() {
        return Some(PathBuf::from(exe_override));
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Ok(root) = std::env::var(var) {
            cands.push(Path::new(&root).join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    for var in ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"] {
        if let Ok(root) = std::env::var(var) {
            cands.push(Path::new(&root).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    cands.extend(
        [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/opt/google/chrome/chrome",
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ]
        .map(PathBuf::from),
    );
    cands.into_iter().find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(9333)
}

async fn launch(exe_override: &str) -> Result<Live, String> {
    let exe = find_browser(exe_override)
        .ok_or("No Chrome or Edge found. Set the browser path in Settings.")?;
    let profile = crate::settings::browser_profile_dir();
    let _ = std::fs::create_dir_all(&profile);
    let port = free_port();

    // Every extra flag is a bot signal, so this list stays short on purpose.
    let mut cmd = Command::new(&exe);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-features=Translate,OptimizationHints")
        .arg("--window-size=1280,900")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (console only)
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;

    let http = reqwest::Client::new();
    let started = Instant::now();
    let ws = loop {
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            return Err(format!("{} did not open its debugging port", exe.display()));
        }
        if let Ok(Some(_)) = child.try_wait() {
            return Err(format!(
                "{} exited at launch. Is another copy using the BetterMoon profile?",
                exe.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let Ok(r) = http.get(format!("http://127.0.0.1:{port}/json/version")).send().await else {
            continue;
        };
        if let Ok(v) = r.json::<Value>().await {
            if let Some(ws) = v.get("webSocketDebuggerUrl").and_then(|w| w.as_str()) {
                break ws.to_string();
            }
        }
    };

    let cfg = HandlerConfig { viewport: None, request_timeout: Duration::from_secs(60), ..Default::default() };
    let (browser, mut handler) = Browser::connect_with_config(ws, cfg)
        .await
        .map_err(|e| format!("could not attach to the browser: {e}"))?;
    let handler = tokio::spawn(async move {
        // Newer Chrome builds emit events this CDP schema does not know; those
        // surface as errors here and are safe to skip.
        while let Some(_ev) = handler.next().await {}
    });
    log::info!("attached to {} on port {port}", exe.display());
    Ok(Live { browser: Arc::new(browser), child: Some(child), handler })
}

// ── page helpers ────────────────────────────────────────────────────────────

pub async fn new_page(browser: &Browser) -> Result<Page, String> {
    let page = browser.new_page("about:blank").await.map_err(|e| e.to_string())?;
    let _ = page.execute(AddScriptToEvaluateOnNewDocumentParams::new(STEALTH_JS)).await;
    Ok(page)
}

/// Call a JS function source with an optional JSON argument and decode the
/// result. Survives a navigation landing mid-call by retrying a few times.
pub async fn eval<T: DeserializeOwned>(page: &Page, func: &str, arg: Option<Value>) -> Option<T> {
    let arg = arg.map(|a| a.to_string()).unwrap_or_default();
    let expr = format!("({func})({arg})");
    for i in 0..4u64 {
        let params = EvaluateParams::builder()
            .expression(expr.clone())
            .return_by_value(true)
            .await_promise(true)
            .build()
            .ok()?;
        match tokio::time::timeout(Duration::from_secs(10), page.execute(params)).await {
            Ok(Ok(resp)) => {
                if resp.result.exception_details.is_some() {
                    return None;
                }
                let v = resp.result.result.value.clone().unwrap_or(Value::Null);
                return serde_json::from_value(v).ok();
            }
            _ => tokio::time::sleep(Duration::from_millis(350 * (i + 1))).await,
        }
    }
    None
}

impl crate::extract::PageOps for Page {
    async fn eval_value(&self, func: &str, arg: Option<Value>) -> Option<Value> {
        eval::<Value>(self, func, arg).await
    }
    async fn goto(&self, url: &str) {
        let _ = self
            .execute(chromiumoxide::cdp::browser_protocol::page::NavigateParams::new(url))
            .await;
    }
    async fn reload(&self) {
        let _ = self
            .execute(chromiumoxide::cdp::browser_protocol::page::ReloadParams::default())
            .await;
    }
    async fn captcha(&self, wait: u64, attention: &Attention, label: &str) -> bool {
        solve_turnstile(self, wait, attention, label).await
    }
}

async fn mouse(page: &Page, kind: DispatchMouseEventType, x: f64, y: f64, pressed: bool) {
    let mut b = DispatchMouseEventParams::builder().r#type(kind).x(x).y(y);
    if pressed {
        b = b.button(MouseButton::Left).click_count(1);
    }
    if let Ok(p) = b.build() {
        let _ = page.execute(p).await;
    }
}

/// A trusted click with some pointer travel first: Turnstile scores movement,
/// and a JS `.click()` is an untrusted event it ignores.
pub async fn human_click(page: &Page, x: f64, y: f64) {
    let (sx, sy) = (x - 140.0, y - 70.0);
    for i in 1..=14 {
        let t = i as f64 / 14.0;
        let e = 1.0 - (1.0 - t).powi(3);
        mouse(page, DispatchMouseEventType::MouseMoved, sx + (x - sx) * e, sy + (y - sy) * e, false).await;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    tokio::time::sleep(Duration::from_millis(120)).await;
    mouse(page, DispatchMouseEventType::MousePressed, x, y, true).await;
    tokio::time::sleep(Duration::from_millis(95)).await;
    mouse(page, DispatchMouseEventType::MouseReleased, x, y, true).await;
}

// ── Turnstile ───────────────────────────────────────────────────────────────

const WIDGET_BOX_JS: &str = r#"() => {
    const d = document.querySelector('.cf-turnstile, #cf-turnstile, [data-sitekey]')
          || document.querySelector('iframe[src*="challenges.cloudflare"]')?.parentElement;
    if (!d) return null;
    d.scrollIntoView({block: 'center', behavior: 'instant'});
    const r = d.getBoundingClientRect();
    if (r.width < 40 || r.height < 20) return null;
    return {x: r.x, y: r.y, w: r.width, h: r.height};
}"#;

const TOKEN_JS: &str = r#"() => {
    const i = document.querySelector('[name="cf-turnstile-response"]');
    return i && i.value ? i.value.length : 0;
}"#;

const HARD_FAIL_JS: &str = r#"() => {
    const t = (document.body?.innerText || '').toLowerCase();
    return t.includes('verification failed') || t.includes('challenge failed');
}"#;

#[derive(serde::Deserialize)]
pub struct BoxRect {
    pub x: f64,
    pub y: f64,
    #[allow(dead_code)]
    pub w: f64,
    pub h: f64,
}

pub async fn widget_box(page: &Page) -> Option<BoxRect> {
    eval::<Option<BoxRect>>(page, WIDGET_BOX_JS, None).await.flatten()
}

/// Get a Turnstile token: auto-click for 45 s, then leave it to the human for
/// `manual_wait` seconds with the attention banner up. Bails at once on the
/// widget's own "Verification failed": clicking more cannot help.
pub async fn solve_turnstile(page: &Page, manual_wait: u64, attention: &Attention, label: &str) -> bool {
    const AUTO: Duration = Duration::from_secs(45);
    const RECLICK: Duration = Duration::from_secs(13);
    let t0 = Instant::now();
    let budget = AUTO + Duration::from_secs(manual_wait);
    let mut last_click: Option<Instant> = None;
    let mut asked = false;
    let result = loop {
        if t0.elapsed() > budget {
            break false;
        }
        if eval::<u64>(page, TOKEN_JS, None).await.unwrap_or(0) > 0 {
            break true;
        }
        if eval::<bool>(page, HARD_FAIL_JS, None).await.unwrap_or(false) {
            break false;
        }
        if t0.elapsed() < AUTO {
            if last_click.map_or(true, |t| t.elapsed() > RECLICK) {
                last_click = Some(Instant::now());
                if let Some(b) = widget_box(page).await {
                    human_click(page, b.x + 30.0, b.y + b.h / 2.0).await;
                }
            }
        } else if !asked {
            asked = true;
            attention.raise(label);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    if asked {
        attention.clear(label);
    }
    result
}
