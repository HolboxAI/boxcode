import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CONNECTION_LIMIT, PASSWORD_RE,
  dbName, roleName, databaseUrl, provisionSql, harden, dropSql, listenAddresses,
} from "./database.mjs";
import { SLOT_COUNT, slotSubnet } from "./network.mjs";

const PW = "a".repeat(64);
const ID = "k9depef6";
const all = (id = ID, password = PW) => {
  const s = provisionSql({ id, password });
  return [...s.cluster, s.createDatabase, ...s.grants, ...s.inDatabase].join("\n");
};

// ---- the isolation that is not the default -------------------------------

test("PUBLIC loses CONNECT on the project's database", () => {
  // PostgreSQL grants CONNECT on every database to PUBLIC. Without revoking it,
  // creating a role per project buys nothing: any role can open any database it
  // can reach. This is the line that isolates tenants.
  const sql = all();
  assert.match(sql, new RegExp(`REVOKE CONNECT ON DATABASE ${dbName(ID)} FROM PUBLIC`));
  assert.match(sql, new RegExp(`GRANT CONNECT ON DATABASE ${dbName(ID)} TO ${roleName(ID)}`));
  // The revoke has to come before the grant, or it takes the grant away again.
  assert.ok(sql.indexOf("REVOKE CONNECT") < sql.indexOf("GRANT CONNECT"));
});

test("PUBLIC loses the public schema too", () => {
  // The same default one level down: PUBLIC can create objects in the public
  // schema of any database it can reach.
  const sql = all();
  assert.match(sql, /REVOKE ALL ON SCHEMA public FROM PUBLIC/);
  assert.match(sql, new RegExp(`GRANT ALL ON SCHEMA public TO ${roleName(ID)}`));
  assert.ok(sql.indexOf("REVOKE ALL ON SCHEMA") < sql.indexOf("GRANT ALL ON SCHEMA"));
});

test("the databases that exist before any project are hardened too", () => {
  const h = harden().join("\n");
  assert.match(h, /REVOKE CONNECT ON DATABASE postgres FROM PUBLIC/);
  // Without template1, every database created afterwards starts with the hole
  // reopened.
  assert.match(h, /REVOKE CONNECT ON DATABASE template1 FROM PUBLIC/);
});

test("a project role has no privileges beyond its own database", () => {
  const sql = all();
  assert.match(sql, /NOSUPERUSER/);
  assert.match(sql, /NOCREATEDB/);
  assert.match(sql, /NOCREATEROLE/);
  assert.match(sql, /NOREPLICATION/);
  assert.match(sql, /NOBYPASSRLS/);
});

test("a project cannot exhaust everyone else's connections", () => {
  // max_connections is 60, shared by ten projects, the control plane, and a
  // person during an incident.
  assert.match(all(), new RegExp(`CONNECTION LIMIT ${CONNECTION_LIMIT}`));
  assert.ok(CONNECTION_LIMIT * 10 < 60, "ten projects must not be able to take the lot");
});

test("a runaway query and a stuck transaction both end", () => {
  const sql = all();
  assert.match(sql, /statement_timeout = '10s'/);
  // Without this, one project's abandoned transaction holds locks that block
  // its own future migrations forever.
  assert.match(sql, /idle_in_transaction_session_timeout = '60s'/);
});

// ---- the connection string -----------------------------------------------

test("the guest is pointed at its own gateway, not a shared address", () => {
  for (const slot of [0, 1, 9, SLOT_COUNT - 1]) {
    const url = databaseUrl({ id: ID, password: PW, slot });
    assert.match(url, new RegExp(`@${slotSubnet(slot).hostIp.replace(/\./g, "\\.")}:5432/`));
  }
  // Two projects never get the same host.
  assert.notEqual(databaseUrl({ id: ID, password: PW, slot: 0 }), databaseUrl({ id: ID, password: PW, slot: 1 }));
});

test("the url names the project's own database and role", () => {
  const url = databaseUrl({ id: ID, password: PW, slot: 3 });
  assert.match(url, new RegExp(`^postgresql://${roleName(ID)}:`));
  assert.match(url, new RegExp(`/${dbName(ID)}$`));
});

test("database and role share one name, so \\l and \\du line up", () => {
  assert.equal(dbName(ID), roleName(ID));
  assert.match(dbName(ID), /^app_[a-z2-9]{4,16}$/);
});

// ---- listening -----------------------------------------------------------

test("postgres never listens on a wildcard", () => {
  // '*' binds the public interface, leaving only a security group between the
  // database and the internet.
  const a = listenAddresses(SLOT_COUNT);
  assert.ok(!a.includes("*"), a);
  assert.ok(!a.includes("0.0.0.0"), a);
  assert.match(a, /^localhost,/);
});

test("it listens on every app slot's gateway and no more", () => {
  const a = listenAddresses(SLOT_COUNT).split(",");
  assert.equal(a.length, SLOT_COUNT + 1, "one per slot, plus loopback");
  for (let s = 0; s < SLOT_COUNT; s++) {
    assert.ok(a.includes(slotSubnet(s).hostIp), `missing ${slotSubnet(s).hostIp}`);
  }
});

