import { test } from "node:test";
import assert from "node:assert/strict";
import {
  APP_UID, APP_GID, BASES, MIN_IMAGE_MIB, MAX_IMAGE_MIB, HEADROOM_MIB,
  baseFor, imageSizeMib, tooLarge, renderInit, mke2fsArgs, paths,
} from "./rootfs.mjs";

const MIB = 1024 * 1024;
const OK = { startCommand: ["/usr/bin/node", "server.js"] };

// ---- sizing -------------------------------------------------------------

test("a small project still gets room for the base and its headroom", () => {
  // Not simply MIN_IMAGE_MIB: an empty tree still sits on a ~192 MiB base and
  // still needs headroom, so the floor is a floor rather than the answer.
  const empty = imageSizeMib(0, 192);
  assert.ok(empty >= MIN_IMAGE_MIB, `${empty} is below the floor`);
  assert.equal(empty, 192 + HEADROOM_MIB);
  // Partial megabytes round up, never down -- an image one byte short of its
  // contents fails inside mke2fs, so the arithmetic errs upward on purpose.
  assert.equal(imageSizeMib(1024, 192), empty + 1, "a 1 KiB tree still claims a whole MiB");
  assert.equal(imageSizeMib(MIB, 192), empty + 1, "and so does exactly 1 MiB");
  assert.equal(imageSizeMib(MIB + 1, 192), empty + 2, "one byte over takes another");
  // A base small enough that the floor is what binds.
  assert.equal(imageSizeMib(0, 0), MIN_IMAGE_MIB);
});

test("size grows with the tree and leaves headroom", () => {
  // A 400 MiB tree on a 192 MiB base needs 400 + 192 + headroom.
  const s = imageSizeMib(400 * MIB, 192);
  assert.equal(s, Math.min(MAX_IMAGE_MIB, 400 + 192 + HEADROOM_MIB));
  assert.ok(s > 400, "must be larger than the tree it holds");
});

test("size is clamped, and oversize is detectable rather than silent", () => {
  // An ext4 image too small for its contents fails inside mke2fs -d. Much
  // better to say "your project is too large" than to ship that error.
  assert.equal(imageSizeMib(50 * 1024 * MIB), MAX_IMAGE_MIB);
  assert.equal(tooLarge(50 * 1024 * MIB), true);
  assert.equal(tooLarge(10 * MIB), false);
});

test("ten projects at the ceiling still fit the disk", () => {
  // The disk is 50 GiB and a full disk takes every microVM and Postgres down
  // together, so this is the constraint the ceiling exists to satisfy.
  assert.ok((MAX_IMAGE_MIB * 10) / 1024 < 25, "10 images must stay well under the 50 GiB volume");
});

test("a nonsense tree size is refused rather than coerced", () => {
  for (const bad of [-1, NaN, "big", null, undefined, {}, []]) {
    assert.throws(() => imageSizeMib(bad), /refusing to size an image/, JSON.stringify(bad));
  }
});

// ---- the init ------------------------------------------------------------

test("the app does not run as root", () => {
  const i = renderInit(OK);
  assert.match(i, new RegExp(`su-exec ${APP_UID}:${APP_GID} `));
  assert.notEqual(APP_UID, 0);
  assert.notEqual(APP_GID, 0);
});

test("init execs the app rather than forking it", () => {
  // exec, so the app IS pid 1. If it forks, the app exiting leaves pid 1 alive
  // and the microVM sits there holding 256 MiB having stopped serving.
  const i = renderInit(OK);
  assert.match(i, /^exec su-exec /m);
});

test("init mounts what a runtime expects and nothing more", () => {
  const i = renderInit(OK);
  assert.match(i, /mount -t proc\s+none \/proc/);
  assert.match(i, /mount -t sysfs\s+none \/sys/);
});

test("there is no dhcp client to wait for", () => {
  // The kernel configures eth0 from the ip= boot argument, which is most of why
  // a microVM is serving in a fraction of a second.
  //
  // Comments are stripped before matching: the init explains *why* there is no
  // DHCP client, and an earlier version of this test failed on its own
  // explanation.
  const code = renderInit(OK)
    .split("\n")
    .filter((l) => !l.trim().startsWith("#"))
    .join("\n");
  assert.ok(!/dhcp|udhcpc|dhclient/i.test(code), "nothing should be doing DHCP");
});

test("environment is exported before the app starts", () => {
  const i = renderInit({ ...OK, env: { DATABASE_URL: "postgresql://u:p@10.200.0.1:5432/app_k9depef6", PORT: "8080" } });
  assert.match(i, /export DATABASE_URL='postgresql:\/\/u:p@10\.200\.0\.1:5432\/app_k9depef6'/);
  assert.match(i, /export PORT='8080'/);
  assert.ok(i.indexOf("export DATABASE_URL") < i.indexOf("exec su-exec"), "exports must precede exec");
});

// ---- injection into a shell script ---------------------------------------

