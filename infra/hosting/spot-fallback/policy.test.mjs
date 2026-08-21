// Tests for the spot fallback decision.
//
// The important assertions are the ones about *not* acting: an ASG that is not
// ours, a group mid-recovery, a state that could not be read. Getting the
// fallback wrong in the "act" direction costs money; getting it wrong in the
// "act when we should not have" direction costs the platform.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ASG_NAME,
  REQUIRED_TAG,
  RETURN_TO_SPOT_AFTER_MIN,
  PERCENT_FOR,
  mayTouch,
  decide,
} from "./policy.mjs";

const TAGGED = { [REQUIRED_TAG]: "true" };

// ---- scoping -----------------------------------------------------------

test("touches its own group when tagged", () => {
  assert.equal(mayTouch(ASG_NAME, TAGGED), true);
});

test("refuses every neighbouring name", () => {
  // This account is shared. None of these may ever be touched, and the
  // prefix-lookalikes are the ones a careless `startsWith` would catch.
  for (const name of [
    "boxcode-runner-old",
    "boxcode-runner-2",
    "not-boxcode-runner",
    "boxcode-runnerx",
    "gpu-router-prod",
    "bedrock-gateway-UI-prod",
    "boxcode-auth",
    "",
  ]) {
    assert.equal(mayTouch(name, TAGGED), false, `${name} must not be touchable`);
  }
});

test("refuses the right name without the tag", () => {
  assert.equal(mayTouch(ASG_NAME, {}), false);
  assert.equal(mayTouch(ASG_NAME, null), false);
  assert.equal(mayTouch(ASG_NAME, undefined), false);
});

test("refuses non-string names rather than throwing", () => {
  for (const n of [null, undefined, 42, {}, []]) {
    assert.equal(mayTouch(n, TAGGED), false);
  }
});

// ---- falling back ------------------------------------------------------

test("a failed spot launch falls back to on-demand", () => {
  const d = decide({ trigger: "launch-failed", onDemandPercent: 0 });
  assert.equal(d.action, "to-on-demand");
  assert.equal(PERCENT_FOR[d.action], 100);
});

test("a failed launch while already on on-demand changes nothing", () => {
  // Not a capacity shortage -- setting the field to its current value would
  // just log an action that did nothing.
  const d = decide({ trigger: "launch-failed", onDemandPercent: 100 });
  assert.equal(d.action, "hold");
  assert.match(d.why, /already on on-demand/);
});

// ---- returning to spot -------------------------------------------------

test("returns to spot once healthy past the cooldown", () => {
  const d = decide({
    trigger: "periodic",
    onDemandPercent: 100,
    desired: 1,
    inService: 1,
    minutesSinceChange: RETURN_TO_SPOT_AFTER_MIN,
  });
  assert.equal(d.action, "to-spot");
  assert.equal(PERCENT_FOR[d.action], 0);
});

test("holds one minute short of the cooldown", () => {
  const d = decide({
    trigger: "periodic",
    onDemandPercent: 100,
    desired: 1,
    inService: 1,
    minutesSinceChange: RETURN_TO_SPOT_AFTER_MIN - 1,
  });
  assert.equal(d.action, "hold");
});

test("never changes preference mid-recovery", () => {
  // The replacement in flight would be pointed back at the pool that just
  // failed. Long past the cooldown and still refuses, because health is the
  // binding condition, not time.
  const d = decide({
    trigger: "periodic",
    onDemandPercent: 100,
    desired: 1,
    inService: 0,
    minutesSinceChange: RETURN_TO_SPOT_AFTER_MIN * 10,
  });
  assert.equal(d.action, "hold");
  assert.match(d.why, /still recovering/);
});

test("the periodic sweep is a no-op in the normal state", () => {
  // Runs hourly forever; the overwhelmingly common case must be silent.
  const d = decide({
    trigger: "periodic",
    onDemandPercent: 0,
    desired: 1,
    inService: 1,
    minutesSinceChange: 99999,
  });
  assert.equal(d.action, "hold");
  assert.match(d.why, /already preferring spot/);
});

// ---- unreadable state fails toward availability ------------------------

test("unreadable percentage still falls back on a failed launch", () => {
  // Asymmetric on purpose. Setting 100 when it is already 100 does nothing;
  // holding leaves the platform down. Uncertainty resolves toward availability
  // in this direction only -- see the periodic case below for the mirror.
  for (const pct of [undefined, null, NaN, "banana", "", false, []]) {
    const d = decide({ trigger: "launch-failed", onDemandPercent: pct });
    assert.equal(d.action, "to-on-demand", `percent ${JSON.stringify(pct)} must still act`);
  }
});

test("unreadable percentage never returns to spot", () => {
  // The money-saving direction requires certainty. Note null/""/false/[] --
  // Number() turns every one of them into 0, which would read as "already on
  // spot" and silently skip the fallback that is actually still needed.
  for (const pct of [undefined, null, NaN, "banana", "", false, []]) {
    const d = decide({
      trigger: "periodic", onDemandPercent: pct,
      desired: 1, inService: 1, minutesSinceChange: 99999,
    });
    assert.equal(d.action, "hold", `percent ${JSON.stringify(pct)} must not act`);
  }
});

test("unreadable capacity or clock holds", () => {
  const base = { trigger: "periodic", onDemandPercent: 100, desired: 1, inService: 1 };
  for (const bad of [undefined, null, "", false, [], "x"]) {
    assert.equal(decide({ ...base, minutesSinceChange: bad }).action, "hold");
    assert.equal(decide({ ...base, desired: bad, minutesSinceChange: 9999 }).action, "hold");
    assert.equal(decide({ ...base, inService: bad, minutesSinceChange: 9999 }).action, "hold");
  }
});

test("an unknown trigger does nothing and says so", () => {
  const d = decide({ trigger: "surprise", onDemandPercent: 0 });
  assert.equal(d.action, "hold");
  assert.match(d.why, /unknown trigger/);
});

test("no arguments at all does not throw", () => {
  assert.equal(decide().action, "hold");
});

// ---- the round trip ----------------------------------------------------

test("a full outage-and-recovery cycle ends back on spot", () => {
  let pct = 0;

  // Spot capacity vanishes.
  let d = decide({ trigger: "launch-failed", onDemandPercent: pct });
  assert.equal(d.action, "to-on-demand");
  pct = PERCENT_FOR[d.action];

  // The hourly sweep holds while the replacement is still coming up...
  d = decide({ trigger: "periodic", onDemandPercent: pct, desired: 1, inService: 0, minutesSinceChange: 5 });
  assert.equal(d.action, "hold");

  // ...and keeps holding through the cooldown once it is healthy.
  for (const m of [10, 60, 180, RETURN_TO_SPOT_AFTER_MIN - 1]) {
    d = decide({ trigger: "periodic", onDemandPercent: pct, desired: 1, inService: 1, minutesSinceChange: m });
    assert.equal(d.action, "hold", `should still hold at ${m}m`);
  }

  // Then returns to spot -- which does not disturb the instance now running.
  d = decide({ trigger: "periodic", onDemandPercent: pct, desired: 1, inService: 1, minutesSinceChange: RETURN_TO_SPOT_AFTER_MIN });
  assert.equal(d.action, "to-spot");
  assert.equal(PERCENT_FOR[d.action], 0);
});
