// A database and a login role per project.
//
// This is the reason the platform is not on Lambda: a real wire protocol, so
// Prisma, SQLAlchemy, the Django ORM and everything else work untouched against
// a plain DATABASE_URL.
//
// The isolation rule worth stating, because it is not the default and getting
// it wrong is invisible until someone looks:
//
//   **PostgreSQL grants CONNECT on every database to PUBLIC.** A fresh role can
//   therefore open any other project's database the moment it can reach the
//   port. Creating a role per project buys nothing on its own. What buys
//   something is revoking CONNECT from PUBLIC on each database and granting it
//   back only to that database's own role -- which is what provisionSql does,
//   and why it also revokes on `postgres` and `template1`.
//
// Identifiers cannot be parameterised in SQL, so the id is validated against an
// anchored pattern and then interpolated. That validation is the only thing
// standing between a project id and arbitrary SQL, so it is not merely a tidy
// check -- it is the control.
//
// Pure. Produces SQL text and connection strings; the shell runs them.

export const ID_RE = /^[a-z2-9]{4,16}$/;

/// Passwords are 32 bytes of hex from the caller. Validated here anyway,
/// because it ends up inside a SQL string literal and inside a URL.
export const PASSWORD_RE = /^[0-9a-f]{32,128}$/;

/// max_connections is 60 on this box, shared by ten projects, the control plane
/// and whatever a person has open during an incident. Five each leaves room for
/// all three; a project that exhausts its own pool is a bug in that project and
/// must not be able to exhaust everyone else's.
export const CONNECTION_LIMIT = 5;

/// A query that runs longer than this is not serving a demo page.
export const STATEMENT_TIMEOUT = "10s";

/// An ORM that opens a transaction and wanders off holds locks until this
/// fires. Without it one project's stuck transaction blocks its own future
/// migrations forever.
export const IDLE_IN_TRANSACTION_TIMEOUT = "60s";

function mustId(id) {
  if (typeof id !== "string" || !ID_RE.test(id)) {
    throw new Error(`refusing to build SQL for invalid project id ${JSON.stringify(id)}`);
  }
  return id;
}

function mustPassword(pw) {
  if (typeof pw !== "string" || !PASSWORD_RE.test(pw)) {
    // Deliberately does not echo the value into the error.
    throw new Error("refusing a password that is not 32-128 hex characters");
  }
  return pw;
}

/// Both are `app_<id>`. One name rather than two, so a person reading
/// `\l` or `\du` during an incident does not have to correlate anything.
export function dbName(id) {
  return `app_${mustId(id)}`;
}

export function roleName(id) {
  return `app_${mustId(id)}`;
}

/// What the guest gets as DATABASE_URL.
///
/// The host address is the guest's own gateway -- the host end of its
/// point-to-point link. Every slot has a different one, so this is per-project
/// rather than a constant.
export function databaseUrl({ id, password, slot, host = null, port = 5432 }) {
  mustId(id);
  mustPassword(password);
  if (host === null) {
    if (!Number.isInteger(slot) || slot < 0 || slot > 255) {
      throw new Error(`refusing to build a database URL for slot ${JSON.stringify(slot)}`);
    }
    host = `10.200.${slot}.1`;
  }
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`refusing database port ${JSON.stringify(port)}`);
  }
  return `postgresql://${roleName(id)}:${password}@${host}:${port}/${dbName(id)}`;
}

/// SQL run as the superuser to create or reset one project's database.
///
/// Returned as separate statements because CREATE DATABASE cannot run inside a
/// transaction block, and because the last group has to run connected to the
/// new database rather than to `postgres`.
export function provisionSql({ id, password }) {
  const db = dbName(id);
  const role = roleName(id);
  mustPassword(password);

  return {
    // Run against the `postgres` database.
    cluster: [
      // Rotated on every deploy rather than kept. A password that never changes
      // is one that outlives the project it belonged to, sitting in an image
      // and a shell history for as long as anyone keeps either.
      `DO $$ BEGIN
   IF EXISTS (SELECT FROM pg_roles WHERE rolname = '${role}') THEN
     EXECUTE format('ALTER ROLE %I LOGIN PASSWORD %L CONNECTION LIMIT ${CONNECTION_LIMIT}', '${role}', '${password}');
   ELSE
     EXECUTE format('CREATE ROLE %I LOGIN PASSWORD %L CONNECTION LIMIT ${CONNECTION_LIMIT}', '${role}', '${password}');
   END IF;
 END $$;`,
      `ALTER ROLE ${role} SET statement_timeout = '${STATEMENT_TIMEOUT}';`,
      `ALTER ROLE ${role} SET idle_in_transaction_session_timeout = '${IDLE_IN_TRANSACTION_TIMEOUT}';`,
      // No inheriting anything, no creating databases or roles of its own.
      `ALTER ROLE ${role} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;`,
    ],

    // Separate, because CREATE DATABASE cannot run inside a transaction block
    // and has no IF NOT EXISTS. The caller checks existence first -- an earlier
    // version used psql's \\gexec to do it in one statement, which works but
    // puts a meta-command in the middle of SQL that is otherwise portable and
    // testable as text.
    createDatabase: `CREATE DATABASE ${db} OWNER ${role};`,
    databaseExists: `SELECT 1 FROM pg_database WHERE datname = '${db}';`,

    // Run against `postgres`, after the database exists.
    grants: [
      // The line that actually isolates tenants. Without it PUBLIC keeps the
      // CONNECT that PostgreSQL grants by default, and every project's role can
      // open every other project's database.
      `REVOKE CONNECT ON DATABASE ${db} FROM PUBLIC;`,
      `GRANT CONNECT ON DATABASE ${db} TO ${role};`,
    ],

    // Run connected to the project's own database.
    inDatabase: [
      // Same default, one level down: PUBLIC can create objects in the public
      // schema of any database it can reach.
      `REVOKE ALL ON SCHEMA public FROM PUBLIC;`,
      `GRANT ALL ON SCHEMA public TO ${role};`,
      `ALTER SCHEMA public OWNER TO ${role};`,
    ],
  };
}

/// Run once when the box is provisioned, not per project.
///
/// Closes the same PUBLIC default on the databases that exist before any
/// project does. Without the template1 line every database created afterwards
/// starts with the hole reopened.
export function harden() {
  return [
    `REVOKE CONNECT ON DATABASE postgres FROM PUBLIC;`,
    `REVOKE CONNECT ON DATABASE template1 FROM PUBLIC;`,
    `REVOKE ALL ON SCHEMA public FROM PUBLIC;`,
  ];
}

/// Removing a project. Ordered so it is safe to run against a half-created
/// project, or twice.
export function dropSql(id) {
  const db = dbName(id);
  const role = roleName(id);
  return [
    // Sessions the guest left open would otherwise hold DROP DATABASE off
    // indefinitely, and the VM is already gone by the time this runs.
    `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '${db}';`,
    `DROP DATABASE IF EXISTS ${db};`,
    `DROP ROLE IF EXISTS ${role};`,
  ];
}

/// Addresses Postgres should listen on: the host end of every slot's link, plus
/// loopback. Never `*` -- that binds the public interface too, and the only
/// thing standing between that and the internet would be a security group.
export function listenAddresses(slotCount, prefix = "10.200") {
  if (!Number.isInteger(slotCount) || slotCount < 1 || slotCount > 256) {
    throw new Error(`refusing slot count ${JSON.stringify(slotCount)}`);
  }
  const addrs = ["localhost"];
  for (let s = 0; s < slotCount; s++) addrs.push(`${prefix}.${s}.1`);
  return addrs.join(",");
}
