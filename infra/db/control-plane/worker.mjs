// One SQLite worker. Opens a project's file, runs one statement, posts the
// result back, and does nothing else.
//
// This exists because `node:sqlite`'s `DatabaseSync` is exactly what its name
// says: every call blocks the thread it runs on until SQLite returns. Run that
// on the control-plane's only thread -- which is what this service did before
// -- and one slow query from one project stops every other project's requests
// from being read off the socket at all. Not slowed: not served, because the
// event loop is not running.
//
// Moving the blocking call to a pool of threads is the whole fix. The main
// thread never touches SQLite again, so it stays responsive no matter what a
// query is doing, and a pathological statement costs one worker rather than
// the service.
//
// What this deliberately does NOT claim: that a query can be *cancelled*.
// `DatabaseSync` exposes `open close prepare exec function createSession
// applyChangeset enableLoadExtension loadExtension` and nothing else -- no
// interrupt, no progress handler, no busy timeout. SQLite's own
// `sqlite3_interrupt` is not reachable from here. So when the pool gives up on
// a job it terminates this thread, and V8 stops JavaScript as soon as it can --
// but a native `sqlite3_step` already in flight runs to completion first. The
// caller gets its answer on time either way; the thread is simply not reusable
// until the native call returns. See the pool in index.mjs.

import { parentPort, workerData } from "node:worker_threads";
import { DatabaseSync } from "node:sqlite";
import { statSync } from "node:fs";
import path from "node:path";

const { dataDir, maxRows, namedQueriesTable, maxDbBytes } = workerData;

// Copied rather than imported: this file is loaded as a worker entry point,
// and importing index.mjs would re-run the HTTP server in every thread.
function isReadStatement(sql) {
  const head = sql.trim().slice(0, 10).toUpperCase();
  return head.startsWith("SELECT") || head.startsWith("PRAGMA") || head.startsWith("EXPLAIN");
}

// Statements that can only ever make the file smaller. These stay allowed at
// the size cap, and that is not a nicety: refusing them would leave a project
// that hit the limit with no statement it could run to get back under it --
// full, and permanently unable to do anything about it. Reads are already
// allowed for the same reason, so the owner can see what to delete.
//
// UPDATE is deliberately not on this list. It can shrink a row or grow one,
// and there is no way to know which before running it, so it is treated as
// growth. Delete the rows and insert them again.
function isReclaimStatement(sql) {
  const head = sql.trim().slice(0, 6).toUpperCase();
  return head.startsWith("DELETE") || head.startsWith("DROP") || head.startsWith("VACUUM");
}

function dbPathFor(projectId) {
  return path.join(dataDir, `${projectId}.sqlite`);
}

// Bytes the project's file currently occupies, or 0 when it does not exist
// yet. Only ever consulted before a write -- a project that is already over
// the cap must still be able to read its data back out, and to DELETE its way
// back under the line.
function currentSize(projectId) {
  try {
    return statSync(dbPathFor(projectId)).size;
  } catch {
    return 0;
  }
}

function runQuery({ projectId, sql, params, namedParams }) {
  // Checked here rather than in the main thread because it is a filesystem
  // stat, and the main thread does no blocking I/O by design now.
  if (!isReadStatement(sql) && !isReclaimStatement(sql) && maxDbBytes > 0) {
    const size = currentSize(projectId);
    if (size >= maxDbBytes) {
      const err = new Error(
        `this project's database is ${Math.round(size / 1048576)} MB, at the ` +
          `${Math.round(maxDbBytes / 1048576)} MB limit. DELETE, DROP and VACUUM still ` +
          `work -- free some space, then write again.`
      );
      err.overCapacity = true;
      throw err;
    }
  }

  const db = new DatabaseSync(dbPathFor(projectId));
  try {
    const stmt = db.prepare(sql);
    const args = namedParams ? [namedParams, ...params] : params;
    if (isReadStatement(sql)) {
      const rows = stmt.all(...args);
      const truncated = rows.length > maxRows;
      return { rows: truncated ? rows.slice(0, maxRows) : rows, truncated };
    }
    const result = stmt.run(...args);
    return { changes: result.changes, last_insert_rowid: Number(result.lastInsertRowid) };
  } finally {
    db.close();
  }
}

// A missing table (nothing registered yet) and a missing row (wrong name) both
// resolve to `null` -- a public caller does not need to know which, only that
// there is nothing to run under that name.
function lookupNamedQuery({ projectId, name }) {
  const db = new DatabaseSync(dbPathFor(projectId));
  try {
    const row = db.prepare(`SELECT sql FROM ${namedQueriesTable} WHERE name = ?`).get(name);
    return row ? row.sql : null;
  } catch {
    return null;
  } finally {
    db.close();
  }
}

parentPort.on("message", (job) => {
  try {
    const value = job.op === "lookup" ? lookupNamedQuery(job) : runQuery(job);
    parentPort.postMessage({ id: job.id, ok: true, value });
  } catch (e) {
    // The message travels, not the Error: a structured-clone of an Error
    // loses custom properties, and `overCapacity` is what tells the main
    // thread to answer 413 rather than 400.
    parentPort.postMessage({
      id: job.id,
      ok: false,
      message: e.message,
      overCapacity: e.overCapacity === true,
    });
  }
});
