// Tests for the systemd unit, which is the whole of an app's sandbox now that
// containers are gone. Each control gets its own assertion, because the failure
// mode is a directive quietly disappearing during an edit.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  APP_MEMORY_MB, unitName, userName, appRoot, renderUnit, renderNginxRoute,
} from "./unit.mjs";

const OK = { id: "k9depef6", port: 10000, execStart: "/usr/bin/node server.js" };
const has = (u, line) => u.split("\n").some((l) => l.trim() === line);

// ---- the sandbox ------------------------------------------------------

test("runs as its own unix user, so the filesystem separates tenants", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, `User=${userName(OK.id)}`));
  assert.notEqual(userName("aaaa"), userName("bbbb"));
  // The thing a shared process manager could not give us.
  assert.ok(!has(u, "User=root"));
});

test("has no capabilities and cannot acquire any", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, "NoNewPrivileges=yes"));
  assert.ok(has(u, "CapabilityBoundingSet="), "the bounding set must be emptied");
  assert.ok(has(u, "AmbientCapabilities="));
  assert.ok(has(u, "RestrictSUIDSGID=yes"));
});

test("cannot reach the network except loopback", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, "IPAddressDeny=any"), "egress must be denied by default");
  assert.ok(has(u, "IPAddressAllow=localhost"), "nginx and postgres are on loopback");
  // Order matters to nobody in systemd (allow is more specific than deny),
  // but the absence of a wider allow does.
  assert.ok(!/IPAddressAllow=(any|0\.0\.0\.0)/.test(u));
});

test("cannot see the filesystem outside its own directory", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, "ProtectSystem=strict"));
  assert.ok(has(u, "ProtectHome=yes"));
  assert.ok(has(u, "PrivateTmp=yes"));
  // The empty tmpfs is what makes other tenants not exist, rather than exist
  // and be unreadable.
  assert.ok(has(u, "TemporaryFileSystem=/opt/boxcode-apps:ro"));
  assert.ok(has(u, `BindReadOnlyPaths=${appRoot(OK.id)}/app`), "its code must be read-only");
  assert.ok(has(u, `BindPaths=${appRoot(OK.id)}/data`), "it needs one writable place");
});

test("cannot see other processes", () => {
  const u = renderUnit(OK);
  // Command lines are where secrets get left.
  assert.ok(has(u, "ProtectProc=invisible"));
  assert.ok(has(u, "ProcSubset=pid"));
});

test("cannot touch the kernel", () => {
  const u = renderUnit(OK);
  for (const d of [
    "ProtectKernelTunables=yes", "ProtectKernelModules=yes", "ProtectKernelLogs=yes",
    "ProtectControlGroups=yes", "ProtectClock=yes", "PrivateDevices=yes",
    "RestrictNamespaces=yes", "SystemCallArchitectures=native",
  ]) {
    assert.ok(has(u, d), `missing ${d}`);
  }
  assert.match(u, /SystemCallFilter=~@privileged/);
});

test("bounded on memory, cpu and tasks", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, `MemoryMax=${APP_MEMORY_MB}M`));
  // As important as MemoryMax: without it a leak swaps instead of dying and
  // takes the other apps down through the disk.
  assert.ok(has(u, "MemorySwapMax=0"));
  assert.match(u, /CPUQuota=\d+%/);
  assert.match(u, /TasksMax=\d+/);
});