test("the build slot is excluded when asked", () => {
  // Its gateway lives inside a network namespace, so the host cannot bind it --
  // and a build VM has no business reaching a project's database.
  const a = listenAddresses(SLOT_COUNT, "10.200", 15).split(",");
  assert.ok(!a.includes("10.200.15.1"), a.join(","));
  assert.ok(a.includes("10.200.0.1"));
  assert.equal(a.length, SLOT_COUNT, "one fewer than with the build slot in");
});

test("an absurd slot count is refused", () => {
  for (const bad of [0, -1, 257, 1.5, "16", null]) {
    assert.throws(() => listenAddresses(bad), /refusing slot count/);
  }
});

// ---- identifiers cannot be parameterised, so validation is the control ----

test("a hostile project id is refused everywhere", () => {
  // SQL identifiers cannot be bound as parameters, so this validation is the
  // only thing between a project id and arbitrary SQL.
  for (const bad of [
    "a'; DROP DATABASE postgres; --",
    'a" OR 1=1 --',
    "app_x; GRANT ALL",
    "../../etc", "a b", "", "A9depef6", "x".repeat(17), "abc", null, 42, {},
  ]) {
    assert.throws(() => dbName(bad), /invalid project id/, JSON.stringify(bad));
    assert.throws(() => roleName(bad), /invalid project id/);
    assert.throws(() => provisionSql({ id: bad, password: PW }), /invalid project id/);
    assert.throws(() => databaseUrl({ id: bad, password: PW, slot: 0 }), /invalid project id/);
  }
});

test("only a real project id survives into the SQL", () => {
  // Belt and braces on the above: whatever is interpolated must match the
  // pattern, so a future edit that loosened the check would fail here.
  const sql = all();
  for (const m of sql.matchAll(/app_([A-Za-z0-9_]*)/g)) {
    assert.match(m[1], /^[a-z2-9]{4,16}$/, `interpolated ${JSON.stringify(m[0])}`);
  }
});

test("a password that is not hex is refused, and never echoed", () => {
  for (const bad of ["short", "g".repeat(32), "a'; DROP DATABASE postgres; --", "", null, 42, "A".repeat(64)]) {
    assert.throws(() => provisionSql({ id: ID, password: bad }), /32-128 hex/, JSON.stringify(bad));
    assert.throws(() => databaseUrl({ id: ID, password: bad, slot: 0 }), /32-128 hex/);
  }
  // The failure message must not leak the value it rejected into a log.
  try {
    provisionSql({ id: ID, password: "secretish-value" });
    assert.fail("should have thrown");
  } catch (e) {
    assert.ok(!e.message.includes("secretish-value"), e.message);
  }
});

test("the password shape the generator produces is accepted", () => {
  // 32 bytes of hex is 64 characters.
  assert.ok(PASSWORD_RE.test("0123456789abcdef".repeat(4)));
  assert.doesNotThrow(() => databaseUrl({ id: ID, password: "0123456789abcdef".repeat(4), slot: 0 }));
});

// ---- teardown ------------------------------------------------------------

test("dropping is safe to run twice, or on a half-created project", () => {
  const d = dropSql(ID).join("\n");
  assert.match(d, /DROP DATABASE IF EXISTS/);
  assert.match(d, /DROP ROLE IF EXISTS/);
});

test("open sessions are terminated before the database is dropped", () => {
  // Sessions the guest left behind would hold DROP DATABASE off indefinitely,
  // and the VM is already gone by the time this runs.
  const d = dropSql(ID);
  assert.match(d[0], /pg_terminate_backend/);
  assert.ok(d.findIndex((s) => /pg_terminate_backend/.test(s)) < d.findIndex((s) => /DROP DATABASE/.test(s)));
});

test("creating the database is separate, because it cannot be in a transaction", () => {
  const s = provisionSql({ id: ID, password: PW });
  assert.equal(s.createDatabase, `CREATE DATABASE ${dbName(ID)} OWNER ${roleName(ID)};`);
  assert.match(s.databaseExists, /SELECT 1 FROM pg_database/);
  // And it must not be buried in a list the caller runs blindly -- CREATE
  // DATABASE has no IF NOT EXISTS, so the caller has to check first.
  assert.ok(!s.cluster.some((x) => /CREATE DATABASE/.test(x)));
  assert.ok(!s.grants.some((x) => /CREATE DATABASE/.test(x)));
});

test("no psql meta-command leaks into the SQL", () => {
  // An earlier version used \gexec, which works but puts a meta-command in the
  // middle of SQL that is otherwise portable and testable as plain text.
  assert.ok(!all().includes("\\g"), "SQL should be SQL");
});

test("dropping refuses a hostile id too", () => {
  for (const bad of ["a'; DROP DATABASE postgres; --", "", null]) {
    assert.throws(() => dropSql(bad), /invalid project id/);
  }
});
