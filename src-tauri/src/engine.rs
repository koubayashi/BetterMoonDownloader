//! The queue: scheduling, retries, stall killer, persistence, and the snapshot
//! the UI pulls several times a second.

use crate::browser::BrowserGate;
use crate::download::{self, DlError, DlRequest, Progress, RateLimiter};
use crate::extract::{self, ExtractCtx, ResolveError};
use crate::settings::{self, Settings};
use crate::util::{self, Host};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const STALL_MIN_BPS: f64 = 0.5 * 1_000_000.0;
const STALL_GRACE: Duration = Duration::from_secs(90);
const STALL_WINDOW: Duration = Duration::from_secs(60);
const STALL_MIN_FILE: u64 = 50 * 1024 * 1024;
const STALL_MAX_KILLS: u32 = 1;
const LINK_REFRESHES: u32 = 3;
const LOG_CAP: usize = 2000;
const HISTORY_CAP: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Resolving,
    Ready,
    Downloading,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub host: Host,
    pub state: State,
    pub total: u64,
    pub done: u64,
    pub error: Option<String>,
    pub note: Option<String>,
    pub direct: Option<String>,
    /// In memory only: session cookies never touch the disk.
    #[serde(skip)]
    pub cookies: Option<String>,
    pub out_dir: String,
    pub added_at: i64,
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub link_refreshes: u32,
    #[serde(default)]
    pub stall_kills: u32,
    /// The release this file belongs to (`ExampleGame`). Jobs sharing it form a group.
    #[serde(default)]
    pub group: String,
    /// Folder under `out_dir` the file lands in; empty = `out_dir` itself.
    #[serde(default)]
    pub subfolder: String,
    /// Every link known for this file: the same part on other hosts. `url` is
    /// the one in use; the next is tried when it fails for good.
    #[serde(default)]
    pub sources: Vec<String>,
}

impl JobRecord {
    pub fn folder(&self) -> PathBuf {
        let base = PathBuf::from(&self.out_dir);
        if self.subfolder.is_empty() { base } else { base.join(&self.subfolder) }
    }
    pub fn dest(&self) -> PathBuf {
        self.folder().join(&self.name)
    }

