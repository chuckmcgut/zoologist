// Zoologist dashboard. Plain JavaScript, no build step, no third-party code.
// Helpers (el, svg, niceMax, colour, fmt) and the SSE live indicator follow birdsong's app.js.
"use strict";

const API = "api/v1"; // relative, so the page also works behind a reverse-proxy path prefix
const PAGE = 48;
const TOP_SPECIES = 12;
const CHART_REFRESH_MS = 5000; // at most one chart refresh per 5 s from live updates
const CLIP_POLL_MS = 3000;
const CAMERA_REFRESH_MS = 10000;

const LABELS = [
  { id: "person", name: "Person", icon: "🧍" },
  { id: "vehicle", name: "Vehicle", icon: "🚗" },
  { id: "animal", name: "Animal", icon: "🦌" },
  { id: "motion", name: "Motion", icon: "〰️" },
];
const LABEL = Object.fromEntries(LABELS.map((l) => [l.id, l]));

const state = {
  tz: undefined,
  window: "24h",
  camera: "",
  label: "",
  species: "", // exact common name, "" for none
  wrong: false, // only events marked as wrong
  date: null,
  cameras: [], // from /cameras
  cameraNames: new Map(),
  tiles: new Map(), // event id -> tile element
  events: new Map(), // event id -> event JSON
  nextBeforeId: null,
  viewing: null, // event id shown in the viewer
  clipPoll: null,
  chartTimer: null,
  lastChartRefresh: 0,
  lastDetectorDrops: null,
  health: null,
  live: null, // the open live view
};

const $ = (selector) => document.querySelector(selector);

// ---------- helpers ----------

