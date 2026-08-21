// Tests for the container launch contract.
//
// These are not "does the function return a string" tests. Each containment
// control from the hosting design gets an assertion that its flag is present,
// because the failure mode this file exists to prevent is a flag quietly going
// missing during an edit and nothing anywhere looking wrong afterwards.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ID_RE, APP_MEMORY_MB, APP_UID, BUILD_TIMEOUT_S,
  appNetworkName, appContainerName, createAppNetworkArgs, connectPostgresArgs,
  runAppArgs, runBuildArgs,
} from "./container.mjs";

const APP = {
  id: "k9depef6",
  image: "node:22-slim",
  port: 10000,
  appDir: "/opt/boxcode-hosting/apps/k9depef6",
  command: ["node", "server.js"],
};

/// Reads a repeatable flag's values, e.g. every --label.
const valuesOf = (args, flag) =>
  args.flatMap((a, i) => (a === flag ? [args[i + 1]] : []));
const has = (args, flag, value) => valuesOf(args, flag).includes(value);

// ---- the containment flags --------------------------------------------

test("an app container drops every capability and cannot regain any", () => {
  const a = runAppArgs(APP);
  assert.ok(has(a, "--cap-drop", "ALL"), "must drop all capabilities");
  assert.ok(has(a, "--security-opt", "no-new-privileges"), "must set no-new-privileges");
});

test("an app container is not root and not the image's user", () => {
  const a = runAppArgs(APP);
  assert.ok(has(a, "--user", `${APP_UID}:${APP_UID}`));
  assert.notEqual(APP_UID, 0);
});

test("an app container cannot write its own code", () => {
  const a = runAppArgs(APP);
  assert.ok(a.includes("--read-only"), "rootfs must be read-only");
  assert.ok(has(a, "-v", `${APP.appDir}:/app:ro`), "the code mount must be :ro");
  // The one writable place, and it must not be executable -- a writable+
  // executable directory is where a dropped payload gets run from.
  const tmpfs = valuesOf(a, "--tmpfs")[0];
  assert.match(tmpfs, /^\/tmp:/);
  assert.match(tmpfs, /noexec/);
  assert.match(tmpfs, /nosuid/);
  assert.match(tmpfs, /size=\d+m/);
});

test("an app container cannot swap, so a leak is killed rather than spread", () => {
  const a = runAppArgs(APP);
  const mem = valuesOf(a, "--memory")[0];
  const swap = valuesOf(a, "--memory-swap")[0];
  assert.equal(mem, `${APP_MEMORY_MB}m`);
  assert.equal(swap, mem, "memory-swap must equal memory, or the app swaps instead of dying");
});

test("an app container is bounded on cpu and pids", () => {
  const a = runAppArgs(APP);
  assert.ok(valuesOf(a, "--cpus").length === 1);
  assert.ok(Number(valuesOf(a, "--cpus")[0]) < 1, "one app must not be able to take a whole vCPU");
  assert.ok(Number(valuesOf(a, "--pids-limit")[0]) > 0, "a fork bomb must hit a ceiling");
});

test("an app container runs under gvisor, not the host kernel", () => {
  assert.ok(has(runAppArgs(APP), "--runtime", "runsc"));
});

test("an app is published on loopback only", () => {
  // 0.0.0.0 would put ten demo apps straight onto the box's public address,
  // behind neither nginx nor CloudFront nor WAF.
  const p = valuesOf(runAppArgs(APP), "-p")[0];
  assert.ok(p.startsWith("127.0.0.1:"), `published as ${p}, must bind loopback`);
});

test("an app is on its own network and nobody else's", () => {
  const a = runAppArgs(APP);
  assert.ok(has(a, "--network", appNetworkName(APP.id)));
  assert.notEqual(appNetworkName("aaaa"), appNetworkName("bbbb"));
});

test("app networks are created internal, which is what blocks egress", () => {
  assert.ok(createAppNetworkArgs("k9depef6").includes("--internal"));
});

test("app containers are labelled so the kill switch can find them", () => {
  const a = runAppArgs(APP);
  assert.ok(has(a, "--label", "boxcode:hosting=true"));
  assert.ok(has(a, "--label", `boxcode:id=${APP.id}`));
});

