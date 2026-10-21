//! datanodes.to
//!
//! Premium API key → one JSON GET. Otherwise the two-step browser flow, ported
//! from moondownloader's `moon_extract.py`, where each of these rules was learned
//! the hard way:
//! - The server may serve step 1 (form inside a collapsed `#downloadReveal`) or,
//!   since Aug 2026, step 2 directly. The page decides; we detect both shapes.
//! - Exactly ONE `POST /download`. A second re-runs SecSave and invalidates the
//!   token step 2 holds. `method_free` must be in the body.
//! - Step 2 is Turnstile + countdown + a *chain* of buttons, each revealing the next.
//! - Stylesheets and ad hosts must load (button finder measures layout; the
//!   page runs adblock detection). Only images, media and fonts are dropped.

use super::{ExtractCtx, ResolveError, Resolved};
use crate::browser::{self, eval};
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, ContinueRequestParams, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{self, ErrorReason, GetCookiesParams, ResourceType};
use chromiumoxide::cdp::browser_protocol::page::{NavigateParams, ReloadParams};
use chromiumoxide::cdp::browser_protocol::target::{
    CloseTargetParams, EventTargetCreated, EventTargetInfoChanged,
};
use futures::StreamExt;
use percent_encoding::percent_decode_str;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const API_ENDPOINT: &str = "https://datanodes.to/api/file/direct_link";
const STEP2_TIMEOUT: Duration = Duration::from_secs(420);
const STEP2_MAX_CLICKS: usize = 4;

const ALWAYS_ALLOW: &[&str] = &["challenges.cloudflare.com", "cdn-cgi/challenge-platform", "datanodes.to"];
const BLOCKED_DOMS: &[&str] = &[
    "google-analytics.com",
    "analytics.google.com",
    "stats.g.doubleclick.net",
    "hotjar",
    "clarity.ms",
    "facebook.com/tr",
];

fn file_ext_re() -> Regex {
    Regex::new(r"(?i)\.(?:r(?:ar|\d{2})|zip|7z|tar|gz|bin|iso|exe|mkv|mp4|part\d+)(?:$|[?#])").unwrap()
}

pub fn file_code(url: &str) -> Option<String> {
    let re = Regex::new(r"^[A-Za-z0-9]{8,20}$").unwrap();
    url::Url::parse(url)
        .ok()?
        .path_segments()?
        .find(|p| re.is_match(p))
        .map(str::to_string)
}

/// True when a request looks like the final file handoff rather than page
/// chrome. The share URL ends in the filename too, so anything on the landing
/// host needs an explicit handoff marker, or the first navigation matches itself.
pub fn is_file_url(candidate: &str, landing_host: &str, want_name: Option<&str>, self_urls: &HashSet<String>) -> bool {
    if candidate.len() < 40 || self_urls.contains(candidate) {
        return false;
    }
    let Ok(p) = url::Url::parse(candidate) else { return false };
    let low = candidate.to_lowercase();
    let host = p.host_str().unwrap_or("").to_lowercase();
    let path = percent_decode_str(p.path()).decode_utf8_lossy().to_string();
    let trimmed = path.trim_end_matches('/');
    if path.is_empty() || path == "/" || ["/download", "/premium", "/login"].contains(&trimmed) {
        return false;
    }
    let base = landing_host.strip_prefix("www.").unwrap_or(landing_host);
    if host == landing_host || host == base || host == format!("www.{base}") {
        return low.contains("dlproxy") || path.contains("/dl/") || path.starts_with("/d/");
    }
    let ext = file_ext_re();
    let node = Regex::new(r"^(?:s\d+|dl\d*|cdn\d*|node\d*|fs\d+|files?)\.").unwrap();
    if node.is_match(&host) && (ext.is_match(&path) || path.contains("/d/") || path.contains("/dl/")) {
        return true;
    }
    if low.contains("dlproxy") || ext.is_match(&path) {
        return true;
    }
    matches!(want_name, Some(n) if low.contains(&n.to_lowercase()))
}

pub async fn resolve(ctx: &ExtractCtx, url: &str, label: &str) -> Result<Resolved, ResolveError> {
    if !ctx.settings.dn_api_key.is_empty() {
        if let Some(direct) = via_api(ctx, url).await {
            return Ok(Resolved { url: direct, cookies: None });
        }
    }
    via_browser(ctx, url, label).await
}

