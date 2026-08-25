// Whether there is room for one more project.
//
// Decided from what the box actually has free rather than from a number
// somebody guessed. A fixed count cannot answer this honestly: ten idle FastAPI
// services and ten Next.js servers are the same number and nothing like the same
// load.
//
// The arithmetic is harsher here than it would be for containers, and that is
// worth understanding rather than working around:
//
//   **A microVM's memory is spent the moment it boots.** It has its own guest
//   kernel, and the host cannot reclaim any of it -- there is no page cache to
//   evict, no shared libraries to deduplicate, and deliberately no balloon
//   device, because a ceiling the host can move is not a ceiling.
//
// So 256 MiB per project is 256 MiB of the box, always, whether the app is
// serving or idle. That is the price of the isolation and it is why ten is the
// number rather than thirty.
//
// Fails closed, unlike most decisions in this codebase: admitting means letting
// a stranger's code start running, and there is no safe guess.
//
// Pure -- the caller passes what it measured.

import { VM_MEM_MIB } from "./machine.mjs";
import { BUILD_MEM_MIB } from "./build.mjs";
import { appSlots } from "./registry.mjs";

/// Never lent out. The kernel, nginx, Postgres and this control plane live
/// here, and a box that has swapped its own control plane out cannot reap,
/// cannot refuse a deploy, and cannot be recovered without a console.
export const SYSTEM_RESERVE_MIB = 400;

/// A deploy has to be able to run its build VM. Without this reserve the tenth
/// deploy is admitted and then OOM-kills a running tenant while building.
export const BUILD_RESERVE_MIB = BUILD_MEM_MIB;

/// Disk is the outage this box will actually have: it takes out every microVM
/// and Postgres at once, and ordinary use causes it. Cheaper to refuse one
/// deploy than to lose ten.
export const MIN_DISK_FREE_MIB = 5 * 1024;

/// Backstop for the case where memory accounting flatters us. The slot range is
/// the real ceiling -- there is no address for an eleventh guest.
export const HARD_CAP = appSlots().length;

function strictNumber(v) {
  // Validated before coercing, never after: Number(null), Number(""),
  // Number(false) and Number([]) are all 0, and Number("512") is a perfectly
  // good 512, so a check written the other way round accepts every one of them.
  return typeof v === "number" && Number.isFinite(v) ? v : NaN;
}

function humanise(ms) {
  const m = Math.round(ms / 60000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 === 0 ? `${h}h` : `${h}h ${m % 60}m`;
}

function whenFree(expiresAt, now) {
  if (!Array.isArray(expiresAt)) return "";
  const next = expiresAt.map(strictNumber).filter((t) => Number.isFinite(t) && t > now).sort((a, b) => a - b)[0];
  return Number.isFinite(next) ? `; the next slot frees in ${humanise(next - now)}` : "";
}

/// Can one more project start right now?
export function canFitAnother({ memAvailableMB, diskFreeMB, running, expiresAt = [], now = 0 } = {}) {
  const mem = strictNumber(memAvailableMB);
  const disk = strictNumber(diskFreeMB);
  const live = strictNumber(running);

  if (!Number.isFinite(live) || live < 0) {
    return { admit: false, reason: "the server cannot tell how many projects are running; try again shortly" };
  }
  if (live >= HARD_CAP) {
    return { admit: false, reason: `all ${HARD_CAP} slots are in use${whenFree(expiresAt, now)}` };
  }
  if (!Number.isFinite(mem)) {
    return { admit: false, reason: "the server cannot read its available memory; try again shortly" };
  }
  if (!Number.isFinite(disk)) {
    return { admit: false, reason: "the server cannot read its free disk; try again shortly" };
  }

  if (disk < MIN_DISK_FREE_MIB) {
    return {
      admit: false,
      reason: `only ${Math.round(disk)} MB of disk is free and ${MIN_DISK_FREE_MIB} MB is kept in hand; ` +
        `a full disk stops every project at once`,
    };
  }

  const needed = VM_MEM_MIB + BUILD_RESERVE_MIB + SYSTEM_RESERVE_MIB;
  if (mem < needed) {
    return {
      admit: false,
      reason:
        `no room for another project: ${Math.round(mem)} MB available, and one needs ${VM_MEM_MIB} MB ` +
        `plus ${BUILD_RESERVE_MIB} MB to build it and ${SYSTEM_RESERVE_MIB} MB left for the box. ` +
        `${live} running${whenFree(expiresAt, now)}`,
    };
  }

  return { admit: true, reason: `room for ${howManyMore(mem, live)} more; ${live} running` };
}

/// How many more would fit. Not a promise -- a microVM claims its full
/// allocation at boot, so this only shrinks as projects start.
///
/// Bounded by the slots left as well as by memory. On an empty box memory alone
/// says 23, and there are only 15 addresses to put them at; reporting the larger
/// number would be a promise the slot allocator then breaks.
export function howManyMore(memAvailableMB, running = 0) {
  const mem = strictNumber(memAvailableMB);
  if (!Number.isFinite(mem)) return 0;
  const byMemory = Math.floor((mem - BUILD_RESERVE_MIB - SYSTEM_RESERVE_MIB) / VM_MEM_MIB);
  const live = strictNumber(running);
  const bySlots = HARD_CAP - (Number.isFinite(live) && live > 0 ? live : 0);
  return Math.max(0, Math.min(byMemory, bySlots));
}

/// For /healthz. Never throws: a health endpoint that fails because it could
/// not read a number is worse than one that says it could not read the number.
export function describeCapacity({ memAvailableMB, diskFreeMB, running }) {
  const mem = strictNumber(memAvailableMB);
  const disk = strictNumber(diskFreeMB);
  return {
    running: Number.isFinite(strictNumber(running)) ? running : null,
    slots: HARD_CAP,
    memAvailableMB: Number.isFinite(mem) ? Math.round(mem) : null,
    diskFreeMB: Number.isFinite(disk) ? Math.round(disk) : null,
    roomForMore: howManyMore(mem, running),
  };
}
