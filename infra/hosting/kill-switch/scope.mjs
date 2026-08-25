// What the kill switch is allowed to stop, and nothing else.
//
// It now stops microVMs on the runner box rather than throttling Lambda
// functions, and the shape of the guarantee changed with it. Worth reading
// before editing, because the old reasoning is still half-visible in the tests.
//
// **Then:** hosted backends were Lambda functions in an account shared with 42
// others -- gpurouter-agent, fsi-genai-workshop-*, mach11-registration-*. A
// kill switch that throttled "every function" would have turned a boxcode cost
// problem into a company-wide outage, so three things had to agree: the name,
// a boxcode:hosting tag, and a never-touch list.
//
// **Now:** the things being stopped are microVMs on a box that runs nothing
// else. The tag check has no analogue there -- a VM has no AWS tags -- and
// inventing one would be theatre. Two checks remain:
//
//   1. The name matches the boxcode-app prefix.
//   2. The name is not on the never-touch list.
//
// What replaced the tag is not in this file. The braces are now the IAM policy
// on the kill switch's role, which grants ssm:SendCommand on **one instance
// id** and one document -- so even a bug here cannot reach another box, because
// AWS refuses the call before the code runs. Code you can get wrong; an IAM
// resource constraint you cannot.
//
// The tag constant and its checks are kept below for the Lambda-era callers
// that still pass tags, and ignored when none are supplied. The test suite
// still asserts against the real names of the production functions in this
// account, which stays worth doing: those names must never match, whatever
// this switch is pointed at next.

/// Every hosted backend is named `boxcode-app-<project id>`, and project ids
/// are `[a-z2-9]{4,16}` -- the same shape the artifact signer mints and every
/// control-plane already validates. Anchored at both ends so
/// `boxcode-app-x-prod` or `not-boxcode-app-x` cannot match.
export const APP_NAME_RE = /^boxcode-app-[a-z2-9]{4,16}$/;

/// The tag every hosted backend is created with. Checked in addition to the
/// name, not instead of it: a tag is metadata anyone with Lambda write access
/// can add, and a name is not enough on its own either.
export const REQUIRED_TAG = "boxcode:hosting";

/// Names that must never be touched no matter what else says so. Belt-and-
/// braces against a future function being named into the prefix by accident --
/// the deploy-control and reaper functions are boxcode's own, but throttling
/// them during an incident would remove the very thing that cleans up after it.
export const NEVER_TOUCH = new Set([
  "boxcode-artifact-signer",
  "boxcode-deploy-control",
  "boxcode-reaper",
  "boxcode-kill-switch",
]);

/// True only when every applicable check agrees.
///
/// `tags` is optional. Pass a tag map and it is required to carry
/// [`REQUIRED_TAG`] -- that is the Lambda-era rule and it still holds for
/// anything that has tags. Pass nothing, as the on-box caller does, and the
/// name checks alone decide, because a microVM has no tags to check.
///
/// Deliberately takes already-fetched tags rather than fetching them itself: a
/// function that does I/O is a function whose test needs a network, and this is
/// the one piece of logic that has to be provably correct.
export function mayTouch(name, tags) {
  if (typeof name !== "string") return false;
  if (NEVER_TOUCH.has(name)) return false;
  if (!APP_NAME_RE.test(name)) return false;
  // `undefined` means "this thing has no tags" -- a microVM. An empty object
  // means "it has tags and none of them is ours" -- a Lambda that should be
  // refused. Those are different answers and collapsing them would quietly
  // drop the Lambda-era check.
  if (tags !== undefined && (!tags || tags[REQUIRED_TAG] === undefined)) return false;
  return true;
}

/// Filter a listing down to what may be touched, and say what was skipped and
/// why. The reason strings are logged during an incident, when "why did it not
/// stop that one" is the question being asked at speed.
export function partition(items) {
  const allowed = [];
  const skipped = [];
  for (const item of items) {
    // Accepts both shapes: `{ FunctionName, Tags }` from Lambda's ListFunctions,
    // and `{ name }` from the box's own VM listing.
    const name = item.FunctionName ?? item.name;
    const tags = item.Tags ?? item.tags;
    if (typeof name !== "string") skipped.push({ name: null, why: "no name" });
    else if (NEVER_TOUCH.has(name)) skipped.push({ name, why: "on the never-touch list" });
    else if (!APP_NAME_RE.test(name)) skipped.push({ name, why: "not a boxcode-app-* name" });
    else if (tags !== undefined && (!tags || tags[REQUIRED_TAG] === undefined))
      skipped.push({ name, why: `no ${REQUIRED_TAG} tag` });
    else allowed.push(name);
  }
  return { allowed, skipped };
}
