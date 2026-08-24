import { test } from "node:test";
import assert from "node:assert/strict";
import {
  SYSTEM_RESERVE_MIB, BUILD_RESERVE_MIB, MIN_DISK_FREE_MIB, HARD_CAP,
  canFitAnother, howManyMore, describeCapacity,
} from "./capacity.mjs";
import { VM_MEM_MIB } from "./machine.mjs";
import { appSlots } from "./registry.mjs";

const OK = { memAvailableMB: 4000, diskFreeMB: 40000, running: 2, now: 1_700_000_000_000 };

test("an empty box has room", () => {
  const r = canFitAnother({ ...OK, running: 0 });
  assert.equal(r.admit, true);
});

test("room is bounded by slots as well as by memory", () => {
  // On an empty box memory alone says 23 and there are only 15 addresses. The
  // larger number would be a promise the slot allocator then breaks.
  assert.ok(howManyMore(99999, 0) <= HARD_CAP);
  assert.equal(HARD_CAP, appSlots().length);
  assert.equal(howManyMore(99999, HARD_CAP - 1), 1);
  assert.equal(howManyMore(99999, HARD_CAP), 0);
});

test("the reserves are respected exactly", () => {
  const edge = VM_MEM_MIB + BUILD_RESERVE_MIB + SYSTEM_RESERVE_MIB;
  assert.equal(canFitAnother({ ...OK, memAvailableMB: edge }).admit, true);
  const r = canFitAnother({ ...OK, memAvailableMB: edge - 1 });
  assert.equal(r.admit, false);
  assert.match(r.reason, /no room for another project/);
});

test("the build reserve is real, not decorative", () => {
  // Without it the tenth deploy is admitted and then OOM-kills a running
  // tenant while its build VM comes up.
  assert.ok(BUILD_RESERVE_MIB >= 1024, "a build VM needs 1 GiB");
  assert.ok(canFitAnother({ ...OK, memAvailableMB: VM_MEM_MIB + SYSTEM_RESERVE_MIB + 10 }).admit === false);
});

test("a refusal says when a slot frees", () => {
  const now = OK.now;
  const r = canFitAnother({
    ...OK, memAvailableMB: 500, running: 9,
    expiresAt: [now + 3 * 3600_000, now + 90 * 60_000, now - 5000],
  });
  assert.match(r.reason, /next slot frees in 1h 30m/);
});

test("low disk refuses before memory is considered", () => {
  const r = canFitAnother({ ...OK, memAvailableMB: 99999, diskFreeMB: MIN_DISK_FREE_MIB - 1 });
  assert.equal(r.admit, false);
  assert.match(r.reason, /disk/);
  assert.match(r.reason, /every project at once/);
});

test("the slot ceiling backstops flattering memory accounting", () => {
  const r = canFitAnother({ memAvailableMB: 99999, diskFreeMB: 99999, running: HARD_CAP, now: 1 });
  assert.equal(r.admit, false);
  assert.match(r.reason, new RegExp(`all ${HARD_CAP} slots`));
});

test("it fails CLOSED on anything unreadable", () => {
  // Admitting means letting a stranger's code start running. There is no safe
  // guess, so this is the opposite of the spot-fallback direction.
  for (const bad of [undefined, null, NaN, "", false, [], "4000"]) {
    assert.equal(canFitAnother({ ...OK, memAvailableMB: bad }).admit, false, `mem ${JSON.stringify(bad)}`);
    assert.equal(canFitAnother({ ...OK, diskFreeMB: bad }).admit, false, `disk ${JSON.stringify(bad)}`);
    assert.equal(canFitAnother({ ...OK, running: bad }).admit, false, `running ${JSON.stringify(bad)}`);
  }
  assert.equal(canFitAnother().admit, false);
});

test("a negative running count is refused rather than treated as zero", () => {
  assert.equal(canFitAnother({ ...OK, running: -1 }).admit, false);
});

test("describeCapacity never throws, whatever it is handed", () => {
  // A health endpoint that fails because it could not read a number is worse
  // than one that says it could not read the number.
  for (const bad of [undefined, null, NaN, "x", [], false]) {
    const d = describeCapacity({ memAvailableMB: bad, diskFreeMB: bad, running: bad });
    assert.equal(d.memAvailableMB, null);
    assert.equal(d.diskFreeMB, null);
    assert.equal(d.slots, HARD_CAP);
  }
  const good = describeCapacity({ memAvailableMB: 4000, diskFreeMB: 40000, running: 3 });
  assert.equal(good.running, 3);
  assert.ok(good.roomForMore > 0);
  assert.ok(good.roomForMore <= HARD_CAP - 3);
});

test("ten projects plus a build fit the box this is sized for", () => {
  // 8 GiB, about 700 MiB of host, so roughly 7.4 GiB reported available.
  const r = canFitAnother({ memAvailableMB: 7400, diskFreeMB: 40000, running: 0, now: 1 });
  assert.equal(r.admit, true);
  assert.ok(howManyMore(7400, 0) >= 10, `expected room for 10, got ${howManyMore(7400, 0)}`);
});
