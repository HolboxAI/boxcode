// The one always-running process on the runner box.
//
// It accepts deploys, drives the pipeline, and keeps what is running in step
// with what should be. Everything it decides lives in ../runtime/ as pure,
// tested modules; this file is orchestration, I/O and the HTTP surface, and is
// deliberately thin because it is the part with no tests.
//
// Shape follows infra/auth/control-plane: node:http, zero npm dependencies, a
// JSON file for state, a systemd unit. That service has been in production long
// enough to have learned what breaks, and none of those lessons argue for more
// machinery.
//
// Two things worth knowing before reading further:
//
//   Deploys are asynchronous. A build takes minutes and CloudFront gives an
//   origin 60 seconds to respond, so POST /deploy accepts the work and returns
//   immediately; the client polls GET /status/<id>. A synchronous deploy would
//   have worked in testing and timed out in production.
//
//   This process does not keep microVMs alive -- the jailer processes do, and
//   they outlive it. Restarting is therefore safe, and reconcile() on boot is
//   what makes it safe rather than merely survivable.

import { createServer } from "node:http";
import { readFile, writeFile, rename, mkdir, rm } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { randomBytes } from "node:crypto";
import { dirname, join } from "node:path";

import * as registryMod from "../runtime/registry.mjs";
import * as gate from "../runtime/gate.mjs";
import { reconcile, summarise } from "../runtime/reconcile.mjs";
import { canFitAnother, describeCapacity } from "../runtime/capacity.mjs";
import * as unpack from "../runtime/unpack.mjs";

const run = promisify(execFile);

const PORT = Number(process.env.PORT || 8085);
const HOST = process.env.HOST || "127.0.0.1";
const REPO = process.env.BOXCODE_HOSTING_DIR || "/opt/boxcode-hosting";
const STATE_DIR = process.env.BOXCODE_STATE_DIR || "/opt/boxcode-hosting/state";
const APPS_DIR = process.env.APPS_DIR || "/opt/boxcode-apps";
const SITE_BASE = process.env.SITE_BASE || "https://boxcode.sh";

const REGISTRY_PATH = join(STATE_DIR, "registry.json");
const GATE_PATH = join(STATE_DIR, "gate.json");

/// Written by lifecycle/kill-switch.sh. While it exists, nothing is started.
///
/// Without this the switch would undo itself: it stops ten VMs, and within
/// fifteen minutes reconciliation sees ten registry entries with nothing
/// running and helpfully starts them all again. Stopping still happens, and so
/// does reaping -- the switch is about not serving, not about not tidying up.
const KILLED_PATH = join(STATE_DIR, "killed");

async function isKilled() {
  try {
    await readFile(KILLED_PATH, "utf8");
    return true;
  } catch {
    return false;
  }
}

const REAP_INTERVAL_MS = Number(process.env.REAP_INTERVAL_MS || 15 * 60_000);

// A deploy holds a slot and the build slot while it runs. Serialised, because
// two concurrent builds each want 1 GiB and the box is sized for ten apps plus
// one build, not ten plus several.
let deployInFlight = false;
const status = new Map(); // id -> { state, reason, at }

// ---------------------------------------------------------------------------
// State, written atomically
// ---------------------------------------------------------------------------

