// BetterMoonDownloader UI. The page pulls a snapshot from the Rust engine a few
// times a second and patches only what changed.
"use strict";

// No `window.__TAURI__` global (withGlobalTauri is off, so remote pages in
// the resolver windows don't see the app's API either). Dialogs, files and
// window controls are the app's own Rust commands.
const I = window.__TAURI_INTERNALS__;
const inApp = !!(I && I.invoke);
const invoke = inApp ? (cmd, args = {}) => I.invoke(cmd, args) : mockInvoke;
function listen(event, cb) {
  return I.invoke("plugin:event|listen", { event, target: { kind: "Any" }, handler: I.transformCallback(cb) });
}
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

const state = {
  filter: "all",
  query: "",
  jobs: [],
  stats: null,
  selected: new Set(),
  anchor: null,
  rows: new Map(),
  logSeq: 0,
  log: [],
  settings: null,
  platform: null,
  history: [],
};

// ── formatting ─────────────────────────────────────────────────────────────
const KB = 1024, MB = KB * 1024, GB = MB * 1024;
function bytes(n) {
  if (!n) return "0 B";
  if (n >= GB) return (n / GB).toFixed(n >= 100 * GB ? 0 : 2) + " GB";
  if (n >= MB) return (n / MB).toFixed(n >= 100 * MB ? 0 : 1) + " MB";
  if (n >= KB) return (n / KB).toFixed(0) + " KB";
  return n + " B";
}
function rate(bps) {
  if (!bps || bps < 1) return "—";
  const m = bps / 1e6;
  return m >= 100 ? m.toFixed(0) + " MB/s" : m >= 1 ? m.toFixed(1) + " MB/s" : (bps / 1e3).toFixed(0) + " KB/s";
}
function duration(s) {
  if (s == null || !isFinite(s)) return "—";
  s = Math.max(0, Math.round(s));
  if (s < 60) return s + "s";
  const m = Math.floor(s / 60), h = Math.floor(m / 60);
  if (h >= 24) return Math.floor(h / 24) + "d " + (h % 24) + "h";
  if (h) return h + "h " + String(m % 60).padStart(2, "0") + "m";
  return m + "m " + String(s % 60).padStart(2, "0") + "s";
}
function humanLeft(s, rough) {
  const about = rough ? "roughly " : "about ";
  if (s < 50) return "less than a minute left";
  if (s < 90) return about + "a minute left";
  if (s < 3600) return about + Math.round(s / 60) + " min left";
  const h = Math.floor(s / 3600), m = Math.round((s % 3600) / 60);
  return about + h + " h" + (m ? " " + m + " min" : "") + " left";
}
const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
function finishClock(ms) {
  const d = new Date(ms), now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  return sameDay ? clock(ms) : d.toLocaleDateString([], { weekday: "short" }) + " " + clock(ms);
}
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// ── the moon: lit fraction drawn as a waxing phase ─────────────────────────
function moonSvg(frac, r = 10) {
  const f = Math.max(0, Math.min(1, frac || 0));
  const c = r, top = `${c},${c - r}`, bot = `${c},${c + r}`;
  const rx = Math.abs(1 - 2 * f) * r;
  const lit = f <= 0.001 ? "" :
    f >= 0.999 ? `<circle cx="${c}" cy="${c}" r="${r}" fill="var(--moon)"/>` :
    `<path fill="var(--moon)" d="M${top} A${r} ${r} 0 0 1 ${bot} A${rx.toFixed(2)} ${r} 0 0 ${f < 0.5 ? 0 : 1} ${top}Z"/>`;
  return `<circle cx="${c}" cy="${c}" r="${r}" fill="var(--moon-dim)"/>${lit}`;
}

// ── row rendering ──────────────────────────────────────────────────────────
const STATE_TEXT = {
  queued: "Queued", ready: "Link ready, waiting for a slot", resolving: "Getting the download link…",
  downloading: "Downloading", paused: "Paused", done: "Done", failed: "Failed",
};
const HOST = { fuckingfast: "fuckingfast", datanodes: "datanodes", unsupported: "?" };

// "|" can't appear in a Windows path, so the key is unambiguous and survives
// being written into an HTML attribute.
const gkey = (j) => j.group + "|" + j.folder;
function groupsOf(jobs) {
  const m = new Map();
  for (const j of jobs) {
    const k = gkey(j);
    if (!m.has(k)) m.set(k, { key: k, name: j.group, folder: j.folder, jobs: [] });
    m.get(k).jobs.push(j);
  }
  return [...m.values()];
}

// Same arithmetic as the engine's batch stats, for one group.
function statsOf(jobs) {
  const s = { count: jobs.length, done: 0, failed: 0, active: 0, queued: 0, paused: 0, bytesDone: 0, bytesTotal: 0, unknownSizes: 0, speed: 0, eta: null, etaEstimated: false };
  let remaining = 0, known = 0;
  for (const j of jobs) {
    if (j.state === "done") s.done++;
    else if (j.state === "failed") s.failed++;
    else if (j.state === "paused") s.paused++;
    else if (j.state === "queued" || j.state === "ready") s.queued++;
    else s.active++;
    s.speed += j.speed || 0;
    if (j.total) {
      known++; s.bytesTotal += j.total; s.bytesDone += Math.min(j.done, j.total);
      if (j.state !== "done" && j.state !== "failed") remaining += j.total - j.done;
    } else if (j.state !== "done" && j.state !== "failed") s.unknownSizes++;
  }
  const avg = known ? s.bytesTotal / known : 0;
  remaining += avg * s.unknownSizes;
  if (s.speed > 1 && remaining > 0) { s.eta = remaining / s.speed; s.etaEstimated = s.unknownSizes > 0; }
  return s;
}

function matchesFilter(j) {
  if (state.filter.startsWith("g:")) return gkey(j) === state.filter.slice(2);
  switch (state.filter) {
    case "active": return j.state === "downloading" || j.state === "resolving";
    case "queued": return j.state === "queued" || j.state === "ready";
    case "paused": return j.state === "paused";
    case "done": return j.state === "done";
    case "failed": return j.state === "failed";
    default: return true;
  }
}

function subLine(j) {
  const host = `<span class="host">${HOST[j.host]}</span><span class="sep">·</span>`;
  if (j.state === "done") {
    const when = j.finishedAt ? "Done " + finishClock(j.finishedAt) : "Done";
    return host + `<span class="ok">${esc(j.note && j.note !== "Link resolved" ? j.note : when)}</span>`;
  }
  if (j.state === "failed") return host + `<span class="err">${esc(j.error || "Failed")}</span>`;
  if (j.state === "queued" && j.retryIn) return host + `<span>Retrying in ${Math.ceil(j.retryIn)}s</span><span class="sep">·</span><span>${esc(j.error || "")}</span>`;
  if (j.state === "downloading") {
    const conns = j.conns > 1 ? ` · ${j.conns} connections` : "";
    const fin = j.eta ? ` · done ${finishClock(Date.now() + j.eta * 1000)}` : "";
    return host + `<span>Downloading${conns}${fin}</span>`;
  }
  if (j.state === "paused" && j.error) return host + `<span class="err">${esc(j.error)}</span>`;
  return host + `<span>${STATE_TEXT[j.state]}</span>`;
}

function actionsFor(j) {
  const b = (act, icon, title) => `<button class="icon-btn" data-act="${act}" title="${title}"><svg><use href="#i-${icon}"/></svg></button>`;
  const main = (j.state === "downloading" || j.state === "resolving" || j.state === "queued" || j.state === "ready")
    ? b("pause", "pause", "Pause")
    : j.state === "failed" ? b("resume", "retry", "Retry")
    : j.state === "paused" ? b("resume", "play", "Resume") : "";
  return main + b("reveal", "folder", "Show in folder");
}