test("an environment value cannot break out of its quotes", () => {
  // This text becomes a line in a shell script that runs as pid 1. A single
  // quote ends the string; a newline ends the line and starts a command.
  assert.throws(() => renderInit({ ...OK, env: { X: "a'; rm -rf /; echo '" } }), /single quote/);
  assert.throws(() => renderInit({ ...OK, env: { X: "a\nrm -rf /" } }), /newline/);
  assert.throws(() => renderInit({ ...OK, env: { X: "a\rb" } }), /newline/);
  assert.throws(() => renderInit({ ...OK, env: { X: "a\0b" } }), /null byte/);
});

test("a hostile environment variable name is refused", () => {
  assert.throws(() => renderInit({ ...OK, env: { "X='; rm -rf /": "y" } }), /invalid name/);
  assert.throws(() => renderInit({ ...OK, env: { "lowercase": "y" } }), /invalid name/);
});

test("a hostile start command is refused", () => {
  assert.throws(() => renderInit({ startCommand: ["node", "a'; rm -rf /"] }), /single quote/);
  assert.throws(() => renderInit({ startCommand: ["node\nrm -rf /"] }), /newline/);
  assert.throws(() => renderInit({ startCommand: [] }), /needs a start command/);
  assert.throws(() => renderInit({ startCommand: "node server.js" }), /needs a start command/);
  assert.throws(() => renderInit({}), /needs a start command/);
});

// ---- mke2fs --------------------------------------------------------------

test("the image is built without mounting anything", () => {
  const a = mke2fsArgs({ imagePath: "/opt/boxcode-apps/k9depef6/rootfs.ext4", stagingDir: "/opt/boxcode-apps/k9depef6/staging", sizeMib: 512 });
  // -d populates from a directory with no loop device and no mount, so this
  // needs no CAP_SYS_ADMIN on the one box where granting it is least appealing.
  assert.ok(a.includes("-d"), "must populate from a directory");
  assert.equal(a[a.indexOf("-d") + 1], "/opt/boxcode-apps/k9depef6/staging");
  assert.ok(a.includes("-F"), "the target is a plain file, not a block device");
});

test("block count matches the requested size exactly", () => {
  for (const mib of [MIN_IMAGE_MIB, 512, MAX_IMAGE_MIB]) {
    const a = mke2fsArgs({ imagePath: "/i/x.ext4", stagingDir: "/s", sizeMib: mib });
    const blockSize = Number(a[a.indexOf("-b") + 1]);
    const blocks = Number(a[a.length - 1]);
    assert.equal(blockSize, 4096);
    assert.equal((blocks * blockSize) / MIB, mib, `${mib} MiB should be ${blocks} blocks`);
  }
});

test("no space is reserved for root", () => {
  // The default holds 5% back, a server-filesystem convention that only wastes
  // space in a throwaway single-app image.
  const a = mke2fsArgs({ imagePath: "/i/x.ext4", stagingDir: "/s", sizeMib: 512 });
  assert.equal(a[a.indexOf("-m") + 1], "0");
});

test("a path that could escape is refused", () => {
  for (const bad of ["../x", "opt/x", "/opt/../../etc/x", "", null, 42]) {
    assert.throws(() => mke2fsArgs({ imagePath: bad, stagingDir: "/s", sizeMib: 512 }), /refusing image path/);
    assert.throws(() => mke2fsArgs({ imagePath: "/i/x", stagingDir: bad, sizeMib: 512 }), /refusing staging dir/);
  }
});

test("an out-of-range image size is refused", () => {
  for (const bad of [0, MIN_IMAGE_MIB - 1, MAX_IMAGE_MIB + 1, 512.5, "512", null]) {
    assert.throws(() => mke2fsArgs({ imagePath: "/i/x", stagingDir: "/s", sizeMib: bad }), /refusing image size/);
  }
});

// ---- naming --------------------------------------------------------------

test("every runtime the detector produces has a base image", () => {
  // src/deploy/backend.rs has exactly these two runtimes; a third added there
  // without one here would fail at deploy rather than at build.
  assert.deepEqual(Object.keys(BASES).sort(), ["node", "python"]);
  assert.equal(baseFor("node"), "node22");
  assert.equal(baseFor("python"), "python312");
  assert.throws(() => baseFor("ruby"), /no base image/);
  assert.throws(() => baseFor(undefined), /no base image/);
});

test("paths stay inside the apps directory", () => {
  const p = paths("k9depef6");
  for (const v of Object.values(p)) {
    assert.ok(v.startsWith("/opt/boxcode-apps/k9depef6"), v);
    assert.ok(!v.includes(".."), v);
  }
  assert.notEqual(paths("k9depef6").image, paths("aaaa").image);
});

test("a hostile id is refused", () => {
  for (const bad of ["../../etc", "a b", "", "x".repeat(17), "A9depef6", "a/b", null, 42]) {
    assert.throws(() => paths(bad), /invalid id/, JSON.stringify(bad));
  }
});
