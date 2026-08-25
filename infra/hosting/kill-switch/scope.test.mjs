// The scoping is the part that must not be wrong, so it is tested against the
// real contents of the account rather than invented names.
//
//   node --test infra/hosting/kill-switch/scope.test.mjs

import { test } from "node:test";
import assert from "node:assert/strict";
import { mayTouch, partition, APP_NAME_RE, REQUIRED_TAG, NEVER_TOUCH } from "./scope.mjs";

// Read off account 992382417943 on 2026-08-20. Every one of these is somebody
// else's production service, and the kill switch firing must leave all of them
// running.
const REAL_OTHER_FUNCTIONS = [
  "gpurouter-agent-agent",
  "fsi-genai-workshop-config-management",
  "fsi-genai-workshop-strands-websocket-agent",
  "fsi-genai-workshop-document-processor",
  "fsi-genai-workshop-websocket-layer-downloader",
  "fsi-genai-workshop-apigateway-account-settings",
  "mach11-registration-6e237ba2-e4a4-4545-9ac5-54b3551089ed",
  "mach11-registration-d0b0483c-7463-4320-b713-009a086f2bd0",
  "github-slack-alert",
  "maketplacemailing",
  "Yaksha-testing-agent",
  "holbox-demo-start-builds",
  "s3-file-operations",
  "ec2-large-instance-alerts-notifier",
];

const tagged = { [REQUIRED_TAG]: "true" };

test("not one real function in this account can be touched", () => {
  for (const name of REAL_OTHER_FUNCTIONS) {
    assert.equal(mayTouch(name, tagged), false, `${name} must never be touchable`);
  }
});

test("even carrying the tag, a foreign function is refused", () => {
  // The tag is metadata; anyone with Lambda write access could add it. The
  // name check has to stand on its own.
  assert.equal(mayTouch("gpurouter-agent-agent", tagged), false);
  assert.equal(mayTouch("mach11-registration-abc", tagged), false);
});

test("a hosted backend with both name and tag is allowed", () => {
  assert.equal(mayTouch("boxcode-app-k9depef6", tagged), true);
  assert.equal(mayTouch("boxcode-app-abcd", tagged), true);
});

test("the right name without the tag is refused", () => {
  assert.equal(mayTouch("boxcode-app-k9depef6", {}), false);
  assert.equal(mayTouch("boxcode-app-k9depef6", null), false);
});

test("boxcode's own control-plane functions are never touched", () => {
  // Throttling these during an incident would take out the thing that cleans
  // up after it.
  for (const name of [
    "boxcode-artifact-signer",
    "boxcode-deploy-control",
    "boxcode-reaper",
    "boxcode-kill-switch",
  ]) {
    assert.equal(mayTouch(name, tagged), false, `${name} is on the never-touch list`);
  }
});

test("names that merely look like the prefix are refused", () => {
  for (const name of [
    "boxcode-app-k9depef6-prod",   // suffixed
    "not-boxcode-app-k9depef6",    // prefixed
    "boxcode-app-",                // empty id
    "boxcode-app-ab",              // id too short
    "boxcode-app-K9DEPEF6",        // wrong case
    "boxcode-app-abc1",            // 1 is not in the id charset
    "boxcode-app-abc0",            // nor is 0
    "boxcode-app-toolongtobeavalididentifier",
  ]) {
    assert.equal(mayTouch(name, tagged), false, `${name} must not match`);
  }
});

test("the anchors really are anchors", () => {
  assert.equal(APP_NAME_RE.test("xboxcode-app-abcd"), false);
  assert.equal(APP_NAME_RE.test("boxcode-app-abcdx!"), false);
  assert.equal(APP_NAME_RE.test("boxcode-app-abcd\nboxcode-app-efgh"), false);
});

test("nothing but a string is ever touchable", () => {
  for (const v of [undefined, null, 42, {}, [], true]) {
    assert.equal(mayTouch(v, tagged), false);
  }
});

