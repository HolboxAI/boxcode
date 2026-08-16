// boxcode change-request control-plane -- a mailbox, not an editor.
//
// Devs publish an artifact, then check it later (sometimes from a phone,
// hours later) and want to leave a plain-English change request without
// running boxcode themselves. There is no hosted agent here and never will
// be: interpreting "move the button right" and editing real code has to
// happen with the developer's own LLM key, on their own machine, via the
// ordinary boxcode agent loop (see src/requests.rs). All this service does
// is hold the note until then.
//
// Zero npm dependencies, same stance as the auth and db control-planes:
// `node:http`/`node:crypto`/`node:fs` are all this needs.
import { createServer } from "node:http";
import { randomUUID } from "node:crypto";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import path from "node:path";

const STORE_PATH = process.env.STORE_PATH || "/opt/boxcode-requests/requests.json";
const PORT = Number(process.env.PORT || 8082);
// Origin published boxcode pages load the widget from and submit requests
// from -- SITE_BASE in the auth control-plane, kept as its own env var here
// rather than importing that file, since this is a separate process with
// its own deploy story.
const ALLOWED_ORIGIN = process.env.ALLOWED_ORIGIN || "https://boxcode.sh";

// Same shape as the auth/db control-planes' PROJECT_ID_RE: an artifact id
// is how every project is identified everywhere in boxcode.
const PROJECT_ID_RE = /^[a-z2-9]{4,16}$/;
const MAX_TEXT_LENGTH = 4000;

function fail(res, code, message, extraHeaders = {}) {
  res.writeHead(code, { "content-type": "application/json", ...extraHeaders });
  res.end(JSON.stringify({ error: message }));
}

function corsHeaders() {
  return {
    "access-control-allow-origin": ALLOWED_ORIGIN,
    "access-control-allow-methods": "POST, GET, OPTIONS",
    "access-control-allow-headers": "content-type",
  };
}

async function loadStore() {
  try {
    return JSON.parse(await readFile(STORE_PATH, "utf8"));
  } catch {
    return {};
  }
}

async function saveStore(store) {
  await mkdir(path.dirname(STORE_PATH), { recursive: true });
  await writeFile(STORE_PATH, JSON.stringify(store, null, 2), { mode: 0o600 });
  // See the auth control-plane's saveRegistry for why this chmod is needed
  // even though `mode` is also passed above: it only applies when
  // writeFile creates the file, not when it overwrites one that already
  // existed under wider permissions.
  await chmod(STORE_PATH, 0o600);
}

// The widget is generic and dependency-free on purpose: it is not something
// a "enable_change_requests" tool generates per project, it is one static
// file every project's published page can point at with its own
// data-project attribute. See src/tools.rs's LIST_CHANGE_REQUESTS schema
// description for how a developer wires it in with edit_file.
function widgetScript() {
  return `(function () {
  var script = document.currentScript;
  var projectId = script && script.getAttribute("data-project");
  if (!projectId) return;
  var apiBase = new URL(script.src).origin;

  var btn = document.createElement("button");
  btn.textContent = "Request a change";
  btn.setAttribute("aria-label", "Request a change to this page");
  btn.style.cssText =
    "position:fixed;right:16px;bottom:16px;z-index:2147483647;padding:10px 14px;" +
    "border-radius:999px;border:none;background:#111;color:#fff;font:14px system-ui," +
    "sans-serif;box-shadow:0 2px 8px rgba(0,0,0,.25);cursor:pointer;";

  var panel = document.createElement("div");
  panel.style.cssText =
    "position:fixed;right:16px;bottom:64px;z-index:2147483647;width:min(320px,90vw);" +
    "background:#fff;color:#111;border-radius:12px;box-shadow:0 4px 20px rgba(0,0,0,.3);" +
    "padding:12px;display:none;font:14px system-ui,sans-serif;";
  panel.innerHTML =
    '<div style="margin-bottom:8px;font-weight:600;">What should change?</div>' +
    '<textarea rows="3" style="width:100%;box-sizing:border-box;font:inherit;' +
    'padding:8px;border-radius:8px;border:1px solid #ccc;resize:vertical;"' +
    ' placeholder="e.g. move the search button to the right"></textarea>' +
    '<div style="margin-top:8px;display:flex;gap:8px;justify-content:flex-end;">' +
    '<button type="button" data-role="cancel" style="padding:6px 10px;border-radius:8px;' +
    'border:1px solid #ccc;background:#fff;cursor:pointer;">Cancel</button>' +
    '<button type="button" data-role="send" style="padding:6px 10px;border-radius:8px;' +
    'border:none;background:#111;color:#fff;cursor:pointer;">Send</button>' +
    '</div><div data-role="status" style="margin-top:6px;font-size:12px;"></div>';

  document.body.appendChild(btn);
  document.body.appendChild(panel);

  var textarea = panel.querySelector("textarea");
  var status = panel.querySelector('[data-role="status"]');

  btn.addEventListener("click", function () {
    panel.style.display = panel.style.display === "none" ? "block" : "none";
    if (panel.style.display === "block") textarea.focus();
  });
  panel.querySelector('[data-role="cancel"]').addEventListener("click", function () {
    panel.style.display = "none";
  });
  panel.querySelector('[data-role="send"]').addEventListener("click", function () {
    var text = textarea.value.trim();
    if (!text) return;
    status.textContent = "Sending...";
    fetch(apiBase + "/requests", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ project_id: projectId, text: text }),
    })
      .then(function (r) {
        if (!r.ok) throw new Error("request failed");
        return r.json();
      })
      .then(function () {
        status.textContent = "Sent. Thanks!";
        textarea.value = "";
        setTimeout(function () {
          panel.style.display = "none";
          status.textContent = "";
        }, 1500);
      })
      .catch(function () {
        status.textContent = "Could not send that -- try again.";
      });
  });
})();
`;
}

