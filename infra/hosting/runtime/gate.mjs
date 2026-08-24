// Who may deploy, how often, and how much they may hold.
//
// The deploy endpoint is open -- no accounts, no signup. So assume it will be
// attacked, and that any control which can be bypassed will be. Four principles,
// carried over from the hosting plan:
//
//   defense in depth   no single control is load-bearing
//   fail closed        a check that cannot run refuses the deploy
//   server side only   anything the client enforces is UX, never security
//   attributable       every deploy is logged with token and source
//
// The pair that matters most is **A2 and A4 together**. A cap of two live
// projects per token is defeated by minting tokens; a cap on new tokens per
// address is defeated by one token taking every slot. Together, occupying the
// platform needs many addresses as well as patience.
//
// Pure, and deliberately so: this is the file where a mistake is a stranger
// running code on the box, and a function that reads a clock or a disk is one
// whose tests need a clock and a disk.

import { createHash } from "node:crypto";

/// A2. Two, not one: a person iterating on a frontend and its API legitimately
/// wants both alive at once. Three is where it starts being a way to hold the
/// platform.
export const MAX_APPS_PER_TOKEN = 2;

/// A3. Redeploying is normal and should not be punished; redeploying two
/// hundred times an hour is not a person.
export const DEPLOYS_PER_HOUR_PER_TOKEN = 5;
export const DEPLOYS_PER_DAY_PER_TOKEN = 20;

/// A4. The control that gives A2 its teeth.
export const TOKENS_PER_DAY_PER_SOURCE = 3;

/// A5. Above any one token's limit, because a shared office NAT is one address
/// with several honest people behind it.
export const DEPLOYS_PER_HOUR_PER_SOURCE = 10;

export const HOUR_MS = 3600_000;
export const DAY_MS = 24 * HOUR_MS;

export const ID_RE = /^[a-z2-9]{4,16}$/;
/// Tokens are 32 bytes of hex, minted by the client. Never stored in the clear.
export const TOKEN_RE = /^[0-9a-f]{64}$/;

/// Same normalisation infra/auth uses, for the same reason: a single IPv6
/// customer is handed a /64, so rate limiting a full address limits nothing.
export function sourceKey(address) {
  if (!address) return "unknown";
  if (address.startsWith("::ffff:")) return address.slice(7);
  if (!address.includes(":")) return address;
  return address.split(":").slice(0, 4).join(":") + "::/64";
}

/// Tokens are stored hashed. A registry file readable by anyone who gets a copy
/// of the disk should not hand them the ability to take over live projects.
export function hashToken(token) {
  if (typeof token !== "string" || !TOKEN_RE.test(token)) {
    throw new Error("refusing a token that is not 64 hex characters");
  }
  return createHash("sha256").update(token).digest("hex");
}

