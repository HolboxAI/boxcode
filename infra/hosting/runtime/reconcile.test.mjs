import { test } from "node:test";
import assert from "node:assert/strict";
import { reconcile, summarise, vmName, idFromVmName, VM_NAME_RE } from "./reconcile.mjs";
import { empty, add, TTL_HOURS } from "./registry.mjs";
import { BUILD_SLOT } from "./build.mjs";

const NOW = 1_700_000_000_000;
const HOUR = 3600_000;
const vm = (id, slot, pid = 1000 + slot) => ({ name: vmName(id), slot, pid });
const reg = (...specs) => specs.reduce(
  (r, [id, slot, ttl]) => add(r, { id, slot, runtime: "node", now: NOW, ttlHours: ttl ?? TTL_HOURS }),
  empty(),
);
const ids = (list) => list.map((x) => x.id).sort();

// ---- the four cases ------------------------------------------------------

test("a running VM the registry knows is left alone", () => {
  const p = reconcile({ registry: reg(["aaaa", 0]), running: [vm("aaaa", 0)], now: NOW });
  assert.deepEqual(ids(p.adopt), ["aaaa"]);
  assert.deepEqual(p.start, []);
  assert.deepEqual(p.stop, []);
});

test("a project with nothing running is started", () => {
  const p = reconcile({ registry: reg(["aaaa", 0], ["bbbb", 1]), running: [vm("aaaa", 0)], now: NOW });
  assert.deepEqual(ids(p.start), ["bbbb"]);
  assert.deepEqual(ids(p.adopt), ["aaaa"]);
});

test("a VM nothing claims is stopped, because it is leaked memory", () => {
  // This is how a box slowly fills with 256 MiB allocations nobody can account
  // for, until the eleventh deploy fails for no visible reason.
  const p = reconcile({ registry: reg(["aaaa", 0]), running: [vm("aaaa", 0), vm("zzzz", 5)], now: NOW });
  assert.deepEqual(ids(p.stop), ["zzzz"]);
  assert.match(p.stop[0].why, /no project in the registry claims it/);
});

test("an expired project is reaped whether or not it is running", () => {
  const registry = reg(["aaaa", 0, 1], ["bbbb", 1, 1]);
  const p = reconcile({ registry, running: [vm("aaaa", 0)], now: NOW + 2 * HOUR });
  assert.deepEqual(ids(p.reap), ["aaaa", "bbbb"]);
  // Reaping one that is not running still matters: its image, its nginx route
  // and its database are all still there.
  assert.equal(p.reap.find((r) => r.id === "bbbb").running, false);
  assert.equal(p.reap.find((r) => r.id === "aaaa").running, true);
  assert.deepEqual(p.start, [], "an expired project must never be started");
});

// ---- not ours ------------------------------------------------------------

test("anything not named boxcode-app-* is never touched", () => {
  const p = reconcile({
    registry: reg(["aaaa", 0]),
    running: [
      vm("aaaa", 0),
      { name: "postgres", slot: null, pid: 40 },
      { name: "boxcode-app", slot: 2, pid: 41 },
      { name: "not-boxcode-app-xxxx", slot: 3, pid: 42 },
      { name: "boxcode-app-XXXX", slot: 4, pid: 43 },
      { name: "boxcode-app-xxxx-prod", slot: 5, pid: 44 },
      { name: null, slot: 6, pid: 45 },
    ],
    now: NOW,
  });
  assert.deepEqual(p.stop, [], "nothing unrecognised may be stopped");
  assert.equal(p.ignored.length, 6);
});

test("a build in flight is not mistaken for a leak", () => {
  // It is nobody's project and it is supposed to be there; killing it would
  // fail a deploy that is part-way through.
  const p = reconcile({
    registry: reg(["aaaa", 0]),
    running: [vm("aaaa", 0), vm("bbbb", BUILD_SLOT)],
    now: NOW,
  });
  assert.deepEqual(p.stop, []);
  assert.match(p.ignored[0].why, /build is running/);
});

// ---- the awkward cases ---------------------------------------------------

test("two VMs for one project leaves one running, not zero", () => {
  const p = reconcile({
    registry: reg(["aaaa", 0]),
    running: [vm("aaaa", 0, 100), vm("aaaa", 0, 101)],
    now: NOW,
  });
  assert.equal(p.stop.length, 1, "exactly one duplicate is stopped");
  assert.equal(p.stop[0].pid, 101);
  assert.deepEqual(ids(p.adopt), ["aaaa"], "and the survivor is adopted, not restarted");
  assert.deepEqual(p.start, []);
});