async function api(path, { method = "GET", body } = {}) {
  const headers = { Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(`${API}/${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      message = (await res.json()).error || message;
    } catch (_) {
      /* not JSON */
    }
    throw new Error(message);
  }
  return res.json();
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children) {
    if (child === null || child === undefined) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function svg(tag, attrs = {}, ...children) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  for (const child of children) node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  return node;
}

function formatter(options) {
  try {
    return new Intl.DateTimeFormat(undefined, { timeZone: state.tz, ...options });
  } catch (_) {
    return new Intl.DateTimeFormat(undefined, options); // unknown time zone: browser's own
  }
}

const fmt = {
  time: (iso) => formatter({ hour: "2-digit", minute: "2-digit" }).format(new Date(iso)),
  dateTime: (iso) => formatter({ month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(new Date(iso)),
  percent: (x) => `${Math.round(x * 100)}%`,
  number: (n) => new Intl.NumberFormat().format(n),
  relative(iso) {
    const seconds = Math.round((Date.now() - new Date(iso).getTime()) / 1000);
    if (seconds < 45) return "just now";
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return `${minutes} min ago`;
    const hours = Math.round(minutes / 60);
    if (hours < 36) return `${hours} h ago`;
    return `${Math.round(hours / 24)} d ago`;
  },
  duration(seconds) {
    if (seconds < 60) return `${Math.max(1, Math.round(seconds))} s`;
    const m = Math.floor(seconds / 60);
    return `${m} min ${Math.round(seconds - m * 60)} s`;
  },
};

function todayInStation() {
  // en-CA formats as YYYY-MM-DD.
  try {
    return new Intl.DateTimeFormat("en-CA", { timeZone: state.tz, year: "numeric", month: "2-digit", day: "2-digit" }).format(new Date());
  } catch (_) {
    return new Date().toISOString().slice(0, 10);
  }
}

function showMessage(container, text, isError = false) {
  container.replaceChildren(el("p", { class: isError ? "empty error" : "empty" }, text));
}

function colour(index) {
  return `var(--c${(index % 8) + 1})`;
}

function capitalise(s) {
  return s ? s[0].toUpperCase() + s.slice(1) : s;
}

function cameraName(id) {
  return state.cameraNames.get(id) || id;
}

/// What an event is called: the species for animals, otherwise the label.
function eventTitle(e) {
  if (e.label === "animal") return e.species ? capitalise(e.species.common_name) : "Unidentified animal";
  return LABEL[e.label]?.name || e.label;
}

function query(params) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== "" && v !== null && v !== undefined) q.set(k, v);
  const s = q.toString();
  return s ? `?${s}` : "";
}

// ---------- status ----------

async function loadHealth() {
  const status = $("#status");
  try {
    const h = await api("health");
    state.health = h;
    let today = 0;
    try {
      const hourly = await api(`stats/hourly${query({ date: todayInStation() })}`);
      today = hourly.hours.reduce((sum, x) => sum + x.person + x.vehicle + x.animal + x.motion, 0);
    } catch (_) {
      /* keep 0 */
    }
    const watching = Math.max(state.cameras.length, h.cameras.length);
    const parts = [`Watching ${watching} camera${watching === 1 ? "" : "s"}`, `${fmt.number(today)} events today`];
    if (h.detector) parts.push(`detector ${Math.round(h.detector.mean_ms)} ms`);
    let level = "ok";
    const down = h.cameras.filter((c) => !["streaming", "ended"].includes(c.detect.state));
    if (down.length > 0 && h.uptime_s > 30) {
      parts.push(`${down.length} camera${down.length === 1 ? "" : "s"} down`);
      level = down.length === h.cameras.length ? "bad" : "warn";
    }
    const drops = h.cameras.reduce((sum, c) => sum + c.drops.detector_jobs, 0);
    if (state.lastDetectorDrops !== null && drops > state.lastDetectorDrops) {
      parts.push("detector falling behind");
      if (level === "ok") level = "warn";
    }
    state.lastDetectorDrops = drops;
    if (!h.cpu.avx2) {
      parts.push("CPU without AVX2 (slow)");
      if (level === "ok") level = "warn";
    }
    if (h.species_problem) {
      parts.push("species model not loaded: animals are not named");
      if (level === "ok") level = "warn";
    }
    status.textContent = parts.join(" · ");
    status.title = h.species_problem || "";
    status.dataset.state = level;
    renderCameraStates();
  } catch (e) {
    status.textContent = `Cannot reach Zoologist: ${e.message}`;
    status.dataset.state = "bad";
  }
}

// ---------- activity and animals (horizontal bars) ----------

function barRow({ name, title, count, max, fill, pressed, onClick, ariaLabel }) {
  return el(
    "button",
    { type: "button", class: "row-button", "aria-pressed": String(pressed), "aria-label": ariaLabel, title, onclick: onClick },
    el("span", { class: "name" }, name),
    el("span", { class: "track", "aria-hidden": "true" },
      el("span", { class: count ? "fill" : "fill zero", style: `width:${max ? (100 * count) / max : 0}%;background:${fill}` })),
    el("span", { class: "count", "aria-hidden": "true" }, fmt.number(count)),
  );
}

async function loadActivity() {
  const container = $("#activity-chart");
  try {
    const data = await api(`stats/labels${query({ window: state.window, camera: state.camera })}`);
    const max = Math.max(...data.items.map((i) => i.count));
    const grid = el("div", { class: "bars", role: "group", "aria-label": `Events per type in the last ${data.window}` });
    for (const item of data.items) {
      const l = LABEL[item.label];
      grid.append(barRow({
        name: `${l.icon} ${l.name}`,
        count: item.count,
        max,
        fill: `var(--${item.label})`,
        pressed: state.label === item.label && !state.species,
        ariaLabel: `${l.name}: ${item.count} events. Show only ${l.name.toLowerCase()} events`,
        onClick: () => setLabelFilter(state.label === item.label ? "" : item.label),
      }));
    }
    container.replaceChildren(grid);
    if (max === 0) container.append(el("p", { class: "muted small" }, `Nothing seen in the last ${data.window}.`));
  } catch (e) {
    showMessage(container, `Could not load: ${e.message}`, true);
  }
}

let speciesData = null;

async function loadSpeciesStats() {
  try {
    speciesData = await api(`stats/species${query({ window: state.window, camera: state.camera })}`);
    renderAnimals();
    renderSpeciesTable();
  } catch (e) {
    showMessage($("#animals-chart"), `Could not load: ${e.message}`, true);
  }
}

function renderAnimals() {
  const container = $("#animals-chart");
  const items = speciesData.items;
  if (items.length === 0) {
    showMessage(container, `No animals in the last ${speciesData.window}.`);
    return;
  }
  const top = items.slice(0, TOP_SPECIES);
  const rest = items.slice(TOP_SPECIES);
  const rows = top.map((s) => ({ name: s.common_name ? capitalise(s.common_name) : "Unidentified animal", sci: s.scientific_name, count: s.count, species: s.common_name }));
  if (rest.length) rows.push({ name: `Other (${rest.length})`, count: rest.reduce((a, s) => a + s.count, 0), species: null, other: true });
  const max = Math.max(...rows.map((r) => r.count));
  const grid = el("div", { class: "bars", role: "group", "aria-label": `Animal events per species in the last ${speciesData.window}` });
  rows.forEach((r, i) => {
    grid.append(barRow({
      name: r.name,
      title: r.sci || undefined,
      count: r.count,
      max,
      fill: r.other ? "var(--c8)" : colour(i),
      pressed: r.species !== null && state.species === r.species,
      ariaLabel: `${r.name}: ${r.count} events${r.species ? `. Show only ${r.name}` : ""}`,
      onClick: () => {
        if (r.species) setSpeciesFilter(state.species === r.species ? "" : r.species);
        else setLabelFilter("animal");
      },
    }));
  });
  container.replaceChildren(grid);
}

// ---------- by hour (stacked columns) ----------

function niceMax(n) {
  // Smallest of 5, 10, 20, 25, 50, 100, 200, 250, 500, ... that is at least n; all divide by 5.
  if (n <= 5) return 5;
  const magnitude = 10 ** Math.floor(Math.log10(n));
  for (const step of [1, 2, 2.5, 5, 10]) {
    const candidate = step * magnitude;
    if (candidate >= n && candidate % 5 === 0) return candidate;
  }
  return 10 * magnitude;
}

async function loadHourly() {
  const container = $("#hourly-chart");
  const legend = $("#hourly-legend");
  try {
    const data = await api(`stats/hourly${query({ date: state.date, camera: state.camera })}`);
    renderHourly(container, legend, data);
  } catch (e) {
    legend.replaceChildren();
    showMessage(container, `Could not load: ${e.message}`, true);
  }
}

function renderHourly(container, legend, data) {
  legend.replaceChildren();
  const totals = data.hours.map((h) => LABELS.reduce((sum, l) => sum + h[l.id], 0));
  if (totals.every((t) => t === 0)) {
    showMessage(container, `No events on ${data.date}.`);
    return;
  }
  // Draw at the container's own width, so the chart is never scaled up and the axis text stays
  // readable on phones. Kept short: it is an overview, not the main thing on the page.
  const W = Math.max(320, container.clientWidth || 960);
  const H = W < 600 ? 150 : 120, left = 30, right = 6, top = 8, bottom = 20;
  const plotW = W - left - right, plotH = H - top - bottom;
  const yMax = niceMax(Math.max(...totals));
  const colW = plotW / 24;
  const y = (v) => top + plotH - (v / yMax) * plotH;

  const chart = svg("svg", { viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": `Events per hour on ${data.date}, stacked by type` });
  // niceMax() returns 5, 10, 20, 25, 50, 100, ... so five intervals always give whole numbers.
  for (let i = 0; i <= 5; i++) {
    const v = (yMax * i) / 5;
    chart.append(
      svg("line", { class: "gridline", x1: left, x2: W - right, y1: y(v), y2: y(v) }),
      svg("text", { class: "axis", x: left - 5, y: y(v) + 4, "text-anchor": "end" }, String(Math.round(v))),
    );
  }
  data.hours.forEach((h, hour) => {
    let base = 0;
    for (const l of LABELS) {
      const v = h[l.id];
      if (v === 0) continue;
      const rect = svg("rect", {
        x: left + hour * colW + (colW > 12 ? 3 : 1),
        y: y(base + v),
        width: Math.max(colW - (colW > 12 ? 6 : 2), 1),
        height: Math.max(y(base) - y(base + v), 1),
        style: `fill:var(--${l.id})`,
        rx: 2,
      });
      rect.append(svg("title", {}, `${String(hour).padStart(2, "0")}:00 ${l.name}: ${v}`));
      chart.append(rect);
      base += v;
    }
    if (hour % 3 === 0) {
      chart.append(svg("text", { class: "axis", x: left + hour * colW + colW / 2, y: H - 8, "text-anchor": "middle" }, String(hour).padStart(2, "0")));
    }
  });
  container.replaceChildren(el("div", { class: "hourly" }, chart));
  for (const l of LABELS) {
    const total = data.hours.reduce((sum, h) => sum + h[l.id], 0);
    legend.append(el("li", {}, el("span", { class: "swatch", style: `background:var(--${l.id})` }), `${l.icon} ${l.name} ${total}`));
  }
}

// ---------- recent events ----------

function matchesFilters(e) {
  if (state.camera && e.camera_id !== state.camera) return false;
  if (state.label && e.label !== state.label) return false;
  if (state.species && e.species?.common_name?.toLowerCase() !== state.species.toLowerCase()) return false;
  if (state.wrong && !e.feedback) return false;
  return true;
}

function eventDuration(e) {
  const end = e.ended_at ? new Date(e.ended_at) : new Date();
  return (end - new Date(e.started_at)) / 1000;
}

function tile(e) {
  const title = eventTitle(e);
  const l = LABEL[e.label];
  const pic = el("div", { class: "pic" });
  if (e.thumb_url || e.snapshot_url) {
    pic.append(el("img", { src: `${API}/events/${e.id}/thumb.jpg`, alt: "", loading: "lazy", width: 320, height: 200 }));
  } else {
    pic.append(el("span", { class: "noimg", "aria-hidden": "true" }, l?.icon || "?"));
  }
  pic.append(el("span", { class: `badge ${e.label}` }, `${l?.icon || ""} ${l?.name || e.label}`));
  if (e.active) pic.append(el("span", { class: "badge live-badge" }, "LIVE"));
  if (e.feedback) pic.append(el("span", { class: "badge wrong-badge" }, "✗ wrong"));
  const when = el("span", { class: "when", "data-iso": e.started_at }, fmt.relative(e.started_at));
  const duration = e.active ? "ongoing" : fmt.duration(eventDuration(e));
  return el(
    "button",
    {
      type: "button",
      class: "tile",
      "data-id": e.id,
      "aria-label": `${title} on ${cameraName(e.camera_id)} at ${fmt.time(e.started_at)}${e.active ? ", happening now" : ""}. Open`,
      onclick: () => openViewer(e.id),
    },
    pic,
    el("span", { class: "body" },
      el("span", { class: "title" }, title),
      el("span", { class: "meta" }, `${cameraName(e.camera_id)} · ${duration}`),
      el("span", { class: "meta" }, `${fmt.time(e.started_at)} · `, when)),
  );
}

/// Adds or replaces the tile of `e`. New live events go to the top; others keep their place.
function putEvent(e, { live = false } = {}) {
  state.events.set(e.id, e);
  if (state.viewing === e.id) renderViewer(e);
  const existing = state.tiles.get(e.id);
  if (!existing && !matchesFilters(e)) return;
  const node = tile(e);
  const grid = $("#events");
  if (existing) {
    existing.replaceWith(node);
  } else if (live) {
    node.classList.add("new");
    grid.prepend(node);
  } else {
    grid.append(node);
  }
  state.tiles.set(e.id, node);
  $("#events-empty").hidden = state.tiles.size > 0;
}

async function loadEvents(reset = true) {
  const more = $("#load-more");
  if (reset) {
    state.tiles.clear();
    state.nextBeforeId = null;
    $("#events").replaceChildren();
  }
  try {
    const page = await api(`events${query({
      limit: PAGE,
      before_id: reset ? null : state.nextBeforeId,
      camera: state.camera,
      label: state.species ? "animal" : state.label,
      species: state.species,
      window: state.window,
      wrong: state.wrong ? "true" : null,
    })}`);
    for (const e of page.items) putEvent(e);
    state.nextBeforeId = page.next_before_id;
    more.hidden = page.items.length < PAGE;
    const empty = $("#events-empty");
    empty.classList.remove("error");
    empty.textContent = state.label || state.species || state.camera || state.wrong
      ? `No events match these filters in the last ${state.window}.`
      : `No events in the last ${state.window}. New ones appear here as they happen.`;
    empty.hidden = state.tiles.size > 0;
  } catch (e) {
    const empty = $("#events-empty");
    empty.hidden = false;
    empty.textContent = `Could not load events: ${e.message}`;
    empty.classList.add("error");
  }
}

function syncChips() {
  for (const b of document.querySelectorAll("#label-chips button[data-label]")) {
    b.setAttribute("aria-pressed", String(b.dataset.label === (state.species ? "animal" : state.label)));
  }
  $("#wrong-chip").setAttribute("aria-pressed", String(state.wrong));
  const chip = $("#species-chip");
  chip.hidden = !state.species;
  chip.textContent = state.species ? capitalise(state.species) : "";
}

function setLabelFilter(label) {
  state.label = label;
  state.species = "";
  syncChips();
  loadEvents();
  loadActivity();
  if (speciesData) renderAnimals();
  $("#events-title").scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function setSpeciesFilter(species) {
  state.species = species;
  state.label = species ? "animal" : state.label;
  syncChips();
  loadEvents();
  loadActivity();
  if (speciesData) renderAnimals();
  $("#events-title").scrollIntoView({ behavior: "smooth", block: "nearest" });
}

// ---------- viewer ----------

function tileOrder() {
  return [...document.querySelectorAll("#events .tile")].map((t) => Number(t.dataset.id));
}

async function openViewer(id) {
  let e = state.events.get(id);
  try {
    e = await api(`events/${id}`); // freshest state (clip may have become ready)
    state.events.set(id, e);
  } catch (_) {
    /* use what we have */
  }
  if (!e) return;
  state.viewing = id;
  renderViewer(e, true);
  const dialog = $("#viewer");
  if (!dialog.open) dialog.showModal();
}

function renderViewer(e, fresh = false) {
  $("#viewer-title").textContent = `${LABEL[e.label]?.icon || ""} ${eventTitle(e)}`;
  const parts = [cameraName(e.camera_id), fmt.dateTime(e.started_at), e.active ? "happening now" : fmt.duration(eventDuration(e)), `score ${fmt.percent(e.top_score)}`];
  $("#viewer-meta").textContent = parts.join(" · ");
  const sp = $("#viewer-species");
  if (e.species) {
    const top = e.species.candidates.slice(0, 3).map(([name, p]) => `${capitalise(name)} ${fmt.percent(p)}`);
    sp.textContent = `${e.species.scientific_name} · ${top.join(", ")}`;
    sp.hidden = false;
  } else {
    sp.hidden = true;
  }

  const video = $("#viewer-video");
  const note = $("#viewer-clip-note");
  clearTimeout(state.clipPoll);
  if (e.clip_url) {
    const src = `${API}/events/${e.id}/clip.mp4`;
    if (fresh || !video.src.endsWith(src)) {
      video.src = src;
      video.play().catch(() => {});
    }
    video.hidden = false;
    note.hidden = true;
  } else {
    video.pause();
    video.removeAttribute("src");
    video.load();
    video.hidden = true;
    note.hidden = false;
    if (e.clip_state === "pending") {
      note.textContent = "Clip is being prepared…";
      state.clipPoll = setTimeout(() => refreshViewer(e.id), CLIP_POLL_MS);
    } else {
      note.textContent = "Clip not available.";
    }
  }
  const img = $("#viewer-snapshot");
  if (e.snapshot_url) {
    img.src = `${API}/events/${e.id}/snapshot.jpg?v=${encodeURIComponent(e.best_bbox ? e.best_bbox.x1 : 0)}`;
    img.alt = `Snapshot of ${eventTitle(e)}`;
    img.hidden = false;
  } else {
    img.hidden = true;
  }
  renderFeedback(e);
  const order = tileOrder();
  const i = order.indexOf(e.id);
  $("#viewer-prev").disabled = i <= 0;
  $("#viewer-next").disabled = i < 0 || i >= order.length - 1;
}

// ---------- marking events wrong, naming animals again ----------

const ACTUAL_TEXT = {
  nothing: "nothing was there",
  person: "it was a person",
  vehicle: "it was a vehicle",
  animal: "it was an animal",
  motion: "it was just motion",
};

function renderFeedback(e) {
  if (state.feedbackFor !== e.id) {
    // Another event: close the form and forget the last message.
    state.feedbackFor = e.id;
    $("#wrong-form").hidden = true;
    $("#viewer-action-status").textContent = "";
  }
  const f = e.feedback;
  const note = $("#viewer-feedback");
  if (f) {
    const actual = f.actual ?? "nothing";
    let text = `✗ Marked wrong: ${ACTUAL_TEXT[actual] || actual}`;
    if (f.species) text += ` (${f.species})`;
    if (f.note) text += `. “${f.note}”`;
    note.textContent = text;
  }
  note.hidden = !f;
  $("#viewer-wrong").hidden = !!f;
  $("#viewer-undo-wrong").hidden = !f;
  $("#viewer-rename").hidden = !(e.label === "animal" && e.clip_url && !e.active);
}

function openWrongForm() {
  const e = state.events.get(state.viewing);
  if (!e) return;
  const form = $("#wrong-form");
  form.reset();
  for (const input of form.querySelectorAll('input[name="actual"]')) {
    // "Animal" stays possible for an animal event: the species may be what is wrong.
    input.disabled = input.value === e.label && e.label !== "animal";
  }
  const preset = e.label === "animal" && e.species ? "animal" : "nothing";
  form.querySelector(`input[name="actual"][value="${preset}"]`).checked = true;
  syncWrongForm();
  form.hidden = false;
  form.querySelector(`input[name="actual"]:checked`).focus();
}

function syncWrongForm() {
  const actual = $("#wrong-form").querySelector('input[name="actual"]:checked')?.value;
  $("#wrong-species-field").hidden = actual !== "animal";
}

async function saveWrong(ev) {
  ev.preventDefault();
  const id = state.viewing;
  const form = new FormData($("#wrong-form"));
  const status = $("#viewer-action-status");
  try {
    const body = { actual: form.get("actual"), note: form.get("note") || null };
    if (body.actual === "animal") body.species = form.get("species") || null;
    const e = await api(`events/${id}/feedback`, { method: "POST", body });
    $("#wrong-form").hidden = true;
    status.textContent = "Saved. Thank you: this helps tune detection.";
    putEvent(e);
  } catch (err) {
    status.textContent = `Could not save: ${err.message}`;
  }
}

async function undoWrong() {
  const id = state.viewing;
  try {
    const e = await api(`events/${id}/feedback`, { method: "DELETE" });
    $("#viewer-action-status").textContent = "";
    putEvent(e);
  } catch (err) {
    $("#viewer-action-status").textContent = `Could not undo: ${err.message}`;
  }
}

async function nameAgain() {
  const id = state.viewing;
  const button = $("#viewer-rename");
  const status = $("#viewer-action-status");
  button.disabled = true;
  status.textContent = "Looking at the clip again… (up to a minute)";
  try {
    const result = await api(`events/${id}/reclassify`, { method: "POST", body: { store: true } });
    const text = {
      named: () => `Named: ${capitalise(result.species.common_name)} ${fmt.percent(result.species.score)}`,
      not_animal: () => `Not an animal: a ${result.label}. Relabelled.`,
      still_unnamed: () => "It never moved and could not be named: now motion.",
      unknown: () => "No confident answer: left as it was.",
      no_animal: () => "No animal found in the clip: left as it was.",
    }[result.outcome];
    status.textContent = text ? text() : "Done.";
    if (state.viewing === id) putEvent(await api(`events/${id}`));
  } catch (err) {
    status.textContent = `Could not name it: ${err.message}`;
  } finally {
    button.disabled = false;
  }
}

async function refreshViewer(id) {
  if (state.viewing !== id) return;
  try {
    const e = await api(`events/${id}`);
    if (state.viewing === id) putEvent(e);
  } catch (_) {
    state.clipPoll = setTimeout(() => refreshViewer(id), CLIP_POLL_MS);
  }
}

function stepViewer(delta) {
  const order = tileOrder();
  const i = order.indexOf(state.viewing);
  const next = order[i + delta];
  if (i >= 0 && next !== undefined) openViewer(next);
}

// ---------- cameras ----------

async function loadCameras() {
  try {
    state.cameras = await api("cameras");
  } catch (e) {
    showMessage($("#cameras"), `Could not load cameras: ${e.message}`, true);
    return;
  }
  state.cameraNames = new Map(state.cameras.map((c) => [c.id, c.name]));
  const select = $("#camera-filter");
  select.replaceChildren(el("option", { value: "" }, "All cameras"), ...state.cameras.map((c) => el("option", { value: c.id }, c.name)));
  select.value = state.camera;
  $("#cameras-count").textContent = state.cameras.length ? `${state.cameras.length} total` : "";
  $("#cameras-empty").hidden = state.cameras.length > 0;
  $("#cameras").replaceChildren(...state.cameras.map(cameraTile));
  refreshCameraCounts();
}

function cameraTile(c) {
  const hub = c.kind === "hub_clips";
  const pic = el("div", { class: "pic" });
  const img = el("img", { alt: `Latest picture from ${c.name}`, width: 320, height: 180, loading: "lazy" });
  img.addEventListener("error", () => {
    img.hidden = true;
    pic.querySelector(".noimg").hidden = false;
  });
  img.addEventListener("load", () => {
    img.hidden = false;
    pic.querySelector(".noimg").hidden = true;
  });
  pic.append(img, el("span", { class: "noimg", hidden: true }, hub ? "No events yet" : "No picture yet"));
  if (!hub) {
    // Stream cameras: the picture opens the live view.
    pic.classList.add("watch");
    pic.setAttribute("role", "button");
    pic.setAttribute("tabindex", "0");
    pic.setAttribute("aria-label", `Watch ${c.name} live`);
    pic.addEventListener("click", () => openLive(c));
    pic.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        openLive(c);
      }
    });
  }
  return el(
    "div",
    { class: "cam", "data-id": c.id, "data-kind": c.kind },
    pic,
    el("div", { class: "body" },
      el("div", { class: "name" },
        el("span", { class: "dot", "data-state": "unknown", role: "img", "aria-label": "state unknown" }),
        c.name,
        hub ? el("span", { class: "battery", title: "Battery camera, imported from the Reolink Hub" }, "🔋") : null),
      el("div", { class: "meta state" }, hub ? "Imported from the Hub" : "…"),
      el("div", { class: "meta counts" }, "")),
  );
}

function renderCameraStates() {
  const h = state.health;
  if (!h) return;
  for (const node of document.querySelectorAll("#cameras .cam")) {
    const id = node.dataset.id;
    const hub = node.dataset.kind === "hub_clips";
    const c = h.cameras.find((x) => x.id === id);
    const dot = node.querySelector(".dot");
    const meta = node.querySelector(".state");
    if (hub) {
      renderHubCamera(node, id, h.hubs || []);
      continue;
    }
    if (!c) {
      dot.dataset.state = "bad";
      dot.setAttribute("aria-label", "not running");
      meta.textContent = "Not running";
      continue;
    }
    const ok = (s) => s === "streaming" || s === "ended";
    const detectOk = ok(c.detect.state);
    const recordOk = ok(c.record.state) || c.record.state === "connecting";
    dot.dataset.state = detectOk && recordOk ? "ok" : detectOk || recordOk ? "warn" : "bad";
    dot.setAttribute("aria-label", `detect ${c.detect.state}, record ${c.record.state}`);
    const parts = [`detect ${c.detect.state}`, `record ${c.record.state}`];
    if (detectOk) parts.push(`${c.detect.analysed_fps.toFixed(1)} fps analysed`);
    meta.textContent = parts.join(" · ");
    meta.title = c.detect.error || c.record.error || "";
  }
}

/// A battery camera's tile: when its newest recording was imported, and the importer's state.
function renderHubCamera(node, id, hubs) {
  const cam = state.cameras.find((c) => c.id === id);
  const hub = hubs.find((x) => x.id === cam?.hub) || hubs[0];
  const dot = node.querySelector(".dot");
  const meta = node.querySelector(".state");
  if (!hub) {
    dot.dataset.state = "unknown";
    meta.textContent = "Imported from the Hub";
    return;
  }
  const last = hub.cameras?.[id];
  dot.dataset.state = hub.state === "ok" ? "ok" : hub.state === "error" ? "bad" : "warn";
  dot.setAttribute("aria-label", `Hub importer ${hub.state}`);
  const parts = [last ? `last recording ${fmt.relative(last)}` : "no recordings imported yet"];
  if (hub.state === "error") parts.push("Hub error");
  if (hub.pending_files > 0) parts.push(`${hub.pending_files} waiting`);
  meta.textContent = parts.join(" · ");
  meta.title = hub.last_error || "";
}

async function refreshCameraPictures() {
  for (const node of document.querySelectorAll("#cameras .cam")) {
    const id = node.dataset.id;
    const img = node.querySelector("img");
    if (node.dataset.kind === "hub_clips") {
      // Battery cameras have no live stream: show their newest event's snapshot.
      try {
        const page = await api(`events${query({ camera: id, limit: 1 })}`);
        const e = page.items[0];
        if (e && e.snapshot_url) img.src = `${API}/events/${e.id}/snapshot.jpg`;
      } catch (_) {
        /* keep the old picture */
      }
    } else {
      img.src = `${API}/cameras/${encodeURIComponent(id)}/latest.jpg?t=${Date.now()}`;
    }
  }
}

async function refreshCameraCounts() {
  const today = todayInStation();
  await Promise.all(state.cameras.map(async (c) => {
    const node = document.querySelector(`#cameras .cam[data-id="${CSS.escape(c.id)}"] .counts`);
    if (!node) return;
    try {
      const data = await api(`stats/hourly${query({ date: today, camera: c.id })}`);
      const counts = LABELS.map((l) => [l, data.hours.reduce((s, h) => s + h[l.id], 0)]).filter(([, n]) => n > 0);
      node.textContent = counts.length ? `Today: ${counts.map(([l, n]) => `${l.icon} ${n}`).join("  ")}` : "Nothing today";
    } catch (_) {
      node.textContent = "";
    }
  }));
}