function makeRow(j) {
  const el = document.createElement("div");
  el.className = "row";
  el.setAttribute("role", "option");
  el.dataset.id = j.id;
  el.innerHTML = `
    <svg class="moon" viewBox="0 0 20 20"></svg>
    <div class="c-name"><div class="name"></div><div class="sub"></div></div>
    <div class="c-prog"><div class="bar"><i></i></div><span class="pct"></span></div>
    <div class="c-size"></div><div class="c-speed"></div><div class="c-eta"></div>
    <div class="c-act"></div>`;
  el._ = { moon: $(".moon", el), name: $(".name", el), sub: $(".sub", el), bar: $(".bar i", el), pct: $(".pct", el),
    size: $(".c-size", el), speed: $(".c-speed", el), eta: $(".c-eta", el), act: $(".c-act", el), last: {} };
  return el;
}

function set(el, key, cache, val, html = false) {
  if (cache[key] === val) return;
  cache[key] = val;
  if (html) el.innerHTML = val; else el.textContent = val;
}

// The original filename, split so that a long name is trimmed in the middle:
// the tail (".part1.rar") always stays visible.
function nameHtml(name) {
  const m = name.match(/(?:\.part\d+)?\.[a-z0-9]{1,5}$/i);
  const cut = Math.max(0, name.length - Math.max(m ? m[0].length : 0, 10));
  return `<span class="nh">${esc(name.slice(0, cut))}</span><span class="nt">${esc(name.slice(cut))}</span>`;
}

function patchRow(el, j) {
  const p = el._, c = p.last;
  const frac = j.total ? j.done / j.total : (j.state === "done" ? 1 : 0);
  if (c.state !== j.state) { el.dataset.state = j.state; c.state = j.state; }
  const moonKey = Math.round(frac * 40);
  if (c.moon !== moonKey) { c.moon = moonKey; p.moon.innerHTML = moonSvg(frac); }
  set(p.name, "name", c, nameHtml(j.name), true);
  if (c.title !== j.url) { c.title = j.url; p.name.title = j.name + "\n" + j.url; }
  set(p.sub, "sub", c, subLine(j), true);
  const w = (frac * 100).toFixed(1) + "%";
  if (c.w !== w) { c.w = w; p.bar.style.width = w; }
  set(p.pct, "pct", c, j.state === "resolving" || (!j.total && j.state !== "done") ? "" : Math.floor(frac * 100) + "%");
  set(p.size, "size", c, j.total ? (j.state === "done" ? bytes(j.total) : `${bytes(j.done)} / ${bytes(j.total)}`) : "—");
  set(p.speed, "speed", c, j.state === "downloading" ? rate(j.speed) : "");
  set(p.eta, "eta", c, j.state === "downloading" ? duration(j.eta) : "");
  set(p.act, "act", c, actionsFor(j), true);
  el.classList.toggle("is-sel", state.selected.has(j.id));
  el.setAttribute("aria-selected", state.selected.has(j.id));
}

// ── group headers ──────────────────────────────────────────────────────────
const collapsed = new Set(JSON.parse(safeGet("bmd.collapsed") || "[]"));
const opened = new Set(JSON.parse(safeGet("bmd.opened") || "[]"));
const doneOpen = new Set();
function toggleGroup(g) {
  const k = g.key;
  if (isCollapsed(g)) { collapsed.delete(k); opened.add(k); } else { collapsed.add(k); opened.delete(k); }
  safeSet("bmd.collapsed", JSON.stringify([...collapsed]));
  safeSet("bmd.opened", JSON.stringify([...opened]));
}

function makeSection(kind) {
  const el = document.createElement("div");
  el.className = "section";
  el.dataset.kind = kind;
  el.innerHTML = kind === "fin"
    ? `<h2>Finished</h2><span class="section-acts"><button class="btn btn-sm" data-clear title="Remove from the list; the files stay on disk">Clear all</button><button class="btn btn-sm btn-danger" data-purge title="Remove from the list and move the files to the Recycle Bin"><svg><use href="#i-trash"/></svg><span>Delete files…</span></button></span>`
    : `<h2>In progress</h2>`;
  return el;
}

// Finished releases only: parts already done in a release that's still
// downloading belong to it and stay.
function finishedIds() {
  return groupsOf(state.jobs).filter(isFinished).flatMap((g) => g.jobs.map((j) => j.id));
}
async function clearFinished() {
  const ids = finishedIds();
  if (!ids.length) return toast("Nothing finished to clear");
  await invoke("remove", { ids, deletePartial: false });
  toast(`Cleared ${ids.length} finished file${ids.length === 1 ? "" : "s"} from the list. Files on disk are kept.`);
}

function makeFold() {
  const el = document.createElement("div");
  el.className = "fold";
  el.innerHTML = `<svg class="chev" viewBox="0 0 20 20"><path d="M7.5 5l5 5-5 5"/></svg><svg class="ok-ic"><use href="#i-check"/></svg><span class="fold-text"></span><span class="fold-hint"></span>`;
  el._ = { text: $(".fold-text", el), hint: $(".fold-hint", el), last: {} };
  return el;
}
function patchFold(el, g, done) {
  const open = doneOpen.has(g.key);
  el.dataset.key = g.key;
  el.classList.toggle("is-open", open);
  const size = done.reduce((a, j) => a + (j.total || 0), 0);
  set(el._.text, "text", el._.last, `${done.length} finished${size ? " · " + bytes(size) : ""}`);
  set(el._.hint, "hint", el._.last, open ? "Hide" : "Show");
}
function safeGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function safeSet(k, v) { try { localStorage.setItem(k, v); } catch {} }

function makeGroup() {
  const el = document.createElement("div");
  el.className = "group";
  el.innerHTML = `
    <svg class="chev" viewBox="0 0 20 20"><path d="M7.5 5l5 5-5 5"/></svg>
    <svg class="moon" viewBox="0 0 20 20"></svg>
    <div class="c-name"><div class="name"></div><div class="sub"></div></div>
    <div class="c-prog"><div class="bar"><i></i></div><span class="pct"></span></div>
    <div class="c-size"></div><div class="c-speed"></div><div class="c-eta"></div>
    <div class="c-act"></div>`;
  el._ = { moon: $(".moon", el), name: $(".name", el), sub: $(".sub", el), bar: $(".bar i", el), pct: $(".pct", el),
    size: $(".c-size", el), speed: $(".c-speed", el), eta: $(".c-eta", el), act: $(".c-act", el), last: {} };
  return el;
}

function groupMode(s) {
  if (s.active || s.queued) return "pause";
  if (s.paused || s.failed) return "resume";
  return "done";
}

// Files with no size yet count as the average known size, so a release
// doesn't read 90% done while half its parts haven't started.
const expectedBytes = (s) => s.bytesTotal + (s.unknownSizes * s.bytesTotal) / Math.max(1, s.count - s.unknownSizes);
const groupFrac = (s) => s.bytesTotal ? s.bytesDone / expectedBytes(s) : s.done / s.count;

