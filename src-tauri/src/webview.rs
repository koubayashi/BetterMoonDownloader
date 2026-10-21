//! Built-in browser: resolve links in a window of the app's own WebView2
//! (the Edge engine every Windows 10/11 PC has), instead of launching an
//! external Chrome/Edge over CDP.
//!
//! The window stays hidden while the page clears Cloudflare on its own and is
//! shown only when a human has to tick "Verify you are human". It runs on a
//! dedicated persistent profile so `cf_clearance` survives between links.
//! Remote pages get no IPC: the app's capabilities only cover the `main` window.

use crate::engine::Attention;
use crate::extract::{datanodes, ExtractCtx, PageOps, ResolveError, Resolved};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// The app's own globals are a fingerprint no real browser has.
const HIDE_APP_JS: &str = r#"
try { delete window.__TAURI__; } catch (e) {}
try { delete window.__TAURI_INTERNALS__; } catch (e) {}
try { delete window.__TAURI_EVENT_PLUGIN_INTERNALS__; } catch (e) {}
"#;

const TOKEN_JS: &str = r#"() => {
    const i = document.querySelector('[name="cf-turnstile-response"]');
    return i && i.value ? i.value.length : 0;
}"#;

const HARD_FAIL_JS: &str = r#"() => {
    const t = (document.body?.innerText || '').toLowerCase();
    return t.includes('verification failed') || t.includes('challenge failed');
}"#;

const WIDGET_JS: &str = r#"() => !!document.querySelector('.cf-turnstile, #cf-turnstile, [data-sitekey]')"#;

fn profile_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(crate::settings::data_dir)
        .join("BetterMoonDownloader")
        .join("webview-profile")
}

pub struct WvPage {
    win: WebviewWindow,
    shown: Mutex<bool>,
}

impl WvPage {
    pub fn open(app: &AppHandle, take: Arc<dyn Fn(&str) -> bool + Send + Sync>) -> Result<Self, String> {
        let label = format!("resolver-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let (t1, t2, t3) = (take.clone(), take.clone(), take);
        let win = WebviewWindowBuilder::new(app, &label, WebviewUrl::External("about:blank".parse().unwrap()))
            .title("BetterMoonDownloader · verification")
            .inner_size(1100.0, 820.0)
            .center()
            .visible(false)
            .data_directory(profile_dir())
            .initialization_script(HIDE_APP_JS)
            // The file link is all we want; the engine downloads it with ranges.
            .on_navigation(move |u| !t1(u.as_str()))
            .on_download(move |_, ev| match ev {
                DownloadEvent::Requested { url, .. } => {
                    t2(url.as_str());
                    false
                }
                _ => true,
            })
            // Ad popups never open; one that carries the file link is captured.
            .on_new_window(move |u, _| {
                t3(u.as_str());
                NewWindowResponse::Deny
            })
            .build()
            .map_err(|e| format!("couldn't open the built-in browser: {e}"))?;
        Ok(Self { win, shown: Mutex::new(false) })
    }

    pub fn close(&self) {
        let _ = self.win.destroy();
    }

    /// All cookies the page holds for these URLs, as one Cookie header.
    pub async fn cookie_header(&self, urls: &[&str]) -> Option<String> {
        let win = self.win.clone();
        let urls: Vec<url::Url> = urls.iter().filter_map(|u| u.parse().ok()).collect();
        // Reading cookies deadlocks on the WebView2 thread; do it off-thread.
        tokio::task::spawn_blocking(move || {
            let mut pairs: Vec<String> = Vec::new();
            for u in urls {
                for c in win.cookies_for_url(u).unwrap_or_default() {
                    let p = format!("{}={}", c.name(), c.value());
                    if !pairs.contains(&p) {
                        pairs.push(p);
                    }
                }
            }
            (!pairs.is_empty()).then(|| pairs.join("; "))
        })
        .await
        .ok()
        .flatten()
    }
}

impl PageOps for WvPage {
    async fn eval_value(&self, func: &str, arg: Option<Value>) -> Option<Value> {
        let arg = arg.map(|a| a.to_string()).unwrap_or_default();
        let js = format!("(function(){{try{{return ({func})({arg});}}catch(e){{return null;}}}})()");
        for i in 0..4u64 {
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let tx = Mutex::new(Some(tx));
            let sent = self.win.eval_with_callback(js.clone(), move |s| {
                if let Some(t) = tx.lock().unwrap().take() {
                    let _ = t.send(s);
                }
            });
            if sent.is_ok() {
                if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(8), rx).await {
                    return serde_json::from_str::<Value>(&s).ok();
                }
            }
            // A navigation landed mid-call; try again on the new document.
            tokio::time::sleep(Duration::from_millis(350 * (i + 1))).await;
        }
        None
    }

