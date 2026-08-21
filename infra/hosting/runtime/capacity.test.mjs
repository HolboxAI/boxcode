import { test } from "node:test";
import assert from "node:assert/strict";
import {
  APP_MEMORY_MB, SYSTEM_RESERVE_MB, BUILD_RESERVE_MB, HARD_CAP, MIN_DISK_FREE_MB,
  canAdmit, howManyMore,
} from "./capacity.mjs";

const ROOMY = { memAvailableMB: 3000, diskFreeMB: 40000, running: 3 };

test("admits when there is genuinely room", () => {
  const d = canAdmit(ROOMY);
  assert.equal(d.admit, true);
  assert.match(d.reason, /room for \d+ more/);
});

test("refuses when memory would dip into the reserves", () => {
  // Exactly one MB short of what a new app plus its build plus the box needs.
  const edge = APP_MEMORY_MB + BUILD_RESERVE_MB + SYSTEM_RESERVE_MB;
  assert.equal(canAdmit({ ...ROOMY, memAvailableMB: edge }).admit, true);
  const d = canAdmit({ ...ROOMY, memAvailableMB: edge - 1 });
  assert.equal(d.admit, false);
  assert.match(d.reason, /no room for another app/);
});

test("the refusal explains itself in numbers a person can act on", () => {
  const d = canAdmit({ memAvailableMB: 400, diskFreeMB: 40000, running: 9 });
  assert.equal(d.admit, false);
  assert.match(d.reason, /400 MB available/);
  assert.match(d.reason, new RegExp(`${APP_MEMORY_MB} MB`));
  assert.match(d.reason, /9 apps running/);
});

test("says when the next slot frees, so the caller knows to come back", () => {
  const now = 1_000_000;
  const d = canAdmit({
    memAvailableMB: 400, diskFreeMB: 40000, running: 10, now,
    expiresAt: [now + 3 * 3600_000, now + 90 * 60_000, now - 5000],
  });
  // The soonest one that has not already passed: 90 minutes.
  assert.match(d.reason, /next slot frees in 1h 30m/);
});

test("singular when one app is running", () => {
  const d = canAdmit({ memAvailableMB: 100, diskFreeMB: 40000, running: 1 });
  assert.match(d.reason, /1 app running/);
  assert.ok(!/1 apps/.test(d.reason));
});

test("the hard cap backstops flattering memory accounting", () => {
  // Plenty of memory reported, but the count is already at the ceiling.
  const d = canAdmit({ memAvailableMB: 99999, diskFreeMB: 99999, running: HARD_CAP });
  assert.equal(d.admit, false);
  assert.match(d.reason, /hard limit/);
});

test("refuses on low disk, before memory is even considered", () => {
  const d = canAdmit({ memAvailableMB: 99999, diskFreeMB: MIN_DISK_FREE_MB - 1, running: 0 });
  assert.equal(d.admit, false);
  assert.match(d.reason, /disk free/);
  assert.match(d.reason, /every app down at once/);
});

test("fails CLOSED on anything it cannot read", () => {
  // Opposite of the spot fallback, on purpose: admitting means letting a
  // stranger's code start running, and there is no safe guess.
  for (const bad of [undefined, null, NaN, "", false, [], "banana"]) {
    assert.equal(canAdmit({ ...ROOMY, memAvailableMB: bad }).admit, false, `mem ${JSON.stringify(bad)}`);
    assert.equal(canAdmit({ ...ROOMY, diskFreeMB: bad }).admit, false, `disk ${JSON.stringify(bad)}`);
    assert.equal(canAdmit({ ...ROOMY, running: bad }).admit, false, `running ${JSON.stringify(bad)}`);
  }
  assert.equal(canAdmit().admit, false, "no arguments at all must refuse");
});

test("a negative running count is refused rather than treated as zero", () => {
  assert.equal(canAdmit({ ...ROOMY, running: -1 }).admit, false);
});

test("howManyMore never promises negative room", () => {
  assert.equal(howManyMore(0), 0);
  assert.equal(howManyMore(100), 0);
  assert.equal(howManyMore("nonsense"), 0);
  assert.ok(howManyMore(3000) > 0);
});

test("a t3.medium with nothing running admits about ten", () => {
  // The sizing claim, as a test: 4 GiB minus ~525 MB of system leaves roughly
  // 3.5 GB reported available on an idle box.
  assert.ok(howManyMore(3570) >= 10, `expected >= 10, got ${howManyMore(3570)}`);
  assert.ok(howManyMore(3570) <= HARD_CAP, "must not exceed the hard cap");
});
