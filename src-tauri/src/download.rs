//! Segmented, resumable downloads (IDM-style).
//!
//! The file is preallocated as `<name>.bmdpart`, which catches a full disk
//! before a byte is fetched. N connections each own a byte range. A connection
//! that finishes early steals the back half of the largest remaining range, so
//! the last few MB never crawl along on a single slow stream. Progress per
//! range is checkpointed to `<name>.bmdpart.json` so a restart resumes exactly.

use futures::StreamExt;
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MIN_SPLIT: u64 = 4 * 1024 * 1024;
const MIN_SEGMENTED: u64 = 8 * 1024 * 1024;
const WRITE_BUF: usize = 1024 * 1024;
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const SEGMENT_RETRIES: u32 = 6;

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub conns: AtomicUsize,
}

#[derive(Debug)]
pub enum DlError {
    /// The direct link stopped working (expired, 403/404/410, HTML instead of a file).
    LinkDead(String),
    Cancelled,
    DiskFull(String),
    Io(String),
    Net(String),
    /// 429 / 503: the host wants fewer connections. Seconds from Retry-After.
    Busy(u64),
}

impl std::fmt::Display for DlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DlError::Busy(_) => write!(f, "The host is limiting connections (HTTP 429)"),
            DlError::LinkDead(m) => write!(f, "Link expired: {m}"),
            DlError::Cancelled => write!(f, "Stopped"),
            DlError::DiskFull(m) => write!(f, "Disk full: {m}"),
            DlError::Io(m) => write!(f, "Write error: {m}"),
            DlError::Net(m) => write!(f, "Network error: {m}"),
        }
    }
}

pub struct DlRequest {
    pub url: String,
    pub cookies: Option<String>,
    pub dest: PathBuf,
    pub connections: usize,
    pub client: reqwest::Client,
    pub progress: Arc<Progress>,
    pub cancel: CancellationToken,
    pub limiter: Arc<RateLimiter>,
}

// ── rate limiter (global token bucket) ──────────────────────────────────────

pub struct RateLimiter {
    bytes_per_sec: AtomicU64,
    state: Mutex<(Instant, f64)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self { bytes_per_sec: AtomicU64::new(0), state: Mutex::new((Instant::now(), 0.0)) }
    }
    pub fn set_mbs(&self, mbs: f64) {
        self.bytes_per_sec.store((mbs * 1_000_000.0) as u64, Ordering::Relaxed);
    }
    pub async fn take(&self, n: usize) {
        let rate = self.bytes_per_sec.load(Ordering::Relaxed) as f64;
        if rate <= 0.0 {
            return;
        }
        let wait = {
            let mut s = self.state.lock().unwrap();
            let now = Instant::now();
            let refill = now.duration_since(s.0).as_secs_f64() * rate;
            s.0 = now;
            s.1 = (s.1 + refill).min(rate); // at most one second of burst
            s.1 -= n as f64;
            if s.1 < 0.0 { -s.1 / rate } else { 0.0 }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

// ── segments ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SegMeta {
    from: u64,
    end: u64,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    total: u64,
    segments: Vec<SegMeta>,
}

struct Seg {
    /// Next byte a reader will reserve.
    pos: u64,
    /// Bytes below this are on disk.
    flushed: u64,
    /// Exclusive. Shrinks when another connection steals the tail.
    end: u64,
    claimed: bool,
}

type Segs = Arc<Mutex<Vec<Arc<Mutex<Seg>>>>>;

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".bmdpart");
    PathBuf::from(s)
}

fn meta_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".bmdpart.json");
    PathBuf::from(s)
}

fn is_disk_full(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::StorageFull || matches!(e.raw_os_error(), Some(112) | Some(39) | Some(28))
}

