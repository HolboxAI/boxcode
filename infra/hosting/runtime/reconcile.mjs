// What to start, stop, and leave alone.
//
// The control plane is not the thing that keeps microVMs alive -- the jailer
// processes are, and they outlive it. So restarting the control plane, whether
// deliberately or because it crashed, finds a box with VMs already running on
// it and a registry describing what *should* be running. Those two disagree in
// every interesting case:
//
//   a VM is running and the registry knows it   -> adopt, do nothing
//   the registry wants it, nothing is running   -> start
//   a VM is running that nothing claims         -> stop, it is leaked memory
//   the registry wants it but it has expired    -> reap
//
// Getting the third case wrong is how a box slowly fills with 256 MiB
// allocations nobody can account for, until the eleventh deploy fails for no
// visible reason. Getting it *too* aggressive is worse: stopping something we
// merely failed to recognise takes a live tenant down.
//
// So the rule is the same one the kill switch uses -- a VM is only ever stopped
// when its name says it is ours. Anything else on the box is somebody else's
// problem and stays running.
//
// Pure.

import { expiredIds } from "./registry.mjs";
import { BUILD_SLOT } from "./build.mjs";

/// Only these are ever touched. Same anchored shape the kill switch matches, so
/// there is one naming rule on this box rather than two that can drift.
export const VM_NAME_RE = /^boxcode-app-([a-z2-9]{4,16})$/;

export function vmName(id) {
  return `boxcode-app-${id}`;
}

/// Extract the project id from a running VM's name, or null if it is not ours.
export function idFromVmName(name) {
  const m = typeof name === "string" ? name.match(VM_NAME_RE) : null;
  return m ? m[1] : null;
}

/// Decide what to do.
///
/// `running` is what was found on the box: `[{ name, slot, pid }]`.
/// Returns four disjoint lists, plus `ignored` for anything not ours -- logged,
/// never acted on, and worth seeing precisely because it should be empty.
export function reconcile({ registry, running = [], now }) {
  const start = [];
  const stop = [];
  const adopt = [];
  const reap = [];
  const ignored = [];

  const clockOk = Number.isInteger(now) && now > 0;
  const expired = clockOk ? new Set(expiredIds(registry, now)) : new Set();

  const live = new Map();
  for (const vm of running) {
    const id = idFromVmName(vm?.name);
    if (!id) {
      // Not ours. The box shares nothing with other services today, but the
      // rule holds regardless: we stop what we can name, and nothing else.
      ignored.push({ name: vm?.name ?? null, why: "not a boxcode-app-* name" });
      continue;
    }
    if (live.has(id)) {
      // Two processes for one project. The second is a leak from a start that
      // was retried after appearing to fail.
      stop.push({ id, slot: vm.slot, pid: vm.pid, why: "duplicate VM for one project" });
      continue;
    }
    live.set(id, vm);
  }

  for (const [id, entry] of Object.entries(registry.projects)) {
    const vm = live.get(id);

    if (expired.has(id)) {
      // Reaping a project that is not running is still worth doing: its image,
      // its nginx route and its database are all still there.
      reap.push({ id, slot: entry.slot, pid: vm?.pid ?? null, running: Boolean(vm) });
      continue;
    }

    if (!vm) {
      start.push({ id, slot: entry.slot, runtime: entry.runtime });
      continue;
    }

    if (Number.isInteger(vm.slot) && vm.slot !== entry.slot) {
      // The running VM is on a different slot than the registry records, so
      // nginx is pointed at an address nothing is serving. Restart it rather
      // than rewrite the registry: the address in the registry is the one the
      // route and the database grant were built from.
      stop.push({ id, slot: vm.slot, pid: vm.pid, why: `running on slot ${vm.slot}, registry says ${entry.slot}` });
      start.push({ id, slot: entry.slot, runtime: entry.runtime });
      continue;
    }

    adopt.push({ id, slot: entry.slot, pid: vm.pid });
  }

  for (const [id, vm] of live) {
    if (registry.projects[id]) continue;
    if (Number.isInteger(vm.slot) && vm.slot === BUILD_SLOT) {
      // A build in flight. It is nobody's project and it is supposed to be
      // there; killing it would fail a deploy that is part-way through.
      ignored.push({ name: vm.name, why: "a build is running in the build slot" });
      continue;
    }
    stop.push({ id, slot: vm.slot, pid: vm.pid, why: "no project in the registry claims it" });
  }

  return { start, stop, adopt, reap, ignored, clockOk };
}

/// A one-line summary for the log. Reconciliation runs on every start and every
/// sweep, so the overwhelmingly common case -- nothing to do -- has to be
/// readable at a glance rather than a wall of empty arrays.
export function summarise(plan) {
  const parts = [];
  if (plan.start.length) parts.push(`start ${plan.start.length}`);
  if (plan.stop.length) parts.push(`stop ${plan.stop.length}`);
  if (plan.reap.length) parts.push(`reap ${plan.reap.length}`);
  if (plan.ignored.length) parts.push(`ignored ${plan.ignored.length}`);
  if (!parts.length) return `nothing to do (${plan.adopt.length} running)`;
  return `${parts.join(", ")} (${plan.adopt.length} already running)`;
}
