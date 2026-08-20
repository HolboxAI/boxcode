// What the kill switch is allowed to touch, and nothing else.
//
// This account is shared. It runs 42 Lambda functions and 8 EC2 instances that
// have nothing to do with boxcode -- gpurouter-agent, fsi-genai-workshop-*,
// mach11-registration-*, bedrock-gateway-UI-prod and the rest. A kill switch
// that fired during an incident and throttled "every function" would turn a
// boxcode cost problem into a company-wide outage. That is a worse failure
// than the one it exists to prevent, so the scoping is the feature and the
// killing is the easy part.
//
// Three independent things have to agree before a function is touched:
//
//   1. The name matches the boxcode-app prefix.
//   2. The function carries the boxcode:hosting tag.
//   3. The name is not on the never-touch list.
//
// Any one of them saying no is a no. They are independent on purpose: a name
// collision alone is not enough, a stray tag alone is not enough, and the
// denylist backstops both.
//
// And none of it is the real guarantee. The real guarantee is the IAM policy
// on the kill switch's own role, which grants
// PutFunctionConcurrency only on `function:boxcode-app-*` -- so even a bug in
// this file cannot touch gpurouter-agent, because AWS itself refuses the call.
// Code you can get wrong; IAM resource scoping you cannot. This module is the
// belt; the policy is the braces.

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

/// True only when all three checks agree. `tags` is the function's tag map as
/// returned by ListTags.
///
/// Deliberately takes already-fetched tags rather than fetching them itself:
/// a function that does I/O is a function whose test needs a network, and this
/// is the one piece of logic that has to be provably correct.
export function mayTouch(functionName, tags) {
  if (typeof functionName !== "string") return false;
  if (NEVER_TOUCH.has(functionName)) return false;
  if (!APP_NAME_RE.test(functionName)) return false;
  if (!tags || tags[REQUIRED_TAG] === undefined) return false;
  return true;
}

/// Filter a listing down to what may be touched, and say what was skipped and
/// why. The reason strings are logged during an incident, when "why did it not
/// stop that one" is the question being asked at speed.
export function partition(functions) {
  const allowed = [];
  const skipped = [];
  for (const fn of functions) {
    const name = fn.FunctionName;
    if (NEVER_TOUCH.has(name)) skipped.push({ name, why: "on the never-touch list" });
    else if (!APP_NAME_RE.test(name)) skipped.push({ name, why: "not a boxcode-app-* name" });
    else if (!fn.Tags || fn.Tags[REQUIRED_TAG] === undefined)
      skipped.push({ name, why: `no ${REQUIRED_TAG} tag` });
    else allowed.push(name);
  }
  return { allowed, skipped };
}