fn io_err(e: std::io::Error, dir: &Path) -> DlError {
    if is_disk_full(&e) {
        DlError::DiskFull(format!("not enough space in {}", dir.display()))
    } else {
        DlError::Io(e.to_string())
    }
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write made no progress"));
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

fn request(req: &DlRequest, range: Option<(u64, Option<u64>)>) -> reqwest::RequestBuilder {
    let mut b = req
        .client
        .get(&req.url)
        .header("Referer", crate::util::referer_for(&req.url))
        .header("Accept", "*/*");
    if let Some(c) = &req.cookies {
        b = b.header("Cookie", c);
    }
    if let Some((from, to)) = range {
        b = b.header(
            "Range",
            match to {
                Some(to) => format!("bytes={from}-{to}"),
                None => format!("bytes={from}-"),
            },
        );
    }
    b
}

fn check_status(resp: &reqwest::Response) -> Result<(), DlError> {
    let st = resp.status();
    if matches!(st, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::UNAUTHORIZED) {
        return Err(DlError::LinkDead(format!("HTTP {}", st.as_u16())));
    }
    if matches!(st, StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE) {
        let after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        return Err(DlError::Busy(after.min(120)));
    }
    if !st.is_success() {
        return Err(DlError::Net(format!("HTTP {}", st.as_u16())));
    }
    let html = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if html {
        return Err(DlError::LinkDead("server answered with a web page, not the file".into()));
    }
    Ok(())
}

/// Download `req.url` to `req.dest`. Returns the final size in bytes.
pub async fn download(req: DlRequest) -> Result<u64, DlError> {
    let dir = req.dest.parent().map(Path::to_path_buf).unwrap_or_default();
    std::fs::create_dir_all(&dir).map_err(|e| io_err(e, &dir))?;
    let part = part_path(&req.dest);
    let meta = meta_path(&req.dest);

    // Probe: size and range support in one tiny request.
    let probe = tokio::select! {
        r = request(&req, Some((0, Some(0)))).send() => r.map_err(|e| DlError::Net(e.to_string()))?,
        _ = req.cancel.cancelled() => return Err(DlError::Cancelled),
    };
    check_status(&probe)?;
    let (total, ranges) = if probe.status() == StatusCode::PARTIAL_CONTENT {
        let t = probe
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        (t, t > 0)
    } else {
        (probe.content_length().unwrap_or(0), false)
    };
    drop(probe);

    if total == 0 {
        return sequential(&req, &part, &dir).await;
    }
    req.progress.total.store(total, Ordering::Relaxed);

    // Resume from a checkpoint when it matches this file.
    let resumed: Option<Vec<SegMeta>> = std::fs::read(&meta)
        .ok()
        .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
        .filter(|m| ranges && m.total == total)
        .filter(|_| std::fs::metadata(&part).map(|m| m.len() == total).unwrap_or(false))
        .map(|m| m.segments);

    let file = match &resumed {
        Some(_) => OpenOptions::new().write(true).open(&part).map_err(|e| io_err(e, &dir))?,
        None => {
            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&part)
                .map_err(|e| io_err(e, &dir))?;
            f.set_len(total).map_err(|e| io_err(e, &dir))?;
            f
        }
    };
    let file = Arc::new(file);

    let plan: Vec<SegMeta> = match resumed {
        Some(s) => s.into_iter().filter(|s| s.from < s.end).collect(),
        None => {
            let n = if ranges && total >= MIN_SEGMENTED { req.connections.max(1) as u64 } else { 1 };
            let step = total.div_ceil(n);
            (0..n)
                .map(|i| SegMeta { from: i * step, end: ((i + 1) * step).min(total) })
                .filter(|s| s.from < s.end)
                .collect()
        }
    };
    let remaining: u64 = plan.iter().map(|s| s.end - s.from).sum();
    req.progress.done.store(total - remaining, Ordering::Relaxed);

    let segs: Segs = Arc::new(Mutex::new(
        plan.iter()
            .map(|s| Arc::new(Mutex::new(Seg { pos: s.from, flushed: s.from, end: s.end, claimed: false })))
            .collect(),
    ));

    let save_meta = {
        let segs = segs.clone();
        let meta = meta.clone();
        move || {
            let list: Vec<SegMeta> = segs
                .lock()
                .unwrap()
                .iter()
                .map(|s| {
                    let s = s.lock().unwrap();
                    SegMeta { from: s.flushed, end: s.end }
                })
                .filter(|s| s.from < s.end)
                .collect();
            if let Ok(b) = serde_json::to_vec(&Meta { total, segments: list }) {
                let _ = std::fs::write(&meta, b);
            }
        }
    };
    if ranges {
        save_meta();
    }

    let req = Arc::new(req);
    let workers = if ranges { req.connections.max(1) } else { 1 };
    let stop = req.cancel.child_token();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..workers {
        let (req, segs, file, stop, dir) = (req.clone(), segs.clone(), file.clone(), stop.clone(), dir.clone());
        set.spawn(async move { worker(req, segs, file, stop, ranges, dir).await });
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut first_err: Option<DlError> = None;
    loop {
        tokio::select! {
            r = set.join_next() => match r {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(e))) => {
                    if first_err.is_none() { first_err = Some(e); }
                    stop.cancel();
                }
                Some(Err(e)) => {
                    if first_err.is_none() { first_err = Some(DlError::Io(e.to_string())); }
                    stop.cancel();
                }
            },
            _ = ticker.tick() => if ranges { save_meta() },
        }
    }
    if ranges {
        save_meta();
    }
    if req.cancel.is_cancelled() {
        return Err(DlError::Cancelled);
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    let complete = segs.lock().unwrap().iter().all(|s| {
        let s = s.lock().unwrap();
        s.flushed >= s.end
    });
    if !complete {
        return Err(DlError::Net("connection closed before the file was complete".into()));
    }
    let _ = file.sync_all();
    drop(file);
    std::fs::rename(&part, &req.dest).map_err(|e| io_err(e, &dir))?;
    let _ = std::fs::remove_file(&meta);
    Ok(total)
}