function patchGroup(el, g) {
  const p = el._, c = p.last, s = statsOf(g.jobs);
  el.dataset.key = g.key;
  const frac = groupFrac(s);
  const mode = groupMode(s);
  el.dataset.state = s.active ? "downloading" : mode === "done" ? (s.failed ? "failed" : "done") : s.paused && !s.queued ? "paused" : "queued";
  el.classList.toggle("is-collapsed", isCollapsed(g));
  const moonKey = Math.round(frac * 40);
  if (c.moon !== moonKey) { c.moon = moonKey; p.moon.innerHTML = moonSvg(s.done === s.count ? 1 : frac); }
  set(p.name, "name", c, g.name);
  if (c.title !== g.folder) { c.title = g.folder; p.name.title = g.folder; }
  let what;
  if (s.done === s.count) {
    const fin = Math.max(...g.jobs.map((j) => j.finishedAt || 0));
    what = `<span class="ok">All ${s.count} files done${fin ? " " + finishClock(fin) : ""}</span>`;
  } else {
    const bits = [`${s.done} of ${s.count} files done`];
    if (s.active) bits.push("downloading");
    else if (s.paused && !s.queued) bits.push("paused");
    else if (s.queued) bits.push("waiting");
    if (s.eta && s.speed > 1) bits.push(`done ${finishClock(Date.now() + s.eta * 1000)}`);
    what = bits.join(" · ");
    if (s.failed) what += ` · <span class="err">${s.failed} failed</span>`;
  }
  const mirrors = g.jobs.reduce((a, j) => a + (j.mirrors || 0), 0);
  if (mirrors) what += ` <span class="sep">·</span> <span>${mirrors} mirror link${mirrors === 1 ? "" : "s"}</span>`;
  set(p.sub, "sub", c, what, true);
  const w = (frac * 100).toFixed(1) + "%";
  if (c.w !== w) { c.w = w; p.bar.style.width = w; }
  set(p.pct, "pct", c, Math.floor(frac * 100) + "%");
  set(p.size, "size", c, s.bytesTotal ? (s.done === s.count ? bytes(s.bytesTotal) : `${bytes(s.bytesDone)} / ${s.unknownSizes ? "~" : ""}${bytes(expectedBytes(s))}`) : "—");
  set(p.speed, "speed", c, s.speed > 1 ? rate(s.speed) : "");
  set(p.eta, "eta", c, s.speed > 1 ? duration(s.eta) : "");
  const b = (act, icon, title) => `<button class="icon-btn" data-gact="${act}" title="${title}"><svg><use href="#i-${icon}"/></svg></button>`;
  set(p.act, "act", c,
    (mode === "pause" ? b("pause", "pause", "Pause this release") : mode === "resume" ? b("resume", s.failed && !s.paused ? "retry" : "play", "Resume this release") : "")
    + b("folder", "folder", "Open folder"), true);
}

// Releases that are moving come first, finished ones sink; the newest paste
// leads its tier, so it's on screen right after adding it.
function groupRank(s) { return s.active ? 0 : s.queued ? 1 : s.paused || s.failed ? 2 : 3; }
function orderedGroups() {
  return groupsOf(state.jobs)
    .map((g) => ({ g, rank: groupRank(statsOf(g.jobs)), added: Math.max(...g.jobs.map((j) => j.addedAt || 0)) }))
    .sort((a, b) => a.rank - b.rank || b.added - a.added)
    .map((x) => x.g);
}

// A finished release folds itself away unless it was opened by hand.
const isCollapsed = (g) => collapsed.has(g.key) || (!opened.has(g.key) && g.jobs.every((j) => j.state === "done"));

// Inside a release, what needs watching sits on top; part order holds within each state.
const ROW_RANK = { downloading: 0, resolving: 0, failed: 1, ready: 2, queued: 2, paused: 3, done: 4 };

function displayItems() {
  const q = state.query.trim().toLowerCase();
  // Only the mixed views fold finished parts; "Completed" shows them as they are.
  const foldable = !q && (state.filter === "all" || state.filter.startsWith("g:"));
  // "In progress" above "Finished" in the everything view; other views are one kind already.
  const sections = !q && state.filter === "all";
  const anyFinished = sections && groupsOf(state.jobs).some(isFinished);
  const items = [];
  let section = null;
  for (const g of orderedGroups()) {
    const vis = g.jobs.filter((j) => matchesFilter(j) && (!q || j.name.toLowerCase().includes(q) || j.group.toLowerCase().includes(q)));
    if (!vis.length) continue;
    if (sections) {
      const s = isFinished(g) ? "fin" : "live";
      if (s !== section) {
        // A lone "In progress" heading says nothing; it only appears above a Finished one.
        if (anyFinished) items.push({ section: s });
        section = s;
      }
    }
    if (g.jobs.length === 1) { items.push({ j: vis[0] }); continue; }
    items.push({ g });
    if (isCollapsed(g)) continue;
    const sorted = vis.slice().sort((a, b) => ROW_RANK[a.state] - ROW_RANK[b.state]);
    const rest = sorted.filter((j) => j.state !== "done");
    const done = foldable && rest.length ? sorted.filter((j) => j.state === "done") : [];
    for (const j of done.length ? rest : sorted) items.push({ j, inGroup: true });
    if (done.length) {
      items.push({ fold: g, done });
      if (doneOpen.has(g.key)) done.forEach((j) => items.push({ j, inGroup: true }));
    }
  }
  return items;
}

function visibleJobs() {
  return displayItems().filter((it) => it.j).map((it) => it.j);
}

