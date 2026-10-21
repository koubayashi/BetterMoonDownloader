//! fuckingfast.co
//!
//! The landing page is Alpine + htmx and the `/dl/` URL is never in the HTML:
//! it only comes back in the `hx-redirect` response header of `POST /f/{id}/go`.
//! Since Aug 2026 that POST sits behind Cloudflare Turnstile, so the plain HTTPS
//! attempt usually 403s and the shared browser finishes the job.

use super::{ExtractCtx, ResolveError, Resolved};
use crate::browser::{self, eval};
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{self, ErrorReason, EventResponseReceived};
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{CloseTargetParams, EventTargetCreated};
use futures::StreamExt;
use regex::Regex;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HOST: &str = "https://fuckingfast.co";

pub fn file_id(url: &str) -> Option<String> {
    let re = Regex::new(r"^[A-Za-z0-9]{6,32}$").unwrap();
    let u = url::Url::parse(url).ok()?;
    let segs: Vec<String> = u.path_segments()?.map(str::to_string).collect();
    segs.into_iter().rev().find(|s| re.is_match(s))
}

pub async fn resolve(ctx: &ExtractCtx, url: &str, label: &str) -> Result<Resolved, ResolveError> {
    let id = file_id(url).ok_or_else(|| ResolveError::Dead("Couldn't read a file id from the link".into()))?;

    match http_go(ctx, &id).await {
        Ok(direct) => return Ok(Resolved { url: direct, cookies: None }),
        Err(HttpOutcome::Dead) => return Err(ResolveError::Dead("File not found on fuckingfast".into())),
        Err(HttpOutcome::NeedsBrowser) => {}
    }
    if let Some(app) = ctx.builtin() {
        return crate::webview::resolve_fuckingfast(app, ctx, &id, label).await;
    }
    browser_go(ctx, &id, label).await
}

enum HttpOutcome {
    Dead,
    NeedsBrowser,
}

async fn http_go(ctx: &ExtractCtx, id: &str) -> Result<String, HttpOutcome> {
    let page = format!("{HOST}/{id}");
    let resp = ctx
        .http
        .post(format!("{HOST}/f/{id}/go"))
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("HX-Request", "true")
        .header("HX-Current-URL", &page)
        .header("Origin", HOST)
        .header("Referer", &page)
        .header("Sec-Fetch-Dest", "empty")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "same-origin")
        .body(Vec::<u8>::new())
        .send()
        .await
        .map_err(|_| HttpOutcome::NeedsBrowser)?;
    let status = resp.status().as_u16();
    if let Some(t) = resp.headers().get("hx-redirect").and_then(|v| v.to_str().ok()) {
        if t.contains("/dl/") {
            return Ok(t.trim().to_string());
        }
    }
    let body = resp.text().await.unwrap_or_default();
    let head = body.chars().take(400).collect::<String>().to_lowercase();
    if status == 404 || head.contains("not found") {
        return Err(HttpOutcome::Dead);
    }
    // Legacy shape, in case they ever revert to putting the link in the body.
    let legacy = Regex::new(r"https://(?:dl\.)?fuckingfast\.co/dl/[A-Za-z0-9_\-]{16,}").unwrap();
    if let Some(m) = legacy.find(&body) {
        return Ok(m.as_str().to_string());
    }
    Err(HttpOutcome::NeedsBrowser)
}

/// htmx owns the submit, so the `hx-post` element is what gets clicked.
pub(crate) const CLICK_JS: &str = r#"() => {
    const el = document.querySelector('[hx-post*="/go"]');
    if (!el) return false;
    el.click();
    return true;
}"#;

pub(crate) const READY_JS: &str = r#"() => !!(window.turnstileToken || window.dlCleared)"#;

pub(crate) const DEAD_JS: &str = r#"() => {
    const t = (document.body?.innerText || '').toLowerCase();
    return t.includes('file not found') || t.includes('has been removed') || t.includes('no longer available');
}"#;