// ---------- live view ----------

/// Plays a camera's live stream: fragmented MP4 from the server, fed to MediaSource and kept
/// close to the live edge.
async function openLive(camera) {
  closeLive();
  const dialog = $("#live-view");
  const video = $("#live-video");
  const note = $("#live-note");
  const status = $("#live-status");
  $("#live-title").textContent = camera.name;
  status.textContent = "connecting…";
  note.hidden = true;
  video.hidden = false;
  if (!dialog.open) dialog.showModal();

  const controller = new AbortController();
  const session = { controller, url: null, source: null };
  state.live = session;
  const fail = (text) => {
    if (state.live !== session) return;
    video.hidden = true;
    note.hidden = false;
    note.textContent = text;
    status.textContent = "not available";
  };

  let res;
  try {
    res = await fetch(`${API}/cameras/${encodeURIComponent(camera.id)}/live.mp4`, { signal: controller.signal });
  } catch (e) {
    if (e.name !== "AbortError") fail(`Cannot connect: ${e.message}`);
    return;
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      message = (await res.json()).error || message;
    } catch (_) {
      /* not JSON */
    }
    fail(message);
    return;
  }
  const mime = `video/mp4; codecs="${res.headers.get("X-Codec")}"`;
  const MS = window.ManagedMediaSource || window.MediaSource;
  if (!MS || !MS.isTypeSupported(mime)) {
    controller.abort();
    fail("This browser cannot play this live stream.");
    return;
  }
  const source = new MS();
  session.source = source;
  session.url = URL.createObjectURL(source);
  video.disableRemotePlayback = true; // required by ManagedMediaSource (Safari)
  video.src = session.url;
  await new Promise((resolve) => source.addEventListener("sourceopen", resolve, { once: true }));
  if (state.live !== session) return;
  const buffer = source.addSourceBuffer(mime);
  buffer.mode = "segments";
  const queue = [];
  const pump = () => {
    if (buffer.updating || state.live !== session) return;
    // Stay near the live edge and keep the buffer short.
    if (buffer.buffered.length) {
      const end = buffer.buffered.end(buffer.buffered.length - 1);
      if (end - video.currentTime > 1.5) video.currentTime = end - 0.3;
      const start = buffer.buffered.start(0);
      if (video.currentTime - start > 30) {
        buffer.remove(start, video.currentTime - 10);
        return;
      }
    }
    if (queue.length) {
      const total = queue.reduce((n, c) => n + c.length, 0);
      const joined = new Uint8Array(total);
      let offset = 0;
      for (const chunk of queue.splice(0)) {
        joined.set(chunk, offset);
        offset += chunk.length;
      }
      try {
        buffer.appendBuffer(joined);
      } catch (e) {
        fail(`Playback stopped: ${e.message}`);
      }
    }
  };
  buffer.addEventListener("updateend", pump);
  video.play().catch(() => {});

  const reader = res.body.getReader();
  let first = true;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done || state.live !== session) break;
      if (first) {
        first = false;
        status.textContent = "live";
      }
      queue.push(value);
      pump();
    }
  } catch (e) {
    if (e.name === "AbortError") return;
  }
  // The server ended the stream (e.g. the camera reconnected with new settings): start again.
  if (state.live === session && dialog.open) setTimeout(() => state.live === session && openLive(camera), 1000);
}

