// Strymek stream page: draws one app's display and sends input back.
//
// Server → browser
//   binary 0x11: WebP tile  [u8 type][u32 seq][u16 x][u16 y][u16 w][u16 h][u8 fmt][webp…]
//   text JSON:   {t:"f",s}  frame end (draw, then ack) · {t:"cfg",w,h,scale} · {t:"cur",d,x,y,w,h}
//                {t:"title",v} · {t:"clip",v} · {t:"bye",reason}
// Browser → server: JSON {t:"hello"|"rs"|"ack"|"kf"|"opt"|"pm"|"pb"|"wh"|"key"|"txt"|"rel"|"clip", …}

const $ = (id) => document.getElementById(id);
const slug = document.body.dataset.slug;
const appName = document.body.dataset.name;
const canvas = $("screen");
const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

let ws = null;
let cfg = { w: 0, h: 0, scale: 1 };
let retry = 0;
let evicted = false;
let frameChain = Promise.resolve();
let tiles = [];
let lastClip = "";
let pendingCopy = null;
let pendingPaste = false;
let stats = { frames: 0, bytes: 0, since: performance.now(), fps: 0, kbps: 0 };

function loadPref(k, d) {
  try { const v = localStorage.getItem(k); return v === null ? d : v === "1"; } catch (_) { return d; }
}
function savePref(k, v) {
  try { localStorage.setItem(k, v ? "1" : "0"); } catch (_) {}
}
let cmdCtrl = loadPref("strymek.cmdCtrl", true);
// Full-desktop streams: scale the whole screen to the window, or show it 1:1.
let actualSize = loadPref("strymek.actualSize", false);
let lastCursor = null;

const dpr = () => window.devicePixelRatio || 1;
const viewSize = () => ({ w: Math.round(window.innerWidth * dpr()), h: Math.round(window.innerHeight * dpr()), dpr: dpr() });
// Display pixels per CSS pixel: the browser's ratio for private displays, the
// desktop's own scale for live desktop windows.
const ppc = () => cfg.ppc || dpr();

function send(obj) {
  if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
}

// ---------- status UI ----------

let statusTimer = 0;
function status(text, level = "ok", sticky = false) {
  $("status-text").textContent = text;
  $("status-dot").className = "dot " + (level === "ok" ? "on" : level);
  $("status").classList.add("show");
  clearTimeout(statusTimer);
  if (!sticky) statusTimer = setTimeout(() => $("status").classList.remove("show"), 1800);
}

function overlay(title, text, show = true) {
  $("ov-title").textContent = title;
  $("ov-text").textContent = text;
  $("overlay").classList.toggle("show", show);
}

function updateMenuInfo() {
  $("m-cmd").textContent = `Cmd key acts as Ctrl: ${cmdCtrl ? "on" : "off"}`;
  $("m-fit").textContent = actualSize ? "Fit to window" : "Actual size (scroll)";
  $("m-info").textContent = `${slug} · ${cfg.w}×${cfg.h} · ${stats.fps} fps · ${stats.kbps} kbit/s`;
}

setInterval(() => {
  const dt = (performance.now() - stats.since) / 1000;
  stats.fps = Math.round(stats.frames / dt);
  stats.kbps = Math.round((stats.bytes * 8) / 1000 / dt);
  stats.frames = 0; stats.bytes = 0; stats.since = performance.now();
  if ($("menu").classList.contains("open")) updateMenuInfo();
}, 1000);

// ---------- layout ----------

function layout() {
  if (!cfg.w) return;
  const fit = cfg.fit && !actualSize;
  document.body.classList.toggle("scroll", !!(cfg.fit && actualSize));
  let w = cfg.w / ppc(), h = cfg.h / ppc();
  let left = 0, top = 0;
  if (fit) {
    // Whole screen, scaled down (never up) to fit the window, centred.
    const k = Math.min(window.innerWidth / w, window.innerHeight / h, 1);
    w *= k; h *= k;
    left = Math.max(0, (window.innerWidth - w) / 2);
    top = Math.max(0, (window.innerHeight - h) / 2);
  }
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  canvas.style.left = `${left}px`;
  canvas.style.top = `${top}px`;
  if (lastCursor) setCursor(lastCursor);
}