    /// Move to the next mirror, if one is left. Resets per-source counters.
    fn next_mirror(&mut self) -> bool {
        let i = self.sources.iter().position(|u| *u == self.url).unwrap_or(0);
        let Some(next) = self.sources.get(i + 1).cloned() else { return false };
        self.url = next;
        self.host = Host::of(&self.url);
        self.direct = None;
        self.cookies = None;
        self.attempts = 0;
        self.link_refreshes = 0;
        self.state = State::Queued;
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    Pause,
    Remove,
    Stall,
}

struct Job {
    rec: JobRecord,
    progress: Arc<Progress>,
    cancel: Option<CancellationToken>,
    reason: Option<Reason>,
    samples: VecDeque<(Instant, u64)>,
    speed: f64,
    dl_started: Option<Instant>,
    slow_since: Option<Instant>,
    retry_after: Option<Instant>,
    removed: bool,
    /// Delete the partial file once the task has stopped.
    purge: bool,
}

impl Job {
    fn new(mut rec: JobRecord) -> Self {
        if rec.group.is_empty() {
            rec.group = util::release_name(&rec.name);
        }
        if rec.sources.is_empty() {
            rec.sources.push(rec.url.clone());
        }
        let progress = Arc::new(Progress::default());
        progress.done.store(rec.done, Ordering::Relaxed);
        progress.total.store(rec.total, Ordering::Relaxed);
        Self {
            rec,
            progress,
            cancel: None,
            reason: None,
            samples: VecDeque::new(),
            speed: 0.0,
            dl_started: None,
            slow_since: None,
            retry_after: None,
            removed: false,
            purge: false,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct LogLine {
    pub seq: u64,
    pub ts: i64,
    pub level: &'static str,
    pub msg: String,
}

/// Links waiting on a human to tick a captcha in the browser window.
#[derive(Default)]
pub struct Attention {
    labels: Mutex<Vec<String>>,
}

impl Attention {
    pub fn raise(&self, label: &str) {
        let mut l = self.labels.lock().unwrap();
        if !l.iter().any(|x| x == label) {
            l.push(label.to_string());
        }
    }
    pub fn clear(&self, label: &str) {
        self.labels.lock().unwrap().retain(|x| x != label);
    }
    pub fn list(&self) -> Vec<String> {
        self.labels.lock().unwrap().clone()
    }
}

struct Inner {
    jobs: Vec<Job>,
    next_id: u64,
    dirty: bool,
    disk_full: Option<String>,
    notice: Option<String>,
    history: VecDeque<f64>,
    log: VecDeque<LogLine>,
    log_seq: u64,
    clients: Vec<reqwest::Client>,
    rr: usize,
}

pub struct Engine {
    inner: Mutex<Inner>,
    settings: RwLock<Settings>,
    pub gate: Arc<BrowserGate>,
    pub attention: Arc<Attention>,
    limiter: Arc<RateLimiter>,
    extract_http: reqwest::Client,
    wake: Notify,
    app: std::sync::OnceLock<tauri::AppHandle>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    id: u64,
    name: String,
    url: String,
    host: Host,
    state: State,
    total: u64,
    done: u64,
    speed: f64,
    eta: Option<f64>,
    conns: usize,
    error: Option<String>,
    note: Option<String>,
    has_direct: bool,
    added_at: i64,
    finished_at: Option<i64>,
    retry_in: Option<f64>,
    group: String,
    folder: String,
    mirrors: usize,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    count: usize,
    done: usize,
    failed: usize,
    active: usize,
    queued: usize,
    paused: usize,
    bytes_done: u64,
    bytes_total: u64,
    unknown_sizes: usize,
    speed: f64,
    eta: Option<f64>,
    eta_estimated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    jobs: Vec<JobView>,
    stats: Stats,
    history: Vec<f64>,
    attention: Vec<String>,
    disk_full: Option<String>,
    notice: Option<String>,
    browser_open: bool,
    log: Vec<LogLine>,
    log_seq: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddResult {
    added: usize,
    duplicates: usize,
    unsupported: usize,
    mirrors: usize,
    groups: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResult {
    trashed: usize,
    failed: usize,
}

/// What a pasted text would turn into, before anything is queued.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanGroup {
    pub name: String,
    pub files: usize,
    pub links: usize,
    pub fuckingfast: usize,
    pub datanodes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub groups: Vec<PlanGroup>,
    pub unsupported: usize,
    pub total: usize,
}

pub fn plan_links(text: &str) -> Plan {
    let links = util::parse_links(text);
    let mut groups: Vec<(PlanGroup, Vec<String>)> = Vec::new();
    let supported: Vec<&String> = links.iter().filter(|u| Host::of(u) != Host::Unsupported).collect();
    let unsupported = links.len() - supported.len();
    let names: Vec<String> = supported.iter().map(|u| util::filename_for(u)).collect();
    let releases = util::batch_release_names(&names);
    for ((url, name), group) in supported.into_iter().zip(names).zip(releases) {
        let host = Host::of(url);
        let idx = match groups.iter().position(|(g, _)| g.name == group) {
            Some(i) => i,
            None => {
                groups.push((PlanGroup { name: group, files: 0, links: 0, fuckingfast: 0, datanodes: 0 }, vec![]));
                groups.len() - 1
            }
        };
        let (g, names) = &mut groups[idx];
        g.links += 1;
        match host {
            Host::Fuckingfast => g.fuckingfast += 1,
            _ => g.datanodes += 1,
        }
        if !names.contains(&name) {
            names.push(name);
            g.files += 1;
        }
    }
    Plan { groups: groups.into_iter().map(|(g, _)| g).collect(), unsupported, total: links.len() }
}

fn build_clients(proxies: &str) -> Vec<reqwest::Client> {
    let base = || {
        reqwest::Client::builder()
            .user_agent(util::USER_AGENT)
            // HTTP/2 would multiplex every segment onto one TCP connection and
            // defeat the point of segmenting.
            .http1_only()
            .redirect(download::redirect_policy())
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
    };
    let mut out = Vec::new();
    for line in proxies.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if let Some(p) = parse_proxy(line) {
            if let Ok(c) = base().proxy(p).build() {
                out.push(c);
            }
        }
    }
    if out.is_empty() {
        out.push(base().build().expect("http client"));
    }
    out
}

/// `http://h:p`, `socks5://h:p`, `ip:port`, `ip:port:user:pass`, `user:pass:ip:port`.
fn parse_proxy(line: &str) -> Option<reqwest::Proxy> {
    if line.contains("://") {
        return reqwest::Proxy::all(line).ok();
    }
    let parts: Vec<&str> = line.split(':').collect();
    match parts.as_slice() {
        [ip, port] => reqwest::Proxy::all(format!("http://{ip}:{port}")).ok(),
        [a, b, c, d] => {
            let is_ip = a.split('.').count() == 4 && a.split('.').all(|o| o.parse::<u8>().is_ok());
            let (ip, port, user, pass) = if is_ip { (a, b, c, d) } else { (c, d, a, b) };
            reqwest::Proxy::all(format!("http://{ip}:{port}")).ok().map(|p| p.basic_auth(user, pass))
        }
        _ => None,
    }
}

fn host_label(h: Host) -> &'static str {
    match h {
        Host::Fuckingfast => "fuckingfast",
        Host::Datanodes => "datanodes",
        Host::Unsupported => "other",
    }
}

fn queue_path() -> PathBuf {
    settings::data_dir().join("queue.json")
}

/// Load the queue without ever losing it silently. A file that won't parse
/// is kept as `queue.corrupt-<time>.json`, and the last good copy
/// (`queue.json.bak`, written at every start) is used instead.
/// Lists saved before companion files were grouped with their release have
/// each `fg-selective-*.bin` as a release of its own. Move the ones with
/// nothing on disk yet into the release added just before them; files with
/// data stay where they are. `recs` must be in id order. True if anything moved.
fn adopt_companions(recs: &mut [JobRecord]) -> bool {
    let mut moved = false;
    for i in 0..recs.len() {
        let r = &recs[i];
        let alone = util::is_companion(&r.name) && r.group == util::release_name(&r.name);
        if !alone || r.done > 0 || r.state == State::Done {
            continue;
        }
        let Some(host) = recs[..i].iter().rev().find(|p| p.out_dir == r.out_dir && !util::is_companion(&p.name)) else {
            continue;
        };
        let (group, subfolder) = (host.group.clone(), host.subfolder.clone());
        let taken = recs.iter().any(|o| o.name == r.name && o.out_dir == r.out_dir && o.subfolder == subfolder);
        if taken {
            continue;
        }
        // Only follow the release folder if this file was in its own one.
        let follow = !r.subfolder.is_empty() || subfolder.is_empty();
        let r = &mut recs[i];
        r.group = group;
        if follow {
            r.subfolder = subfolder;
        }
        moved = true;
    }
    moved
}

fn load_queue() -> (Vec<JobRecord>, Option<String>) {
    load_queue_at(&queue_path())
}

fn load_queue_at(path: &std::path::Path) -> (Vec<JobRecord>, Option<String>) {
    let path = path.to_path_buf();
    let bak = path.with_extension("json.bak");
    let parse = |p: &std::path::Path| -> Option<Option<Vec<JobRecord>>> {
        let bytes = std::fs::read(p).ok()?; // None: no file
        Some(serde_json::from_slice::<Vec<JobRecord>>(&bytes).ok()) // Some(None): unreadable
    };
    match parse(&path) {
        None => (Vec::new(), None),
        Some(Some(list)) => {
            let _ = std::fs::copy(&path, &bak);
            (list, None)
        }
        Some(None) => {
            let kept = path.with_file_name(format!("queue.corrupt-{}.json", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
            let _ = std::fs::rename(&path, &kept);
            let kept_name = kept.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match parse(&bak) {
                Some(Some(list)) => (
                    list,
                    Some(format!("The download list was damaged. Restored the last good copy; the damaged file was kept as {kept_name}.")),
                ),
                _ => (
                    Vec::new(),
                    Some(format!("The download list was damaged and couldn't be restored. It was kept as {kept_name}; partial files are still on disk.")),
                ),
            }
        }
    }
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let s = settings::load();
        let attention = Arc::new(Attention::default());
        let limiter = Arc::new(RateLimiter::new());
        limiter.set_mbs(s.speed_limit_mbs);

        let (records, notice) = load_queue();
        let mut jobs: Vec<Job> = records
            .into_iter()
            .map(|mut r| {
                // Anything mid-flight when the app closed picks up where it was.
                if matches!(r.state, State::Resolving | State::Downloading | State::Ready) {
                    r.state = State::Queued;
                }
                // Cookies are never saved, and datanodes links don't work
                // without them: get a fresh link after a restart.
                if r.host == Host::Datanodes {
                    r.direct = None;
                }
                Job::new(r)
            })
            .collect();
        jobs.sort_by_key(|j| j.rec.id);
        let mut recs: Vec<JobRecord> = jobs.iter().map(|j| j.rec.clone()).collect();
        let regrouped = adopt_companions(&mut recs);
        if regrouped {
            for (j, r) in jobs.iter_mut().zip(recs) {
                j.rec = r;
            }
        }
        let next_id = jobs.iter().map(|j| j.rec.id).max().unwrap_or(0) + 1;
        let clients = build_clients(&s.proxies);

        Arc::new(Self {
            inner: Mutex::new(Inner {
                jobs,
                next_id,
                dirty: regrouped,
                disk_full: None,
                notice,
                history: VecDeque::with_capacity(HISTORY_CAP),
                log: VecDeque::new(),
                log_seq: 0,
                clients,
                rr: 0,
            }),
            settings: RwLock::new(s),
            gate: Arc::new(BrowserGate::new(attention.clone())),
            attention,
            limiter,
            extract_http: extract::extraction_client(),
            wake: Notify::new(),
            app: std::sync::OnceLock::new(),
        })
    }

    pub fn set_app(&self, app: tauri::AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn set_settings(&self, s: Settings) -> Settings {
        let s = s.clamped();
        let proxies_changed = self.settings.read().unwrap().proxies != s.proxies;
        self.limiter.set_mbs(s.speed_limit_mbs);
        settings::save(&s);
        *self.settings.write().unwrap() = s.clone();
        if proxies_changed {
            self.inner.lock().unwrap().clients = build_clients(&s.proxies);
        }
        self.wake.notify_one();
        s
    }

    pub fn log(&self, level: &'static str, msg: impl Into<String>) {
        let mut g = self.inner.lock().unwrap();
        g.log_seq += 1;
        let line = LogLine { seq: g.log_seq, ts: util::now_ms(), level, msg: msg.into() };
        g.log.push_back(line);
        while g.log.len() > LOG_CAP {
            g.log.pop_front();
        }
    }

    // ── commands ────────────────────────────────────────────────────────────

    pub fn add_links(&self, text: &str, out_dir: Option<String>, start: bool) -> AddResult {
        let links = util::parse_links(text);
        // Decided over the whole paste, so a release's companion files join it.
        let releases: std::collections::HashMap<String, String> = {
            let ok: Vec<&String> = links.iter().filter(|u| Host::of(u) != Host::Unsupported).collect();
            let names: Vec<String> = ok.iter().map(|u| util::filename_for(u)).collect();
            ok.into_iter().cloned().zip(util::batch_release_names(&names)).collect()
        };
        let s = self.settings();
        let out = out_dir.filter(|d| !d.trim().is_empty()).unwrap_or(s.out_dir.clone());
        let mut res = AddResult { added: 0, duplicates: 0, unsupported: 0, mirrors: 0, groups: vec![] };
        {
            let mut g = self.inner.lock().unwrap();
            // Indexes, so adding tens of thousands of links stays linear.
            let mut known: std::collections::HashSet<String> =
                g.jobs.iter().flat_map(|j| j.rec.sources.iter().cloned()).collect();
            let mut by_name: std::collections::HashMap<(String, String, String), usize> = g
                .jobs
                .iter()
                .enumerate()
                .map(|(i, j)| ((j.rec.name.clone(), j.rec.out_dir.clone(), j.rec.subfolder.clone()), i))
                .collect();
            for url in links {
                if !known.insert(url.clone()) {
                    res.duplicates += 1;
                    continue;
                }
                let host = Host::of(&url);
                if host == Host::Unsupported {
                    res.unsupported += 1;
                    continue;
                }
                let name = util::filename_for(&url);
                let group = releases.get(&url).cloned().unwrap_or_else(|| util::release_name(&name));
                let subfolder = if s.release_folders { group.clone() } else { String::new() };
                if !res.groups.contains(&group) {
                    res.groups.push(group.clone());
                }

                // The same part from another host is a mirror, not a new file.
                let key = (name.clone(), out.clone(), subfolder.clone());
                if let Some(j) = by_name.get(&key).copied().map(|i| &mut g.jobs[i]) {
                    j.rec.sources.push(url.clone());
                    // fuckingfast usually resolves over plain HTTPS in under a
                    // second; datanodes needs the browser. Prefer it while nothing
                    // has started yet.
                    let untouched = matches!(j.rec.state, State::Queued | State::Paused)
                        && j.rec.direct.is_none()
                        && j.progress.done.load(Ordering::Relaxed) == 0;
                    if untouched && host == Host::Fuckingfast && j.rec.host != Host::Fuckingfast {
                        j.rec.sources.retain(|u| *u != url);
                        j.rec.sources.insert(0, url.clone());
                        j.rec.url = url;
                        j.rec.host = host;
                    }
                    res.mirrors += 1;
                    continue;
                }

                by_name.insert(key, g.jobs.len());
                let id = g.next_id;
                g.next_id += 1;
                g.jobs.push(Job::new(JobRecord {
                    id,
                    url: url.clone(),
                    name,
                    host,
                    group,
                    subfolder,
                    sources: vec![url],
                    state: if start { State::Queued } else { State::Paused },
                    total: 0,
                    done: 0,
                    error: None,
                    note: None,
                    direct: None,
                    cookies: None,
                    out_dir: out.clone(),
                    added_at: util::now_ms(),
                    finished_at: None,
                    attempts: 0,
                    link_refreshes: 0,
                    stall_kills: 0,
                }));
                res.added += 1;
            }
            g.dirty = true;
        }
        if res.added > 0 || res.mirrors > 0 {
            let mirrors = if res.mirrors > 0 { format!(" (+{} mirror links)", res.mirrors) } else { String::new() };
            self.log("info", format!("Added {} file(s){mirrors} in {} → {}", res.added, res.groups.join(", "), out));
        }
        self.wake.notify_one();
        res
    }

    pub fn pause(&self, ids: &[u64]) {
        let mut g = self.inner.lock().unwrap();
        for j in g.jobs.iter_mut().filter(|j| ids.contains(&j.rec.id)) {
            match j.rec.state {
                State::Resolving | State::Downloading => {
                    j.reason = Some(Reason::Pause);
                    if let Some(c) = &j.cancel {
                        c.cancel();
                    }
                }
                State::Queued | State::Ready => j.rec.state = State::Paused,
                _ => {}
            }
        }
        g.dirty = true;
    }

    pub fn resume(&self, ids: &[u64]) {
        let mut g = self.inner.lock().unwrap();
        g.disk_full = None;
        for j in g.jobs.iter_mut().filter(|j| ids.contains(&j.rec.id)) {
            if matches!(j.rec.state, State::Paused | State::Failed) {
                j.rec.state = State::Queued;
                j.rec.error = None;
                j.rec.attempts = 0;
                j.rec.link_refreshes = 0;
                j.retry_after = None;
            }
        }
        g.dirty = true;
        drop(g);
        self.wake.notify_one();
    }

    pub fn pause_all(&self) {
        let ids: Vec<u64> = self.inner.lock().unwrap().jobs.iter().map(|j| j.rec.id).collect();
        self.pause(&ids);
    }

    pub fn resume_all(&self) {
        let ids: Vec<u64> = self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|j| j.rec.state == State::Paused)
            .map(|j| j.rec.id)
            .collect();
        self.resume(&ids);
    }

    pub fn retry_failed(&self) {
        let ids: Vec<u64> = self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|j| j.rec.state == State::Failed)
            .map(|j| j.rec.id)
            .collect();
        self.resume(&ids);
    }

    /// Remove jobs from the list; optionally delete partial downloads.
    pub fn remove(&self, ids: &[u64], delete_partial: bool) {
        {
            let mut g = self.inner.lock().unwrap();
            for j in g.jobs.iter_mut().filter(|j| ids.contains(&j.rec.id)) {
                j.removed = true;
                // Deleting now would race the running task, which writes its
                // checkpoint once more as it stops. Delete when it's gone.
                j.purge = delete_partial && j.rec.state != State::Done;
                if let Some(c) = &j.cancel {
                    j.reason = Some(Reason::Remove);
                    c.cancel();
                }
            }
            g.dirty = true;
        }
        self.drop_removed();
    }

    /// Remove jobs from the list and their files from disk. Finished files go
    /// to the Recycle Bin (Trash on Linux), so a slip can be undone; partial
    /// downloads are deleted as in `remove(.., true)`. A finished file that
    /// can't be moved (open in another program) stays in the list.
    pub fn delete_files(&self, ids: &[u64]) -> DeleteResult {
        const BIN: &str = if cfg!(windows) { "Recycle Bin" } else { "Trash" };
        let (finished, unfinished): (Vec<(u64, PathBuf, Option<PathBuf>)>, Vec<u64>) = {
            let g = self.inner.lock().unwrap();
            let mut fin = Vec::new();
            let mut rest = Vec::new();
            for j in g.jobs.iter().filter(|j| ids.contains(&j.rec.id)) {
                if j.rec.state == State::Done {
                    // Only a release folder this app made is a candidate for tidying up.
                    let folder = (!j.rec.subfolder.is_empty()).then(|| j.rec.folder());
                    fin.push((j.rec.id, j.rec.dest(), folder));
                } else {
                    rest.push(j.rec.id);
                }
            }
            (fin, rest)
        };
        let mut res = DeleteResult { trashed: 0, failed: 0 };
        let mut gone = unfinished;
        let mut folders = Vec::new();
        for (id, dest, folder) in finished {
            // Already gone from disk (deleted by hand): nothing to move.
            let ok = !dest.exists() || trash::delete(&dest).is_ok();
            if ok {
                res.trashed += 1;
                gone.push(id);
                folders.extend(folder);
            } else {
                res.failed += 1;
            }
        }
        self.remove(&gone, true);
        folders.sort();
        folders.dedup();
        for f in folders {
            // Fails, harmlessly, unless the folder is now empty.
            let _ = std::fs::remove_dir(f);
        }
        if res.trashed > 0 {
            self.log("info", format!("Moved {} finished file(s) to the {BIN}", res.trashed));
        }
        if res.failed > 0 {
            self.log("warn", format!("{} file(s) couldn't be moved to the {BIN}; they may be open in another program", res.failed));
        }
        res
    }

    pub fn dismiss_notice(&self) {
        self.inner.lock().unwrap().notice = None;
    }

    pub fn clear_finished(&self) {
        let mut g = self.inner.lock().unwrap();
        g.jobs.retain(|j| j.rec.state != State::Done);
        g.dirty = true;
    }

    pub fn job_paths(&self, id: u64) -> Option<(PathBuf, bool)> {
        let g = self.inner.lock().unwrap();
        let j = g.jobs.iter().find(|j| j.rec.id == id)?;
        let dest = j.rec.dest();
        Some((dest, j.rec.state == State::Done))
    }

    pub fn direct_links(&self, ids: Option<&[u64]>) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.jobs
            .iter()
            .filter(|j| ids.is_none_or(|ids| ids.contains(&j.rec.id)))
            .filter_map(|j| j.rec.direct.clone())
            .collect()
    }

    pub fn source_links(&self, ids: &[u64]) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.jobs.iter().filter(|j| ids.contains(&j.rec.id)).map(|j| j.rec.url.clone()).collect()
    }