function closeLive() {
  const session = state.live;
  state.live = null;
  if (!session) return;
  session.controller.abort();
  const video = $("#live-video");
  video.pause();
  video.removeAttribute("src");
  video.load();
  if (session.url) URL.revokeObjectURL(session.url);
}

// ---------- species table ----------

function renderSpeciesTable() {
  const tbody = $("#species tbody");
  const items = speciesData.items;
  tbody.replaceChildren(...items.map((s) => {
    const name = s.common_name ? capitalise(s.common_name) : "Unidentified animal";
    const nameCell = el("td", {}, name);
    if (s.scientific_name && s.scientific_name.toLowerCase() !== s.common_name?.toLowerCase()) nameCell.append(el("span", { class: "sci" }, s.scientific_name));
    return el(
      "tr",
      {},
      nameCell,
      el("td", { class: "num" }, fmt.number(s.count)),
      el("td", {}, fmt.dateTime(s.last_seen), el("span", { class: "when", "data-iso": s.last_seen }, fmt.relative(s.last_seen))),
      el("td", { class: "num" }, fmt.percent(s.best_score)),
      el("td", {}, el("button", { type: "button", class: "play", "aria-label": `Play the best ${name} clip`, onclick: () => openViewer(s.best_event_id) }, "▶")),
    );
  }));
  $("#species-empty").hidden = items.length > 0;
  $("#species-count").textContent = items.length ? `${items.length} total` : "";
}