async fn browser_go(ctx: &ExtractCtx, id: &str, label: &str) -> Result<Resolved, ResolveError> {
    let s = &ctx.settings;
    let lane = ctx
        .gate
        .acquire(s.browser_pages, &s.browser_path)
        .await
        .map_err(ResolveError::Retry)?;
    let page = browser::new_page(&lane.browser).await.map_err(ResolveError::Retry)?;
    let found: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut tasks = Vec::new();

    // The URL is what we want, not the bytes: the engine downloads it with
    // range support. Following the redirect here would start a second copy.
    let _ = page.execute(network::EnableParams::default()).await;
    let _ = page
        .execute(
            fetch::EnableParams::builder()
                .pattern(RequestPattern::builder().url_pattern("*/dl/*").build())
                .build(),
        )
        .await;

    if let Ok(mut ev) = page.event_listener::<EventResponseReceived>().await {
        let f = found.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = ev.next().await {
                if !e.response.url.contains("/go") {
                    continue;
                }
                if let Some(obj) = e.response.headers.inner().as_object() {
                    for (k, v) in obj {
                        if k.eq_ignore_ascii_case("hx-redirect") {
                            if let Some(t) = v.as_str().filter(|t| t.contains("/dl/")) {
                                f.lock().unwrap().get_or_insert(t.trim().to_string());
                            }
                        }
                    }
                }
            }
        }));
    }
    if let Ok(mut ev) = page.event_listener::<EventRequestPaused>().await {
        let f = found.clone();
        let p = page.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = ev.next().await {
                if e.request.url.contains("/dl/") {
                    f.lock().unwrap().get_or_insert(e.request.url.clone());
                }
                let _ = p
                    .execute(FailRequestParams::new(e.request_id.clone(), ErrorReason::BlockedByClient))
                    .await;
            }
        }));
    }
    // The first click opens an ad tab instead of submitting. Close it, as a
    // person would, and press again.
    if let Ok(mut ev) = lane.browser.event_listener::<EventTargetCreated>().await {
        let me = page.target_id().clone();
        let b = lane.browser.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = ev.next().await {
                if e.target_info.opener_id.as_ref() == Some(&me) {
                    let _ = b.execute(CloseTargetParams::new(e.target_info.target_id.clone())).await;
                }
            }
        }));
    }

    let outcome = async {
        let _ = page.execute(NavigateParams::new(format!("{HOST}/{id}"))).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if eval::<bool>(&page, DEAD_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Dead("File not found on fuckingfast".into()));
        }

        // Give the challenge a chance to clear on its own; only touch the
        // widget if it is actually sitting there interactive.
        let t_ready = Instant::now();
        while t_ready.elapsed() < Duration::from_secs(6) {
            if eval::<bool>(&page, READY_JS, None).await.unwrap_or(false) {
                break;
            }
            if browser::widget_box(&page).await.is_some() {
                browser::solve_turnstile(&page, s.captcha_wait, &ctx.gate.attention, label).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }

        let deadline = Instant::now() + Duration::from_secs(75);
        for _ in 0..4 {
            if found.lock().unwrap().is_some() || Instant::now() > deadline {
                break;
            }
            if !eval::<bool>(&page, CLICK_JS, None).await.unwrap_or(false) {
                return Err(ResolveError::Retry("No download button on the fuckingfast page".into()));
            }
            let t = Instant::now();
            while found.lock().unwrap().is_none() && t.elapsed() < Duration::from_secs(8) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        let got = found.lock().unwrap().clone();
        got.map(|url| Resolved { url, cookies: None })
            .ok_or_else(|| ResolveError::Retry("fuckingfast didn't hand out a link (captcha?)".into()))
    }
    .await;

    for t in tasks {
        t.abort();
    }
    let _ = page.close().await;
    drop(lane);
    outcome
}

#[cfg(test)]
mod tests {
    #[test]
    fn id_ignores_fragment() {
        assert_eq!(
            super::file_id("https://fuckingfast.co/smeekt12mped#Game.part01.rar").as_deref(),
            Some("smeekt12mped")
        );
    }
}