test("partition keeps the account safe and says why it skipped each one", () => {
  const listing = [
    ...REAL_OTHER_FUNCTIONS.map((FunctionName) => ({ FunctionName, Tags: tagged })),
    { FunctionName: "boxcode-artifact-signer", Tags: tagged },
    { FunctionName: "boxcode-app-k9depef6", Tags: tagged },
    { FunctionName: "boxcode-app-abcdefgh", Tags: tagged },
    { FunctionName: "boxcode-app-untagged", Tags: {} },
  ];
  const { allowed, skipped } = partition(listing);

  assert.deepEqual(allowed, ["boxcode-app-k9depef6", "boxcode-app-abcdefgh"]);
  assert.equal(skipped.length, listing.length - 2);
  // Every skip carries a reason, because during an incident "why was that one
  // left running" is asked at speed.
  for (const s of skipped) assert.ok(s.why && s.why.length > 0);
  assert.ok(skipped.find((s) => s.name === "boxcode-artifact-signer").why.includes("never-touch"));
  assert.ok(skipped.find((s) => s.name === "gpurouter-agent-agent").why.includes("boxcode-app-*"));
});

test("an empty account produces an empty action list, not an error", () => {
  const { allowed, skipped } = partition([]);
  assert.deepEqual(allowed, []);
  assert.deepEqual(skipped, []);
});

// ---- microVMs, which have no tags -----------------------------------------

test("a microVM is judged on its name alone", () => {
  // A VM has no AWS tags. Requiring one would mean the kill switch could never
  // stop anything on the box -- the failure would be silent, and only visible
  // during the incident it exists for.
  assert.equal(mayTouch("boxcode-app-k9depef6"), true);
  assert.equal(mayTouch("boxcode-app-abcd", undefined), true);
});

test("dropping the tag check does not widen what a name may be", () => {
  for (const name of REAL_OTHER_FUNCTIONS) {
    assert.equal(mayTouch(name), false, `${name} must never be touchable`);
  }
  for (const name of [
    "boxcode-app", "boxcode-app-", "boxcode-app-abc", "not-boxcode-app-abcd",
    "boxcode-app-abcd-prod", "BOXCODE-APP-ABCD", "boxcode-app-abcd extra",
  ]) {
    assert.equal(mayTouch(name), false, `${name} must not match`);
  }
});

test("boxcode's own services are still never touched, tags or not", () => {
  for (const name of NEVER_TOUCH) {
    assert.equal(mayTouch(name), false, `${name} must never be touchable`);
    assert.equal(mayTouch(name, { [REQUIRED_TAG]: "true" }), false);
  }
});

test("an empty tag map is still a refusal, and no tag map is not", () => {
  // These are different answers on purpose: `{}` means "it has tags and none
  // is ours", which is a Lambda that must be refused. `undefined` means "it
  // has no tags at all", which is a microVM.
  assert.equal(mayTouch("boxcode-app-k9depef6", {}), false);
  assert.equal(mayTouch("boxcode-app-k9depef6", null), false);
  assert.equal(mayTouch("boxcode-app-k9depef6", undefined), true);
});

test("partition accepts the box's own listing shape", () => {
  const { allowed, skipped } = partition([
    { name: "boxcode-app-k9depef6", slot: 0, pid: 100 },
    { name: "boxcode-app-abcd", slot: 1, pid: 101 },
    { name: "boxcode-kill-switch", slot: 2, pid: 102 },
    { name: "gpurouter-agent-agent", slot: 3, pid: 103 },
    { name: null, slot: 4, pid: 104 },
  ]);
  assert.deepEqual(allowed, ["boxcode-app-k9depef6", "boxcode-app-abcd"]);
  assert.equal(skipped.length, 3);
  assert.ok(skipped.every((s) => s.why));
});

test("both listing shapes still work from one function", () => {
  // Lambda's ListFunctions shape and the box's, side by side.
  const { allowed } = partition([
    { FunctionName: "boxcode-app-aaaa", Tags: { [REQUIRED_TAG]: "true" } },
    { FunctionName: "boxcode-app-bbbb", Tags: {} },
    { name: "boxcode-app-cccc" },
  ]);
  assert.deepEqual(allowed, ["boxcode-app-aaaa", "boxcode-app-cccc"]);
});