// ---------- live updates ----------

function setLive(value) {
  const live = $("#live");
  live.dataset.state = value;
  live.textContent = value === "live" ? "live" : value === "offline" ? "reconnecting" : "connecting";
}

function connectStream() {
  if (!("EventSource" in window)) {
    setLive("offline");
    return;
  }
  // The browser reconnects by itself and sends Last-Event-ID, so nothing is missed.
  const stream = new EventSource(`${API}/stream`);
  stream.addEventListener("open", () => setLive("live"));
  stream.addEventListener("error", () => setLive("offline"));
  const handle = (kind) => (event) => {
    try {
      const e = JSON.parse(event.data);
      putEvent(e, { live: kind === "started" && !state.tiles.has(e.id) });
      scheduleChartRefresh();
    } catch (err) {
      console.warn("bad event", err);
    }
  };
  stream.addEventListener("started", handle("started"));
  stream.addEventListener("updated", handle("updated"));
  stream.addEventListener("ended", handle("ended"));
}

function refreshCharts() {
  state.lastChartRefresh = Date.now();
  loadActivity();
  loadSpeciesStats();
  if (state.date === todayInStation()) loadHourly();
  refreshCameraCounts();
}

function scheduleChartRefresh() {
  if (state.chartTimer) return;
  const wait = Math.max(0, state.lastChartRefresh + CHART_REFRESH_MS - Date.now());
  state.chartTimer = setTimeout(() => {
    state.chartTimer = null;
    refreshCharts();
  }, wait);
}

