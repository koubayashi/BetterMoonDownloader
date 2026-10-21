//! Share link → direct CDN link. One module per host.

pub mod datanodes;
pub mod fuckingfast;

use crate::browser::BrowserGate;
use crate::settings::Settings;
use crate::util::Host;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub url: String,
    /// Cookie header the CDN wants alongside the URL (datanodes browser flow).
    pub cookies: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    /// The file is gone. No retry can help.
    Dead(String),
    /// Worth another attempt.
    Retry(String),
}

/// What a provider flow needs from a browser page. Implemented by the external
/// Chrome/Edge tab (CDP) and by the app's own WebView2 window, so one flow
/// drives either.
pub trait PageOps: Sync {
    /// Call a JS function source with an optional JSON argument; the result as JSON.
    fn eval_value(&self, func: &str, arg: Option<serde_json::Value>)
        -> impl std::future::Future<Output = Option<serde_json::Value>> + Send;
    fn goto(&self, url: &str) -> impl std::future::Future<Output = ()> + Send;
    fn reload(&self) -> impl std::future::Future<Output = ()> + Send;
    /// Get a Turnstile token, involving the user if it doesn't clear on its own.
    fn captcha(&self, wait: u64, attention: &crate::engine::Attention, label: &str)
        -> impl std::future::Future<Output = bool> + Send;
    /// Put the page in front of the user (a Cloudflare check needs a human) or
    /// take it away again. The external browser is always on screen.
    fn show_to_user(&self, _on: bool) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }
}

pub async fn ev<T: serde::de::DeserializeOwned, P: PageOps>(
    page: &P,
    func: &str,
    arg: Option<serde_json::Value>,
) -> Option<T> {
    page.eval_value(func, arg).await.and_then(|v| serde_json::from_value(v).ok())
}

pub struct ExtractCtx {
    /// HTTP client for extraction calls: no redirect following.
    pub http: reqwest::Client,
    pub gate: Arc<BrowserGate>,
    pub settings: Settings,
    /// Set inside the app; enables the built-in browser.
    pub app: Option<tauri::AppHandle>,
}

impl ExtractCtx {
    /// The app handle when pages should open in the built-in browser.
    pub fn builtin(&self) -> Option<&tauri::AppHandle> {
        self.app.as_ref().filter(|_| self.settings.builtin_browser)
    }
}

pub fn extraction_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::util::USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client")
}

pub async fn resolve(ctx: &ExtractCtx, url: &str, label: &str) -> Result<Resolved, ResolveError> {
    match Host::of(url) {
        Host::Fuckingfast => fuckingfast::resolve(ctx, url, label).await,
        Host::Datanodes => datanodes::resolve(ctx, url, label).await,
        Host::Unsupported => Err(ResolveError::Dead(
            "Unsupported host. Only fuckingfast.co and datanodes.to are supported.".into(),
        )),
    }
}