function renderList() {
  const list = $("#list");
  const items = displayItems();
  const seen = new Set();
  let prev = null;
  let reveal = null;
  for (const it of items) {
    const key = it.section ? "s" + it.section : it.g ? "g" + it.g.key : it.fold ? "f" + it.fold.key : it.j.id;
    let el = state.rows.get(key);
    if (!el) { el = it.section ? makeSection(it.section) : it.g ? makeGroup() : it.fold ? makeFold() : makeRow(it.j); state.rows.set(key, el); }
    if (it.section) { /* static */ }
    else if (it.g) patchGroup(el, it.g);
    else if (it.fold) patchFold(el, it.fold, it.done);
    else { patchRow(el, it.j); el.classList.toggle("in-group", !!it.inGroup); }
    if (!reveal && state.reveal && !it.fold && !it.section && (it.g ? it.g.name : it.j.group) === state.reveal) reveal = el;
    seen.add(key);
    const want = prev ? prev.nextSibling : list.firstChild;
    if (want !== el) list.insertBefore(el, want);
    prev = el;
  }
  for (const [key, el] of state.rows) {
    if (!seen.has(key)) {
      el.remove();
      const alive = typeof key === "number" ? state.jobs.some((j) => j.id === key) : key[0] === "s" || state.jobs.some((j) => key.slice(1) === gkey(j));
      if (!alive) state.rows.delete(key);
    }
  }
  // Just added: bring the release into view and mark it once.
  if (reveal) {
    state.reveal = null;
    reveal.scrollIntoView({ block: "nearest", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    reveal.classList.remove("is-new"); void reveal.offsetWidth; reveal.classList.add("is-new");
  }
  for (const id of [...state.selected]) if (!state.jobs.some((j) => j.id === id)) state.selected.delete(id);
  const jobs = items;
  renderReleases();

  const isLog = state.filter === "log";
  const none = jobs.length === 0;
  $("#list").hidden = isLog;
  $("#listHead").hidden = isLog || none;
  $("#log").hidden = !isLog;
  $("#empty").hidden = isLog || !none;
  if (none && !isLog) {
    const all = state.jobs.length === 0;
    $("#emptyTitle").textContent = all ? "Nothing to download yet" : state.query ? "No files match" : "Nothing here";
    $("#emptyText").innerHTML = all
      ? "Drop a <code>.txt</code> of fuckingfast or datanodes links on this window, or paste them."
      : state.query ? `No file names contain “${esc(state.query)}”.` : "Downloads in this state will show up here.";
    $("#emptyAdd").hidden = !all;
  }
}

// ── summary ────────────────────────────────────────────────────────────────
// The releases still being worked on. Finished releases are history and stay
// out of the summary, so its bar answers "how far along is what I'm downloading".
const isFinished = (g) => g.jobs.every((j) => j.state === "done");
function liveGroups() { return orderedGroups().filter((g) => !isFinished(g)); }

function renderSummary() {
  const box = $("#summary");
  if (!state.jobs.length || state.filter === "log") { box.hidden = true; return; }
  box.hidden = false;
  // A release view summarises that release; everything else, what's unfinished.
  const groups = state.filter.startsWith("g:") ? [groupByKey(state.filter.slice(2))].filter(Boolean) : liveGroups();
  const idle = groups.length === 0;
  box.classList.toggle("is-idle", idle);
  $("#sumOpen").hidden = !idle;
  $("#sumBar").hidden = idle;
  if (idle) {
    // Nothing left: point at what finished last.
    const last = orderedGroups().map((g) => ({ g, at: Math.max(...g.jobs.map((j) => j.finishedAt || 0)) })).sort((a, b) => b.at - a.at)[0];
    state.lastFolder = last && last.g.folder;
    $("#sumMoon").innerHTML = moonSvg(1, 24);
    $("#sumLine1").textContent = "Everything is downloaded";
    const size = last ? last.g.jobs.reduce((a, j) => a + (j.total || 0), 0) : 0;
    $("#sumLine2").innerHTML = last
      ? `Last finished: ${esc(last.g.jobs.length === 1 ? last.g.jobs[0].name : last.g.name)}${size ? " · " + bytes(size) : ""}${last.at ? " · " + finishClock(last.at) : ""}`
      : "";
    drawTide();
    return;
  }
  const s = statsOf(groups.flatMap((g) => g.jobs));
  const frac = groupFrac(s);
  $("#sumMoon").innerHTML = moonSvg(frac, 24);

  const left = s.count - s.done - s.failed;
  // Name the release that's in front; the rest is a count.
  const lead = esc(groups[0].jobs.length === 1 ? groups[0].jobs[0].name : groups[0].name);
  const title = groups.length === 1 ? lead : `${lead} <span class="soft">+ ${groups.length - 1} more</span>`;
  // Sizes not known yet count as the average known size: "~40 GB", not "at least 27 GB".
  const sizeTxt = s.bytesTotal
    ? ` <span class="soft">· ${bytes(s.bytesDone)} of ${s.unknownSizes ? "~" : ""}${bytes(expectedBytes(s))} · ${Math.floor(frac * 100)}%</span>`
    : "";
  $("#sumLine1").innerHTML = title + sizeTxt;

  let live;
  if (s.speed > 1 && s.eta) {
    const at = finishClock(Date.now() + s.eta * 1000);
    live = `<b>${rate(s.speed)}</b> · ${humanLeft(s.eta, s.etaEstimated)} · finishes around <span class="clock">${at}</span>`;
  } else if (s.active) {
    live = s.speed > 1 ? `<b>${rate(s.speed)}</b> · working out the time left…` : `Getting download links… ${left} to go`;
  } else if (left === 0) {
    live = s.failed ? `${s.failed} failed. Retry them from the menu.` : "Everything is downloaded.";
  } else if (s.paused && !s.queued) {
    live = `Paused · ${left} file${left === 1 ? "" : "s"} left`;
  } else {
    live = `${left} file${left === 1 ? "" : "s"} waiting`;
  }
  if (s.failed && left) live += ` · <span style="color:var(--danger)">${s.failed} failed</span>`;
  live += ` · ${s.done} of ${s.count} files`;
  $("#sumLine2").innerHTML = live;
  $("#sumBar i").style.width = (frac * 100).toFixed(2) + "%";
  drawTide();
}

function drawTide() {
  const cv = $("#tide");
  if (!cv || cv.offsetParent === null) return;
  const dpr = window.devicePixelRatio || 1;
  const W = 240, H = 64;
  if (cv.width !== W * dpr) { cv.width = W * dpr; cv.height = H * dpr; }
  const ctx = cv.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);
  const pts = state.history.slice(-120);
  const css = getComputedStyle(document.documentElement);
  const accent = css.getPropertyValue("--accent").trim();
  const line = css.getPropertyValue("--line-soft").trim();
  ctx.strokeStyle = line; ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(0, H - 0.5); ctx.lineTo(W, H - 0.5); ctx.stroke();
  const peak = Math.max(...pts, 1);
  if (pts.length < 2 || peak <= 1) { $("#tideCap").textContent = "Speed, last 2 min"; return; }
  const n = 120, step = W / (n - 1), off = n - pts.length;
  const xy = pts.map((v, i) => [(off + i) * step, H - 3 - (v / peak) * (H - 10)]);
  ctx.beginPath();
  ctx.moveTo(xy[0][0], H);
  ctx.lineTo(xy[0][0], xy[0][1]);
  for (let i = 1; i < xy.length; i++) {
    const [x0, y0] = xy[i - 1], [x1, y1] = xy[i];
    const mx = (x0 + x1) / 2;
    ctx.bezierCurveTo(mx, y0, mx, y1, x1, y1);
  }
  ctx.lineTo(xy[xy.length - 1][0], H);
  ctx.closePath();
  const g = ctx.createLinearGradient(0, 0, 0, H);
  g.addColorStop(0, `color-mix(in oklch, ${accent} 30%, transparent)`);
  g.addColorStop(1, `color-mix(in oklch, ${accent} 2%, transparent)`);
  ctx.fillStyle = g; ctx.fill();
  ctx.beginPath();
  ctx.moveTo(xy[0][0], xy[0][1]);
  for (let i = 1; i < xy.length; i++) {
    const [x0, y0] = xy[i - 1], [x1, y1] = xy[i];
    const mx = (x0 + x1) / 2;
    ctx.bezierCurveTo(mx, y0, mx, y1, x1, y1);
  }
  ctx.strokeStyle = accent; ctx.lineWidth = 1.5; ctx.stroke();
  const last = xy[xy.length - 1];
  ctx.fillStyle = accent; ctx.beginPath(); ctx.arc(last[0] - 1.5, last[1], 2.5, 0, Math.PI * 2); ctx.fill();
  $("#tideCap").textContent = `Speed, last 2 min · peak ${rate(peak)}`;
}

function renderCounts() {
  const s = state.stats || {};
  const counts = { all: s.count, active: s.active, queued: s.queued, paused: s.paused, done: s.done, failed: s.failed };
  for (const [k, v] of Object.entries(counts)) {
    const el = $(`[data-count="${k}"]`);
    el.textContent = v || (k === "all" ? 0 : "");
    if (k === "failed") el.classList.toggle("has", v > 0);
  }
  // One button that does the obvious thing.
  const runBtn = $("#runBtn");
  const working = (s.active || 0) + (s.queued || 0) > 0;
  const paused = (s.paused || 0) > 0;
  const mode = working ? "pause" : paused ? "resume" : "idle";
  if (runBtn.dataset.mode !== mode) {
    runBtn.dataset.mode = mode;
    runBtn.innerHTML = mode === "pause"
      ? `<svg><use href="#i-pause"/></svg><span>Pause all</span>`
      : `<svg><use href="#i-play"/></svg><span>Resume all</span>`;
    runBtn.disabled = mode === "idle";
  }
}

function renderBanners(snap) {
  const attn = snap.attention || [];
  $("#attn").hidden = attn.length === 0;
  if (attn.length) {
    const what = attn.length === 1 ? `“${esc(attn[0])}”` : `${attn.length} files`;
    $("#attnText").innerHTML = `<b>Captcha needed for ${what}.</b> Tick “Verify you are human” in the browser window and the download continues.`;
  }
  $("#notice").hidden = !snap.notice;
  if (snap.notice) $("#noticeText").textContent = snap.notice;
  $("#diskFull").hidden = !snap.diskFull;
  if (snap.diskFull) $("#diskText").innerHTML = `<b>The disk is full.</b> ${esc(snap.diskFull)}. Everything is paused and partial files are kept. Free some space, then resume.`;
}

function renderLog(newLines) {
  if (newLines.length) {
    state.log.push(...newLines);
    if (state.log.length > 2000) state.log.splice(0, state.log.length - 2000);
  }
  $('[data-count="log"]').textContent = "";
  if (state.filter !== "log") return;
  const box = $("#log");
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  if (!state.log.length) { box.innerHTML = `<div class="log-empty">Nothing has happened yet.</div>`; return; }
  if (box.dataset.rendered !== String(state.log.length) || !newLines.length) {
    box.innerHTML = state.log.map((l) =>
      `<div class="log-line" data-level="${l.level}"><time>${new Date(l.ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</time><span>${esc(l.msg)}</span></div>`).join("");
    box.dataset.rendered = String(state.log.length);
    if (atBottom) box.scrollTop = box.scrollHeight;
  }
}

// ── polling ────────────────────────────────────────────────────────────────
let polling = false;
async function poll() {
  if (polling) return;
  polling = true;
  try {
    const snap = await invoke("snapshot", { logAfter: state.logSeq });
    state.jobs = snap.jobs;
    state.stats = snap.stats;
    state.history = snap.history;
    state.logSeq = snap.logSeq;
    renderCounts();
    renderList();
    renderSummary();
    renderBanners(snap);
    renderLog(snap.log);
  } catch (e) {
    console.error(e);
  } finally {
    polling = false;
    const busy = state.stats && (state.stats.active || state.stats.queued);
    setTimeout(poll, document.hidden ? 1500 : busy ? 250 : 800);
  }
}

// ── selection & list interaction ───────────────────────────────────────────
function idsFromSelection(fallbackId) {
  if (fallbackId != null && !state.selected.has(fallbackId)) return [fallbackId];
  return [...state.selected];
}

// ── releases (groups) ──────────────────────────────────────────────────────
function groupByKey(key) { return groupsOf(state.jobs).find((g) => g.key === key); }

async function groupAction(act, key) {
  const g = groupByKey(key);
  if (!g) return;
  const ids = g.jobs.map((j) => j.id);
  try {
    if (act === "pause") await invoke("pause", { ids });
    else if (act === "resume") await invoke("resume", { ids: g.jobs.filter((j) => j.state === "paused" || j.state === "failed").map((j) => j.id) });
    else if (act === "folder") await invoke("open_folder", { path: g.folder });
    else if (act === "copy-src") copy((await invoke("source_links", { ids })).join("\n"), "Links copied");
    else if (act === "copy-direct") {
      const l = await invoke("direct_links", { ids });
      l.length ? copy(l.join("\n"), `${l.length} direct link${l.length === 1 ? "" : "s"} copied`) : toast("No direct links yet");
    } else if (act === "remove") await removeJobs(ids, false);
    else if (act === "remove-delete") await removeJobs(ids, true);
    else if (act === "only") {
      // Focus the bandwidth: pause every other release, run this one.
      const others = state.jobs.filter((j) => gkey(j) !== key).map((j) => j.id);
      if (others.length) await invoke("pause", { ids: others });
      await invoke("resume", { ids: g.jobs.filter((j) => j.state === "paused").map((j) => j.id) });
      toast(`Only ${g.name} is downloading now`);
    }
  } catch (e) { toastErr(e); }
  poll();
}

function groupMenu(x, y, key) {
  const g = groupByKey(key);
  if (!g) return;
  const s = statsOf(g.jobs);
  const multi = groupsOf(state.jobs).length > 1;
  openMenu(x, y, [
    (s.active || s.queued) && ["pause", "pause", "Pause release"],
    (s.paused || s.failed) && ["resume", "play", "Resume release"],
    multi && s.done < s.count && ["only", "down", "Download only this release"],
    ["folder", "folder", "Open folder"],
    "-",
    ["copy-src", "copy", "Copy links"],
    ["copy-direct", "copy", "Copy direct links"],
    "-",
    ["remove", "x", "Remove release from list"],
    ["remove-delete", "trash", "Delete from disk…", "danger"],
  ], (act) => groupAction(act, key));
}

function renderReleases() {
  const box = $("#releases");
  const groups = orderedGroups().filter((g) => g.jobs.length > 1);
  $("#relSection").hidden = groups.length < 2;
  if (groups.length < 2) {
    if (state.filter.startsWith("g:") && !groupByKey(state.filter.slice(2))) setFilter("all");
    return;
  }
  // Live releases on top; finished ones below a rule, quieter.
  const firstDone = groups.findIndex(isFinished);
  const html = groups.map((g, i) => {
    const sep = i === firstDone && i > 0 ? `<div class="filters-sep"></div>` : "";
    const s = statsOf(g.jobs);
    const frac = groupFrac(s);
    const pct = s.done === s.count ? "done" : Math.floor(frac * 100) + "%";
    const active = state.filter === "g:" + g.key ? " is-active" : "";
    const paused = groupMode(s) === "resume" ? " is-paused" : "";
    const fin = isFinished(g) ? " is-done" : "";
    return `${sep}<button class="filter release${active}${paused}${fin}" data-filter="g:${esc(g.key)}" title="${esc(g.folder)}">
      <svg class="moon" viewBox="0 0 20 20">${moonSvg(s.done === s.count ? 1 : frac)}</svg><span>${esc(g.name)}</span><b>${pct}</b></button>`;
  }).join("");
  if (box.dataset.html !== html) { box.dataset.html = html; box.innerHTML = html; }
}

$("#releases").addEventListener("click", (e) => {
  const b = e.target.closest("[data-filter]");
  if (b) setFilter(b.dataset.filter);
});
$("#releases").addEventListener("contextmenu", (e) => {
  const b = e.target.closest("[data-filter]");
  if (!b) return;
  e.preventDefault();
  groupMenu(e.clientX, e.clientY, b.dataset.filter.slice(2));
});

$("#list").addEventListener("pointerdown", (e) => {
  const grp = e.target.closest(".group");
  if (grp) {
    if (e.button !== 0 || e.target.closest(".c-act")) return;
    const g = groupByKey(grp.dataset.key);
    if (g) { toggleGroup(g); renderList(); }
    return;
  }
  const fold = e.target.closest(".fold");
  if (fold) {
    if (e.button !== 0) return;
    const k = fold.dataset.key;
    doneOpen.has(k) ? doneOpen.delete(k) : doneOpen.add(k);
    renderList();
    return;
  }
  const row = e.target.closest(".row");
  if (!row || e.target.closest(".c-act")) return;
  const id = +row.dataset.id;
  const vis = visibleJobs().map((j) => j.id);
  if (e.shiftKey && state.anchor != null) {
    const a = vis.indexOf(state.anchor), b = vis.indexOf(id);
    if (!e.ctrlKey) state.selected.clear();
    for (let i = Math.min(a, b); i <= Math.max(a, b); i++) state.selected.add(vis[i]);
  } else if (e.ctrlKey) {
    state.selected.has(id) ? state.selected.delete(id) : state.selected.add(id);
    state.anchor = id;
  } else if (e.button === 0 || !state.selected.has(id)) {
    state.selected.clear(); state.selected.add(id); state.anchor = id;
  }
  renderList();
});

$("#list").addEventListener("dblclick", (e) => {
  const row = e.target.closest(".row");
  if (row) invoke("reveal", { id: +row.dataset.id }).catch(toastErr);
});

$("#list").addEventListener("click", (e) => {
  if (e.target.closest("[data-clear]")) return clearFinished().catch(toastErr).then(poll);
  if (e.target.closest("[data-purge]")) return deleteFromDisk(finishedIds()).catch(toastErr).then(poll);
  const gbtn = e.target.closest("[data-gact]");
  if (gbtn) return groupAction(gbtn.dataset.gact, gbtn.closest(".group").dataset.key);
  const btn = e.target.closest("[data-act]");
  if (!btn) return;
  const id = +btn.closest(".row").dataset.id;
  rowAction(btn.dataset.act, [id]);
});

async function rowAction(act, ids) {
  try {
    if (act === "pause") await invoke("pause", { ids });
    else if (act === "resume") await invoke("resume", { ids });
    else if (act === "reveal") await invoke("reveal", { id: ids[0] });
    else if (act === "remove") await removeJobs(ids, false);
    else if (act === "remove-delete") await removeJobs(ids, true);
    else if (act === "copy-src") copy((await invoke("source_links", { ids })).join("\n"), "Link copied");
    else if (act === "copy-direct") {
      const l = await invoke("direct_links", { ids });
      l.length ? copy(l.join("\n"), l.length === 1 ? "Direct link copied" : `${l.length} direct links copied`) : toast("No direct link yet");
    }
  } catch (e) { toastErr(e); }
  poll();
}

async function removeJobs(ids, deleteFiles) {
  if (deleteFiles) return deleteFromDisk(ids);
  await invoke("remove", { ids, deletePartial: false });
  ids.forEach((id) => state.selected.delete(id));
}

// Finished files go to the Recycle Bin (recoverable); partial downloads are
// deleted outright, since they're useless on their own.
async function deleteFromDisk(ids) {
  if (!ids.length) return toast("Nothing to delete");
  const jobs = state.jobs.filter((j) => ids.includes(j.id));
  const fin = jobs.filter((j) => j.state === "done").length, part = jobs.length - fin;
  const bin = state.platform && state.platform.nativeFrame ? "Trash" : "Recycle Bin";
  const n = (k, w) => `${k} ${w}${k === 1 ? "" : "s"}`;
  const what = [fin && `move ${n(fin, "finished file")} to the ${bin}`, part && `delete ${n(part, "partial download")}`].filter(Boolean).join(" and ");
  if (inApp) {
    const ok = await invoke("confirm", {
      message: `Remove from the list, and ${what}?` + (fin ? ` You can restore finished files from the ${bin}.` : ""),
      okLabel: "Delete",
    });
    if (!ok) return;
  }
  const r = await invoke("delete_files", { ids });
  ids.forEach((id) => state.selected.delete(id));
  const done = [r.trashed && `${n(r.trashed, "file")} moved to the ${bin}`, part && `${n(part, "partial download")} deleted`].filter(Boolean).join(", ");
  toast(r.failed ? `${n(r.failed, "file")} couldn't be moved. Is it open in another program?` : done || "Deleted");
}

$("#list").addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const grp = e.target.closest(".group");
  if (grp) return groupMenu(e.clientX, e.clientY, grp.dataset.key);
  const row = e.target.closest(".row");
  if (!row) return;
  const id = +row.dataset.id;
  if (!state.selected.has(id)) { state.selected.clear(); state.selected.add(id); renderList(); }
  const ids = [...state.selected];
  const jobs = state.jobs.filter((j) => ids.includes(j.id));
  const canPause = jobs.some((j) => ["downloading", "resolving", "queued", "ready"].includes(j.state));
  const canResume = jobs.some((j) => ["paused", "failed"].includes(j.state));
  const many = ids.length > 1 ? ` (${ids.length})` : "";
  openMenu(e.clientX, e.clientY, [
    canPause && ["pause", "pause", "Pause" + many],
    canResume && ["resume", jobs.some((j) => j.state === "failed") ? "retry" : "play", (jobs.some((j) => j.state === "failed") ? "Retry" : "Resume") + many],
    ids.length === 1 && ["reveal", "folder", "Show in folder"],
    "-",
    ["copy-src", "copy", "Copy link" + (ids.length > 1 ? "s" : "")],
    ["copy-direct", "copy", "Copy direct link" + (ids.length > 1 ? "s" : "")],
    "-",
    ["remove", "x", "Remove from list" + many, "", "Del"],
    ["remove-delete", "trash", "Delete from disk…" + many, "danger", "Shift+Del"],
  ], (act) => rowAction(act, ids));
});