/// Written to a temporary file and renamed, because rename is atomic on the
/// same filesystem and a plain write is not. A power loss mid-write is exactly
/// how the registry ends up truncated -- which registry.parse survives, but not
/// having to is better.
async function saveJson(path, value) {
  await mkdir(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  await writeFile(tmp, value, { mode: 0o600 });
  await rename(tmp, path);
}

async function loadRegistry() {
  let text = "";
  try {
    text = await readFile(REGISTRY_PATH, "utf8");
  } catch {
    // Missing is the normal state of a box that has never hosted anything.
  }
  const { registry, dropped } = registryMod.parse(text);
  for (const d of dropped) {
    // Logged individually. During an incident "why is my project gone" needs an
    // answer that was written down, not one inferred afterwards.
    console.warn(`registry: dropped ${d.id ?? "(whole file)"}: ${d.why}`);
  }
  return registry;
}

async function loadGateState() {
  try {
    const raw = JSON.parse(await readFile(GATE_PATH, "utf8"));
    return {
      owners: raw.owners && typeof raw.owners === "object" ? raw.owners : {},
      history: Array.isArray(raw.history) ? raw.history : [],
      blocked: {
        tokens: Array.isArray(raw.blocked?.tokens) ? raw.blocked.tokens : [],
        sources: Array.isArray(raw.blocked?.sources) ? raw.blocked.sources : [],
      },
    };
  } catch {
    return { owners: {}, history: [], blocked: { tokens: [], sources: [] } };
  }
}

// ---------------------------------------------------------------------------
// The box
// ---------------------------------------------------------------------------

const script = (...a) => join(REPO, ...a);

async function sh(file, args, { timeout = 600_000, env } = {}) {
  return run("/bin/bash", [file, ...args], {
    timeout,
    maxBuffer: 8 * 1024 * 1024,
    ...(env ? { env: { ...process.env, ...env } } : {}),
  });
}

/// What is actually running, in the shape reconcile expects.
async function listRunning() {
  try {
    const { stdout } = await sh(script("lifecycle", "vm.sh"), ["list"], { timeout: 15_000 });
    return stdout.trim().split("\n").filter(Boolean).map((line) => {
      const [name, slot, pid] = line.split(/\s+/);
      return { name, slot: slot === "-" ? null : Number(slot), pid: Number(pid) };
    });
  } catch (e) {
    // Not knowing what is running is not the same as nothing running, and
    // reconcile would read an empty list as "start everything, stop nothing".
    // Better to skip a sweep than to act on a wrong picture.
    console.error(`could not list running VMs: ${e.message}`);
    return null;
  }
}

async function availableMemoryMb() {
  const meminfo = await readFile("/proc/meminfo", "utf8");
  const m = meminfo.match(/^MemAvailable:\s+(\d+) kB/m);
  return m ? Math.floor(Number(m[1]) / 1024) : null;
}

async function freeDiskMb() {
  const { stdout } = await run("/bin/df", ["-Pm", APPS_DIR]);
  const line = stdout.trim().split("\n").pop();
  const cols = line.split(/\s+/);
  return Number(cols[3]);
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

async function sweep() {
  const registry = await loadRegistry();
  const running = await listRunning();
  if (running === null) return; // see listRunning

  const plan = reconcile({ registry, running, now: Date.now() });
  const killed = await isKilled();
  console.log(`reconcile: ${summarise(plan)}${killed ? " -- KILL SWITCH ON, starting nothing" : ""}`);
  for (const i of plan.ignored) console.log(`  ignoring ${i.name}: ${i.why}`);

  for (const s of plan.stop) {
    console.log(`  stopping ${s.id}: ${s.why}`);
    await sh(script("lifecycle", "vm.sh"), ["stop", s.id, String(s.slot ?? 0)]).catch(
      (e) => console.error(`  stop ${s.id} failed: ${e.message}`),
    );
  }
  for (const s of plan.start) {
    if (killed) {
      console.log(`  not starting ${s.id}: the kill switch is on`);
      continue;
    }
    console.log(`  starting ${s.id} on slot ${s.slot}`);
    await sh(script("lifecycle", "vm.sh"), ["start", s.id, String(s.slot)]).catch(
      (e) => console.error(`  start ${s.id} failed: ${e.message}`),
    );
  }

  let next = registry;
  for (const r of plan.reap) {
    console.log(`  reaping ${r.id}`);
    try {
      await sh(script("lifecycle", "vm.sh"), ["stop", r.id, String(r.slot)]);
      await sh(script("lifecycle", "database.sh"), ["drop", r.id]);
      await run("/bin/rm", ["-rf", join(APPS_DIR, r.id)]);
      next = registryMod.remove(next, r.id);
      status.delete(r.id);
    } catch (e) {
      // Left in the registry on purpose: a half-reaped project is reaped again
      // next sweep, which is self-healing. Removing it here would strand
      // whatever was left behind with nothing to find it.
      console.error(`  reap ${r.id} failed, will retry next sweep: ${e.message}`);
    }
  }
  if (next !== registry) await saveJson(REGISTRY_PATH, registryMod.serialise(next));
}

// ---------------------------------------------------------------------------
// The deploy pipeline
// ---------------------------------------------------------------------------

/// Write the uploaded files out.
///
/// This step was missing entirely at first: the request carried the project and
/// the pipeline went straight to provisioning, so every deploy died three
/// stages later with "no such source directory". It passed every unit test,
/// because the tests covered what the client sends and what each stage does
/// with a directory -- and nothing covered the join between them.
///
/// Rewritten from scratch on each deploy rather than merged into. A redeploy
/// that deleted a file should not leave it behind in the image, and merging
/// makes the contents depend on every previous deploy.
async function writeSource(id, files) {
  const root = join(APPS_DIR, id, "src");
  await rm(root, { recursive: true, force: true });
  await mkdir(root, { recursive: true });

  for (const f of files) {
    // resolveUnder throws on anything validate would have caught, so a caller
    // that skipped validation cannot get a usable path out of it.
    const full = unpack.resolveUnder(root, f.path);
    await mkdir(dirname(full), { recursive: true });
    await writeFile(full, Buffer.from(f.content, "base64"));
  }
  // The build VM installs as uid 1000 and the app runs as it.
  await run("/bin/chown", ["-R", "1000:1000", root]);
  return root;
}

async function deploy({ id, slot, runtime, entrypoint, source }) {
  const mark = (state, reason) => {
    status.set(id, { state, reason, at: Date.now() });
    console.log(`${id}: ${state}${reason ? ` -- ${reason}` : ""}`);
  };

  mark("provisioning", "database");
  const { stdout: url } = await sh(script("lifecycle", "database.sh"), ["provision", id, String(slot)]);
  const databaseUrl = url.trim();

  mark("assembling", "root filesystem");
  await sh(script("rootfs", "assemble.sh"), [id, runtime, source, ...entrypoint], {
    timeout: 600_000,
    // Passed in the environment rather than the argv, because argv is visible
    // in `ps` to every process on the box and this string contains the
    // project's database password.
    env: { BOXCODE_APP_ENV: JSON.stringify({ DATABASE_URL: databaseUrl }) },
  });

  mark("building", "installing dependencies");
  await sh(script("rootfs", "install-deps.sh"), [id], { timeout: 900_000 });

  mark("starting", null);
  await sh(script("lifecycle", "vm.sh"), ["start", id, String(slot)]);

  mark("running", null);
}

async function acceptDeploy(body, address) {
  const now = Date.now();
  const registry = await loadRegistry();
  const gateState = await loadGateState();

  const verdict = gate.checkGate({
    id: body.id, token: body.token, address, now,
    state: { ...gateState, registry },
  });
  if (!verdict.allow) return { status: verdict.status, body: { error: verdict.reason } };

  const runtime = body.runtime === "python" ? "python" : body.runtime === "node" ? "node" : null;
  if (!runtime) return { status: 400, body: { error: "runtime must be node or python" } };
  if (!Array.isArray(body.entrypoint) || body.entrypoint.length === 0) {
    return { status: 400, body: { error: "an entrypoint command is required" } };
  }

  // Checked here, on the server, because the client checking the same things
  // is UX -- it fails fast and says something useful -- and the client is not
  // the only thing that can POST to this endpoint.
  const payload = unpack.validate(body.files);
  if (!payload.ok) return { status: 400, body: { error: payload.error } };

  if (await isKilled()) {
    // Accepting one would build an image and then refuse to start it, which
    // reads as a broken deploy rather than a stopped platform.
    return { status: 503, body: { error: "boxcode hosting is temporarily stopped" } };
  }

  if (deployInFlight) {
    // Serialised rather than queued. A queue would hold a connection open for
    // however long the deploy ahead takes, and the client is polling anyway.
    return { status: 503, body: { error: "another deploy is in progress; try again in a minute" } };
  }

  // Capacity is measured, not counted -- ten tiny FastAPI services and ten
  // Next.js servers are the same number and nothing like the same load.
  const [memAvailableMB, diskFreeMB] = await Promise.all([availableMemoryMb(), freeDiskMb()]);
  const existing = registry.projects[body.id];
  if (!existing) {
    const room = canFitAnother({
      memAvailableMB, diskFreeMB,
      running: Object.keys(registry.projects).length,
      expiresAt: Object.values(registry.projects).map((p) => p.expiresAt),
      now,
    });
    if (!room.admit) return { status: 503, body: { error: room.reason } };
  }

  const slot = existing ? existing.slot : registryMod.allocateSlot(registry);
  if (slot === null) {
    const next = registryMod.nextExpiry(registry, now);
    return {
      status: 503,
      body: {
        error: next
          ? `every slot is in use; the next frees in ${Math.ceil((next - now) / 60000)} minutes`
          : "every slot is in use",
      },
    };
  }

  // Written before the work starts, so a crash mid-deploy leaves a registry
  // entry that reconcile will either start or reap -- rather than a running VM
  // nothing claims.
  const nextRegistry = registryMod.add(registry, { id: body.id, slot, runtime, now });
  await saveJson(REGISTRY_PATH, registryMod.serialise(nextRegistry));

  const audit = gate.auditRecord({
    id: body.id, token: body.token, address, now,
    newProject: !existing, outcome: "accepted",
  });
  await saveJson(GATE_PATH, JSON.stringify({
    owners: { ...gateState.owners, [body.id]: audit.tokenHash },
    history: gate.pruneHistory([...gateState.history, audit], now),
    blocked: gateState.blocked,
  }, null, 2));

  // Written before the pipeline starts and after the gate has passed, so an
  // unauthorised request never puts a byte on disk.
  let source;
  try {
    source = await writeSource(body.id, body.files);
    console.log(`${body.id}: wrote ${payload.files} files, ${payload.bytes} bytes`);
  } catch (e) {
    console.error(`${body.id}: could not write source: ${e.message}`);
    return { status: 400, body: { error: `could not unpack the project: ${e.message}` } };
  }

  deployInFlight = true;
  status.set(body.id, { state: "queued", reason: null, at: now });

  // Deliberately not awaited. The response goes back now; the client polls.
  deploy({ id: body.id, slot, runtime, entrypoint: body.entrypoint, source })
    .catch((e) => {
      const detail = (e.stderr || e.message || "").toString().trim().split("\n").slice(-3).join(" ");
      status.set(body.id, { state: "failed", reason: detail || "the deploy failed", at: Date.now() });
      console.error(`${body.id}: deploy failed: ${detail}`);
    })
    .finally(() => { deployInFlight = false; });

  return {
    status: 202,
    body: {
      id: body.id,
      url: `${SITE_BASE}/api/${body.id}/`,
      status_url: `${SITE_BASE}/api/deploy/status/${body.id}`,
      expires_in_hours: registryMod.TTL_HOURS,
      state: "queued",
    },
  };
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

function send(res, code, value) {
  const text = JSON.stringify(value);
  res.writeHead(code, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}

/// 16 MB, not 64 KB.
///
/// The first version capped this at 64 KB, which is right for a request that
/// carries only metadata and wrong for one carrying a project. 8 MB of source
/// is 11 MB of base64, and the cap has to sit above that or every real deploy
/// is refused as "too large" -- a limit that only ever fires on legitimate use.
async function readBody(req, limit = 16 * 1024 * 1024) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    // A deploy request is a few hundred bytes. Anything larger is not one, and
    // buffering it would be the cheapest denial of service on offer.
    if (size > limit) throw new Error("request body too large");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}");
}

const server = createServer(async (req, res) => {
  // nginx is the only thing that reaches this, and it passes the real client
  // address here. Without it every deploy would appear to come from 127.0.0.1
  // and every per-address limit would be a per-box limit.
  const address = (req.headers["x-forwarded-for"] || "").split(",")[0].trim()
    || req.socket.remoteAddress;

  try {
    if (req.method === "GET" && req.url === "/healthz") {
      const registry = await loadRegistry();
      const [memAvailableMB, diskFreeMB] = await Promise.all([availableMemoryMb(), freeDiskMb()]);
      return send(res, 200, {
        ok: true,
        projects: Object.keys(registry.projects).length,
        capacity: describeCapacity({ memAvailableMB, diskFreeMB, running: Object.keys(registry.projects).length }),
      });
    }

    if (req.method === "GET" && req.url?.startsWith("/status/")) {
      const id = req.url.slice("/status/".length);
      if (!gate.ID_RE.test(id)) return send(res, 400, { error: "that is not a valid project id" });
      const s = status.get(id);
      if (s) return send(res, 200, { id, ...s });
      // Not in memory does not mean not deployed: this process may have
      // restarted since. The registry is the durable answer.
      const registry = await loadRegistry();
      if (registry.projects[id]) return send(res, 200, { id, state: "running", reason: null });
      return send(res, 404, { error: `no project ${id}` });
    }

    if (req.method === "POST" && req.url === "/deploy") {
      const body = await readBody(req);
      const out = await acceptDeploy(body, address);
      return send(res, out.status, out.body);
    }

    return send(res, 404, { error: "no such endpoint" });
  } catch (e) {
    console.error(`${req.method} ${req.url}: ${e.stack || e.message}`);
    // Never the exception text: it carries paths, and sometimes the contents of
    // whatever was being parsed.
    return send(res, 500, { error: "the request could not be completed" });
  }
});

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

server.listen(PORT, HOST, async () => {
  console.log(`boxcode hosting control-plane on ${HOST}:${PORT}`);
  // Before serving anything: the box may have VMs from before this process
  // existed, and the registry may describe projects that are not running.
  await sweep().catch((e) => console.error(`initial reconcile failed: ${e.message}`));
  setInterval(() => sweep().catch((e) => console.error(`sweep failed: ${e.message}`)), REAP_INTERVAL_MS);
});

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.on(signal, () => {
    // The VMs are deliberately left running. They are not children of this
    // process, and a restart that killed ten tenants would make every deploy of
    // this service an outage.
    console.log(`${signal}: stopping the control plane, leaving microVMs running`);
    server.close(() => process.exit(0));
  });
}

export { acceptDeploy, sweep };
