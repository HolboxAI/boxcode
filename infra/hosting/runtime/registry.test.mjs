import { test } from "node:test";
import assert from "node:assert/strict";
import {
  TTL_HOURS, GUEST_PORT, REGISTRY_VERSION,
  appSlots, empty, parse, allocateSlot, add, remove, expiredIds, nextExpiry, serialise,
} from "./registry.mjs";
import { SLOT_COUNT } from "./network.mjs";
import { BUILD_SLOT } from "./build.mjs";

const NOW = 1_700_000_000_000;
const HOUR = 3600_000;
const entry = (slot, over = {}) => ({
  slot, runtime: "node", createdAt: NOW, expiresAt: NOW + TTL_HOURS * HOUR, ...over,
});
const withProjects = (projects) => ({ version: REGISTRY_VERSION, projects });

// ---- slots ---------------------------------------------------------------

test("the build slot is never handed to an app", () => {
  // An app parked there would be evicted by the next deploy, and the build's
  // namespace and NAT are not what an app should be sitting in.
  assert.ok(!appSlots().includes(BUILD_SLOT));
  assert.equal(appSlots().length, SLOT_COUNT - 1);
});

// Project ids are [a-z2-9]: 0 and 1 are excluded because they are confusable
// with o and l. An earlier version of these tests generated ids with digits and
// failed against that rule rather than against anything in the code.
// One letter per slot, so ids stay unique across the whole range. A two-digit
// modulo scheme was tried first and collided at n and n+10, which showed up as
// a "full box" test that never filled the box.
const idFor = (n) => `proj${"abcdefghijkmnpqr"[n]}${"zyxwvutsrqpnmkjh"[n]}`;

test("ten projects fit, and the eleventh does not need the build slot", () => {
  let r = empty();
  for (let i = 0; i < 10; i++) {
    const s = allocateSlot(r);
    assert.notEqual(s, null, `project ${i} should get a slot`);
    r = add(r, { id: idFor(i), slot: s, runtime: "node", now: NOW });
  }
  assert.equal(Object.keys(r.projects).length, 10);
  assert.ok(!Object.values(r.projects).some((p) => p.slot === BUILD_SLOT));
});

test("slots are handed out lowest first, so ps and ip addr stay legible", () => {
  let r = empty();
  const got = [];
  for (let i = 0; i < 3; i++) {
    const s = allocateSlot(r);
    got.push(s);
    r = add(r, { id: idFor(i), slot: s, runtime: "node", now: NOW });
  }
  assert.deepEqual(got, [0, 1, 2]);
});