let resizeTimer = 0;
window.addEventListener("resize", () => {
  layout();
  clearTimeout(resizeTimer);
  // The full desktop keeps its own size; only app windows follow the browser.
  if (!cfg.fit) resizeTimer = setTimeout(() => send({ t: "rs", ...viewSize() }), 200);
});

// ---------- connection ----------

function connect() {
  evicted = false;
  overlay("", "", false);
  ws = new WebSocket(`wss://${location.host}/${encodeURIComponent(slug)}/ws`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => {
    retry = 0;
    status("Connected");
    send({ t: "hello", ...viewSize(), cmd_ctrl: cmdCtrl });
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") onText(JSON.parse(ev.data));
    else onBinary(ev.data);
  };
  ws.onclose = () => {
    ws = null;
    tiles = [];
    if (evicted) return;
    status("Reconnecting…", "warn", true);
    checkGone().then((gone) => {
      if (gone) return;
      const delay = Math.min(5000, 400 * 2 ** retry++);
      setTimeout(connect, delay);
    });
  };
}

async function checkGone() {
  try {
    const res = await fetch(`/${encodeURIComponent(slug)}`, { cache: "no-store", credentials: "same-origin" });
    if (res.redirected && new URL(res.url).pathname === "/login") {
      location.href = `/login?next=/${encodeURIComponent(slug)}`;
      return true;
    }
    if (res.status === 404) {
      location.reload();
      return true;
    }
  } catch (_) {
    // Network down: keep retrying.
  }
  return false;
}

function onBinary(buf) {
  const dv = new DataView(buf);
  if (dv.getUint8(0) !== 0x11) return;
  const t = {
    x: dv.getUint16(5, true), y: dv.getUint16(7, true),
    w: dv.getUint16(9, true), h: dv.getUint16(11, true),
  };
  stats.bytes += buf.byteLength;
  t.p = createImageBitmap(new Blob([new Uint8Array(buf, 14)], { type: "image/webp" }));
  tiles.push(t);
}

function onText(m) {
  switch (m.t) {
    case "f": {
      const batch = tiles;
      tiles = [];
      const seq = m.s;
      frameChain = frameChain.then(async () => {
        const bitmaps = await Promise.all(batch.map((t) => t.p.catch(() => null)));
        batch.forEach((t, i) => {
          const b = bitmaps[i];
          if (b) { ctx.drawImage(b, t.x, t.y, t.w, t.h); b.close(); }
        });
        stats.frames++;
        send({ t: "ack", s: seq });
      });
      break;
    }
    case "cfg":
      cfg = m;
      if (canvas.width !== m.w || canvas.height !== m.h) {
        canvas.width = m.w;
        canvas.height = m.h;
      }
      layout();
      if (m.title) document.title = m.title;
      $("m-unlock").hidden = !m.desktop;
      $("m-fit").hidden = !m.fit;
      $("m-fulldesk").hidden = !m.desktop || !!m.fit;
      updateMenuInfo();
      break;
    case "title":
      document.title = m.v || appName;
      break;
    case "cur":
      setCursor(m);
      break;
    case "clip":
      lastClip = m.v;
      if (pendingCopy) { const r = pendingCopy; pendingCopy = null; r(m.v); }
      break;
    case "bye":
      evicted = true;
      overlay("Opened in another window", m.reason || "", true);
      addReconnectButton();
      break;
  }
}

function addReconnectButton() {
  const box = document.querySelector("#overlay .box p:last-child");
  if (box.querySelector("[data-reconnect]")) return;
  const b = document.createElement("button");
  b.className = "primary";
  b.dataset.reconnect = "1";
  b.textContent = "Use it here";
  b.addEventListener("click", () => { b.remove(); connect(); });
  box.prepend(b, " ");
}

function setCursor(m) {
  lastCursor = m;
  const img = new Image();
  img.onload = () => {
    // Size the pointer like the picture it moves over (scaled desktops too).
    const shown = canvas.getBoundingClientRect().width;
    let r = shown > 0 && canvas.width > 0 ? canvas.width / shown : ppc();
    r = Math.min(r, Math.max(1, m.w / 12)); // never smaller than ~12 px
    let url = img.src, hx = m.x, hy = m.y;
    if (Math.abs(r - 1) > 0.01) {
      const c = document.createElement("canvas");
      c.width = Math.max(1, Math.round(m.w / r));
      c.height = Math.max(1, Math.round(m.h / r));
      const cx = c.getContext("2d");
      cx.imageSmoothingQuality = "high";
      cx.drawImage(img, 0, 0, c.width, c.height);
      url = c.toDataURL("image/png");
      hx = Math.round(hx / r); hy = Math.round(hy / r);
    }
    canvas.style.cursor = `url(${url}) ${hx} ${hy}, default`;
  };
  img.src = `data:image/png;base64,${m.d}`;
}

// ---------- pointer ----------

function toDisplay(e) {
  const r = canvas.getBoundingClientRect();
  const sx = canvas.width / (r.width || 1), sy = canvas.height / (r.height || 1);
  return { x: Math.round((e.clientX - r.left) * sx), y: Math.round((e.clientY - r.top) * sy) };
}

let movePos = null;
canvas.addEventListener("pointermove", (e) => {
  const first = movePos === null;
  movePos = toDisplay(e);
  if (first) {
    requestAnimationFrame(() => { if (movePos) send({ t: "pm", ...movePos }); movePos = null; });
  }
});

function xButton(e) {
  if (e.button === 0 && e.ctrlKey && isMac) return 3; // Mac habit: Ctrl-click is a right click
  return { 0: 1, 1: 2, 2: 3, 3: 8, 4: 9 }[e.button] || 0;
}
const heldButtons = new Map();
canvas.addEventListener("pointerdown", (e) => {
  canvas.focus();
  closeMenu();
  canvas.setPointerCapture(e.pointerId);
  const b = xButton(e);
  heldButtons.set(e.button, b);
  send({ t: "pb", b, d: true, ...toDisplay(e) });
  e.preventDefault();
});
canvas.addEventListener("pointerup", (e) => {
  const b = heldButtons.get(e.button) ?? xButton(e);
  heldButtons.delete(e.button);
  send({ t: "pb", b, d: false, ...toDisplay(e) });
  e.preventDefault();
});
canvas.addEventListener("contextmenu", (e) => e.preventDefault());

let wheelV = 0, wheelH = 0;
const STEP = 50;
canvas.addEventListener("wheel", (e) => {
  e.preventDefault();
  const mult = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1;
  wheelV += e.deltaY * mult;
  wheelH += e.deltaX * mult;
  const v = Math.trunc(wheelV / STEP), h = Math.trunc(wheelH / STEP);
  if (v || h) {
    wheelV -= v * STEP; wheelH -= h * STEP;
    send({ t: "wh", v, h, ...toDisplay(e) });
  }
}, { passive: false });

// ---------- keyboard ----------

const MODIFIERS = new Set(["ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight", "OSLeft", "OSRight", "CapsLock"]);
const menuOpen = () => $("menu").classList.contains("open");

// With Ctrl, Alt (Option) or Cmd held, keys go by physical position so shortcuts
// like Alt+F work; otherwise printable keys go by the character they produce.
function key(code, keyName, d, e) {
  const chord = e && (e.ctrlKey || e.altKey || e.metaKey);
  send({ t: "key", code, key: chord ? "" : keyName, d });
}

function startCopyCapture() {
  if (!navigator.clipboard || !window.ClipboardItem) return;
  const p = new Promise((resolve) => {
    pendingCopy = resolve;
    setTimeout(() => { if (pendingCopy === resolve) { pendingCopy = null; resolve(lastClip); } }, 1500);
  });
  try {
    navigator.clipboard
      .write([new ClipboardItem({ "text/plain": p.then((t) => new Blob([t || ""], { type: "text/plain" })) })])
      .catch(() => {});
  } catch (_) {}
}

window.addEventListener("keydown", (e) => {
  if (menuOpen() || $("overlay").classList.contains("show") || e.isComposing) return;
  // Dead keys (Option+E, ´ …) only compose; the next key carries the accented character.
  if (e.key === "Dead") { e.preventDefault(); return; }
  const shortcut = isMac ? e.metaKey : e.ctrlKey;
  if (shortcut && e.code === "KeyV" && !e.shiftKey) {
    // Let the browser fire a paste event so we can read the Mac clipboard.
    pendingPaste = true;
    setTimeout(() => {
      if (pendingPaste) { pendingPaste = false; key("KeyV", "v", true); key("KeyV", "v", false); }
    }, 300);
    return;
  }
  e.preventDefault();
  if (shortcut && (e.code === "KeyC" || e.code === "KeyX")) startCopyCapture();
  if (isMac && e.metaKey && !MODIFIERS.has(e.code)) {
    // macOS does not deliver keyup for keys pressed while Cmd is held.
    key(e.code, e.key, true, e);
    key(e.code, e.key, false, e);
    return;
  }
  key(e.code, e.key, true, e);
}, true);

window.addEventListener("keyup", (e) => {
  if (menuOpen() || e.isComposing) return;
  if (e.code === "KeyV" && pendingPaste) return;
  if (e.key === "Dead") { e.preventDefault(); return; }
  e.preventDefault();
  key(e.code, e.key, false, e);
}, true);

document.addEventListener("paste", (e) => {
  if (!pendingPaste) return;
  pendingPaste = false;
  e.preventDefault();
  const text = e.clipboardData ? e.clipboardData.getData("text/plain") : "";
  if (text) { send({ t: "clip", text }); lastClip = text; }
  key("KeyV", "v", true);
  key("KeyV", "v", false);
});

window.addEventListener("blur", () => send({ t: "rel" }));
document.addEventListener("visibilitychange", () => { if (document.hidden) send({ t: "rel" }); });

// ---------- menu ----------

function closeMenu() { $("menu").classList.remove("open"); }
$("menu-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  updateMenuInfo();
  $("menu").classList.toggle("open");
});
$("m-cmd").addEventListener("click", () => {
  cmdCtrl = !cmdCtrl;
  savePref("strymek.cmdCtrl", cmdCtrl);
  send({ t: "opt", cmd_ctrl: cmdCtrl });
  updateMenuInfo();
});
$("m-paste").addEventListener("click", async () => {
  closeMenu();
  try {
    const text = await navigator.clipboard.readText();
    if (text) send({ t: "txt", text });
  } catch (_) { status("Clipboard not available", "warn"); }
  canvas.focus();
});
$("m-refresh").addEventListener("click", () => { closeMenu(); send({ t: "kf" }); canvas.focus(); });
$("m-fit").addEventListener("click", () => {
  closeMenu();
  actualSize = !actualSize;
  savePref("strymek.actualSize", actualSize);
  layout();
  updateMenuInfo();
  canvas.focus();
});
$("m-fulldesk").addEventListener("click", async () => {
  closeMenu();
  const res = await fetch("/api/desktop/stream", {
    method: "POST", credentials: "same-origin",
    headers: { "content-type": "application/json", "x-strymek": "1" },
    body: JSON.stringify({ window: 0 }),
  });
  const out = await res.json().catch(() => ({}));
  if (res.ok) window.open(`/${out.slug}`, `strymek-${out.slug}`);
  else status(out.error || "Could not open the full desktop", "warn");
});
$("m-unlock").addEventListener("click", async () => {
  closeMenu();
  const res = await fetch("/api/desktop/unlock", { method: "POST", credentials: "same-origin", headers: { "x-strymek": "1" } });
  status(res.ok ? "Desktop unlocked" : "Could not unlock the desktop", res.ok ? "ok" : "warn");
  send({ t: "kf" });
  canvas.focus();
});
$("m-full").addEventListener("click", async () => {
  closeMenu();
  try {
    if (!document.fullscreenElement) {
      await document.documentElement.requestFullscreen();
      if (navigator.keyboard && navigator.keyboard.lock) navigator.keyboard.lock().catch(() => {});
    } else {
      await document.exitFullscreen();
    }
  } catch (_) {}
  canvas.focus();
});
document.addEventListener("click", (e) => { if (!$("menu").contains(e.target)) closeMenu(); });

document.title = appName;
canvas.focus();
connect();
