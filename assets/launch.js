// Opens an app by id: reuses a running instance or starts a new one, then
// switches this window to its stream. Used as the start page of Dock apps.

const app = document.body.dataset.app;
const msg = document.getElementById("msg");
const dpr = window.devicePixelRatio || 1;

async function go() {
  try {
    const res = await fetch("/api/streams", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json", "x-strymek": "1" },
      body: JSON.stringify({
        app,
        w: Math.round(window.innerWidth * dpr),
        h: Math.round(window.innerHeight * dpr),
        scale: Math.max(1, Math.round(dpr)),
        reuse: true,
      }),
    });
    if (res.status === 401) {
      location.href = `/login?next=${encodeURIComponent(location.pathname)}`;
      return;
    }
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
    location.replace(`/${body.slug}`);
  } catch (e) {
    msg.textContent = `Could not start it: ${e.message}`;
  }
}

go();
