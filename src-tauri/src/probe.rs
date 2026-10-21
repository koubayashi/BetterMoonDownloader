//! Live check of the built-in browser against the real hosts. Runs only when
//! `BMD_PROBE="<links>"` is set; results go to `probe.log` in the data dir.

use crate::engine::Engine;
use crate::extract::{self, ExtractCtx};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

pub fn run(app: tauri::AppHandle, engine: Arc<Engine>, links: String) {
    tauri::async_runtime::spawn(async move {
        let path = crate::settings::data_dir().join("probe.log");
        let mut log = std::fs::File::create(&path).expect("probe log");
        let mut w = |s: String| {
            let _ = writeln!(log, "{s}");
            let _ = log.flush();
        };
        let ctx = ExtractCtx {
            http: extract::extraction_client(),
            gate: engine.gate.clone(),
            settings: engine.settings(),
            app: Some(app.clone()),
        };
        let client = reqwest::Client::builder().user_agent(crate::util::USER_AGENT).http1_only().build().unwrap();
        for url in links.split_whitespace() {
            w(format!("== {url}"));
            let t = Instant::now();
            // fuckingfast usually resolves over plain HTTPS; call its browser
            // fallback directly so that path gets exercised too.
            let res = match crate::extract::fuckingfast::file_id(url).filter(|_| url.contains("fuckingfast")) {
                Some(id) => crate::webview::resolve_fuckingfast(&app, &ctx, &id, "probe").await,
                None => extract::resolve(&ctx, url, "probe").await,
            };
            match res {
                Err(e) => w(format!("FAILED after {:.1}s: {e:?}", t.elapsed().as_secs_f32())),
                Ok(r) => {
                    w(format!("resolved in {:.1}s -> {}", t.elapsed().as_secs_f32(), r.url));
                    w(format!("cookies: {} bytes", r.cookies.as_deref().map_or(0, str::len)));
                    let mut b = client.get(&r.url).header("Range", "bytes=1000000-1999999")
                        .header("Referer", crate::util::referer_for(&r.url));
                    if let Some(c) = &r.cookies { b = b.header("Cookie", c); }
                    match b.send().await {
                        Ok(resp) => {
                            let st = resp.status().as_u16();
                            let cr = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
                            let n = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
                            w(format!("range check: {st} {cr} ({n} bytes)"));
                        }
                        Err(e) => w(format!("range check failed: {e}")),
                    }
                }
            }
        }
        w("DONE".into());
    });
}