test("container and network share one naming rule", () => {
  // The kill switch matches ^boxcode-app-[a-z2-9]{4,16}$ on container names.
  // Two naming rules on one box is two rules that can drift until it silently
  // stops matching anything.
  assert.equal(appContainerName("k9depef6"), appNetworkName("k9depef6"));
  assert.match(appContainerName("k9depef6"), /^boxcode-app-[a-z2-9]{4,16}$/);
});

// ---- the build sandbox -------------------------------------------------

test("a build has network but no route to postgres or to any app", () => {
  const b = runBuildArgs({ id: "k9depef6", image: "node:22-slim", srcDir: "/s", command: ["npm", "ci"] });
  assert.ok(has(b, "--network", "bridge"), "a build needs a package registry");
  assert.ok(!valuesOf(b, "--network").some((n) => n.startsWith("boxcode-app-")),
    "a build must never join an app network");
});

test("a build cannot run forever", () => {
  const b = runBuildArgs({ id: "k9depef6", image: "node:22-slim", srcDir: "/s", command: ["npm", "ci"] });
  const i = b.indexOf("timeout");
  assert.ok(i > 0, "the command must be wrapped in timeout(1)");
  assert.deepEqual(b.slice(i, i + 4), ["timeout", "-s", "KILL", String(BUILD_TIMEOUT_S)]);
  // KILL rather than TERM: the point is a process that has ignored everything
  // politer, and `npm ci` running a hostile postinstall is exactly that case.
  assert.ok(b.includes("--rm"), "a build must not survive its own exit");
});

test("a build is still unprivileged, despite having network", () => {
  const b = runBuildArgs({ id: "k9depef6", image: "node:22-slim", srcDir: "/s", command: ["npm", "ci"] });
  assert.ok(has(b, "--cap-drop", "ALL"));
  assert.ok(has(b, "--security-opt", "no-new-privileges"));
  assert.ok(has(b, "--runtime", "runsc"));
  assert.ok(has(b, "--user", `${APP_UID}:${APP_UID}`));
});

// ---- ids are attacker-supplied ----------------------------------------

test("a hostile id cannot smuggle a docker flag", () => {
  // argv arrays defeat shell injection, but not an id that is itself a flag.
  for (const bad of [
    "x --privileged", "--privileged", "-v/:/host", "a/../b", "A9depef6",
    "abc", "x".repeat(17), "", "with space", "semi;colon", "id\nnewline",
    "$(whoami)", "../../etc",
  ]) {
    assert.throws(() => runAppArgs({ ...APP, id: bad }), /invalid id/, `id ${JSON.stringify(bad)} must be refused`);
    assert.throws(() => createAppNetworkArgs(bad), /invalid id/);
  }
});

test("non-string ids are refused rather than coerced", () => {
  for (const bad of [null, undefined, 42, {}, []]) {
    assert.throws(() => appContainerName(bad), /invalid id/);
  }
});

test("the id shape matches what the rest of the platform validates", () => {
  assert.ok(ID_RE.test("k9depef6"));
  assert.ok(!ID_RE.test("k1depef6"), "1 is not in the alphabet");
  assert.ok(!ID_RE.test("k0depef6"), "0 is not in the alphabet");
});

test("a hostile environment variable name is refused", () => {
  assert.throws(
    () => runAppArgs({ ...APP, env: { "FOO=BAR -v /:/host": "x" } }),
    /invalid name/,
  );
  // The ordinary case still works.
  const a = runAppArgs({ ...APP, env: { DATABASE_URL: "postgres://u:p@boxcode-postgres/db" } });
  assert.ok(has(a, "-e", "DATABASE_URL=postgres://u:p@boxcode-postgres/db"));
});

test("an out-of-range port is refused", () => {
  for (const p of [0, 80, 443, 1023, 65536, -1, 1.5, "10000", null]) {
    assert.throws(() => runAppArgs({ ...APP, port: p }), /invalid port/);
  }
});

test("postgres is connected to the app's network, not the other way round", () => {
  const c = connectPostgresArgs("k9depef6");
  assert.deepEqual(c, ["network", "connect", "boxcode-app-k9depef6", "boxcode-postgres"]);
});