// ── menus ──────────────────────────────────────────────────────────────────
function openMenu(x, y, items, onPick) {
  const m = $("#menu");
  m.innerHTML = items.filter(Boolean).map((it) => it === "-" ? "<hr>" :
    `<button role="menuitem" data-act="${it[0]}" class="${it[3] || ""}"><svg><use href="#i-${it[1]}"/></svg><span>${it[2]}</span>${it[4] ? `<kbd>${it[4]}</kbd>` : ""}</button>`).join("");
  m.hidden = false;
  const r = m.getBoundingClientRect();
  m.style.left = Math.min(x, innerWidth - r.width - 8) + "px";
  m.style.top = Math.min(y, innerHeight - r.height - 8) + "px";
  m.onclick = (e) => { const b = e.target.closest("[data-act]"); if (b) { closeMenu(); onPick(b.dataset.act); } };
}
function closeMenu() { $("#menu").hidden = true; }
addEventListener("pointerdown", (e) => { if (!e.target.closest("#menu") && !e.target.closest("#moreBtn")) closeMenu(); });
addEventListener("blur", closeMenu);

$("#moreBtn").addEventListener("click", (e) => {
  if (!$("#menu").hidden) return closeMenu();
  const r = e.currentTarget.getBoundingClientRect();
  const s = state.stats || {};
  openMenu(r.right - 240, r.bottom + 6, [
    ["retry-failed", "retry", "Retry all failed" + (s.failed ? ` (${s.failed})` : "")],
    ["clear-done", "check", "Clear finished releases"],
    ["purge-done", "trash", "Delete finished files…", "danger"],
    "-",
    ["export", "file", "Export direct links…"],
    ["open-out", "folder", "Open download folder"],
  ], async (act) => {
    try {
      if (act === "retry-failed") await invoke("retry_failed");
      if (act === "clear-done") await clearFinished();
      if (act === "purge-done") await deleteFromDisk(finishedIds());
      if (act === "open-out") await invoke("open_folder", { path: state.settings.outDir });
      if (act === "export") await exportLinks();
    } catch (err) { toastErr(err); }
    poll();
  });
});

