// boxcode anonymous install/usage telemetry — Cloudflare Worker
//
// One URL, two directions:
//   POST { anon_id, event, version, os, date? }  -> logs one event
//   GET                                          -> public HTML view of everything logged
//
// Storage: Workers KV (a free, simple key-value store). Each event is written
// under its own key (timestamp + random suffix, so concurrent installs never
// collide), then listed back out for the GET view. KV is eventually
// consistent and fine at this scale (10s-100s of installs/day) -- it is not
// built for high write concurrency at real scale, which this isn't.
//
// Setup (no local install, all in the Cloudflare dashboard):
//   1. dash.cloudflare.com -> Workers & Pages -> Create -> Create Worker
//   2. Name it (e.g. "boxcode-telemetry"), deploy the default stub first
//   3. Edit code -> delete the stub -> paste this whole file -> Deploy
//   4. Worker's Settings -> Bindings -> Add binding -> KV Namespace
//        - Create a new namespace (e.g. "TELEMETRY")
//        - Variable name: EVENTS  <- must match the binding name used below
//   5. Copy the worker's URL (https://boxcode-telemetry.<you>.workers.dev)
//        - POST to it from install.sh / the binary
//        - GET it in a browser (or link it in the README) for the public view

export default {
  async fetch(request, env) {
    if (request.method === "POST") {
      return handlePing(request, env);
    }
    if (request.method === "GET") {
      return handleView(env);
    }
    return new Response("Method not allowed", { status: 405 });
  },
};

async function handlePing(request, env) {
  let body;
  try {
    body = await request.json();
  } catch (e) {
    return json({ ok: false, error: "invalid json" }, 400);
  }

  // Only these five fields are ever accepted, whatever else is in the body.
  // No IP, no headers, no user-agent -- matches what install.sh/telemetry.rs
  // actually send, and what was described to the user as "only this."
  const event = {
    anon_id: String(body.anon_id || "").slice(0, 64),
    event: String(body.event || "").slice(0, 32),
    version: String(body.version || "").slice(0, 32),
    os: String(body.os || "").slice(0, 32),
    date: String(body.date || "").slice(0, 16),
    received_at: new Date().toISOString(),
  };

  if (!event.anon_id || !event.event) {
    return json({ ok: false, error: "anon_id and event are required" }, 400);
  }

  // Key sorts chronologically and can never collide across concurrent
  // requests, even from the same anon_id in the same millisecond.
  const key = `${event.received_at}-${crypto.randomUUID().slice(0, 8)}`;
  await env.EVENTS.put(key, JSON.stringify(event));

  return json({ ok: true });
}

async function handleView(env) {
  // KV list() pages at 1000 keys by default; fine for this scale, and the
  // view below only ever needs aggregate counts, not every raw row rendered.
  const list = await env.EVENTS.list();
  const events = await Promise.all(
    list.keys.map(async (k) => JSON.parse(await env.EVENTS.get(k.name)))
  );

  const installs = events.filter((e) => e.event === "install");
  const actives = events.filter((e) => e.event === "active");
  const uniqueDevices = new Set(events.map((e) => e.anon_id));

  const activeByDate = {};
  for (const e of actives) {
    activeByDate[e.date] = (activeByDate[e.date] || new Set());
    activeByDate[e.date].add(e.anon_id);
  }
  const dailyActiveRows = Object.entries(activeByDate)
    .sort((a, b) => (a[0] < b[0] ? 1 : -1))
    .map(([date, ids]) => `<tr><td>${esc(date)}</td><td>${ids.size}</td></tr>`)
    .join("");

  const recentRows = events
    .sort((a, b) => (a.received_at < b.received_at ? 1 : -1))
    .slice(0, 200)
    .map(
      (e) =>
        `<tr><td>${esc(e.received_at)}</td><td>${esc(e.event)}</td><td>${esc(
          e.anon_id.slice(0, 8)
        )}…</td><td>${esc(e.version)}</td><td>${esc(e.os)}</td></tr>`
    )
    .join("");

  const html = `<!doctype html>
<html><head><meta charset="utf-8"><title>boxcode — anonymous usage</title>
<style>
  body { font-family: -apple-system, sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; }
  .stats { display: flex; gap: 2rem; margin-bottom: 2rem; }
  .stat { background: #f5f5f5; padding: 1rem 1.5rem; border-radius: 8px; }
  .stat b { display: block; font-size: 1.8rem; }
  table { border-collapse: collapse; width: 100%; margin-bottom: 2rem; }
  td, th { border-bottom: 1px solid #ddd; padding: 0.4rem 0.6rem; text-align: left; font-size: 0.9rem; }
  h2 { margin-top: 2rem; }
  p.note { color: #666; font-size: 0.85rem; }
</style></head>
<body>
  <h1>boxcode — anonymous usage</h1>
  <p class="note">Anonymous, self-reported install/active counts. No login, no IP, no device identity, no conversation content -- see the README for exactly what's collected.</p>
  <div class="stats">
    <div class="stat"><b>${installs.length}</b>install pings</div>
    <div class="stat"><b>${uniqueDevices.size}</b>distinct anonymous devices seen</div>
    <div class="stat"><b>${actives.length}</b>daily-active pings</div>
  </div>
  <h2>Daily active devices</h2>
  <table><tr><th>Date (UTC)</th><th>Distinct devices</th></tr>${dailyActiveRows || "<tr><td colspan=2>No data yet</td></tr>"}</table>
  <h2>Recent events (last 200)</h2>
  <table><tr><th>Received</th><th>Event</th><th>Device (truncated)</th><th>Version</th><th>OS</th></tr>${recentRows || "<tr><td colspan=5>No data yet</td></tr>"}</table>
</body></html>`;

  return new Response(html, { headers: { "content-type": "text/html;charset=UTF-8" } });
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "content-type": "application/json" },
  });
}