/// Constant-time compare, so a caller cannot learn a stored hash a byte at a
/// time from response timing.
function sameHash(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

const deny = (status, reason) => ({ allow: false, status, reason });

/// May this deploy proceed?
///
/// `state` is everything already known:
///   owners     { [id]: tokenHash }   who each project belongs to
///   history    [{ at, tokenHash, source, id }]  recent deploys
///   blocked    { tokens: [hash], sources: [key] }
///   registry   the live project registry, for A2 and capacity
///
/// Returns `{ allow, status, reason }`. `reason` is shown to the person
/// deploying, so it says what happened and what to do about it.
export function checkGate({ id, token, address, now, state } = {}) {
  // Order matters: cheapest and most definitive first, so an attacker learns as
  // little as possible and the box does as little work as possible.
  if (!Number.isInteger(now) || now <= 0) {
    // Every limit below is a window against the clock. Without one, none of
    // them mean anything, so nothing is allowed through.
    return deny(503, "the server could not read its own clock; try again shortly");
  }
  if (!state || typeof state !== "object") {
    return deny(503, "deploy state is unavailable; try again shortly");
  }

  if (typeof id !== "string" || !ID_RE.test(id)) {
    return deny(400, "that is not a valid project id");
  }

  let tokenHash;
  try {
    tokenHash = hashToken(token);
  } catch {
    return deny(400, "a deploy token is required");
  }

  const source = sourceKey(address);
  const owners = state.owners ?? {};
  const history = Array.isArray(state.history) ? state.history : [];
  const blocked = state.blocked ?? {};

  // A6. First, and without explanation: a blocked caller learns nothing about
  // why or about anything else.
  if ((blocked.tokens ?? []).some((h) => sameHash(h, tokenHash))) {
    return deny(403, "this deploy token has been blocked");
  }
  if ((blocked.sources ?? []).includes(source)) {
    return deny(403, "deploys from this address have been blocked");
  }

  // A1. Trust on first use. The first token to claim an id owns it; every
  // later deploy to that id must present the same one. Without this, an
  // eight-character guess replaces somebody's running server.
  const owner = owners[id];
  const isNewProject = owner === undefined;
  if (!isNewProject && !sameHash(owner, tokenHash)) {
    return deny(403, `project ${id} belongs to a different deploy token`);
  }

  const since = (ms) => history.filter((h) => Number.isInteger(h?.at) && now - h.at < ms);
  const byToken = (list) => list.filter((h) => sameHash(h.tokenHash, tokenHash));
  const bySource = (list) => list.filter((h) => h.source === source);

  // A4. Only when a token is about to claim its first project -- otherwise
  // every redeploy would count against a limit meant for token farming.
  if (isNewProject) {
    const mintedToday = new Set(
      bySource(since(DAY_MS)).filter((h) => h.newProject).map((h) => h.tokenHash),
    );
    if (!mintedToday.has(tokenHash) && mintedToday.size >= TOKENS_PER_DAY_PER_SOURCE) {
      return deny(429, `at most ${TOKENS_PER_DAY_PER_SOURCE} new projects a day from one address`);
    }
  }

  // A5 then A3: the address limit is checked first because it is the one that
  // bounds a single attacker holding many tokens.
  if (bySource(since(HOUR_MS)).length >= DEPLOYS_PER_HOUR_PER_SOURCE) {
    return deny(429, `at most ${DEPLOYS_PER_HOUR_PER_SOURCE} deploys an hour from one address`);
  }
  if (byToken(since(HOUR_MS)).length >= DEPLOYS_PER_HOUR_PER_TOKEN) {
    return deny(429, `at most ${DEPLOYS_PER_HOUR_PER_TOKEN} deploys an hour; try again shortly`);
  }
  if (byToken(since(DAY_MS)).length >= DEPLOYS_PER_DAY_PER_TOKEN) {
    return deny(429, `at most ${DEPLOYS_PER_DAY_PER_TOKEN} deploys a day`);
  }

  // A2. Counted from projects this token owns that are actually live -- a
  // redeploy of one it already holds is not a third project.
  if (isNewProject) {
    const live = Object.keys(state.registry?.projects ?? {})
      .filter((other) => sameHash(owners[other] ?? "", tokenHash));
    if (live.length >= MAX_APPS_PER_TOKEN) {
      return {
        allow: false,
        status: 429,
        reason:
          `this deploy token already has ${live.length} projects running ` +
          `(${live.sort().join(", ")}), which is the limit. ` +
          `Wait for one to expire, or deploy over one of them.`,
      };
    }
  }

  return { allow: true, status: 200, reason: isNewProject ? "new project" : "redeploy" };
}

/// The audit record for one deploy. A7.
///
/// `newProject` is what A4 counts, so it is recorded rather than recomputed --
/// working it out later would need the registry as it was at the time.
export function auditRecord({ id, token, address, now, newProject, outcome }) {
  return {
    at: now,
    id,
    tokenHash: hashToken(token),
    source: sourceKey(address),
    newProject: Boolean(newProject),
    outcome,
  };
}

/// Drop history that no limit can still see.
///
/// Called after every deploy. Without it this list grows for the life of the
/// box, and it is read on every request -- a slow denial of service built out
/// of ordinary use.
export function pruneHistory(history, now) {
  if (!Array.isArray(history) || !Number.isInteger(now)) return [];
  return history.filter((h) => Number.isInteger(h?.at) && now - h.at < DAY_MS);
}
