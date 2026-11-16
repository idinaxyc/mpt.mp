const el = (id) => document.getElementById(id);
const audio = el("audio");

let tracks = [];
let playlists = [];
let view = [];          // текущий отфильтрованный список
let index = -1;         // индекс в view
let activePlaylist = null;

function fmt(sec) {
  sec = Math.max(0, Math.floor(sec || 0));
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`
    : `${m}:${String(s).padStart(2, "0")}`;
}

function esc(s) {
  return String(s || "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

async function load() {
  try {
    const res = await fetch("/api/library", { cache: "no-store" });
    const data = await res.json();
    tracks = data.tracks || [];
    playlists = data.playlists || [];
    el("device").textContent = data.name || "mpt.mp";
    renderChips();
    applyFilter();
  } catch (e) {
    el("device").textContent = "нет связи с компьютером";
  }
}

function renderChips() {
  const box = el("chips");
  box.innerHTML = "";
  const all = document.createElement("button");
  all.className = "chip" + (activePlaylist === null ? " active" : "");
  all.textContent = `Все · ${tracks.length}`;
  all.onclick = () => { activePlaylist = null; renderChips(); applyFilter(); };
  box.appendChild(all);

  for (const p of playlists) {
    const b = document.createElement("button");
    b.className = "chip" + (activePlaylist === p.id ? " active" : "");
    b.textContent = `${p.name} · ${(p.track_ids || []).length}`;
    b.onclick = () => { activePlaylist = p.id; renderChips(); applyFilter(); };
    box.appendChild(b);
  }
}

function applyFilter() {
  const q = el("search").value.trim().toLowerCase();
  let base = tracks;
  if (activePlaylist) {
    const p = playlists.find((x) => x.id === activePlaylist);
    const ids = new Set((p && p.track_ids) || []);
    base = tracks.filter((t) => ids.has(t.id));
  }
  view = q
    ? base.filter((t) =>
        (t.title + " " + t.artist + " " + t.album).toLowerCase().includes(q))
    : base;

  renderList();
}

function renderList() {
  const list = el("list");
  list.innerHTML = "";
  el("empty").hidden = view.length > 0;

  const currentId = index >= 0 && view[index] ? view[index].id : null;

  view.forEach((t, i) => {
    const row = document.createElement("div");
    row.className = "track" + (t.id === currentId ? " playing" : "");
    row.innerHTML = `
      <div class="t-cover">♪</div>
      <div class="t-main">
        <div class="t-title">${esc(t.title)}</div>
        <div class="t-sub">${esc(t.artist || "—")}${t.album ? " · " + esc(t.album) : ""}</div>
      </div>
      <div class="t-dur">${fmt((t.duration_ms || 0) / 1000)}</div>`;
    row.onclick = () => playAt(i);
    list.appendChild(row);
  });
}

function playAt(i) {
  if (i < 0 || i >= view.length) return;
  index = i;
  const t = view[i];
  audio.src = `/api/stream/${encodeURIComponent(t.id)}`;
  audio.play().catch(() => {});
  el("now").innerHTML = `<div class="now-title">${esc(t.title)}</div>
    <div class="now-sub">${esc(t.artist || t.album || "")}</div>`;
  const d = el("download");
  d.hidden = false;
  d.href = `/api/stream/${encodeURIComponent(t.id)}`;
  d.setAttribute("download", fileName(t));
  renderList();
}

function fileName(t) {
  const base = (t.path || "").split("/").pop();
  return base || `${t.title}.mp3`;
}

const PLAY_SVG = '<svg viewBox="0 0 24 24" width="21" height="21" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z" /></svg>';
const PAUSE_SVG = '<svg viewBox="0 0 24 24" width="21" height="21" fill="currentColor" aria-hidden="true"><path d="M6.5 5h4v14h-4zM13.5 5h4v14h-4z" /></svg>';

el("play").onclick = () => {
  if (audio.paused) {
    if (!audio.src && view.length) playAt(0);
    else audio.play().catch(() => {});
  } else {
    audio.pause();
  }
};

el("next").onclick = () => playAt(index + 1 >= view.length ? 0 : index + 1);
el("prev").onclick = () => playAt(index <= 0 ? 0 : index - 1);

audio.addEventListener("play", () => {
  el("play").innerHTML = PAUSE_SVG;
  el("play").setAttribute("aria-label", "Пауза");
});
audio.addEventListener("pause", () => {
  el("play").innerHTML = PLAY_SVG;
  el("play").setAttribute("aria-label", "Играть");
});
audio.addEventListener("ended", () => playAt(index + 1 >= view.length ? 0 : index + 1));

audio.addEventListener("loadedmetadata", () => {
  el("dur").textContent = fmt(audio.duration);
  el("seek").max = Math.max(1, Math.floor(audio.duration || 0));
});

audio.addEventListener("timeupdate", () => {
  el("pos").textContent = fmt(audio.currentTime);
  if (!seeking) el("seek").value = Math.floor(audio.currentTime);
});

let seeking = false;
el("seek").addEventListener("input", () => {
  seeking = true;
  el("pos").textContent = fmt(Number(el("seek").value));
});
el("seek").addEventListener("change", () => {
  audio.currentTime = Number(el("seek").value);
  seeking = false;
});

el("search").addEventListener("input", applyFilter);

el("upload").addEventListener("change", async (event) => {
  const files = [...event.target.files];
  if (!files.length) return;
  const status = el("upload-status");
  let done = 0;
  for (const f of files) {
    status.textContent = `Загружаю ${f.name}…`;
    try {
      const res = await fetch(`/api/upload?name=${encodeURIComponent(f.name)}`, {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: f,
      });
      if (res.ok) done++;
    } catch (_) {}
  }
  status.textContent = `Отправлено: ${done} из ${files.length}`;
  event.target.value = "";
  await load();
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

load();
setInterval(load, 30000);