/// `GET /api/file/direct_link?file_code=..&key=..`. Answers HTTP 200 even on
/// failure, so the JSON `status` field is the real outcome. Free keys get 403.
async fn via_api(ctx: &ExtractCtx, url: &str) -> Option<String> {
    let code = file_code(url)?;
    let v: serde_json::Value = ctx
        .http
        .get(API_ENDPOINT)
        .query(&[("file_code", code.as_str()), ("key", ctx.settings.dn_api_key.as_str())])
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    if v.get("status").and_then(|s| s.as_i64()) != Some(200) {
        log::info!("datanodes API refused {code}: {}", v.get("msg").unwrap_or(&serde_json::Value::Null));
        return None;
    }
    v.pointer("/result/url").and_then(|u| u.as_str()).map(str::to_string)
}

const DEAD_JS: &str = r#"() => {
    const t = (document.body?.innerText || '').toLowerCase();
    return t.includes('file not found') || t.includes('could not be found')
        || t.includes('file was deleted') || t.includes('has been removed')
        || t.includes('file expired') || t.includes('no such file');
}"#;

const SHAPE_JS: &str = r#"() => {
    if (document.getElementById('downloadReveal') &&
        document.getElementById('method_free')) return 'step1';
    const txt = document.body ? (document.body.innerText || '') : '';
    if (/step\s*2\s*of\s*2/i.test(txt)) return 'step2';
    for (const e of document.querySelectorAll('button, a, [role=button]')) {
        if (/free\s*download/i.test(e.innerText || '')) return 'step2';
    }
    return 'unknown';
}"#;

const GATE_JS: &str = r#"(force) => {
    const w = document.getElementById('downloadReveal');
    const b = document.getElementById('method_free');
    if (!w || !b) return {present: false, ready: false};
    const cs = getComputedStyle(w);
    let ready = !b.disabled && cs.pointerEvents !== 'none' && cs.opacity !== '0';
    if (!ready && force) {
        w.style.maxHeight = 'none'; w.style.opacity = '1';
        w.style.transform = 'none'; w.style.pointerEvents = 'auto';
        b.disabled = false;
        ready = true;
    }
    return {present: true, ready: ready};
}"#;

const LATCH_JS: &str = r#"() => {
    if (window.__bmdLatched) return false;
    window.__bmdLatched = true;
    const f = document.getElementById('downloadForm');
    if (f) {
        const native = f.submit.bind(f);
        let used = false;
        f.submit = function () { if (used) return; used = true; return native(); };
    }
    return true;
}"#;

const SUBMIT_JS: &str = r#"() => {
    const f = document.getElementById('downloadForm');
    if (!f) return false;
    if (!f.querySelector('input[type=hidden][name=method_free]')) {
        const i = document.createElement('input');
        i.type = 'hidden'; i.name = 'method_free'; i.value = 'Free Download >>';
        f.appendChild(i);
    }
    f.submit();
    return true;
}"#;

const STEP2_JS: &str = r#"() => {
    const t = (document.body?.innerText || '').toLowerCase();
    const tok = document.querySelector('[name="cf-turnstile-response"]');
    const captcha = document.querySelectorAll('.cf-turnstile').length > 0
                 || document.querySelectorAll('iframe[src*="challenges.cloudflare"]').length > 0;
    let trigger = null;
    for (const el of document.querySelectorAll('button, a, input[type=submit]')) {
        const label = (el.innerText || el.value || '').trim().toLowerCase();
        if (!label || label.length > 60 || el.disabled) continue;
        if (/start download|download now|get link|proceed to download|download file|free download/.test(label)) {
            const r = el.getBoundingClientRect();
            if (r.width * r.height > 0) { trigger = label; break; }
        }
    }
    return {
        captcha: captcha,
        solved: !!(tok && tok.value),
        hardFail: t.includes('verification failed') || t.includes('challenge failed'),
        adblock: t.includes('adblock') || t.includes('ad blocker'),
        trigger: trigger,
    };
}"#;

const CLICK_JS: &str = r#"(label) => {
    for (const el of document.querySelectorAll('button, a, input[type=submit]')) {
        const l = (el.innerText || el.value || '').trim().toLowerCase();
        if (l === label) { el.click(); return true; }
    }
    return false;
}"#;

/// Ad overlays hijack z-index on bare body-level divs; the real UI lives in #app.
const OVERLAY_JS: &str = r#"() => {
    for (const el of document.querySelectorAll('body > div')) {
        if (el.id || el.className) continue;
        const s = el.getAttribute('style') || '';
        if (s.includes('z-index') || s.includes('position: fixed')) el.remove();
    }
    for (const el of document.querySelectorAll('body > iframe, body > ins')) el.remove();
}"#;