    pub fn snapshot(&self, log_after: u64) -> Snapshot {
        let g = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut st = Stats { count: g.jobs.len(), ..Default::default() };
        let mut known_sizes: Vec<u64> = Vec::new();
        let mut remaining_known: u64 = 0;
        let mut unknown_left = 0usize;
        let jobs: Vec<JobView> = g
            .jobs
            .iter()
            .map(|j| {
                let done = j.progress.done.load(Ordering::Relaxed);
                let total = j.progress.total.load(Ordering::Relaxed);
                match j.rec.state {
                    State::Done => st.done += 1,
                    State::Failed => st.failed += 1,
                    State::Paused => st.paused += 1,
                    State::Queued | State::Ready => st.queued += 1,
                    State::Resolving | State::Downloading => st.active += 1,
                }
                st.speed += j.speed;
                if total > 0 {
                    known_sizes.push(total);
                    st.bytes_total += total;
                    st.bytes_done += done.min(total);
                    if !matches!(j.rec.state, State::Done | State::Failed) {
                        remaining_known += total.saturating_sub(done);
                    }
                } else if !matches!(j.rec.state, State::Done | State::Failed) {
                    unknown_left += 1;
                }
                let eta = (j.rec.state == State::Downloading && j.speed > 1.0 && total > 0)
                    .then(|| total.saturating_sub(done) as f64 / j.speed);
                JobView {
                    id: j.rec.id,
                    name: j.rec.name.clone(),
                    url: j.rec.url.clone(),
                    host: j.rec.host,
                    state: j.rec.state,
                    total,
                    done,
                    speed: j.speed,
                    eta,
                    conns: j.progress.conns.load(Ordering::Relaxed),
                    error: j.rec.error.clone(),
                    note: j.rec.note.clone(),
                    has_direct: j.rec.direct.is_some(),
                    added_at: j.rec.added_at,
                    finished_at: j.rec.finished_at,
                    retry_in: j
                        .retry_after
                        .filter(|t| *t > now && j.rec.state == State::Queued)
                        .map(|t| (t - now).as_secs_f64()),
                    group: j.rec.group.clone(),
                    folder: j.rec.folder().to_string_lossy().into_owned(),
                    mirrors: j.rec.sources.len().saturating_sub(1),
                }
            })
            .collect();
        // Files not yet probed are estimated at the average known size.
        st.unknown_sizes = unknown_left;
        let avg = if known_sizes.is_empty() { 0 } else { known_sizes.iter().sum::<u64>() / known_sizes.len() as u64 };
        let remaining = remaining_known + avg * unknown_left as u64;
        if st.speed > 1.0 && remaining > 0 {
            st.eta = Some(remaining as f64 / st.speed);
            st.eta_estimated = unknown_left > 0;
        }
        Snapshot {
            jobs,
            stats: st,
            history: g.history.iter().copied().collect(),
            attention: self.attention.list(),
            disk_full: g.disk_full.clone(),
            notice: g.notice.clone(),
            browser_open: self.gate.is_open(),
            log: g.log.iter().filter(|l| l.seq > log_after).cloned().collect(),
            log_seq: g.log_seq,
        }
    }

