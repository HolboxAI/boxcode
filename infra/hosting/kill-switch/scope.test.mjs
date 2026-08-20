// The scoping is the part that must not be wrong, so it is tested against the
// real contents of the account rather than invented names.
//
//   node --test infra/hosting/kill-switch/scope.test.mjs

import { test } from "node:test";
import assert from "node:assert/strict";
import { mayTouch, partition, APP_NAME_RE, REQUIRED_TAG } from "./scope.mjs";

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
