// When the runner box should stop asking for spot capacity, and when it may
// start again.
//
// The hosting design runs one instance, and it runs it on spot: ~$40/month
// instead of ~$87. Spot's price is the whole point and its interruption is the
// whole cost, so the deal only works if losing the instance is survivable and
// *temporary*. This module decides the "temporary" part.
//
// The mechanism is one field on the Auto Scaling group's mixed-instances
// policy, OnDemandPercentageAboveBaseCapacity:
//
//     0    -- launch spot; this is the normal, cheap state
//     100  -- launch on-demand; this is the fallback
//
// An ASG does NOT fall back to on-demand on its own. With the percentage at 0
// and no spot capacity available in the pinned availability zone, it retries
// spot forever and the platform simply stays down. That is the failure this
// exists to prevent, and it is the reason a "spot + on-demand fallback" design
// needs any code at all rather than just a checkbox.
//
// The property that makes this cheap, and which is not obvious:
//
//   **Changing the percentage does not touch the running instance.** The
//   policy applies to the *next* launch, not the current one. So going back to
//   spot costs nothing and interrupts nobody -- the box simply becomes a spot
//   box the next time it is replaced for any reason. That is why the return
//   path below can be unconditional rather than carefully scheduled: there is
//   no outage to schedule around.
//
// Everything here is pure. Deciding is the part that has to be provably
// correct, so it does no I/O and its tests need no network -- the same split
// scope.mjs uses, for the same reason.

/// The one Auto Scaling group this may ever touch. Anchored, exact: this
/// account is shared, and an ASG named `boxcode-runner-old` or
/// `not-boxcode-runner` is somebody else's problem and must stay that way.
export const ASG_NAME = "boxcode-runner";

/// Checked in addition to the name, never instead of it -- same reasoning as
/// scope.mjs. A name can collide; a tag can be added by anyone with write
/// access; neither is sufficient alone.
export const REQUIRED_TAG = "boxcode:hosting";

/// How long the group must have been healthy on on-demand before spot is
/// preferred again.
///
/// Not a flapping guard -- returning to spot is free (see above), so flapping
/// the *policy* costs nothing. It is a signal guard: capacity shortages in an
/// availability zone last hours, not minutes, and switching the preference back
/// after ten minutes just means the next replacement fails and drops the
/// platform again. Six hours is long enough that the shortage is genuinely over
/// and short enough that a month spent accidentally on on-demand cannot happen.
export const RETURN_TO_SPOT_AFTER_MIN = 360;

/// Number() is too forgiving to validate with: Number(null), Number(""),
/// Number(false) and Number([]) are all 0, so an absent field would read as a
/// real "0% on-demand" -- which in this module means "we are on spot", the
/// single most consequential thing to be wrong about. Anything that is not
/// already a finite number, or a string that parses as one, is NaN here.
function num(v) {
  if (typeof v === "number") return Number.isFinite(v) ? v : NaN;
  if (typeof v === "string" && v.trim() !== "") return Number(v);
  return NaN;
}

/// True only when name and tag agree. `tags` is the group's tag map.
export function mayTouch(asgName, tags) {
  if (typeof asgName !== "string") return false;
  if (asgName !== ASG_NAME) return false;
  if (!tags || tags[REQUIRED_TAG] === undefined) return false;
  return true;
}

/// What to do, given the group's current state and what woke us up.
///
/// `trigger` is either:
///   "launch-failed" -- EventBridge saw `EC2 Instance Launch Unsuccessful`,
///                      which for a spot-only group means no capacity.
///   "periodic"      -- the hourly sweep, which is the only thing that ever
///                      moves the preference back toward spot.
///
/// Returns `{ action, why }` where action is "to-on-demand" | "to-spot" |
/// "hold". `why` is logged verbatim; during an incident the question asked at
/// speed is "why is it still on the expensive one", and the answer needs to be
/// in the log rather than reconstructed.
export function decide({
  trigger,
  onDemandPercent,
  desired,
  inService,
  minutesSinceChange,
} = {}) {
  const pct = num(onDemandPercent);

  if (trigger === "launch-failed") {
    // Uncertainty resolves toward availability. If the current percentage could
    // not be read we do not know whether the fallback is already on, and the
    // cost of guessing wrong in each direction is not symmetric: setting 100
    // when it is already 100 does nothing, while holding leaves the platform
    // down. So an unreadable state acts.
    if (!Number.isFinite(pct)) {
      return { action: "to-on-demand", why: "spot launch failed and current state unreadable; falling back anyway" };
    }
    if (pct >= 100) {
      // On-demand launches fail too -- for an instance type genuinely
      // unavailable in this AZ, or an account limit. Neither is fixed by
      // setting the field to the value it already has.
      return { action: "hold", why: "already on on-demand; a failure here is not a capacity shortage" };
    }
    return { action: "to-on-demand", why: "spot launch failed; falling back so the platform comes back" };
  }

  if (trigger === "periodic") {
    // The other direction. This one only ever saves money, never restores
    // service, so it requires certainty about every input and refuses without
    // it -- the mirror image of the launch-failed branch above.
    if (!Number.isFinite(pct)) {
      return { action: "hold", why: "current on-demand percentage unreadable" };
    }
    if (pct === 0) return { action: "hold", why: "already preferring spot" };

    const want = num(desired);
    const have = num(inService);
    if (!Number.isFinite(want) || !Number.isFinite(have)) {
      return { action: "hold", why: "group capacity unreadable" };
    }
    if (have < want) {
      // Mid-recovery. Changing the preference now would point the in-flight
      // replacement back at the capacity pool that just failed.
      return { action: "hold", why: `still recovering (${have}/${want} in service)` };
    }

    const since = num(minutesSinceChange);
    if (!Number.isFinite(since)) {
      return { action: "hold", why: "time since last change unknown" };
    }
    if (since < RETURN_TO_SPOT_AFTER_MIN) {
      return {
        action: "hold",
        why: `healthy on on-demand for ${Math.floor(since)}m; waiting for ${RETURN_TO_SPOT_AFTER_MIN}m`,
      };
    }

    return {
      action: "to-spot",
      why: `healthy on on-demand for ${Math.floor(since)}m; preferring spot again (does not disturb the running instance)`,
    };
  }

  return { action: "hold", why: `unknown trigger ${JSON.stringify(trigger)}` };
}

/// The percentage each action sets. Kept next to the decision so the two
/// cannot drift, and exported so the tests assert on it rather than on
/// literals scattered through index.mjs.
export const PERCENT_FOR = {
  "to-on-demand": 100,
  "to-spot": 0,
};
