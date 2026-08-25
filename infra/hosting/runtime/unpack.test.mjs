import { test } from "node:test";
import assert from "node:assert/strict";
import { MAX_FILES, MAX_FILE_BYTES, pathProblem, validate, resolveUnder } from "./unpack.mjs";

const b64 = (s) => Buffer.from(s).toString("base64");
const ok = (path, body = "x") => ({ path, content: b64(body) });

// ---- a path is an instruction, not a name --------------------------------

test("a path that climbs out is refused, however it is spelled", () => {
  // This is the whole reason the module exists. `../../../etc/cron.d/x` is a
  // valid string that a naive join turns into root on a box hosting other
  // people's projects.
  for (const p of [
    "../etc/passwd", "a/../../etc/passwd", "..", "../", "a/..",
    "./../x", "a/./../../x", "foo/../../bar",
  ]) {
    assert.ok(pathProblem(p), `${p} must be refused`);
  }
});

test("an absolute path or a drive letter is refused", () => {
  for (const p of ["/etc/passwd", "/", "C:/x", "c:\\x"]) {
    assert.ok(pathProblem(p), `${p} must be refused`);
  }
});

test("separators and control characters that are not what they look like", () => {
  assert.ok(pathProblem("a\\b"), "a backslash is not a separator here");
  assert.ok(pathProblem("a\0b"), "null byte");
  assert.ok(pathProblem("a\nb"), "newline");
  assert.ok(pathProblem("a\tb"), "tab");
  assert.ok(pathProblem("a//b"), "empty component");
  assert.ok(pathProblem("a/./b"), "dot component");
});

test("ordinary project paths are allowed", () => {
  // Including the ones that merely look suspicious.
  for (const p of [
    "server.js", "src/index.js", "src/routes/api/users.js",
    ".gitignore", "..config", "a..b", "my file.js", "Dockerfile",
    "src/[id]/page.tsx", "a-b_c.123.js",
  ]) {
    assert.equal(pathProblem(p), null, `${p} should be allowed`);
  }
});

test("a path is not accepted merely for being long or empty", () => {
  assert.ok(pathProblem(""));
  assert.ok(pathProblem("a".repeat(256)));
  assert.equal(pathProblem("a".repeat(255)), null);
  for (const bad of [null, undefined, 42, {}, []]) assert.ok(pathProblem(bad));
});

// ---- the payload as a whole ----------------------------------------------

test("a good payload passes and reports what it holds", () => {
  const r = validate([ok("server.js", "hello"), ok("src/a.js", "world")]);
  assert.equal(r.ok, true);
  assert.equal(r.files, 2);
  assert.ok(r.bytes > 0);
});

test("one bad path rejects the whole payload", () => {
  // All-or-nothing on purpose: a half-written project is one the build stage
  // would happily proceed with, producing an image missing files nobody
  // mentioned.
  const r = validate([ok("server.js"), ok("../../etc/passwd")]);
  assert.equal(r.ok, false);
  assert.match(r.error, /climbs out/);
});

test("the same path twice is refused rather than letting one win", () => {
  const r = validate([ok("a.js"), ok("a.js")]);
  assert.equal(r.ok, false);
  assert.match(r.error, /more than once/);
  // Case-insensitively, because two entries differing only in case collide on
  // a case-insensitive filesystem and silently become one.
  assert.equal(validate([ok("A.js"), ok("a.js")]).ok, false);
});

test("content must be base64, and must be there", () => {
  assert.match(validate([{ path: "a.js" }]).error, /no content/);
  assert.match(validate([{ path: "a.js", content: 42 }]).error, /no content/);
  assert.match(validate([{ path: "a.js", content: "not base64!!" }]).error, /not valid base64/);
});

test("limits are enforced here, not only in the client", () => {
  // The client checks the same things. That is UX. This is the security check,
  // because the client is not the only thing that can POST to this endpoint.
  const many = Array.from({ length: MAX_FILES + 1 }, (_, i) => ok(`f${i}.js`));
  assert.match(validate(many).error, new RegExp(`limit is ${MAX_FILES}`));

  const huge = { path: "big.bin", content: "A".repeat(Math.ceil((MAX_FILE_BYTES + 1024) * 4 / 3)) };
  assert.match(validate([huge]).error, /per-file limit/);
});

test("an empty or malformed payload is refused", () => {
  assert.match(validate([]).error, /no files/);
  assert.match(validate("files").error, /must be an array/);
  assert.match(validate([null]).error, /not an object/);
});

// ---- resolving ------------------------------------------------------------

test("a resolved path always stays under its root", () => {
  const root = "/opt/boxcode-apps/k9depef6/src";
  assert.equal(resolveUnder(root, "server.js"), `${root}/server.js`);
  assert.equal(resolveUnder(root, "src/a/b.js"), `${root}/src/a/b.js`);
  assert.ok(resolveUnder(`${root}/`, "a.js").startsWith(root), "a trailing slash on root is handled");
});

test("resolving throws rather than returning something usable", () => {
  // A caller that forgot to run validate first must not get a path back.
  const root = "/opt/boxcode-apps/k9depef6/src";
  for (const p of ["../x", "/etc/passwd", "a/../../x", ""]) {
    assert.throws(() => resolveUnder(root, p), /refusing path/, p);
  }
  assert.throws(() => resolveUnder("relative/root", "a.js"), /refusing root/);
});