function updateRelativeTimes() {
  for (const node of document.querySelectorAll(".when[data-iso]")) node.textContent = fmt.relative(node.dataset.iso);
}

// ---------- start ----------

async function init() {
  try {
    const cfg = await api("config");
    state.tz = cfg.station.timezone;
  } catch (e) {
    console.warn("config unavailable", e);
  }

  state.date = todayInStation();
  const dateInput = $("#hourly-date");
  dateInput.value = state.date;
  dateInput.max = state.date;
  dateInput.addEventListener("change", () => {
    if (!dateInput.value || dateInput.value > dateInput.max) return;
    state.date = dateInput.value;
    loadHourly();
  });

  for (const button of document.querySelectorAll("#window-buttons button")) {
    button.addEventListener("click", () => {
      state.window = button.dataset.window;
      for (const b of document.querySelectorAll("#window-buttons button")) b.setAttribute("aria-pressed", String(b === button));
      loadActivity();
      loadSpeciesStats();
      loadEvents();
    });
  }
  $("#camera-filter").addEventListener("change", (ev) => {
    state.camera = ev.target.value;
    refreshCharts();
    loadHourly();
    loadEvents();
  });
  for (const b of document.querySelectorAll("#label-chips button[data-label]")) {
    b.addEventListener("click", () => setLabelFilter(b.dataset.label));
  }
  $("#species-chip").addEventListener("click", () => setSpeciesFilter(""));
  $("#wrong-chip").addEventListener("click", () => {
    state.wrong = !state.wrong;
    syncChips();
    loadEvents();
  });
  $("#viewer-wrong").addEventListener("click", openWrongForm);
  $("#viewer-undo-wrong").addEventListener("click", undoWrong);
  $("#viewer-rename").addEventListener("click", nameAgain);
  $("#wrong-form").addEventListener("submit", saveWrong);
  $("#wrong-form").addEventListener("change", syncWrongForm);
  $("#wrong-cancel").addEventListener("click", () => {
    $("#wrong-form").hidden = true;
  });
  $("#load-more").addEventListener("click", () => loadEvents(false));
  let resizeTimer = null;
  let lastWidth = window.innerWidth;
  window.addEventListener("resize", () => {
    if (window.innerWidth === lastWidth) return;
    lastWidth = window.innerWidth;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(loadHourly, 300); // the chart is drawn at its container's width
  });

  const liveDialog = $("#live-view");
  liveDialog.addEventListener("close", closeLive);
  liveDialog.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      liveDialog.close();
    }
  });

  const dialog = $("#viewer");
  dialog.addEventListener("close", () => {
    $("#viewer-video").pause();
    clearTimeout(state.clipPoll);
    state.viewing = null;
  });
  dialog.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      // Browsers close a modal dialog on Esc themselves, but not always when it was opened
      // without a fresh user gesture (e.g. from a live update); make it dependable.
      ev.preventDefault();
      dialog.close();
      return;
    }
    if (ev.target instanceof HTMLVideoElement) return; // arrows seek inside the video
    if (ev.key === "ArrowLeft") stepViewer(-1);
    if (ev.key === "ArrowRight") stepViewer(1);
  });
  $("#viewer-prev").addEventListener("click", () => stepViewer(-1));
  $("#viewer-next").addEventListener("click", () => stepViewer(1));

  await loadCameras();
  await Promise.all([loadHealth(), loadActivity(), loadSpeciesStats(), loadHourly(), loadEvents(), refreshCameraPictures()]);
  state.lastChartRefresh = Date.now();
  connectStream();

  setInterval(loadHealth, 15000);
  setInterval(refreshCameraPictures, CAMERA_REFRESH_MS);
  setInterval(() => {
    updateRelativeTimes();
    const today = todayInStation();
    if (dateInput.max !== today) {
      // Midnight in the station time zone: follow the new day if the old "today" was selected.
      const followToday = state.date === dateInput.max;
      dateInput.max = today;
      if (followToday) {
        state.date = today;
        dateInput.value = today;
      }
    }
    refreshCharts();
  }, 60000);
}

document.addEventListener("DOMContentLoaded", init);