test("a VM on the wrong slot is restarted onto the right one", () => {
  // nginx and the database grant were both built from the slot in the registry,
  // so the running VM is at an address nothing is serving.
  const p = reconcile({ registry: reg(["aaaa", 3]), running: [vm("aaaa", 7)], now: NOW });
  assert.deepEqual(ids(p.stop), ["aaaa"]);
  assert.equal(p.stop[0].slot, 7, "the wrong one is stopped");
  assert.deepEqual(ids(p.start), ["aaaa"]);
  assert.equal(p.start[0].slot, 3, "and it is started on the registry's slot");
  assert.deepEqual(p.adopt, []);
});

test("a VM whose slot is unknown is adopted rather than churned", () => {
  // Slot may be unreadable if the process list did not expose it. Restarting a
  // healthy tenant on a guess is worse than leaving it.
  const p = reconcile({ registry: reg(["aaaa", 3]), running: [{ name: vmName("aaaa"), slot: null, pid: 9 }], now: NOW });
  assert.deepEqual(ids(p.adopt), ["aaaa"]);
  assert.deepEqual(p.stop, []);
});

test("an unreadable clock reaps nothing but still starts what is missing", () => {
  // Reaping is destructive and expiry cannot be judged without a clock. Starting
  // is not destructive, so the platform still comes back.
  const registry = reg(["aaaa", 0, 1]);
  for (const bad of [null, undefined, NaN, "now", 0, -1]) {
    const p = reconcile({ registry, running: [], now: bad });
    assert.deepEqual(p.reap, [], `clock ${JSON.stringify(bad)} must reap nothing`);
    assert.deepEqual(ids(p.start), ["aaaa"]);
    assert.equal(p.clockOk, false);
  }
});

test("an empty box and an empty registry is a no-op", () => {
  const p = reconcile({ registry: empty(), running: [], now: NOW });
  for (const k of ["start", "stop", "adopt", "reap", "ignored"]) assert.deepEqual(p[k], []);
  assert.match(summarise(p), /nothing to do/);
});

test("reconcile does not need running to be passed at all", () => {
  // The very first sweep on a fresh box.
  const p = reconcile({ registry: reg(["aaaa", 0]), now: NOW });
  assert.deepEqual(ids(p.start), ["aaaa"]);
});

// ---- the plan is disjoint ------------------------------------------------

test("no project appears in two conflicting lists", () => {
  const registry = reg(["aaaa", 0], ["bbbb", 1, 1], ["cccc", 2]);
  const p = reconcile({
    registry,
    running: [vm("aaaa", 0), vm("bbbb", 1), vm("dddd", 4)],
    now: NOW + 2 * HOUR,
  });
  const adopted = new Set(ids(p.adopt));
  for (const r of p.reap) assert.ok(!adopted.has(r.id), `${r.id} is both adopted and reaped`);
  for (const s of p.start) assert.ok(!adopted.has(s.id), `${s.id} is both adopted and started`);
  // aaaa and cccc expire later, bbbb has expired, dddd is unclaimed.
  assert.deepEqual(ids(p.reap), ["bbbb"]);
  assert.deepEqual(ids(p.adopt), ["aaaa"]);
  assert.deepEqual(ids(p.start), ["cccc"]);
  assert.deepEqual(ids(p.stop), ["dddd"]);
});

// ---- naming --------------------------------------------------------------

test("the VM name matches what the kill switch looks for", () => {
  assert.match(vmName("k9depef6"), /^boxcode-app-[a-z2-9]{4,16}$/);
  assert.equal(idFromVmName(vmName("k9depef6")), "k9depef6");
});

test("lookalike names do not round-trip", () => {
  for (const n of ["boxcode-app-", "boxcode-app-x", "boxcode-app-xxxx-prod",
                   "xboxcode-app-xxxx", "BOXCODE-APP-XXXX", "", null, 42, {}]) {
    assert.equal(idFromVmName(n), null, JSON.stringify(n));
  }
  assert.ok(VM_NAME_RE.source.startsWith("^"), "must be anchored at the start");
  assert.ok(VM_NAME_RE.source.endsWith("$"), "and at the end");
});

test("the summary is readable at a glance", () => {
  // It runs on every start and every sweep, so the common case must not be a
  // wall of empty arrays.
  const p = reconcile({
    registry: reg(["aaaa", 0], ["bbbb", 1]),
    running: [vm("aaaa", 0), vm("zzzz", 9)],
    now: NOW,
  });
  const s = summarise(p);
  assert.match(s, /start 1/);
  assert.match(s, /stop 1/);
  assert.match(s, /1 already running/);
});