/// Next range for this connection: an unclaimed one, else the back half of the
/// largest range still in flight.
fn next_work(segs: &Segs) -> Option<Arc<Mutex<Seg>>> {
    let mut list = segs.lock().unwrap();
    for s in list.iter() {
        let mut g = s.lock().unwrap();
        if !g.claimed && g.pos < g.end {
            g.claimed = true;
            return Some(s.clone());
        }
    }
    let mut best: Option<(u64, Arc<Mutex<Seg>>)> = None;
    for s in list.iter() {
        let g = s.lock().unwrap();
        let rem = g.end.saturating_sub(g.pos);
        if rem >= 2 * MIN_SPLIT && best.as_ref().is_none_or(|(r, _)| rem > *r) {
            best = Some((rem, s.clone()));
        }
    }
    let (_, victim) = best?;
    let mut v = victim.lock().unwrap();
    let rem = v.end.saturating_sub(v.pos);
    if rem < 2 * MIN_SPLIT {
        return None;
    }
    let mid = v.pos + rem / 2;
    let new = Arc::new(Mutex::new(Seg { pos: mid, flushed: mid, end: v.end, claimed: true }));
    v.end = mid;
    drop(v);
    list.push(new.clone());
    Some(new)
}

async fn worker(
    req: Arc<DlRequest>,
    segs: Segs,
    file: Arc<File>,
    stop: CancellationToken,
    ranges: bool,
    dir: PathBuf,
) -> Result<(), DlError> {
    while let Some(seg) = next_work(&segs) {
        req.progress.conns.fetch_add(1, Ordering::Relaxed);
        let r = fetch_segment(&req, &seg, &file, &stop, ranges, &dir).await;
        let others = req.progress.conns.fetch_sub(1, Ordering::Relaxed) - 1;
        if let Err(DlError::Busy(_)) = r {
            // The host caps connections per IP. Hand the range back and retire
            // this connection; the ones still running will pick it up.
            if others > 0 {
                seg.lock().unwrap().claimed = false;
                return Ok(());
            }
        }
        r?;
        if !ranges {
            break;
        }
    }
    Ok(())
}

async fn fetch_segment(
    req: &DlRequest,
    seg: &Arc<Mutex<Seg>>,
    file: &File,
    stop: &CancellationToken,
    ranges: bool,
    dir: &Path,
) -> Result<(), DlError> {
    let mut attempt = 0u32;
    loop {
        let (from, end) = {
            let g = seg.lock().unwrap();
            (g.pos, g.end)
        };
        if from >= end {
            return Ok(());
        }
        let range = if ranges { Some((from, Some(end - 1))) } else { None };
        let result = stream_range(req, seg, file, stop, range, dir).await;
        match result {
            Ok(()) => {
                let g = seg.lock().unwrap();
                if g.flushed >= g.end {
                    return Ok(());
                }
                // Server closed early; count it as a retryable drop.
                if attempt >= SEGMENT_RETRIES {
                    return Err(DlError::Net("server keeps closing the connection early".into()));
                }
            }
            Err(DlError::Net(m)) => {
                if attempt >= SEGMENT_RETRIES {
                    return Err(DlError::Net(m));
                }
            }
            Err(DlError::Busy(after)) => {
                if attempt >= 3 {
                    return Err(DlError::Busy(after));
                }
                attempt += 1;
                let wait = if after > 0 { after } else { 3 * attempt as u64 };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                    _ = stop.cancelled() => return Err(DlError::Cancelled),
                }
                continue;
            }
            Err(e) => return Err(e),
        }
        if !ranges {
            return Err(DlError::Net("connection dropped and the server can't resume".into()));
        }
        attempt += 1;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500 * attempt as u64)) => {}
            _ = stop.cancelled() => return Err(DlError::Cancelled),
        }
    }
}

