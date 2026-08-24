import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MAX_APPS_PER_TOKEN, DEPLOYS_PER_HOUR_PER_TOKEN, DEPLOYS_PER_DAY_PER_TOKEN,
  TOKENS_PER_DAY_PER_SOURCE, DEPLOYS_PER_HOUR_PER_SOURCE, HOUR_MS, DAY_MS,
  sourceKey, hashToken, checkGate, auditRecord, pruneHistory,
} from "./gate.mjs";
import { empty, add } from "./registry.mjs";

const NOW = 1_700_000_000_000;
const T1 = "1".repeat(64);
const T2 = "2".repeat(64);
const T3 = "3".repeat(64);
const IP = "203.0.113.9";

const state = (over = {}) => ({ owners: {}, history: [], blocked: {}, registry: empty(), ...over });
const go = (over = {}) => checkGate({ id: "k9depef6", token: T1, address: IP, now: NOW, state: state(), ...over });
const deploys = (n, { token = T1, source = IP, at = NOW, newProject = false } = {}) =>
  Array.from({ length: n }, () => ({ at, tokenHash: hashToken(token), source, newProject }));

// ---- the happy path ------------------------------------------------------

test("a first deploy of a new project is allowed", () => {
  const r = go();
  assert.equal(r.allow, true);
  assert.equal(r.reason, "new project");
});

test("a redeploy by the owning token is allowed", () => {
  const r = go({ state: state({ owners: { k9depef6: hashToken(T1) } }) });
  assert.equal(r.allow, true);
  assert.equal(r.reason, "redeploy");
});

// ---- A1, the one that matters most --------------------------------------

test("a different token cannot take over a live project", () => {
  // Without this an eight-character guess replaces somebody's running server.
  const r = go({ token: T2, state: state({ owners: { k9depef6: hashToken(T1) } }) });
  assert.equal(r.allow, false);
  assert.equal(r.status, 403);
  assert.match(r.reason, /belongs to a different deploy token/);
});

test("tokens are never stored in the clear", () => {
  const h = hashToken(T1);
  assert.notEqual(h, T1);
  assert.match(h, /^[0-9a-f]{64}$/);
  assert.equal(hashToken(T1), h, "and hashing is stable");
  assert.notEqual(hashToken(T2), h);
});

test("a malformed token is refused before anything else happens", () => {
  for (const bad of ["", "short", "g".repeat(64), "A".repeat(64), T1 + "0", null, 42, {}]) {
    const r = go({ token: bad });
    assert.equal(r.allow, false, JSON.stringify(bad));
    assert.equal(r.status, 400);
  }
});

test("a malformed project id is refused", () => {
  for (const bad of ["abc", "A9depef6", "a b", "../../etc", "", "x".repeat(17), null, 42]) {
    assert.equal(go({ id: bad }).status, 400, JSON.stringify(bad));
  }
});

// ---- A2 and A4, the pair -------------------------------------------------