#[derive(Deserialize, Default)]
struct Gate {
    ready: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step2 {
    captcha: bool,
    solved: bool,
    hard_fail: bool,
    adblock: bool,
    trigger: Option<String>,
}

async fn via_browser(ctx: &ExtractCtx, url: &str, label: &str) -> Result<Resolved, ResolveError> {
    let s = &ctx.settings;
    let lane = ctx
        .gate
        .acquire(s.browser_pages, &s.browser_path)
        .await
        .map_err(ResolveError::Retry)?;
    let page = browser::new_page(&lane.browser).await.map_err(ResolveError::Retry)?;

    let parsed = url::Url::parse(url).map_err(|e| ResolveError::Dead(e.to_string()))?;
    let landing_host = parsed.host_str().unwrap_or("datanodes.to").to_lowercase();
    let want_name = {
        let last = percent_decode_str(parsed.path().rsplit('/').next().unwrap_or(""))
            .decode_utf8_lossy()
            .to_string();
        (!last.is_empty() && file_ext_re().is_match(&last)).then_some(last)
    };
    let self_urls: HashSet<String> = [
        url.to_string(),
        url.split('#').next().unwrap_or(url).to_string(),
        format!("https://{landing_host}/download"),
        format!("https://{landing_host}/download/"),
    ]
    .into_iter()
    .collect();

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let take = {
        let captured = captured.clone();
        let lh = landing_host.clone();
        let wn = want_name.clone();
        let su = self_urls.clone();
        Arc::new(move |cand: &str| -> bool {
            let mut c = captured.lock().unwrap();
            if c.is_some() || cand.is_empty() {
                return false;
            }
            if is_file_url(cand, &lh, wn.as_deref(), &su) {
                *c = Some(cand.to_string());
                return true;
            }
            false
        })
    };

    let mut tasks = Vec::new();
    let _ = page.execute(network::EnableParams::default()).await;
    let _ = page
        .execute(
            fetch::EnableParams::builder()
                .pattern(RequestPattern::builder().url_pattern("*").build())
                .build(),
        )
        .await;
    if let Ok(mut ev) = page.event_listener::<EventRequestPaused>().await {
        let p = page.clone();
        let take = take.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = ev.next().await {
                let u = e.request.url.as_str();
                let id = e.request_id.clone();
                let block = if take(u) {
                    true // the URL is all we need; the engine transfers it
                } else if ALWAYS_ALLOW.iter().any(|a| u.contains(a)) {
                    false
                } else {
                    matches!(e.resource_type, ResourceType::Image | ResourceType::Media | ResourceType::Font)
                        || BLOCKED_DOMS.iter().any(|d| u.contains(d))
                };
                let _ = if block {
                    p.execute(FailRequestParams::new(id, ErrorReason::BlockedByClient)).await.map(|_| ())
                } else {
                    p.execute(ContinueRequestParams::new(id)).await.map(|_| ())
                };
            }
        }));
    }
    // Popups may carry the file URL; ad popups get closed.
    let me = page.target_id().clone();
    for kind in 0..2 {
        let b = lane.browser.clone();
        let me = me.clone();
        let take = take.clone();
        let handle = async move |info: chromiumoxide::cdp::browser_protocol::target::TargetInfo| {
            if info.opener_id.as_ref() == Some(&me) {
                take(&info.url);
                if info.url != "about:blank" && !info.url.is_empty() {
                    let _ = b.execute(CloseTargetParams::new(info.target_id.clone())).await;
                }
            }
        };
        if kind == 0 {
            if let Ok(mut ev) = lane.browser.event_listener::<EventTargetCreated>().await {
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        handle(e.target_info.clone()).await;
                    }
                }));
            }
        } else if let Ok(mut ev) = lane.browser.event_listener::<EventTargetInfoChanged>().await {
            tasks.push(tokio::spawn(async move {
                while let Some(e) = ev.next().await {
                    handle(e.target_info.clone()).await;
                }
            }));
        }
    }

    let outcome = flow(ctx, &page, url, label, &captured).await;

    let result = match outcome {
        Ok(()) => {
            let got = captured.lock().unwrap().clone();
            match got {
                Some(direct) => {
                    let cookies = page
                        .execute(GetCookiesParams {
                            urls: Some(vec![format!("https://{landing_host}/"), direct.clone()]),
                        })
                        .await
                        .ok()
                        .map(|r| {
                            r.result
                                .cookies
                                .iter()
                                .map(|c| format!("{}={}", c.name, c.value))
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .filter(|c| !c.is_empty());
                    Ok(Resolved { url: direct, cookies })
                }
                None => Err(ResolveError::Retry("datanodes never handed over the file link".into())),
            }
        }
        Err(e) => Err(e),
    };
    for t in tasks {
        t.abort();
    }
    let _ = page.close().await;
    drop(lane);
    result
}

async fn flow(
    ctx: &ExtractCtx,
    page: &chromiumoxide::Page,
    url: &str,
    label: &str,
    captured: &Arc<Mutex<Option<String>>>,
) -> Result<(), ResolveError> {
    let got = || captured.lock().unwrap().is_some();
    let _ = page.execute(NavigateParams::new(url)).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    if eval::<bool>(page, DEAD_JS, None).await.unwrap_or(false) {
        return Err(ResolveError::Dead("File not found on datanodes".into()));
    }

    // Which step did the server send? The controls only appear once Cloudflare
    // has been answered, so the wait covers a human's reaction time too.
    let t_gate = Instant::now();
    let deadline = Duration::from_secs(22.max(ctx.settings.captcha_wait));
    let mut shape = String::from("unknown");
    let mut gate = Gate::default();
    let mut asked = false;
    while t_gate.elapsed() < deadline && !got() {
        shape = eval::<String>(page, SHAPE_JS, None).await.unwrap_or_default();
        if shape == "step2" {
            break;
        }
        if shape == "step1" {
            gate = eval::<Gate>(page, GATE_JS, Some(false.into())).await.unwrap_or_default();
            if gate.ready {
                break;
            }
        } else if eval::<bool>(page, DEAD_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Dead("File not found on datanodes".into()));
        } else if !asked && t_gate.elapsed() > Duration::from_secs(15) {
            asked = true;
            ctx.gate.attention.raise(label);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if asked {
        ctx.gate.attention.clear(label);
    }

    if shape == "step2" {
        let _ = eval::<serde_json::Value>(page, OVERLAY_JS, None).await;
    } else if !got() {
        if !gate.ready {
            // The site's own escape hatch for its broken-bundle case.
            gate = eval::<Gate>(page, GATE_JS, Some(true.into())).await.unwrap_or_default();
            if !gate.ready {
                return Err(ResolveError::Retry(
                    "datanodes page never showed its download controls (Cloudflare check pending?)".into(),
                ));
            }
        }
        let _ = eval::<serde_json::Value>(page, OVERLAY_JS, None).await;
        if !eval::<bool>(page, LATCH_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Retry("step 1 already submitted".into()));
        }
        if !eval::<bool>(page, SUBMIT_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Retry("step 1 form missing".into()));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        if eval::<bool>(page, DEAD_JS, None).await.unwrap_or(false) {
            return Err(ResolveError::Dead("File not found on datanodes".into()));
        }
    }

    // Step 2: Turnstile + countdown, then the trigger chain.
    for retry in 0..2 {
        let mut clicked: HashSet<String> = HashSet::new();
        let mut hard_failed = false;
        let mut last_sweep = Instant::now() - Duration::from_secs(5);
        let t2 = Instant::now();
        while t2.elapsed() < STEP2_TIMEOUT && !got() {
            if last_sweep.elapsed() > Duration::from_secs(1) {
                let _ = eval::<serde_json::Value>(page, OVERLAY_JS, None).await;
                last_sweep = Instant::now();
            }
            let Some(st) = eval::<Step2>(page, STEP2_JS, None).await else {
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            };
            if st.adblock {
                return Err(ResolveError::Retry("datanodes thinks an ad blocker is active".into()));
            }
            if st.hard_fail {
                hard_failed = true;
                break;
            }
            if st.captcha && !st.solved {
                if !browser::solve_turnstile(page, ctx.settings.captcha_wait, &ctx.gate.attention, label).await {
                    return Err(ResolveError::Retry("Captcha wasn't solved in time".into()));
                }
                continue;
            }
            if let Some(trig) = st.trigger.filter(|t| !clicked.contains(t)) {
                if clicked.len() >= STEP2_MAX_CLICKS {
                    break;
                }
                let _ = eval::<serde_json::Value>(page, OVERLAY_JS, None).await;
                if eval::<bool>(page, CLICK_JS, Some(trig.clone().into())).await.unwrap_or(false) {
                    clicked.insert(trig);
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        if !hard_failed {
            break;
        }
        if retry == 0 {
            // A fresh widget instance sometimes clears a transient hard-fail.
            let _ = page.execute(ReloadParams::default()).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            return Err(ResolveError::Retry("Turnstile said 'Verification failed'".into()));
        }
    }

    let t = Instant::now();
    while !got() && t.elapsed() < Duration::from_secs(12) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_detection() {
        let su: HashSet<String> = ["https://datanodes.to/abcd1234efgh/Game.part01.rar".to_string()].into();
        let want = Some("Game.part01.rar");
        assert!(!is_file_url("https://datanodes.to/abcd1234efgh/Game.part01.rar", "datanodes.to", want, &su));
        assert!(!is_file_url("https://datanodes.to/download", "datanodes.to", want, &su));
        assert!(is_file_url(
            "https://s12.datanodes.to/d/xyzxyzxyzxyzxyz/Game.part01.rar",
            "datanodes.to",
            want,
            &su
        ));
        assert_eq!(file_code("https://datanodes.to/abcd1234efgh/x.rar").as_deref(), Some("abcd1234efgh"));
    }
}