async fn stream_range(
    req: &DlRequest,
    seg: &Arc<Mutex<Seg>>,
    file: &File,
    stop: &CancellationToken,
    range: Option<(u64, Option<u64>)>,
    dir: &Path,
) -> Result<(), DlError> {
    let resp = tokio::select! {
        r = request(req, range).send() => r.map_err(|e| DlError::Net(e.to_string()))?,
        _ = stop.cancelled() => return Err(DlError::Cancelled),
    };
    check_status(&resp)?;
    if range.is_some_and(|(f, _)| f > 0) && resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(DlError::Net("server ignored the byte range".into()));
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BUF);
    let mut buf_at = seg.lock().unwrap().pos;

    let flush = |buf: &mut Vec<u8>, buf_at: &mut u64| -> Result<(), DlError> {
        if buf.is_empty() {
            return Ok(());
        }
        write_at(file, buf, *buf_at).map_err(|e| io_err(e, dir))?;
        *buf_at += buf.len() as u64;
        seg.lock().unwrap().flushed = *buf_at;
        buf.clear();
        Ok(())
    };

    let outcome = loop {
        let next = tokio::select! {
            n = tokio::time::timeout(CHUNK_TIMEOUT, stream.next()) => n,
            _ = stop.cancelled() => break Err(DlError::Cancelled),
        };
        let chunk = match next {
            Err(_) => break Err(DlError::Net("no data for 60 s".into())),
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(DlError::Net(e.to_string())),
            Ok(Some(Ok(c))) => c,
        };
        req.limiter.take(chunk.len()).await;
        // Reserve bytes under the lock so a concurrent steal never overlaps us.
        let (allowed, finished) = {
            let mut g = seg.lock().unwrap();
            let allowed = (chunk.len() as u64).min(g.end.saturating_sub(g.pos)) as usize;
            g.pos += allowed as u64;
            (allowed, g.pos >= g.end)
        };
        buf.extend_from_slice(&chunk[..allowed]);
        req.progress.done.fetch_add(allowed as u64, Ordering::Relaxed);
        if buf.len() >= WRITE_BUF {
            if let Err(e) = flush(&mut buf, &mut buf_at) {
                break Err(e);
            }
        }
        if finished {
            break Ok(());
        }
    };
    // Whatever arrived is valid data: keep it even when the stream failed.
    flush(&mut buf, &mut buf_at)?;
    // Bytes reserved but never received go back to the range.
    {
        let mut g = seg.lock().unwrap();
        if g.pos > g.flushed {
            let lost = g.pos - g.flushed;
            g.pos = g.flushed;
            req.progress.done.fetch_sub(lost, Ordering::Relaxed);
        }
    }
    outcome
}

/// No size and no ranges: one stream straight to disk, no resume possible.
async fn sequential(req: &DlRequest, part: &Path, dir: &Path) -> Result<u64, DlError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(part)
        .map_err(|e| io_err(e, dir))?;
    req.progress.done.store(0, Ordering::Relaxed);
    let seg = Arc::new(Mutex::new(Seg { pos: 0, flushed: 0, end: u64::MAX, claimed: true }));
    req.progress.conns.store(1, Ordering::Relaxed);
    let r = stream_range(req, &seg, &file, &req.cancel, None, dir).await;
    req.progress.conns.store(0, Ordering::Relaxed);
    r?;
    let size = seg.lock().unwrap().flushed;
    drop(file);
    std::fs::rename(part, &req.dest).map_err(|e| io_err(e, dir))?;
    req.progress.total.store(size, Ordering::Relaxed);
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live network test: `cargo test --lib live_segmented -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_segmented_download_and_resume() {
        let url = std::env::var("BMD_TEST_URL").unwrap_or("https://proof.ovh.net/files/10Mb.dat".into());
        let dir = std::env::temp_dir().join("bmd-test");
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("file.bin");
        let client = reqwest::Client::builder().http1_only().build().unwrap();
        let mk = |cancel: CancellationToken| DlRequest {
            url: url.clone(),
            cookies: None,
            dest: dest.clone(),
            connections: 6,
            client: client.clone(),
            progress: Arc::new(Progress::default()),
            cancel,
            limiter: Arc::new(RateLimiter::new()),
        };
        // Interrupt the first attempt part-way, then resume.
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            c2.cancel();
        });
        let first = download(mk(cancel)).await;
        println!("first attempt: {:?}", first.as_ref().map_err(|e| e.to_string()));
        let size = download(mk(CancellationToken::new())).await.expect("resume completes");
        let on_disk = std::fs::metadata(&dest).unwrap().len();
        println!("size {size}, on disk {on_disk}");
        assert_eq!(size, on_disk);
        assert!(!part_path(&dest).exists());

        // Compare against a plain single-stream fetch.
        let plain = client.get(&url).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(plain.len() as u64, on_disk);
        assert!(std::fs::read(&dest).unwrap() == plain.as_ref(), "content differs");
    }
}