async function exportLinks() {
  if (!inApp) {
    const links = await invoke("direct_links", { ids: null });
    return links.length ? copy(links.join("\n"), `${links.length} links copied`) : toast("No direct links resolved yet");
  }
  // The save dialog runs in Rust; the page never names the file.
  const n = await invoke("export_links");
  if (n === null) return;
  if (n === 0) return toast("No direct links resolved yet");
  toast(`Saved ${n} direct link${n === 1 ? "" : "s"}`);
}

// ── toolbar & sidebar ──────────────────────────────────────────────────────
const TITLES = { all: "All downloads", active: "Downloading", queued: "Queued", paused: "Paused", done: "Completed", failed: "Failed", log: "Activity" };
function setFilter(f) {
  state.filter = f;
  $$(".filter").forEach((x) => x.classList.toggle("is-active", x.dataset.filter === f));
  const g = f.startsWith("g:") && groupByKey(f.slice(2));
  $("#viewTitle").textContent = g ? g.name : TITLES[f];
  $("#log").dataset.rendered = "";
  renderList(); renderSummary(); renderLog([]);
}
$$(".filters > .filter").forEach((b) => b.addEventListener("click", () => setFilter(b.dataset.filter)));

$("#search").addEventListener("input", (e) => { state.query = e.target.value; renderList(); });
$("#runBtn").addEventListener("click", async () => {
  const mode = $("#runBtn").dataset.mode;
  await invoke(mode === "pause" ? "pause_all" : "resume_all").catch(toastErr);
  poll();
});
$("#noticeDismiss").addEventListener("click", () => invoke("dismiss_notice").then(poll));
$("#showBrowser").addEventListener("click", () => invoke("show_browser").catch(toastErr));
$("#diskOpen").addEventListener("click", () => invoke("open_folder", { path: state.settings.outDir }).catch(toastErr));
$("#diskResume").addEventListener("click", () => invoke("resume_all").then(poll).catch(toastErr));
$("#sumOpen").addEventListener("click", () => invoke("open_folder", { path: state.lastFolder || state.settings.outDir }).catch(toastErr));
$("#openOut").addEventListener("click",() => invoke("open_folder", { path: state.settings.outDir }).catch(toastErr));

// Window controls (undecorated window).
if (inApp) {
  const showMax = (max) => {
    $("#capMax").innerHTML = `<svg><use href="#i-${max ? "wrestore" : "wmax"}"/></svg>`;
    $("#capMax").title = max ? "Restore" : "Maximize";
  };
  const syncMax = () => invoke("win_is_maximized").then(showMax).catch(() => {});
  $("#capMin").onclick = () => invoke("win_minimize");
  $("#capMax").onclick = () => invoke("win_toggle_maximize").then(showMax);
  $("#capClose").onclick = () => invoke("win_close");
  addEventListener("resize", syncMax);
  syncMax();
} else {
  $(".caption").hidden = true;
}