    pub fn save_now(&self) {
        let recs: Vec<JobRecord> = {
            let mut g = self.inner.lock().unwrap();
            g.dirty = false;
            g.jobs
                .iter()
                .map(|j| {
                    let mut r = j.rec.clone();
                    r.done = j.progress.done.load(Ordering::Relaxed);
                    r.total = j.progress.total.load(Ordering::Relaxed);
                    r
                })
                .collect()
        };
        if let Ok(b) = serde_json::to_vec(&recs) {
            let p = queue_path();
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }

    // ── scheduler ───────────────────────────────────────────────────────────

    pub fn spawn_scheduler(self: &Arc<Self>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut last_save = Instant::now();
            let mut last_hist = Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    _ = me.wake.notified() => {}
                }
                me.sample(&mut last_hist);
                me.schedule();
                if last_save.elapsed() > Duration::from_secs(2) {
                    let dirty = me.inner.lock().unwrap().dirty
                        || me.inner.lock().unwrap().jobs.iter().any(|j| j.rec.state == State::Downloading);
                    if dirty {
                        me.save_now();
                    }
                    last_save = Instant::now();
                }
            }
        });
    }

    /// Speeds over a 3 s window, the history curve, and the stall killer.
    fn sample(&self, last_hist: &mut Instant) {
        let killer = self.settings.read().unwrap().stall_killer;
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        let mut total_speed = 0.0;
        let mut stalled: Vec<String> = Vec::new();
        for j in g.jobs.iter_mut() {
            if j.rec.state != State::Downloading {
                j.samples.clear();
                j.speed = 0.0;
                continue;
            }
            let done = j.progress.done.load(Ordering::Relaxed);
            j.samples.push_back((now, done));
            while j.samples.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(3)) {
                j.samples.pop_front();
            }
            j.speed = match (j.samples.front(), j.samples.back()) {
                (Some((t0, b0)), Some((t1, b1))) if *t1 > *t0 => {
                    b1.saturating_sub(*b0) as f64 / (*t1 - *t0).as_secs_f64().max(0.25)
                }
                _ => 0.0,
            };
            total_speed += j.speed;

            // A CDN lane pins a session; a slow lane stays slow. Re-extracting
            // gets a fresh one. Only for big files that are well short of done.
            let total = j.progress.total.load(Ordering::Relaxed);
            let eligible = killer
                && j.rec.stall_kills < STALL_MAX_KILLS
                && total >= STALL_MIN_FILE
                && (done as f64) < 0.8 * total as f64
                && j.dl_started.is_some_and(|t| now - t > STALL_GRACE);
            if eligible && j.speed < STALL_MIN_BPS {
                let since = *j.slow_since.get_or_insert(now);
                if now - since > STALL_WINDOW {
                    j.reason = Some(Reason::Stall);
                    if let Some(c) = &j.cancel {
                        c.cancel();
                    }
                    stalled.push(j.rec.name.clone());
                }
            } else {
                j.slow_since = None;
            }
        }
        if now - *last_hist >= Duration::from_secs(1) {
            *last_hist = now;
            g.history.push_back(total_speed);
            while g.history.len() > HISTORY_CAP {
                g.history.pop_front();
            }
        }
        drop(g);
        for n in stalled {
            self.log("warn", format!("{n}: stuck under 0.5 MB/s for a minute, getting a fresh link"));
        }
    }

    fn schedule(self: &Arc<Self>) {
        let s = self.settings();
        let links_mode = s.mode == "links";
        let now = Instant::now();
        let mut to_resolve: Vec<u64> = Vec::new();
        let mut to_download: Vec<u64> = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            if g.disk_full.is_some() {
                return;
            }
            let count = |st: State| g.jobs.iter().filter(|j| j.rec.state == st).count();
            let mut downloading = count(State::Downloading);
            let mut resolving = count(State::Resolving);
            let mut ready = count(State::Ready);

            for j in g.jobs.iter_mut() {
                let due = j.retry_after.is_none_or(|t| t <= now);
                match j.rec.state {
                    State::Ready if !links_mode && downloading < s.max_active => {
                        j.rec.state = State::Downloading;
                        downloading += 1;
                        ready -= 1;
                        to_download.push(j.rec.id);
                    }
                    State::Queued if due && j.rec.direct.is_some() && !links_mode && downloading < s.max_active => {
                        j.rec.state = State::Downloading;
                        downloading += 1;
                        to_download.push(j.rec.id);
                    }
                    State::Queued if due && j.rec.direct.is_none() => {
                        // Resolve a little ahead of free download slots, never far
                        // ahead: direct links expire.
                        let free = s.max_active.saturating_sub(downloading) + 2;
                        if resolving < s.max_resolving && (links_mode || resolving + ready < free) {
                            j.rec.state = State::Resolving;
                            resolving += 1;
                            to_resolve.push(j.rec.id);
                        }
                    }
                    _ => {}
                }
            }
            if !to_resolve.is_empty() || !to_download.is_empty() {
                g.dirty = true;
            }
        }
        for id in to_resolve {
            self.start_resolve(id, s.clone());
        }
        for id in to_download {
            self.start_download(id, s.clone());
        }
    }

    fn with_job<R>(&self, id: u64, f: impl FnOnce(&mut Job) -> R) -> Option<R> {
        let mut g = self.inner.lock().unwrap();
        g.dirty = true;
        g.jobs.iter_mut().find(|j| j.rec.id == id).map(f)
    }

    fn start_resolve(self: &Arc<Self>, id: u64, s: Settings) {
        let cancel = CancellationToken::new();
        let Some((url, name)) = self.with_job(id, |j| {
            j.cancel = Some(cancel.clone());
            j.reason = None;
            j.rec.error = None;
            (j.rec.url.clone(), j.rec.name.clone())
        }) else {
            return;
        };
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let links_mode = s.mode == "links";
            let retries = s.retries;
            let ctx = ExtractCtx { http: me.extract_http.clone(), gate: me.gate.clone(), settings: s, app: me.app.get().cloned() };
            let res = tokio::select! {
                r = extract::resolve(&ctx, &url, &name) => Some(r),
                _ = cancel.cancelled() => None,
            };
            me.attention.clear(&name);
            let mut msg: Option<(&'static str, String)> = None;
            me.with_job(id, |j| {
                j.cancel = None;
                match (res, j.reason.take()) {
                    (_, Some(Reason::Pause)) | (None, _) => j.rec.state = State::Paused,
                    (Some(Ok(r)), _) => {
                        j.rec.direct = Some(r.url.clone());
                        j.rec.cookies = r.cookies;
                        j.rec.attempts = 0;
                        if links_mode {
                            j.rec.state = State::Done;
                            j.rec.note = Some("Link resolved".into());
                            j.rec.finished_at = Some(util::now_ms());
                        } else {
                            j.rec.state = State::Ready;
                        }
                    }
                    (Some(Err(ResolveError::Dead(m))), _) => {
                        if j.rec.next_mirror() {
                            msg = Some(("warn", format!("{}: {m}. Trying the {} mirror", j.rec.name, host_label(j.rec.host))));
                        } else {
                            j.rec.state = State::Failed;
                            msg = Some(("error", format!("{}: {m}", j.rec.name)));
                            j.rec.error = Some(m);
                        }
                    }
                    (Some(Err(ResolveError::Retry(m))), _) => {
                        j.rec.attempts += 1;
                        if j.rec.attempts > retries && j.rec.next_mirror() {
                            msg = Some(("warn", format!("{}: {m}. Trying the {} mirror", j.rec.name, host_label(j.rec.host))));
                        } else if j.rec.attempts > retries {
                            j.rec.state = State::Failed;
                            msg = Some(("error", format!("{}: {m}", j.rec.name)));
                            j.rec.error = Some(m);
                        } else {
                            let backoff = 2u64.pow(j.rec.attempts.min(5)).min(30);
                            j.retry_after = Some(Instant::now() + Duration::from_secs(backoff));
                            j.rec.state = State::Queued;
                            msg = Some(("warn", format!("{}: {m}. Retrying in {backoff}s", j.rec.name)));
                            j.rec.error = Some(m);
                        }
                    }
                }
            });
            me.drop_removed();
            if let Some((lvl, m)) = msg {
                me.log(lvl, m);
            }
            me.wake.notify_one();
        });
    }

    /// A removed job stays listed only until its task has wound down.
    fn drop_removed(&self) {
        let mut purge: Vec<PathBuf> = Vec::new();
        self.inner.lock().unwrap().jobs.retain(|j| {
            let gone = j.removed && j.cancel.is_none();
            if gone && j.purge {
                purge.push(j.rec.dest());
            }
            !gone
        });
        for dest in purge {
            let part = download::part_path(&dest);
            let _ = std::fs::remove_file(&part);
            let mut m = part.into_os_string();
            m.push(".json");
            let _ = std::fs::remove_file(m);
        }
    }

    fn start_download(self: &Arc<Self>, id: u64, s: Settings) {
        let cancel = CancellationToken::new();
        let client = {
            let mut g = self.inner.lock().unwrap();
            g.rr = g.rr.wrapping_add(1);
            g.clients[g.rr % g.clients.len()].clone()
        };
        let Some((direct, cookies, dest, progress)) = self.with_job(id, |j| {
            j.cancel = Some(cancel.clone());
            j.reason = None;
            j.rec.error = None;
            j.dl_started = Some(Instant::now());
            j.slow_since = None;
            (
                j.rec.direct.clone().unwrap_or_default(),
                j.rec.cookies.clone(),
                j.rec.dest(),
                j.progress.clone(),
            )
        }) else {
            return;
        };

        let me = self.clone();
        let limiter = self.limiter.clone();
        tauri::async_runtime::spawn(async move {
            let req = DlRequest {
                url: direct,
                cookies,
                dest: dest.clone(),
                connections: s.connections,
                client,
                progress: progress.clone(),
                cancel,
                limiter,
            };
            let res = download::download(req).await;
            let mut msg: Option<(&'static str, String)> = None;
            let mut disk_full: Option<String> = None;
            me.with_job(id, |j| {
                j.cancel = None;
                j.dl_started = None;
                let reason = j.reason.take();
                match res {
                    Ok(fin) => {
                        j.rec.state = State::Done;
                        j.rec.total = fin.size;
                        j.rec.done = fin.size;
                        j.rec.finished_at = Some(util::now_ms());
                        j.rec.error = None;
                        if fin.already_on_disk {
                            j.rec.note = Some("Already on disk".into());
                            msg = Some(("ok", format!("{} is already on disk (same size), skipped", j.rec.name)));
                        } else {
                            msg = Some(("ok", format!("Saved {}", j.rec.name)));
                        }
                        if let Some(old) = &fin.moved_aside {
                            let old = old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            msg = Some(("warn", format!(
                                "{}: a different file with this name was in the way; it was kept as {old}",
                                j.rec.name
                            )));
                        }
                    }
                    Err(DlError::Cancelled) => match reason {
                        Some(Reason::Stall) => {
                            j.rec.stall_kills += 1;
                            j.rec.direct = None;
                            j.rec.state = State::Queued;
                        }
                        _ => j.rec.state = State::Paused,
                    },
                    Err(DlError::LinkDead(m)) => {
                        j.rec.direct = None;
                        j.rec.link_refreshes += 1;
                        if j.rec.link_refreshes > LINK_REFRESHES && j.rec.next_mirror() {
                            // Same file, so the partial download carries over.
                            msg = Some(("warn", format!("{}: switching to the {} mirror", j.rec.name, host_label(j.rec.host))));
                        } else if j.rec.link_refreshes > LINK_REFRESHES {
                            j.rec.state = State::Failed;
                            j.rec.error = Some(format!("The host keeps refusing the download ({m})"));
                        } else {
                            j.rec.state = State::Queued;
                            msg = Some(("warn", format!("{}: link expired ({m}), getting a fresh one", j.rec.name)));
                        }
                    }
                    Err(DlError::DiskFull(m)) => {
                        j.rec.state = State::Paused;
                        j.rec.error = Some(format!("Disk full: {m}"));
                        disk_full = Some(m);
                    }
                    Err(e) => {
                        j.rec.attempts += 1;
                        if j.rec.attempts > s.retries.max(1) + 2 {
                            j.rec.state = State::Failed;
                            j.rec.error = Some(e.to_string());
                            msg = Some(("error", format!("{}: {e}", j.rec.name)));
                        } else {
                            j.rec.state = State::Queued;
                            j.retry_after = Some(Instant::now() + Duration::from_secs(3 * j.rec.attempts as u64));
                            msg = Some(("warn", format!("{}: {e}. Will retry", j.rec.name)));
                        }
                    }
                }
            });
            me.drop_removed();
            if let Some(m) = disk_full {
                // Stop the run: every other transfer would hit the same wall.
                me.inner.lock().unwrap().disk_full = Some(m.clone());
                me.pause_all();
                me.log("error", format!("Disk full: {m}. Everything is paused; partial files are kept."));
            }
            if let Some((lvl, m)) = msg {
                me.log(lvl, m);
            }
            me.wake.notify_one();
        });
    }

    pub async fn shutdown(&self) {
        {
            let g = self.inner.lock().unwrap();
            for j in g.jobs.iter() {
                if let Some(c) = &j.cancel {
                    c.cancel();
                }
            }
        }
        self.save_now();
        self.gate.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    /// M2: a damaged queue file is kept, the last good copy is restored.
    #[test]
    fn damaged_queue_is_recovered_not_lost() {
        let dir = std::env::temp_dir().join(format!("bmd-queue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let q = dir.join("queue.json");
        let good = r#"[{"id":1,"url":"https://fuckingfast.co/abcdef12#a.rar","name":"a.rar","host":"fuckingfast","state":"queued","total":0,"done":0,"error":null,"note":null,"direct":null,"cookies":"session=old","outDir":"C:/x","addedAt":0,"finishedAt":null}]"#;
        std::fs::write(&q, good).unwrap();
        let (list, notice) = super::load_queue_at(&q);
        assert_eq!(list.len(), 1);
        assert!(notice.is_none());
        assert!(dir.join("queue.json.bak").exists(), "good load keeps a backup");

        std::fs::write(&q, b"{ this is not json").unwrap();
        let (list, notice) = super::load_queue_at(&q);
        assert_eq!(list.len(), 1, "restored from the backup");
        assert!(notice.unwrap().contains("Restored"));
        let kept = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("queue.corrupt-"));
        assert!(kept, "the damaged file is kept for inspection");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirrors_collapse_into_one_release() {
        let text = "##Download links
- https://fuckingfast.co/ffexample001#ExampleGame_--_fitgirl-repacks.site_--_.part1.rar
- https://fuckingfast.co/ffexample002#ExampleGame_--_fitgirl-repacks.site_--_.part2.rar
- https://fuckingfast.co/ffexample003#ExampleGame_--_fitgirl-repacks.site_--_.part3.rar
- https://fuckingfast.co/ffexample004#ExampleGame_--_fitgirl-repacks.site_--_.part4.rar
##Download links
- https://datanodes.to/dnexample001/ExampleGame_--_fitgirl-repacks.site_--_.part1.rar
- https://datanodes.to/dnexample002/ExampleGame_--_fitgirl-repacks.site_--_.part2.rar
- https://datanodes.to/dnexample003/ExampleGame_--_fitgirl-repacks.site_--_.part3.rar
- https://datanodes.to/dnexample004/ExampleGame_--_fitgirl-repacks.site_--_.part4.rar";
        let p = super::plan_links(text);
        assert_eq!(p.total, 8);
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].name, "ExampleGame");
        assert_eq!(p.groups[0].files, 4);
        assert_eq!((p.groups[0].fuckingfast, p.groups[0].datanodes), (4, 4));
    }

    #[test]
    fn selective_files_stay_with_their_release() {
        let text = "- https://fuckingfast.co/ffexample001#ExampleGame_--_fitgirl-repacks.site_--_.part01.rar
- https://fuckingfast.co/ffexample002#ExampleGame_--_fitgirl-repacks.site_--_.part02.rar
- https://fuckingfast.co/ffexample003#fg-selective-english.bin
- https://fuckingfast.co/ffexample004#fg-selective-french.bin";
        let p = super::plan_links(text);
        assert_eq!(p.groups.len(), 1);
        assert_eq!((p.groups[0].name.as_str(), p.groups[0].files), ("ExampleGame", 4));
    }

    #[test]
    fn saved_lone_companions_rejoin_their_release() {
        let rec = |id: u64, name: &str, done: u64| {
            let group = crate::util::release_name(name);
            serde_json::from_value::<super::JobRecord>(serde_json::json!({
                "id": id, "url": format!("https://fuckingfast.co/ffexample00{id}#{name}"), "name": name,
                "host": "fuckingfast", "state": "paused", "total": 0, "done": done, "error": null, "note": null,
                "direct": null, "outDir": "C:/x", "addedAt": 0, "finishedAt": null,
                "group": group, "subfolder": group,
            }))
            .unwrap()
        };
        let mut recs = vec![
            rec(1, "ExampleGame_--_fitgirl-repacks.site_--_.part01.rar", 5),
            rec(2, "fg-selective-english.bin", 0),
            rec(3, "fg-selective-french.bin", 7), // has data on disk: left alone
        ];
        assert!(super::adopt_companions(&mut recs));
        assert_eq!((recs[1].group.as_str(), recs[1].subfolder.as_str()), ("ExampleGame", "ExampleGame"));
        assert_eq!(recs[2].group, "fg-selective-french");
        assert!(!super::adopt_companions(&mut recs));
    }
}