test("a freed slot is reused rather than leaked", () => {
  let r = empty();
  r = add(r, { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  r = add(r, { id: "bbbb", slot: 1, runtime: "node", now: NOW });
  r = remove(r, "aaaa");
  assert.equal(allocateSlot(r), 0);
});

test("a full box returns null rather than an invalid slot", () => {
  let r = empty();
  for (const s of appSlots()) {
    r = add(r, { id: idFor(s), slot: s, runtime: "node", now: NOW });
  }
  assert.equal(allocateSlot(r), null);
});

test("two projects cannot hold one slot", () => {
  const r = add(empty(), { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  assert.throws(() => add(r, { id: "bbbb", slot: 0, runtime: "node", now: NOW }), /held by aaaa/);
  // Re-adding the same project to its own slot is a redeploy, not a conflict.
  assert.doesNotThrow(() => add(r, { id: "aaaa", slot: 0, runtime: "node", now: NOW }));
});

test("invalid input is refused rather than stored", () => {
  const r = empty();
  assert.throws(() => add(r, { id: "A9", slot: 0, runtime: "node", now: NOW }), /invalid project id/);
  assert.throws(() => add(r, { id: "aaaa", slot: BUILD_SLOT, runtime: "node", now: NOW }), /invalid slot/);
  assert.throws(() => add(r, { id: "aaaa", slot: 99, runtime: "node", now: NOW }), /invalid slot/);
  assert.throws(() => add(r, { id: "aaaa", slot: 0, runtime: "ruby", now: NOW }), /invalid runtime/);
  assert.throws(() => add(r, { id: "aaaa", slot: 0, runtime: "node", now: "now" }), /invalid clock/);
});

// ---- surviving a damaged file -------------------------------------------

test("a missing or truncated file starts from empty rather than throwing", () => {
  // This file will eventually be truncated by a power loss mid-write. Refusing
  // to start would take the whole platform down to protect one project.
  for (const bad of ["", undefined, null, "{", '{"projects":', "not json at all"]) {
    const { registry, dropped } = parse(bad);
    assert.deepEqual(registry.projects, {});
    assert.ok(dropped.length > 0, "and it says so");
  }
});

test("a file of the wrong shape is survived", () => {
  for (const bad of ['{"projects":null}', '{"projects":[]}', "[]", '"a string"', "42"]) {
    const { registry, dropped } = parse(bad);
    assert.deepEqual(registry.projects, {});
    assert.ok(dropped.length > 0);
  }
});

test("one bad entry does not discard the good ones", () => {
  const { registry, dropped } = parse(JSON.stringify(withProjects({
    goodaa: entry(0),
    BADID: entry(1),
    goodbb: entry(2),
    noslot: { ...entry(3), slot: "three" },
    reserved: entry(BUILD_SLOT),
    oobslot: { ...entry(0), slot: 999 },
    weird: { ...entry(4), runtime: "ruby" },
    notime: { ...entry(5), createdAt: null },
  })));
  assert.deepEqual(Object.keys(registry.projects).sort(), ["goodaa", "goodbb"]);
  const why = Object.fromEntries(dropped.map((d) => [d.id, d.why]));
  assert.match(why.BADID, /valid project id/);
  assert.match(why.noslot, /whole number/);
  assert.match(why.reserved, /reserved for builds/);
  // A valid id, so this reaches the slot check rather than being rejected for
  // its name -- which is what "oob" did, being three characters long.
  assert.match(why.oobslot, /out of range/);
  assert.match(why.weird, /unknown runtime/);
  assert.match(why.notime, /timestamp/);
});

test("a duplicated slot keeps the older project and says which lost", () => {
  // Both cannot be right, and guessing would mean pointing nginx at the wrong
  // tenant's guest.
  const { registry, dropped } = parse(JSON.stringify(withProjects({
    older: entry(0, { createdAt: NOW }),
    newer: entry(0, { createdAt: NOW + 5000 }),
  })));
  assert.deepEqual(Object.keys(registry.projects), ["older"]);
  assert.equal(dropped[0].id, "newer");
  assert.match(dropped[0].why, /slot 0 was also claimed by older/);
});

test("the older project wins regardless of which came first in the file", () => {
  const { registry } = parse(JSON.stringify(withProjects({
    newer: entry(0, { createdAt: NOW + 5000 }),
    older: entry(0, { createdAt: NOW }),
  })));
  assert.deepEqual(Object.keys(registry.projects), ["older"]);
});

test("a registry round-trips through serialise and parse", () => {
  const r = add(empty(), { id: "k9depef6", slot: 3, runtime: "python", now: NOW });
  const { registry, dropped } = parse(serialise(r));
  assert.deepEqual(registry, r);
  assert.deepEqual(dropped, []);
});

// ---- expiry --------------------------------------------------------------

test("a project expires 48 hours after it is created", () => {
  const r = add(empty(), { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  assert.equal(r.projects.aaaa.expiresAt - NOW, TTL_HOURS * HOUR);
  assert.deepEqual(expiredIds(r, NOW + TTL_HOURS * HOUR - 1), []);
  assert.deepEqual(expiredIds(r, NOW + TTL_HOURS * HOUR), ["aaaa"]);
});

test("an unreadable clock deletes nothing", () => {
  // Fail closed in the direction that matters: reaping is destructive.
  const r = add(empty(), { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  for (const bad of [null, undefined, NaN, "now", 1.5]) {
    assert.deepEqual(expiredIds(r, bad), [], `clock ${JSON.stringify(bad)} must reap nothing`);
  }
});

test("the next expiry is the soonest one still ahead", () => {
  let r = empty();
  r = add(r, { id: "aaaa", slot: 0, runtime: "node", now: NOW, ttlHours: 3 });
  r = add(r, { id: "bbbb", slot: 1, runtime: "node", now: NOW, ttlHours: 1 });
  r = add(r, { id: "cccc", slot: 2, runtime: "node", now: NOW, ttlHours: 9 });
  assert.equal(nextExpiry(r, NOW), NOW + 1 * HOUR);
  // Once the soonest has passed it stops being the answer.
  assert.equal(nextExpiry(r, NOW + 2 * HOUR), NOW + 3 * HOUR);
  assert.equal(nextExpiry(empty(), NOW), null);
});

test("the guest port is fixed, so PORT is the same for every project", () => {
  // Every guest has its own address, so there is nothing to collide with.
  assert.equal(GUEST_PORT, 8080);
});
