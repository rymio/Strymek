// Strymek dashboard: running streams and launchable programs.

const $ = (id) => document.getElementById(id);
let apps = [];
let streams = [];

function toast(msg) {
  const t = $("toast");
  t.textContent = msg;
  t.classList.add("show");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => t.classList.remove("show"), 2600);
}

async function api(path, opts = {}) {
  const res = await fetch(path, {
    ...opts,
    headers: { "x-strymek": "1", ...(opts.body ? { "content-type": "application/json" } : {}), ...(opts.headers || {}) },
    credentials: "same-origin",
  });
  if (res.status === 401) {
    location.href = "/login?next=/";
    throw new Error("login required");
  }
  return res;
}

function el(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids) e.append(k);
  return e;
}

function ago(secs) {
  const d = Math.max(0, Math.floor(Date.now() / 1000 - secs));
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ${Math.floor((d % 3600) / 60)} min`;
  return `${Math.floor(d / 86400)} d`;
}

function viewport() {
  const dpr = window.devicePixelRatio || 1;
  // A new window roughly the size of this screen's usable area.
  const w = Math.round(Math.min(screen.availWidth, 2560) * 0.9 * dpr);
  const h = Math.round(Math.min(screen.availHeight, 1600) * 0.9 * dpr);
  return { w, h, scale: Math.max(1, Math.round(dpr)) };
}

function openStream(slug) {
  const v = viewport();
  const dpr = window.devicePixelRatio || 1;
  const win = window.open(`/${slug}`, `strymek-${slug}`, `popup,width=${Math.round(v.w / dpr)},height=${Math.round(v.h / dpr)}`);
  if (!win) location.href = `/${slug}`;
}

async function launch(app, tile) {
  tile?.classList.add("busy");
  try {
    const v = viewport();
    const res = await api("/api/streams", { method: "POST", body: JSON.stringify({ app: app.id, ...v }) });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
    toast(`Started ${app.name} at /${body.slug}`);
    openStream(body.slug);
    await refreshStreams();
  } catch (e) {
    toast(`Could not start ${app.name}: ${e.message}`);
  } finally {
    tile?.classList.remove("busy");
  }
}

async function stop(s, btn) {
  btn.disabled = true;
  const res = await api(`/api/streams/${encodeURIComponent(s.slug)}`, { method: "DELETE" });
  if (res.ok) toast(`Stopped ${s.name}`);
  await refreshStreams();
}

function renderStreams() {
  const box = $("streams");
  $("run-count").textContent = streams.length;
  $("streams-empty").style.display = streams.length ? "none" : "";
  const now = Date.now();
  box.replaceChildren(
    ...streams.map((s) => {
      const url = `${location.origin}/${s.slug}`;
      const thumb = el("img", { class: "thumb", alt: `${s.name} preview`, src: `/api/streams/${encodeURIComponent(s.slug)}/thumb?t=${now}`, onclick: () => openStream(s.slug) });
      thumb.addEventListener("error", () => thumb.removeAttribute("src"));
      return el(
        "div",
        { class: "stream-card" },
        thumb,
        el(
          "div",
          { class: "meta" },
          el("img", { src: `/api/icon/${encodeURIComponent(s.app_id)}`, alt: "" }),
          el(
            "div",
            { class: "names" },
            el("div", { class: "name" }, s.title || s.name),
            el(
              "div",
              { class: "sub" },
              el("span", { class: s.attached ? "dot on" : "dot", title: s.attached ? "Open in a window" : "Not open anywhere" }),
              ` ${s.kind === "desktop" ? (s.app_id === "desktop-full" ? "Whole screen" : "Live desktop window") : s.real_profile ? `${s.name} (your profile)` : s.name} · ${s.width}×${s.height} · up ${ago(s.started)}`,
            ),
          ),
        ),
        el("div", { class: "url" }, url),
        el(
          "div",
          { class: "actions" },
          el("button", { class: "primary", onclick: () => openStream(s.slug) }, "Open"),
          el("button", { onclick: async () => { await navigator.clipboard.writeText(url); toast("Address copied"); } }, "Copy URL"),
          el("button", { class: "danger", onclick: (e) => stop(s, e.currentTarget), title: s.kind === "desktop" ? "Stop streaming; the window stays open on the desktop" : "Close the app" }, s.kind === "desktop" ? "Stop streaming" : "Stop"),
        ),
      );
    }),
  );
}

function renderApps() {
  const q = $("filter").value.trim().toLowerCase();
  const list = apps.filter((a) => !q || a.name.toLowerCase().includes(q) || a.id.toLowerCase().includes(q) || (a.comment || "").toLowerCase().includes(q));
  $("app-count").textContent = list.length;
  $("apps").replaceChildren(
    ...list.map((a) => {
      const tile = el(
        "button",
        { class: "app-tile", title: a.comment || a.name, type: "button" },
        el("img", { src: `/api/icon/${encodeURIComponent(a.id)}`, alt: "", loading: "lazy" }),
        el("span", { class: "n" }, a.name),
      );
      tile.addEventListener("click", () => launch(a, tile));
      return tile;
    }),
  );
}

async function refreshStreams() {
  try {
    const res = await api("/api/streams");
    streams = await res.json();
    renderStreams();
  } catch (_) {}
}

// ---------- your real desktop ----------

let desk = null;

async function post(path, body) {
  const res = await api(path, { method: "POST", body: body ? JSON.stringify(body) : undefined });
  const out = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(out.error || `HTTP ${res.status}`);
  return out;
}

async function lockAction(action, btn) {
  btn.disabled = true;
  try {
    await post(`/api/desktop/${action}`);
    toast(action === "unlock" ? "Desktop unlocked. It locks again when you stop watching." : "Desktop locked");
  } catch (e) {
    toast(`Could not ${action}: ${e.message}`);
  }
  await refreshDesktop();
}

async function moveHere(a, btn) {
  const what = {
    firefox: "Firefox will close on the desktop and reopen here with your tabs.",
    chromium: "The browser will close on the desktop and reopen here with your tabs.",
    vscode: "VS Code will close on the desktop and reopen here with your workspace and unsaved edits.",
    libreoffice: "LibreOffice will close on the desktop and reopen here; it offers to recover open documents.",
  }[a.family] || `${a.name} will close on the desktop and reopen here.`;
  if (!confirm(`${what}\n\nContinue?`)) return;
  btn.disabled = true;
  btn.textContent = "Moving…";
  try {
    const out = await post("/api/desktop/move", { family: a.family, ...viewport() });
    toast(`${a.name} moved to /${out.slug}`);
    openStream(out.slug);
  } catch (e) {
    toast(`Could not move ${a.name}: ${e.message}`);
  }
  await Promise.all([refreshStreams(), refreshDesktop()]);
}

async function openFullDesktop(btn) {
  if (btn) btn.disabled = true;
  try {
    const out = await post("/api/desktop/stream", { window: 0 });
    openStream(out.slug);
  } catch (e) {
    toast(`Could not open the full desktop: ${e.message}`);
  }
  if (btn) btn.disabled = false;
  await refreshStreams();
}

async function streamWindow(w, btn) {
  btn.disabled = true;
  try {
    const out = await post("/api/desktop/stream", { window: w.id });
    openStream(out.slug);
  } catch (e) {
    toast(`Could not stream it: ${e.message}`);
  }
  btn.disabled = false;
  await refreshStreams();
}

function renderDesktop() {
  const d = desk;
  const status = $("desk-status");
  const s = d.session;
  status.replaceChildren();
  if (!s.kind) {
    status.append("You are not logged in on the workstation's screen. Moving apps and live windows need a desktop login.");
  } else {
    const kind = s.kind === "x11" ? "Xorg session" : s.kind === "wayland" ? "Wayland session" : `${s.kind} session`;
    status.append(el("strong", {}, kind));
    if (s.locked !== null && s.locked !== undefined) {
      status.append(el("span", { class: `badge ${s.locked ? "locked" : "unlocked"}` }, s.locked ? "locked" : "unlocked"));
    }
    status.append(el("span", { class: "spacer" }));
    if (d.live) {
      status.append(el("button", { class: "primary", onclick: (e) => openFullDesktop(e.currentTarget), title: "The whole screen, scaled to your window; type your password on the lock screen here" }, "Full desktop"));
    }
    if (s.id) {
      status.append(
        s.locked
          ? el("button", { onclick: (e) => lockAction("unlock", e.currentTarget), title: "Needed only for live window streaming" }, "Unlock")
          : el("button", { onclick: (e) => lockAction("lock", e.currentTarget) }, "Lock now"),
      );
    }
  }
  const movedBy = Object.fromEntries((d.moved || []).map((m) => [m.family, m.slug]));
  $("desk-apps").replaceChildren(
    ...d.apps.map((a) =>
      el(
        "div",
        { class: "desk-app" },
        el("img", { src: a.app_id ? `/api/icon/${encodeURIComponent(a.app_id)}` : "/assets/app-default.svg", alt: "" }),
        el("div", { class: "names" }, el("div", { class: "name" }, a.name), el("div", { class: "sub" }, "Open on your desktop")),
        el("button", { class: "primary", onclick: (e) => moveHere(a, e.currentTarget), title: "Close it on the desktop and reopen it here with the same tabs / workspace" }, "Move here"),
      ),
    ),
    ...Object.entries(movedBy)
      .filter(([f]) => !d.apps.some((a) => a.family === f))
      .map(([f, slug]) =>
        el(
          "div",
          { class: "desk-app" },
          el("div", { class: "names" }, el("div", { class: "name" }, f), el("div", { class: "sub" }, `Moved here: /${slug}`)),
          el("button", { onclick: () => openStream(slug) }, "Open"),
        ),
      ),
  );
  const now = Date.now();
  $("desk-windows").replaceChildren(
    ...(d.windows || []).map(({ window: w, app_id }) => {
      const thumb = el("img", { class: "thumb", alt: `${w.title} preview`, src: `/api/desktop/thumb/${w.id}?t=${now}` });
      thumb.addEventListener("error", () => thumb.removeAttribute("src"));
      const actions = [el("button", { class: "primary", onclick: (e) => streamWindow(w, e.currentTarget) }, "Stream window")];
      const fam = w.family && d.apps.find((a) => a.family === w.family);
      if (fam) actions.push(el("button", { onclick: (e) => moveHere(fam, e.currentTarget) }, "Move here"));
      return el(
        "div",
        { class: "stream-card" },
        thumb,
        el(
          "div",
          { class: "meta" },
          el("img", { src: app_id ? `/api/icon/${encodeURIComponent(app_id)}` : "/assets/app-default.svg", alt: "" }),
          el("div", { class: "names" }, el("div", { class: "name" }, w.title || w.class), el("div", { class: "sub" }, `${w.class} · ${w.w}×${w.h}${w.minimized ? " · minimised" : ""}`)),
        ),
        el("div", { class: "actions" }, ...actions),
      );
    }),
  );
  $("full-desk").hidden = !d.live;
  $("desk-count").textContent = (d.windows || []).length || d.apps.length;
  let hint = "";
  if (s.kind === "wayland") {
    hint = "Live streaming of any open window needs the \"Ubuntu on Xorg\" session: log out, click the gear at the bottom right of the login screen, choose it, and log in again. \"Move here\" works now.";
  } else if (d.live && s.locked) {
    hint = "Your desktop is locked, so live windows show the lock screen. Open Full desktop and type your password there, or use Unlock. While unlocked, the workstation's monitor shows what you do.";
  } else if (d.live) {
    hint = "While you work in a live window, the workstation's monitor shows it too. Stopping a live stream puts the window back where it was.";
  } else if (d.windows_error) {
    hint = `Could not read desktop windows: ${d.windows_error}`;
  }
  $("desk-hint").textContent = hint;
}

async function refreshDesktop() {
  try {
    const res = await api("/api/desktop");
    desk = await res.json();
    renderDesktop();
  } catch (_) {}
}

async function init() {
  $("host").textContent = location.host;
  $("filter").addEventListener("input", renderApps);
  $("full-desk").addEventListener("click", (e) => openFullDesktop(e.currentTarget));
  const res = await api("/api/apps");
  apps = await res.json();
  renderApps();
  await Promise.all([refreshStreams(), refreshDesktop()]);
  setInterval(() => { if (!document.hidden) refreshStreams(); }, 5000);
  setInterval(() => { if (!document.hidden) refreshDesktop(); }, 10000);
}

init();
