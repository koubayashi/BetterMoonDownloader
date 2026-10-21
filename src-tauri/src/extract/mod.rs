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

pub struct ExtractCtx {
    /// HTTP client for extraction calls: no redirect following.
    pub http: reqwest::Client,
    pub gate: Arc<BrowserGate>,
    pub settings: Settings,
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