test("a token may hold only two live projects", () => {
  let reg = empty();
  reg = add(reg, { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  reg = add(reg, { id: "bbbb", slot: 1, runtime: "node", now: NOW });
  const owners = { aaaa: hashToken(T1), bbbb: hashToken(T1) };
  const r = go({ id: "cccc", state: state({ owners, registry: reg }) });
  assert.equal(r.allow, false);
  assert.equal(r.status, 429);
  // The message names them, so the person knows what to expire or overwrite.
  assert.match(r.reason, /aaaa, bbbb/);
  assert.match(r.reason, /Wait for one to expire, or deploy over one of them/);
});

test("redeploying one it already holds is not a third project", () => {
  let reg = empty();
  reg = add(reg, { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  reg = add(reg, { id: "bbbb", slot: 1, runtime: "node", now: NOW });
  const owners = { aaaa: hashToken(T1), bbbb: hashToken(T1) };
  assert.equal(go({ id: "aaaa", state: state({ owners, registry: reg }) }).allow, true);
});

test("another token's projects do not count against yours", () => {
  let reg = empty();
  reg = add(reg, { id: "aaaa", slot: 0, runtime: "node", now: NOW });
  reg = add(reg, { id: "bbbb", slot: 1, runtime: "node", now: NOW });
  const owners = { aaaa: hashToken(T2), bbbb: hashToken(T2) };
  assert.equal(go({ id: "cccc", state: state({ owners, registry: reg }) }).allow, true);
});

test("one address may mint only three tokens a day", () => {
  // A2 alone is defeated by minting tokens. This is what gives it teeth.
  const history = [
    ...deploys(1, { token: T1, newProject: true }),
    ...deploys(1, { token: T2, newProject: true }),
    ...deploys(1, { token: T3, newProject: true }),
  ];
  const fresh = "4".repeat(64);
  const r = go({ token: fresh, state: state({ history }) });
  assert.equal(r.allow, false);
  assert.equal(r.status, 429);
  assert.match(r.reason, new RegExp(`${TOKENS_PER_DAY_PER_SOURCE} new projects a day`));
});

test("a token that already minted today may still make another project", () => {
  // The limit is on distinct tokens per address, not on projects.
  const history = [
    ...deploys(1, { token: T1, newProject: true }),
    ...deploys(1, { token: T2, newProject: true }),
    ...deploys(1, { token: T3, newProject: true }),
  ];
  assert.equal(go({ token: T1, state: state({ history }) }).allow, true);
});

test("a redeploy is never counted against the token-minting limit", () => {
  const history = [
    ...deploys(1, { token: T1, newProject: true }),
    ...deploys(1, { token: T2, newProject: true }),
    ...deploys(1, { token: T3, newProject: true }),
  ];
  const fresh = "4".repeat(64);
  // Owned already, so this is a redeploy however new the token looks.
  const r = go({ token: fresh, state: state({ history, owners: { k9depef6: hashToken(fresh) } }) });
  assert.equal(r.allow, true);
});

test("another address's token minting does not count against yours", () => {
  const history = [
    ...deploys(1, { token: T1, source: "198.51.100.1", newProject: true }),
    ...deploys(1, { token: T2, source: "198.51.100.1", newProject: true }),
    ...deploys(1, { token: T3, source: "198.51.100.1", newProject: true }),
  ];
  assert.equal(go({ token: "4".repeat(64), state: state({ history }) }).allow, true);
});

// ---- A3 and A5, rate ------------------------------------------------------

test("a token is limited per hour and per day", () => {
  const hourly = go({ state: state({ history: deploys(DEPLOYS_PER_HOUR_PER_TOKEN) }) });
  assert.equal(hourly.status, 429);
  assert.match(hourly.reason, /an hour/);

  // Spread beyond the hour window but inside the day.
  const older = Array.from({ length: DEPLOYS_PER_DAY_PER_TOKEN }, (_, i) => ({
    at: NOW - HOUR_MS - i * 1000, tokenHash: hashToken(T1), source: IP, newProject: false,
  }));
  const daily = go({ state: state({ history: older }) });
  assert.equal(daily.status, 429);
  assert.match(daily.reason, /a day/);
});

test("an address is limited above any one token, for shared NATs", () => {
  // A shared office NAT is one address with several honest people behind it.
  assert.ok(DEPLOYS_PER_HOUR_PER_SOURCE > DEPLOYS_PER_HOUR_PER_TOKEN);
  const history = Array.from({ length: DEPLOYS_PER_HOUR_PER_SOURCE }, (_, i) => ({
    at: NOW - i, tokenHash: hashToken(`${i}`.repeat(64).slice(0, 64).replace(/[^0-9a-f]/g, "a")), source: IP,
  }));
  const r = go({ token: T2, state: state({ history }) });
  assert.equal(r.status, 429);
  assert.match(r.reason, /from one address/);
});

test("deploys outside the window no longer count", () => {
  const stale = deploys(DEPLOYS_PER_HOUR_PER_TOKEN, { at: NOW - HOUR_MS - 1 });
  assert.equal(go({ state: state({ history: stale }) }).allow, true);
  const ancient = deploys(DEPLOYS_PER_DAY_PER_TOKEN, { at: NOW - DAY_MS - 1 });
  assert.equal(go({ state: state({ history: ancient }) }).allow, true);
});

// ---- A6 -------------------------------------------------------------------

test("a blocked token or address is refused without explanation", () => {
  const byToken = go({ state: state({ blocked: { tokens: [hashToken(T1)] } }) });
  assert.equal(byToken.status, 403);
  assert.match(byToken.reason, /has been blocked/);

  const bySource = go({ state: state({ blocked: { sources: [IP] } }) });
  assert.equal(bySource.status, 403);
  // It says nothing about limits, ownership, or whether the project exists.
  assert.ok(!/limit|token belongs|project/.test(bySource.reason));
});

test("a block beats ownership", () => {
  const r = go({ state: state({ owners: { k9depef6: hashToken(T1) }, blocked: { tokens: [hashToken(T1)] } }) });
  assert.equal(r.status, 403);
  assert.match(r.reason, /blocked/);
});

// ---- fail closed ----------------------------------------------------------

test("an unreadable clock refuses everything", () => {
  // Every limit is a window against the clock. Without one, none of them mean
  // anything, so nothing gets through.
  for (const bad of [null, undefined, NaN, "now", 0, -1, 1.5]) {
    const r = go({ now: bad });
    assert.equal(r.allow, false, JSON.stringify(bad));
    assert.equal(r.status, 503);
  }
});

test("missing state refuses everything", () => {
  for (const bad of [null, undefined, 42, "state"]) {
    assert.equal(checkGate({ id: "k9depef6", token: T1, address: IP, now: NOW, state: bad }).status, 503);
  }
  assert.equal(checkGate().allow, false);
});

test("a damaged history does not open the gate", () => {
  // Entries without a usable timestamp must not silently drop out of a window
  // and let a limit be exceeded.
  const history = [
    ...deploys(DEPLOYS_PER_HOUR_PER_TOKEN),
    { at: "recently", tokenHash: hashToken(T1), source: IP },
    null,
  ];
  assert.equal(go({ state: state({ history }) }).allow, false);
});

// ---- addresses ------------------------------------------------------------

test("an IPv6 customer is limited by their /64, not by one address", () => {
  // A single customer is handed a /64, so limiting a full address limits nothing.
  const a = sourceKey("2001:db8:1234:5678:aaaa:bbbb:cccc:dddd");
  const b = sourceKey("2001:db8:1234:5678:9999:8888:7777:6666");
  assert.equal(a, b);
  assert.match(a, /::\/64$/);
});

test("IPv4-mapped addresses are treated as IPv4", () => {
  assert.equal(sourceKey("::ffff:203.0.113.9"), "203.0.113.9");
  assert.equal(sourceKey("203.0.113.9"), "203.0.113.9");
});

test("a missing address still produces a key, so limits still apply", () => {
  // Falling back to no limit at all would be the wrong direction entirely.
  assert.equal(sourceKey(undefined), "unknown");
  assert.equal(sourceKey(""), "unknown");
});

// ---- audit and pruning ----------------------------------------------------

test("the audit record carries what a ban decision needs", () => {
  const r = auditRecord({ id: "k9depef6", token: T1, address: IP, now: NOW, newProject: true, outcome: "ok" });
  assert.equal(r.id, "k9depef6");
  assert.equal(r.tokenHash, hashToken(T1));
  assert.equal(r.source, IP);
  assert.equal(r.newProject, true);
  assert.equal(r.at, NOW);
  // Never the token itself.
  assert.ok(!JSON.stringify(r).includes(T1));
});

test("history is pruned to what a limit can still see", () => {
  // It is read on every request; unbounded growth is a slow denial of service
  // built out of ordinary use.
  const h = [
    { at: NOW - 1 }, { at: NOW - DAY_MS + 1 },
    { at: NOW - DAY_MS - 1 }, { at: "junk" }, null,
  ];
  const kept = pruneHistory(h, NOW);
  assert.equal(kept.length, 2);
  assert.deepEqual(pruneHistory("not a list", NOW), []);
  assert.deepEqual(pruneHistory([], "not a clock"), []);
});