// ── theme ──────────────────────────────────────────────────────────────────
const THEME_ORDER = ["system", "light", "dark"];
const THEME_ICON = { system: "auto", light: "sun", dark: "moon" };
const THEME_LABEL = { system: "Appearance: match system", light: "Appearance: light", dark: "Appearance: dark" };
function applyTheme(t) {
  if (t === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = t;
  $("#themeBtn").innerHTML = `<svg><use href="#i-${THEME_ICON[t]}"/></svg>`;
  $("#themeBtn").title = THEME_LABEL[t];
  drawTide();
}
$("#themeBtn").addEventListener("click", async () => {
  const next = THEME_ORDER[(THEME_ORDER.indexOf(state.settings.theme) + 1) % 3];
  state.settings.theme = next;
  applyTheme(next);
  state.settings = await invoke("set_settings", { settings: state.settings });
  toast(THEME_LABEL[next]);
});
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", drawTide);

// ── sheets ─────────────────────────────────────────────────────────────────
function openSheet(id) {
  const s = $(id);
  s.classList.remove("closing");
  s.hidden = false;
}
function closeSheet(id) {
  const s = $(id);
  if (s.hidden) return;
  s.classList.add("closing");
  setTimeout(() => { s.hidden = true; s.classList.remove("closing"); }, 160);
}
for (const id of ["#addScrim", "#setScrim"]) {
  $(id).addEventListener("pointerdown", (e) => { if (e.target === $(id)) closeSheet(id); });
}

// Add links
let addDir = null;
function openAdd(prefill = "") {
  addDir = state.settings.outDir;
  $("#addDir").textContent = addDir;
  $("#addDir").title = addDir;
  $("#addStart").checked = state.settings.autoStart;
  if (prefill) $("#addText").value = ($("#addText").value.trim() ? $("#addText").value.trim() + "\n" : "") + prefill;
  openSheet("#addScrim");
  updatePreview();
  setTimeout(() => $("#addText").focus(), 30);
}
let previewTimer = 0;
async function updatePreview() {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    const p = await invoke("preview_links", { text: $("#addText").value });
    const files = p.groups.reduce((a, g) => a + g.files, 0);
    $("#addSubmit").disabled = files === 0;
    $("#addSubmit").textContent = files ? `Add ${files} file${files === 1 ? "" : "s"}` : "Add links";
    const sub = state.settings.releaseFolders;
    $("#addDir").textContent = addDir + (sub && p.groups.length === 1 ? "\\" + p.groups[0].name : "");
    if (!p.total) { $("#addPreview").textContent = "Paste links, one per line. Anything else in the text is ignored."; return; }
    const rows = p.groups.map((g) => {
      const hosts = [g.fuckingfast && "fuckingfast", g.datanodes && "datanodes"].filter(Boolean).join(" + ");
      const mirrors = g.links > g.files ? ` · ${g.links - g.files} mirror link${g.links - g.files === 1 ? "" : "s"} as backup` : "";
      return `<li><b>${esc(g.name)}</b><span>${g.files} file${g.files === 1 ? "" : "s"} · ${hosts}${mirrors}</span></li>`;
    });
    $("#addPreview").innerHTML = `<ul class="plan">${rows.join("")}</ul>` +
      (p.unsupported ? `<span class="warn">${p.unsupported} link${p.unsupported === 1 ? "" : "s"} from other sites will be skipped</span>` : "") +
      (sub && p.groups.length > 1 ? `<span>Each release gets its own folder.</span>` : "");
  }, 80);
}
$("#addText").addEventListener("input", updatePreview);
$("#addBtn").addEventListener("click", () => openAdd());
$("#emptyAdd").addEventListener("click", () => openAdd());
$("#addCancel").addEventListener("click", () => closeSheet("#addScrim"));
$("#openTxt").addEventListener("click", async () => {
  if (!inApp) return;
  try {
    const text = await invoke("pick_link_files");
    if (text && text.trim()) openAdd(text);
  } catch (e) { toastErr(e); }
});
$("#addDirBtn").addEventListener("click", async () => {
  if (!inApp) return;
  const dir = await invoke("pick_folder", { current: addDir });
  if (dir) { addDir = dir; $("#addDir").textContent = dir; $("#addDir").title = dir; }
});
$("#addSheet").addEventListener("submit", async (e) => {
  e.preventDefault();
  const r = await invoke("add_links", { text: $("#addText").value, outDir: addDir, start: $("#addStart").checked });
  closeSheet("#addScrim");
  $("#addText").value = "";
  const extra = [r.mirrors && `${r.mirrors} mirror link${r.mirrors === 1 ? "" : "s"}`, r.duplicates && `${r.duplicates} already in the list`, r.unsupported && `${r.unsupported} unsupported`].filter(Boolean).join(", ");
  const what = r.groups.length === 1 ? r.groups[0] + ": " : "";
  if (r.groups.length) {
    state.reveal = r.groups[0];
    // Re-adding to a release that was folded away opens it again.
    for (const g of groupsOf(state.jobs)) if (g.name === r.groups[0]) { collapsed.delete(g.key); opened.add(g.key); }
  }
  toast(`Added ${what}${r.added} file${r.added === 1 ? "" : "s"}` + (extra ? ` · ${extra}` : ""));
  poll();
});

// Settings
const NUM = ["maxActive", "connections", "maxResolving", "retries", "browserPages", "captchaWait", "speedLimitMbs"];
let setDir = null;
function openSettings() {
  const f = $("#setSheet");
  const s = state.settings;
  for (const el of f.elements) {
    if (!el.name || !(el.name in s)) continue;
    if (el.type === "checkbox") el.checked = !!s[el.name];
    else el.value = s[el.name];
  }
  setDir = s.outDir;
  $("#setDir").textContent = setDir; $("#setDir").title = setDir;
  const found = state.platform && state.platform.browser;
  f.elements.browserPath.placeholder = found || "Not found. Install Chrome or Edge";
  $("#verNote").textContent = state.platform ? `BetterMoonDownloader ${state.platform.version}` : "";
  openSheet("#setScrim");
}
$("#openSettings").addEventListener("click", openSettings);
$("#setCancel").addEventListener("click", () => closeSheet("#setScrim"));
$("#setDirBtn").addEventListener("click", async () => {
  if (!inApp) return;
  const dir = await invoke("pick_folder", { current: setDir });
  if (dir) { setDir = dir; $("#setDir").textContent = dir; $("#setDir").title = dir; }
});
$("#setSheet").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = e.currentTarget;
  const next = { ...state.settings, outDir: setDir };
  for (const el of f.elements) {
    if (!el.name || !(el.name in next)) continue;
    if (el.type === "checkbox") next[el.name] = el.checked;
    else if (NUM.includes(el.name)) next[el.name] = Number(el.value) || 0;
    else next[el.name] = el.value;
  }
  state.settings = await invoke("set_settings", { settings: next });
  updateOutLabel();
  closeSheet("#setScrim");
  toast("Settings saved");
});

function updateOutLabel() {
  const p = state.settings.outDir;
  // Parent › folder, so the button reads as a place, not as the app's name.
  const parts = p.split(/[\\/]/).filter(Boolean);
  $("#outLabel").textContent = parts.length > 1 ? parts.slice(-2).join(" › ") : parts[0] || p;
  $("#openOut").title = "Open " + p;
}

// ── keyboard, paste, drop ──────────────────────────────────────────────────
addEventListener("keydown", (e) => {
  const typing = e.target.matches("input, textarea, select");
  if (e.key === "Escape") { closeMenu(); closeSheet("#addScrim"); closeSheet("#setScrim"); return; }
  if (!$("#addScrim").hidden || !$("#setScrim").hidden) {
    if (e.key === "Enter" && e.ctrlKey && !$("#addScrim").hidden && !$("#addSubmit").disabled) $("#addSheet").requestSubmit();
    return;
  }
  if (e.ctrlKey && e.key.toLowerCase() === "n") { e.preventDefault(); openAdd(); return; }
  if (e.ctrlKey && e.key.toLowerCase() === "f") { e.preventDefault(); $("#search").focus(); return; }
  if (typing) return;
  if (e.ctrlKey && e.key.toLowerCase() === "a") { e.preventDefault(); visibleJobs().forEach((j) => state.selected.add(j.id)); renderList(); }
  if (e.key === "Delete" && state.selected.size) rowAction(e.shiftKey ? "remove-delete" : "remove", [...state.selected]);
  if (e.key === " " && state.selected.size) {
    e.preventDefault();
    const ids = [...state.selected];
    const anyRunning = state.jobs.some((j) => ids.includes(j.id) && ["downloading", "resolving", "queued", "ready"].includes(j.state));
    rowAction(anyRunning ? "pause" : "resume", ids);
  }
});