async function submit(projectId, text) {
  const store = await loadStore();
  const id = randomUUID();
  const entry = { id, project_id: projectId, text, status: "pending", created_at: new Date().toISOString() };
  store[id] = entry;
  await saveStore(store);
  return id;
}

async function listPending(projectId, includeAll) {
  const store = await loadStore();
  return Object.values(store)
    .filter((r) => r.project_id === projectId && (includeAll || r.status === "pending"))
    .sort((a, b) => a.created_at.localeCompare(b.created_at))
    .map((r) => ({ id: r.id, text: r.text, created_at: r.created_at }));
}

// Returns "resolved" | "not-found" | "wrong-project".
async function resolveRequest(id, projectId) {
  const store = await loadStore();
  const entry = store[id];
  if (!entry) return "not-found";
  if (entry.project_id !== projectId) return "wrong-project";
  if (entry.status !== "resolved") {
    entry.status = "resolved";
    entry.resolved_at = new Date().toISOString();
    await saveStore(store);
  }
  return "resolved";
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");

  if (req.method === "GET" && url.pathname === "/requests-widget.js") {
    res.writeHead(200, { "content-type": "application/javascript; charset=utf-8" });
    res.end(widgetScript());
    return;
  }

  if (req.method === "OPTIONS" && url.pathname === "/requests") {
    res.writeHead(204, corsHeaders());
    res.end();
    return;
  }

  if (req.method === "POST" && url.pathname === "/requests") {
    let body = "";
    for await (const chunk of req) body += chunk;
    let parsed;
    try {
      parsed = JSON.parse(body || "{}");
    } catch {
      return fail(res, 400, "body is not JSON", corsHeaders());
    }
    const projectId = parsed.project_id;
    const text = typeof parsed.text === "string" ? parsed.text.trim() : "";
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id", corsHeaders());
    }
    if (!text) {
      return fail(res, 400, "text must be a non-empty string", corsHeaders());
    }
    if (text.length > MAX_TEXT_LENGTH) {
      return fail(res, 400, `text must be ${MAX_TEXT_LENGTH} characters or fewer`, corsHeaders());
    }
    const id = await submit(projectId, text);
    res.writeHead(200, { "content-type": "application/json", ...corsHeaders() });
    res.end(JSON.stringify({ ok: true, id }));
    return;
  }

  if (req.method === "GET" && url.pathname === "/requests") {
    const projectId = url.searchParams.get("project_id");
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id");
    }
    const includeAll = url.searchParams.get("status") === "all";
    const requests = await listPending(projectId, includeAll);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(requests));
    return;
  }

  const resolveMatch = req.method === "POST" && url.pathname.match(/^\/requests\/([^/]+)\/resolve$/);
  if (resolveMatch) {
    let body = "";
    for await (const chunk of req) body += chunk;
    let parsed;
    try {
      parsed = JSON.parse(body || "{}");
    } catch {
      return fail(res, 400, "body is not JSON");
    }
    const projectId = parsed.project_id;
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id");
    }
    const outcome = await resolveRequest(resolveMatch[1], projectId);
    if (outcome === "not-found") return fail(res, 404, "no such request");
    // A project id that does not own this request gets the same 404 a
    // nonexistent id would -- not a 403 -- so this endpoint never confirms
    // that a given request id exists for a project the caller does not
    // already know it belongs to.
    if (outcome === "wrong-project") return fail(res, 404, "no such request");
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  fail(res, 404, "no such route");
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode requests control-plane listening on 127.0.0.1:${PORT}`);
});