    async fn goto(&self, url: &str) {
        if let Ok(u) = url.parse() {
            let _ = self.win.navigate(u);
        }
    }

    async fn reload(&self) {
        let _ = self.win.reload();
    }

    /// No trusted clicks from inside a WebView, so a widget that doesn't clear
    /// on its own gets shown to the user, inside the app.
    async fn captcha(&self, wait: u64, attention: &Attention, label: &str) -> bool {
        const SELF_CLEAR: Duration = Duration::from_secs(10);
        let t0 = Instant::now();
        let budget = SELF_CLEAR + Duration::from_secs(wait.max(30));
        let mut asked = false;
        let ok = loop {
            if t0.elapsed() > budget {
                break false;
            }
            if crate::extract::ev::<u64, _>(self, TOKEN_JS, None).await.unwrap_or(0) > 0 {
                break true;
            }
            if crate::extract::ev::<bool, _>(self, HARD_FAIL_JS, None).await.unwrap_or(false) {
                break false;
            }
            if !asked && t0.elapsed() > SELF_CLEAR {
                asked = true;
                attention.raise(label);
                self.show_to_user(true).await;
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
        };
        if asked {
            attention.clear(label);
            self.show_to_user(false).await;
        }
        ok
    }

    async fn show_to_user(&self, on: bool) {
        let mut s = self.shown.lock().unwrap();
        if *s == on {
            return;
        }
        *s = on;
        if on {
            let _ = self.win.show();
            let _ = self.win.unminimize();
            let _ = self.win.set_focus();
        } else {
            let _ = self.win.hide();
        }
    }
}

/// fuckingfast after its plain HTTPS `/go` was refused. htmx turns the
/// `hx-redirect` header into a navigation to `/dl/…`, which `on_navigation`
/// catches; the ad tab the first click opens is denied by `on_new_window`.
pub async fn resolve_fuckingfast(
    app: &AppHandle,
    ctx: &ExtractCtx,
    id: &str,
    label: &str,
) -> Result<Resolved, ResolveError> {
    use crate::extract::{ev, fuckingfast as ff};
    let found: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let take = {
        let found = found.clone();
        Arc::new(move |u: &str| -> bool {
            if u.contains("fuckingfast.co/dl/") {
                found.lock().unwrap().get_or_insert(u.to_string());
                return true;
            }
            false
        }) as Arc<dyn Fn(&str) -> bool + Send + Sync>
    };
    let page = WvPage::open(app, take).map_err(ResolveError::Retry)?;
    let got = || found.lock().unwrap().clone();
    let outcome = async {
        page.goto(&format!("https://fuckingfast.co/{id}")).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if ev::<bool, _>(&page, ff::DEAD_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Dead("File not found on fuckingfast".into()));
        }
        // Let the challenge clear on its own; involve the user only if it won't.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(6) {
            if ev::<bool, _>(&page, ff::READY_JS, None).await.unwrap_or(false) {
                break;
            }
            if ev::<bool, _>(&page, WIDGET_JS, None).await.unwrap_or(false) {
                page.captcha(ctx.settings.captcha_wait, &ctx.gate.attention, label).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        for _ in 0..4 {
            if got().is_some() {
                break;
            }
            if !ev::<bool, _>(&page, ff::CLICK_JS, None).await.unwrap_or(false) {
                return Err(ResolveError::Retry("No download button on the fuckingfast page".into()));
            }
            let t = Instant::now();
            while got().is_none() && t.elapsed() < Duration::from_secs(8) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        got().map(|url| Resolved { url, cookies: None })
            .ok_or_else(|| ResolveError::Retry("fuckingfast didn't hand out a link (captcha?)".into()))
    }
    .await;
    page.close();
    outcome
}

/// datanodes through the built-in browser. Same flow as the external one.
pub async fn resolve_datanodes(
    app: &AppHandle,
    ctx: &ExtractCtx,
    url: &str,
    label: &str,
) -> Result<Resolved, ResolveError> {
    let cap = datanodes::capture_for(url)?;
    let page = WvPage::open(app, cap.take.clone()).map_err(ResolveError::Retry)?;
    let outcome = datanodes::flow(ctx, &page, url, label, &cap.captured).await;
    let got = cap.captured.lock().unwrap().clone();
    let result = match (outcome, got) {
        (Err(e), _) => Err(e),
        (Ok(()), None) => Err(ResolveError::Retry("datanodes never handed over the file link".into())),
        (Ok(()), Some(direct)) => {
            let landing = format!("https://{}/", cap.landing_host);
            let cookies = page.cookie_header(&[&landing, &direct]).await;
            Ok(Resolved { url: direct, cookies })
        }
    };
    page.close();
    result
}
