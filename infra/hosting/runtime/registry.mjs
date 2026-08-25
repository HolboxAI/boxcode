// Which projects exist, which slot each holds, and when each expires.
//
// Persisted as one JSON file on the box, in the shape infra/auth's control-plane
// already uses -- that service has been keeping a registry like this in
// production long enough to have learned what goes wrong with it, and the
// lessons are the same here.
//
// The one worth restating: this file is read at startup and is the only thing
// that knows a microVM belongs to us. It is therefore also the thing that will
// eventually be truncated by a power loss mid-write, or hand-edited during an
// incident. So `parse` never throws on a bad file -- it keeps every entry that
// is intact, discards the rest with a reason, and lets reconciliation deal with
// the consequences. A control plane that refuses to start because one entry is
// malformed takes the whole platform down to protect one project.
//
// Pure. No file I/O, no clock -- the caller passes what it read and what time
// it is, so the tests need neither.

import { SLOT_COUNT } from "./network.mjs";
import { BUILD_SLOT } from "./build.mjs";

export const REGISTRY_VERSION = 1;

/// How long a project lives. Matches the artifact lifecycle, so a frontend and
/// its backend disappear together rather than leaving a page whose API is gone.
export const TTL_HOURS = 48;

/// The port the app listens on inside its guest. Fixed rather than allocated:
/// every guest has its own address, so there is nothing to collide with, and a
/// fixed port means PORT is the same in every project's environment.
export const GUEST_PORT = 8080;

export const ID_RE = /^[a-z2-9]{4,16}$/;

/// Slots an app may hold. The build slot is not one of them -- an app parked
/// there would be evicted by the next deploy, and the build's namespace and NAT
/// are not what an app should be sitting in.
export function appSlots() {
  return Array.from({ length: SLOT_COUNT }, (_, i) => i).filter((s) => s !== BUILD_SLOT);
}

export function empty() {
  return { version: REGISTRY_VERSION, projects: {} };
}

/// Read a registry, keeping whatever is intact.
///
/// Returns `{ registry, dropped }`. `dropped` is a list of `{ id, why }` that
/// the caller logs -- during an incident "why is my project gone" needs an
/// answer that is written down, not inferred.
export function parse(text) {
  const dropped = [];
  let raw;
  try {
    raw = JSON.parse(text ?? "");
  } catch {
    // A truncated write, or a file that was never created. Both are the same
    // recoverable situation: start from nothing and let reconciliation stop
    // whatever is still running.
    return { registry: empty(), dropped: [{ id: null, why: "registry file was missing or not valid JSON" }] };
  }
  // Array.isArray is checked explicitly because `typeof [] === "object"`, so an
  // array passes every other test here and then yields an empty project list --
  // a corrupt registry that would have looked like an empty one, and been
  // reported as nothing wrong.
  if (
    !raw || typeof raw !== "object" || Array.isArray(raw) ||
    typeof raw.projects !== "object" || raw.projects === null || Array.isArray(raw.projects)
  ) {
    return { registry: empty(), dropped: [{ id: null, why: "registry file had an unrecognised shape" }] };
  }

  const registry = empty();
  const seenSlots = new Map();

  for (const [id, entry] of Object.entries(raw.projects)) {
    const why = badEntry(id, entry);
    if (why) {
      dropped.push({ id, why });
      continue;
    }
    // Two projects claiming one slot cannot both be right, and guessing which
    // would mean pointing nginx at the wrong tenant's guest. Keep the older
    // one -- it is the one more likely to be actually running.
    const other = seenSlots.get(entry.slot);
    if (other) {
      const loser = entry.createdAt >= registry.projects[other].createdAt ? id : other;
      const winner = loser === id ? other : id;
      dropped.push({ id: loser, why: `slot ${entry.slot} was also claimed by ${winner}` });
      if (loser === other) {
        delete registry.projects[other];
        seenSlots.set(entry.slot, id);
        registry.projects[id] = normalise(entry);
      }
      continue;
    }
    seenSlots.set(entry.slot, id);
    registry.projects[id] = normalise(entry);
  }

  return { registry, dropped };
}

function normalise(e) {
  return {
    slot: e.slot,
    runtime: e.runtime,
    createdAt: e.createdAt,
    expiresAt: e.expiresAt,
  };
}

function badEntry(id, e) {
  if (!ID_RE.test(id)) return "not a valid project id";
  if (!e || typeof e !== "object") return "entry was not an object";
  if (!Number.isInteger(e.slot)) return "slot was not a whole number";
  if (e.slot === BUILD_SLOT) return `slot ${BUILD_SLOT} is reserved for builds`;
  if (!appSlots().includes(e.slot)) return `slot ${e.slot} is out of range`;
  if (e.runtime !== "node" && e.runtime !== "python") return `unknown runtime ${JSON.stringify(e.runtime)}`;
  if (!Number.isInteger(e.createdAt) || e.createdAt <= 0) return "createdAt was not a timestamp";
  if (!Number.isInteger(e.expiresAt) || e.expiresAt <= 0) return "expiresAt was not a timestamp";
  return null;
}

/// The lowest free slot, or null when the box is full.
///
/// Lowest rather than random so that a box hosting three projects uses slots
/// 0, 1 and 2 -- which makes `ip addr` and `ps` legible during an incident,
/// where a scattered allocation would not be.
export function allocateSlot(registry) {
  const taken = new Set(Object.values(registry.projects).map((p) => p.slot));
  for (const s of appSlots()) if (!taken.has(s)) return s;
  return null;
}

export function add(registry, { id, slot, runtime, now, ttlHours = TTL_HOURS }) {
  if (!ID_RE.test(id ?? "")) throw new Error(`invalid project id ${JSON.stringify(id)}`);
  if (!appSlots().includes(slot)) throw new Error(`invalid slot ${JSON.stringify(slot)}`);
  if (runtime !== "node" && runtime !== "python") throw new Error(`invalid runtime ${JSON.stringify(runtime)}`);
  if (!Number.isInteger(now) || now <= 0) throw new Error(`invalid clock ${JSON.stringify(now)}`);

  const taken = Object.entries(registry.projects).find(([other, p]) => p.slot === slot && other !== id);
  if (taken) throw new Error(`slot ${slot} is held by ${taken[0]}`);

  return {
    ...registry,
    projects: {
      ...registry.projects,
      [id]: { slot, runtime, createdAt: now, expiresAt: now + ttlHours * 3600_000 },
    },
  };
}

export function remove(registry, id) {
  const projects = { ...registry.projects };
  delete projects[id];
  return { ...registry, projects };
}

/// Ids past their expiry.
export function expiredIds(registry, now) {
  if (!Number.isInteger(now)) {
    // A clock we cannot read is not a reason to delete anything.
    return [];
  }
  return Object.entries(registry.projects)
    .filter(([, p]) => p.expiresAt <= now)
    .map(([id]) => id);
}

/// What is left of a project's life, for the message a refused deploy shows.
export function nextExpiry(registry, now) {
  const times = Object.values(registry.projects)
    .map((p) => p.expiresAt)
    .filter((t) => t > now)
    .sort((a, b) => a - b);
  return times.length ? times[0] : null;
}

export function serialise(registry) {
  return `${JSON.stringify({ ...registry, version: REGISTRY_VERSION }, null, 2)}\n`;
}