// Pasting links anywhere opens the add sheet with them.
addEventListener("paste", (e) => {
  if (e.target.matches("input, textarea")) return;
  const text = e.clipboardData.getData("text");
  if (/https?:\/\//.test(text)) { e.preventDefault(); openAdd(text); }
});

if (inApp) {
  // Windows' own drop events; the files are read in Rust (link lists only).
  listen("tauri://drag-enter", () => ($("#drop").hidden = false));
  listen("tauri://drag-over", () => ($("#drop").hidden = false));
  listen("tauri://drag-leave", () => ($("#drop").hidden = true));
  listen("tauri://drag-drop", async (ev) => {
    $("#drop").hidden = true;
    const paths = (ev.payload && ev.payload.paths) || [];
    if (!paths.length) return;
    try {
      const text = await invoke("read_dropped_files", { paths });
      if (text.trim()) openAdd(text);
    } catch (e) { toastErr(e); }
  });
}

// ── toast & clipboard ──────────────────────────────────────────────────────
let toastTimer = 0;
function toast(msg) {
  const t = $("#toast");
  t.textContent = msg;
  t.hidden = false;
  t.style.animation = "none"; void t.offsetWidth; t.style.animation = "";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 2600);
}
const toastErr = (e) => toast(String(e && e.message ? e.message : e));
async function copy(text, msg) {
  try { await navigator.clipboard.writeText(text); toast(msg); } catch { toast("Couldn't reach the clipboard"); }
}

// ── boot ───────────────────────────────────────────────────────────────────
(async function boot() {
  state.settings = await invoke("get_settings");
  state.platform = await invoke("platform_info");
  if (state.platform.mica) document.documentElement.classList.add("mica");
  // Linux uses the system title bar, which brings its own window buttons.
  if (state.platform.nativeFrame) $(".caption").hidden = true;
  applyTheme(state.settings.theme);
  updateOutLabel();
  poll();
})();

// ── browser preview only: a fake engine so the page can be designed without
// the desktop shell. Every value here is synthetic. ─────────────────────────
function mockInvoke(cmd, args = {}) {
  const m = (mockInvoke.s ||= (() => {
    // A long FitGirl release mid-way (parts + selective files), an older one
    // that's finished, and a single file.
    const langs = ["english", "french", "german", "japanese"];
    const names = [
      ...Array.from({ length: 24 }, (_, i) => [`ExampleGame_--_fitgirl-repacks.site_--_.part${String(i + 1).padStart(2, "0")}.rar`, "ExampleGame", 2]),
      ...langs.map((l) => [`fg-selective-${l}.bin`, "ExampleGame", 2]),
      ...Array.from({ length: 5 }, (_, i) => [`Starfield.Odyssey.Repack.part${String(i + 1).padStart(2, "0")}.rar`, "Starfield Odyssey Repack", 1]),
      ["Some.Tool.v2.1.zip", "Some Tool v2.1", 3],
    ];
    const t0 = Date.now();
    const stateOf = (i) => i < 14 || (i >= 28 && i < 33) ? "done" : i < 18 ? "downloading" : i === 18 ? "resolving" : i === 33 ? "failed" : i === 19 ? "paused" : "queued";
    const jobs = names.map(([name, group, batch], i) => ({
      group, folder: "C:\\Users\\you\\Downloads\\BetterMoon\\" + group, mirrors: 0,
      id: i + 1, name, url: `https://fuckingfast.co/demo${i}#${name}`, host: i % 4 === 3 ? "datanodes" : "fuckingfast",
      state: stateOf(i),
      total: stateOf(i) === "queued" || i === 33 ? 0 : 1_048_576_000 * (1 + (i % 3) * 0.4), done: 0, speed: 0, eta: null, conns: 4,
      error: i === 33 ? "File not found on datanodes" : null, note: null, hasDirect: stateOf(i) === "done", addedAt: t0 - (3 - batch) * 3600000,
      finishedAt: stateOf(i) === "done" ? t0 - (34 - i) * 120000 : null,
    }));
    // ?idle previews the "everything is downloaded" state.
    if (location.search.includes("idle")) jobs.forEach((j) => { j.state = "done"; j.total ||= 1_048_576_000; j.error = null; j.finishedAt ||= t0; });
    jobs.forEach((j) => { j.done =j.state === "done" ? j.total : j.state === "paused" ? j.total * 0.37 : j.state === "downloading" ? j.total * (0.15 + 0.17 * (j.id % 4)) : 0; });
    return { jobs, history: [], settings: { outDir: "C:\\Users\\you\\Downloads\\BetterMoon", maxActive: 4, connections: 4, maxResolving: 3, retries: 3, browserPages: 3, captchaWait: 240, browserPath: "", dnApiKey: "", proxies: "", mode: "download", stallKiller: true, speedLimitMbs: 0, theme: "system", autoStart: true, releaseFolders: true, builtinBrowser: true } };
  })());
  switch (cmd) {
    case "get_settings": return Promise.resolve({ ...m.settings });
    case "set_settings": m.settings = args.settings; return Promise.resolve(args.settings);
    case "platform_info": return Promise.resolve({ mica: false, nativeFrame: false, version: "1.0.0 (preview)", browser: "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe" });
    case "preview_links": {
      const l = (args.text.match(/https?:\/\/\S+/g) || []);
      const ok = l.filter((x) => /fuckingfast|datanodes/.test(x));
      const files = new Set(ok.map((x) => decodeURIComponent(x.split("#").pop().split("/").pop())));
      return Promise.resolve({ total: l.length, unsupported: l.length - ok.length, groups: ok.length ? [{ name: "ExampleGame", files: files.size, links: ok.length,
        fuckingfast: ok.filter((x) => x.includes("fuckingfast")).length, datanodes: ok.filter((x) => x.includes("datanodes")).length }] : [] });
    }
    case "snapshot": {
      let speed = 0;
      for (const j of m.jobs) {
        if (j.state !== "downloading") { j.speed = 0; j.eta = null; continue; }
        j.speed = 9e6 + Math.sin(Date.now() / 900 + j.id) * 3e6;
        j.done = Math.min(j.total, j.done + j.speed * 0.25 * 8);
        j.eta = (j.total - j.done) / j.speed;
        speed += j.speed;
        if (j.done >= j.total) { j.state = "done"; j.finishedAt = Date.now(); }
      }
      if (!mockInvoke.lastH || Date.now() - mockInvoke.lastH > 1000) { m.history.push(speed); if (m.history.length > 120) m.history.shift(); mockInvoke.lastH = Date.now(); }
      const c = (f) => m.jobs.filter(f).length;
      const known = m.jobs.filter((j) => j.total);
      const bytesTotal = known.reduce((a, j) => a + j.total, 0), bytesDone = known.reduce((a, j) => a + j.done, 0);
      const unknown = c((j) => !j.total && j.state !== "failed");
      const avg = bytesTotal / known.length;
      return Promise.resolve({
        jobs: m.jobs.map((j) => ({ ...j })), history: [...m.history], attention: [], diskFull: null, browserOpen: false, log: [], logSeq: 0,
        stats: { count: m.jobs.length, done: c((j) => j.state === "done"), failed: c((j) => j.state === "failed"), active: c((j) => ["downloading", "resolving"].includes(j.state)),
          queued: c((j) => ["queued", "ready"].includes(j.state)), paused: c((j) => j.state === "paused"), bytesDone, bytesTotal, unknownSizes: unknown, speed,
          eta: speed ? (bytesTotal - bytesDone + unknown * avg) / speed : null, etaEstimated: unknown > 0 },
      });
    }
    case "delete_files": case "remove": {
      const before = m.jobs.length;
      m.jobs = m.jobs.filter((j) => !args.ids.includes(j.id));
      return Promise.resolve(cmd === "remove" ? null : { trashed: before - m.jobs.length, failed: 0 });
    }
    default: return Promise.resolve(cmd === "direct_links" || cmd === "source_links" ? [] : null);
  }
}
