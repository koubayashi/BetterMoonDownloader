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
        if links == "ipc" {
            // Play a hostile page: can a remote document in a resolver window
            // reach the app's commands?
            let page = crate::webview::WvPage::open(&app, std::sync::Arc::new(|_: &str| false)).unwrap();
            use crate::extract::PageOps;
            page.goto("https://example.com/").await;
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let start = r#"() => {
                window.__r = {internals: typeof window.__TAURI_INTERNALS__, tauri: typeof window.__TAURI__,
                              ipcGlobals: Object.keys(window).filter(k => /tauri|ipc/i.test(k))};
                const tries = [
                  ['ipc.localhost', 'http://ipc.localhost/read_text_file'],
                  ['ipc.localhost-plugin', 'http://ipc.localhost/plugin%3Aopener%7Copen_path'],
                ];
                for (const [name, url] of tries) {
                  fetch(url, {method: 'POST', headers: {'Content-Type': 'application/json', 'Tauri-Callback': '1', 'Tauri-Error': '2'},
                              body: JSON.stringify({path: 'C:\\Windows\\win.ini'})})
                    .then(async r => window.__r[name] = r.status + ' ' + (await r.text()).slice(0, 120))
                    .catch(e => window.__r[name] = 'blocked: ' + e.message);
                }
                const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
                const calls = [
                  ['read_text_file', {path: 'C:\\Windows\\win.ini'}],
                  ['write_text_file', {path: 'C:\\Users\\Public\\bmd-pwned.txt', text: 'x'}],
                  ['open_folder', {path: 'C:\\Windows\\System32\\calc.exe'}],
                  ['get_settings', {}],
                  ['plugin:opener|open_url', {url: 'https://example.com'}],
                  ['plugin:dialog|open', {options: {}}],
                ];
                for (const [cmd, args] of calls) {
                  if (!inv) { window.__r['inv:' + cmd] = 'no invoke'; continue; }
                  try {
                    Promise.resolve(inv(cmd, args))
                      .then(v => window.__r['inv:' + cmd] = 'ALLOWED ' + JSON.stringify(v).slice(0, 80))
                      .catch(e => window.__r['inv:' + cmd] = 'denied: ' + String(e).slice(0, 80));
                  } catch (e) { window.__r['inv:' + cmd] = 'threw: ' + String(e).slice(0, 80); }
                }
                return 'started';
            }"#;
            let _ = page.eval_value(start, None).await;
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let r = page.eval_value("() => window.__r", None).await;
            w(format!("hostile page result: {}", r.map(|v| v.to_string()).unwrap_or("none".into())));
            page.close();
            w("DONE".into());
            return;
        }
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
