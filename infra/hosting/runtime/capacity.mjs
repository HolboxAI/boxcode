// Whether there is room for one more app, decided from what the box actually
// has free rather than from a number somebody guessed.
//
// The brief was "run as many as we can, and when we can't, say so". A fixed
// count cannot do that: ten tiny FastAPI services and ten Next.js servers are
// the same number and nothing like the same load. So admission reads
// MemAvailable and refuses when accepting would eat the reserves.
//
// It fails closed, and that direction is deliberate. Every other decision in
// this codebase that could not read its inputs holds; this one refuses,
// because "admit" means letting a stranger's code start running on the box and
// there is no safe way to do that on a guess.
//
// Pure. No /proc read, no clock -- the caller passes what it measured, so the
// tests need neither.

/// Per-app ceiling, enforced by the systemd unit's MemoryMax. Generous for a
/// demo backend: Express and FastAPI idle around 50-80 MB, Django around 150.
/// Next.js SSR at 150-250 MB is the one that gets close.
export const APP_MEMORY_MB = 256;

/// Never lent out. The kernel, nginx, Postgres and the control-plane live here,
/// and a box that has swapped its own control-plane out cannot reap, cannot
/// refuse, and cannot be recovered without a console.
export const SYSTEM_RESERVE_MB = 300;

/// A deploy has to be able to run `npm install`, which peaks far above what the
/// app it produces will ever use. Without this reserve the tenth deploy
/// succeeds and then OOM-kills a running app while building.
export const BUILD_RESERVE_MB = 512;

/// Backstop for the case where memory accounting flatters us -- lots of
/// reclaimable cache, apps that have not yet touched their working set. Ten is
/// the expected number on a t3.medium; this is the ceiling, not the target.
export const HARD_CAP = 14;

/// Disk is the outage this box will actually have: it takes out every app and
/// Postgres at once, and it is caused by ordinary use rather than by anything
/// going wrong. Cheaper to refuse a deploy than to lose ten.
export const MIN_DISK_FREE_MB = 5 * 1024;

function num(v) {
  if (typeof v === "number") return Number.isFinite(v) ? v : NaN;
  if (typeof v === "string" && v.trim() !== "") return Number(v);
  return NaN;
}

function humanise(ms) {
  if (!Number.isFinite(ms) || ms <= 0) return null;
  const m = Math.round(ms / 60000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 === 0 ? `${h}h` : `${h}h ${m % 60}m`;
}

/// Can one more app start right now?
///
/// `running` is the count of live apps, `expiresAt` their expiry timestamps in
/// ms, used only to tell a refused caller when to come back. Returns
/// `{ admit, reason }` -- `reason` is shown to the person deploying, so it says
/// what happened and what to do, not just "no".
export function canAdmit({
  memAvailableMB,
  diskFreeMB,
  running,
  expiresAt = [],
  now = 0,
} = {}) {
  const mem = num(memAvailableMB);
  const disk = num(diskFreeMB);
  const live = num(running);

  if (!Number.isFinite(live) || live < 0) {
    return { admit: false, reason: "cannot tell how many apps are running; refusing rather than guessing" };
  }
  if (live >= HARD_CAP) {
    return { admit: false, reason: `at the hard limit of ${HARD_CAP} apps${whenFree(expiresAt, now)}` };
  }
  if (!Number.isFinite(mem)) {
    return { admit: false, reason: "cannot read available memory; refusing rather than guessing" };
  }
  if (!Number.isFinite(disk)) {
    return { admit: false, reason: "cannot read free disk; refusing rather than guessing" };
  }

  if (disk < MIN_DISK_FREE_MB) {
    return {
      admit: false,
      reason: `only ${Math.round(disk)} MB of disk free, and ${MIN_DISK_FREE_MB} MB is kept in hand; ` +
        `a full disk takes every app down at once`,
    };
  }

  const needed = APP_MEMORY_MB + BUILD_RESERVE_MB + SYSTEM_RESERVE_MB;
  if (mem < needed) {
    return {
      admit: false,
      reason:
        `no room for another app: ${Math.round(mem)} MB available, and starting one needs ` +
        `${APP_MEMORY_MB} MB plus ${BUILD_RESERVE_MB} MB to build it and ${SYSTEM_RESERVE_MB} MB left for the box. ` +
        `${live} app${live === 1 ? "" : "s"} running${whenFree(expiresAt, now)}`,
    };
  }

  return { admit: true, reason: `room for ${howManyMore(mem)} more; ${live} running` };
}

function whenFree(expiresAt, now) {
  if (!Array.isArray(expiresAt) || expiresAt.length === 0) return "";
  const next = expiresAt
    .map(num)
    .filter((t) => Number.isFinite(t) && t > num(now))
    .sort((a, b) => a - b)[0];
  const inWords = humanise(next - num(now));
  return inWords ? `; the next slot frees in ${inWords}` : "";
}

/// How many more would fit, for the "room for N more" message. Not a promise:
/// apps grow into their ceiling, so this shrinks as they warm up.
export function howManyMore(memAvailableMB) {
  const mem = num(memAvailableMB);
  if (!Number.isFinite(mem)) return 0;
  const spare = mem - BUILD_RESERVE_MB - SYSTEM_RESERVE_MB;
  return Math.max(0, Math.floor(spare / APP_MEMORY_MB));
}