test("does not set MemoryDenyWriteExecute, which would break every runtime", () => {
  // V8 and CPython both map W+X pages for JIT and ctypes. This is the one
  // hardening directive that must stay off, and a future tidy-up adding it
  // would refuse Node and most Python apps at startup.
  assert.ok(!/MemoryDenyWriteExecute/.test(renderUnit(OK).replace(/^#.*$/gm, "")));
});

test("a crash loop stops instead of burning cpu for two days", () => {
  const u = renderUnit(OK);
  assert.ok(has(u, "Restart=always"));
  // StartLimit* must be in [Unit]. In [Service] systemd logs "Unknown key ...
  // ignoring" and starts the unit anyway, so the ceiling is silently absent.
  // This was a real bug, found by systemd-analyze verify rather than by these
  // tests -- which is why verify-unit.sh exists.
  const unitSection = u.slice(u.indexOf("[Unit]"), u.indexOf("[Service]"));
  assert.match(unitSection, /StartLimitIntervalSec=\d+/, "StartLimitIntervalSec must be in [Unit]");
  assert.match(unitSection, /StartLimitBurst=\d+/, "StartLimitBurst must be in [Unit]");
});

test("the syscall denylist uses one ~ for the whole list", () => {
  // Written per entry (~@privileged ~@mount ...) only the first is a denial;
  // the rest parse as literal syscall names, fail to resolve, and are dropped
  // with a log line nobody reads. Also a real bug, also found by systemd.
  const line = renderUnit(OK).split("\n").find((l) => l.startsWith("SystemCallFilter=~"));
  assert.ok(line, "there must be a denylist");
  assert.equal((line.match(/~/g) || []).length, 1, `only one ~ allowed, got: ${line}`);
  assert.match(line, /@privileged/);
  assert.match(line, /@mount/);
  // @resources is deliberately absent: setrlimit/nice/sched_* are used
  // legitimately by Node's thread pool and by CPython.
  assert.ok(!line.includes("@resources"), "denying @resources breaks Node and Python");
});

test("files the app writes are not world-readable", () => {
  assert.ok(has(renderUnit(OK), "UMask=0077"));
});

// ---- injection --------------------------------------------------------

test("an environment value containing a newline is refused", () => {
  // systemd reads unit files line by line, so a newline in a value is not a
  // broken string -- it is a new directive, run as root at start.
  assert.throws(
    () => renderUnit({ ...OK, env: { DATABASE_URL: "x\nExecStartPre=/bin/sh -c curl-evil" } }),
    /newline/,
  );
  assert.throws(() => renderUnit({ ...OK, env: { X: "a\rb" } }), /newline/);
});

test("an environment value containing a quote or backslash is refused", () => {
  assert.throws(() => renderUnit({ ...OK, env: { X: 'a"b' } }), /quote or backslash/);
  assert.throws(() => renderUnit({ ...OK, env: { X: "a\\b" } }), /quote or backslash/);
});

test("an ordinary database url still works", () => {
  const u = renderUnit({ ...OK, env: { DATABASE_URL: "postgresql://u:p@127.0.0.1:5432/app_k9depef6" } });
  assert.ok(has(u, 'Environment="DATABASE_URL=postgresql://u:p@127.0.0.1:5432/app_k9depef6"'));
});

test("a hostile environment variable NAME is refused", () => {
  assert.throws(() => renderUnit({ ...OK, env: { 'X=1"\nUser=root': "y" } }), /invalid name/);
});

test("a hostile ExecStart is refused", () => {
  assert.throws(() => renderUnit({ ...OK, execStart: "node a\nUser=root" }), /invalid ExecStart/);
  assert.throws(() => renderUnit({ ...OK, execStart: "" }), /invalid ExecStart/);
});

test("a hostile id is refused everywhere it is used", () => {
  for (const bad of ["../../etc", "a b", "A9depef6", "", "x".repeat(17), "a;b",
                     "/etc/passwd", "a/b", "a.b", "a-b", "id\nUser=root", null, 42, {}]) {
    assert.throws(() => renderUnit({ ...OK, id: bad }), /invalid id/, `id ${JSON.stringify(bad)}`);
    assert.throws(() => unitName(bad), /invalid id/);
    assert.throws(() => userName(bad), /invalid id/);
  }
});

test("a system-sounding id is namespaced, not refused", () => {
  // "root", "admin" and "daemon" all match the project-id shape, and refusing
  // them would be the wrong fix -- an id is a random 8-character string and
  // colliding with a system name is chance, not attack. The bcapp- prefix is
  // what makes it safe, so this asserts the prefix rather than a denylist that
  // would need maintaining forever.
  for (const id of ["root", "admin", "daemon", "nginx", "postgres"]) {
    assert.equal(userName(id), `bcapp-${id}`);
    assert.notEqual(userName(id), id);
    assert.ok(renderUnit({ ...OK, id }).includes(`User=bcapp-${id}`));
  }
});

test("no id can escape the apps directory", () => {
  // The path is built by concatenation, so this is the assertion that the id
  // regex is load-bearing rather than cosmetic.
  for (const id of ["k9depef6", "root", "aaaa", "9".repeat(16)]) {
    const p = appRoot(id);
    assert.ok(p.startsWith("/opt/boxcode-apps/"), p);
    assert.ok(!p.includes(".."), p);
    assert.equal(p.split("/").length, 4, `unexpected depth in ${p}`);
  }
});

test("an out-of-range port is refused", () => {
  for (const p of [0, 80, 443, 1023, 65536, -1, 1.5, "10000", null]) {
    assert.throws(() => renderUnit({ ...OK, port: p }), /invalid port/);
    assert.throws(() => renderNginxRoute(OK.id, p), /invalid port/);
  }
});

// ---- naming and routing ------------------------------------------------

test("the unit name matches what the kill switch looks for", () => {
  assert.match(unitName("k9depef6"), /^boxcode-app-[a-z2-9]{4,16}$/);
});

test("the unix user fits linux's 32-character limit", () => {
  assert.ok(userName("x".repeat(16).replace(/x/g, "a")).length <= 32);
});

test("the nginx route strips the prefix and carries websockets", () => {
  const r = renderNginxRoute("k9depef6", 10000);
  // ^~ so a longer regex location cannot steal it, and the trailing slash on
  // proxy_pass is what strips /api/<id>/ before the app sees the path.
  assert.match(r, /location \^~ \/api\/k9depef6\//);
  assert.match(r, /proxy_pass http:\/\/127\.0\.0\.1:10000\/;/);
  assert.match(r, /proxy_set_header Upgrade \$http_upgrade;/);
  assert.match(r, /proxy_http_version 1\.1;/);
});
