import { test } from "node:test";
import assert from "node:assert/strict";
import {
  VM_VCPUS, VM_MEM_MIB, JAILER_UID_BASE, JAIL_ROOT,
  machineConfig, jailerArgs, jailerIds, jailPath,
} from "./machine.mjs";
import { SLOT_COUNT, slotPlan } from "./network.mjs";

const OK = {
  id: "k9depef6",
  slot: 0,
  kernel: "/opt/fc/vmlinux",
  rootfs: "/opt/fc/apps/k9depef6/rootfs.ext4",
};
const val = (a, f) => a[a.indexOf(f) + 1];

// ---- what the guest gets ----------------------------------------------

test("hyperthread siblings are off", () => {
  // Siblings share microarchitectural state, which is the substrate for most
  // cross-VM side-channel work.
  assert.equal(machineConfig(OK)["machine-config"].smt, false);
});

test("memory is fixed and not reclaimable", () => {
  const c = machineConfig(OK);
  assert.equal(c["machine-config"].mem_size_mib, VM_MEM_MIB);
  // No balloon device. Ballooning would let the host reclaim guest memory, and
  // a ceiling the host can move is not a ceiling -- the 10-project figure rests
  // on this staying fixed.
  assert.ok(!("balloon" in c), "a balloon device would undo the memory accounting");
});

test("disk and network are rate limited by the VMM, not by the guest", () => {
  const c = machineConfig(OK);
  assert.ok(c.drives[0].rate_limiter?.bandwidth?.size > 0, "disk must be capped");
  const nic = c["network-interfaces"][0];
  assert.ok(nic.rx_rate_limiter?.bandwidth?.size > 0, "rx must be capped");
  assert.ok(nic.tx_rate_limiter?.bandwidth?.size > 0, "tx must be capped");
  // A tenant cannot lift these: they are enforced outside the guest entirely.
});

test("the guest boots on its own slot's network", () => {
  for (const slot of [0, 1, 7, SLOT_COUNT - 1]) {
    const net = slotPlan(slot);
    const c = machineConfig({ ...OK, slot });
    assert.equal(c["network-interfaces"][0].host_dev_name, net.tap);
    assert.equal(c["network-interfaces"][0].guest_mac, net.mac);
    assert.match(c["boot-source"].boot_args, new RegExp(`ip=${net.guestIp}::`));
  }
});

test("one vcpu, and never more than two", () => {
  assert.equal(machineConfig(OK)["machine-config"].vcpu_count, VM_VCPUS);
  for (const bad of [4, 0, 1.5, -1, "1", null]) {
    assert.throws(() => machineConfig({ ...OK, vcpus: bad }), /vcpu count/, JSON.stringify(bad));
  }
});

test("the root device is writable but private to this project", () => {
  const d = machineConfig(OK).drives[0];
  assert.equal(d.is_root_device, true);
  // An app writes logs and temp files, so it cannot be read-only. It is a
  // per-project image discarded at expiry, so nothing written here outlives the
  // project or is visible to another one.
  assert.equal(d.is_read_only, false);
  assert.equal(d.path_on_host, OK.rootfs);
});

// ---- how the HOST runs the VMM ----------------------------------------

test("the VMM never runs as root", () => {
  // A microVM is a strong boundary, but the VMM process sits on the host side
  // of it. Running it as root would turn a Firecracker vulnerability into host
  // root directly.
  for (const slot of [0, SLOT_COUNT - 1]) {
    const { uid, gid } = jailerIds(slot);
    assert.ok(uid >= JAILER_UID_BASE, `uid ${uid}`);
    assert.notEqual(uid, 0);
    assert.notEqual(gid, 0);
    const a = jailerArgs({ id: "k9depef6", slot });
    assert.equal(val(a, "--uid"), String(uid));
    assert.equal(val(a, "--gid"), String(gid));
  }
});

test("every slot's VMM runs as a different user", () => {
  // One shared uid would let a VMM that escaped its chroot signal or ptrace the
  // other nine, which is most of what escaping a chroot is good for.
  const uids = new Set();
  for (let s = 0; s < SLOT_COUNT; s++) {
    const { uid } = jailerIds(s);
    assert.ok(!uids.has(uid), `uid ${uid} used twice`);
    uids.add(uid);
  }
  assert.equal(uids.size, SLOT_COUNT);
});

test("the VMM is chrooted and in its own network namespace", () => {
  const a = jailerArgs({ id: "k9depef6", slot: 3 });
  assert.equal(val(a, "--chroot-base-dir"), JAIL_ROOT);
  // The TAP device lives in this namespace and nowhere else, so the VMM cannot
  // see the host's interfaces even if it escapes the chroot.
  assert.equal(val(a, "--netns"), `/var/run/netns/${slotPlan(3).netns}`);
});

test("there is no live API socket into the VMM", () => {
  // The VM is fully described by its config file at boot. An API socket is a
  // control channel into the VMM that nothing here needs.
  assert.ok(jailerArgs({ id: "k9depef6", slot: 0 }).includes("--no-api"));
});

test("firecracker's own arguments come after the separator", () => {
  const a = jailerArgs({ id: "k9depef6", slot: 0 });
  const sep = a.indexOf("--");
  assert.ok(sep > 0, "there must be a -- separator");
  assert.ok(a.slice(0, sep).includes("--uid"), "jailer args belong before it");
  assert.ok(a.slice(sep).includes("--config-file"), "firecracker args belong after it");
});

test("the jail path is per project and cannot escape", () => {
  const p = jailPath("k9depef6", 0);
  assert.ok(p.startsWith(`${JAIL_ROOT}/`), p);
  assert.ok(!p.includes(".."), p);
  assert.notEqual(jailPath("k9depef6", 0), jailPath("aaaa", 0));
});

// ---- attacker-supplied input -------------------------------------------

test("a hostile id is refused", () => {
  for (const bad of ["../../etc", "a b", "--privileged", "", "x".repeat(17),
                     "A9depef6", "a;b", "a/b", "id\nx", null, 42, {}]) {
    assert.throws(() => machineConfig({ ...OK, id: bad }), /invalid id/, JSON.stringify(bad));
    assert.throws(() => jailerArgs({ id: bad, slot: 0 }), /invalid id/);
    assert.throws(() => jailPath(bad, 0), /invalid id/);
  }
});

test("a path that could escape the intended directory is refused", () => {
  for (const bad of ["../etc/shadow", "opt/fc/vmlinux", "/opt/../../etc/shadow", "", null, 42]) {
    assert.throws(() => machineConfig({ ...OK, kernel: bad }), /refusing kernel path/, JSON.stringify(bad));
    assert.throws(() => machineConfig({ ...OK, rootfs: bad }), /refusing rootfs path/, JSON.stringify(bad));
  }
});

test("the jail config file must be a plain name inside the jail", () => {
  // It is resolved inside the chroot, so a path would either escape or silently
  // resolve somewhere nobody intended.
  for (const bad of ["/etc/passwd", "../vm.json", "a/b.json", null, 42]) {
    assert.throws(() => jailerArgs({ id: "k9depef6", slot: 0, configFile: bad }), /plain name/);
  }
});

test("an absurd memory size is refused", () => {
  for (const bad of [0, 64, 127, 8192, -256, 256.5, "256", null]) {
    assert.throws(() => machineConfig({ ...OK, memMib: bad }), /refusing memory/, JSON.stringify(bad));
  }
});

test("an out-of-range slot is refused", () => {
  for (const bad of [-1, SLOT_COUNT, 1.5, "0", null]) {
    assert.throws(() => machineConfig({ ...OK, slot: bad }), /slot must be an integer/);
    assert.throws(() => jailerArgs({ id: "k9depef6", slot: bad }), /slot must be an integer/);
  }
});

test("extra boot arguments cannot smuggle a second kernel line", () => {
  assert.throws(
    () => machineConfig({ ...OK, extraBootArgs: "quiet\ninit=/bin/sh" }),
    /newline/,
  );
});

// ---- the arithmetic the plan rests on ----------------------------------

test("ten microVMs fit the box they are sized for", () => {
  // A test rather than a comment, because this is the number the whole design
  // was costed against. A microVM's memory is spent from the host the moment it
  // boots -- it has its own kernel, and nothing is shared or reclaimable.
  const guests = 10 * VM_MEM_MIB;
  const hostSide =
    250 + // host OS and kernel
    10 * 5 + // one VMM process per microVM
    250 + // postgres
    80 + // nginx and the control plane
    512; // rootfs build burst
  assert.ok(guests + hostSide < 8192, `${guests + hostSide} MiB exceeds an 8 GiB box`);
  // And that there is real headroom, not a number that only just fits.
  assert.ok(guests + hostSide < 8192 * 0.75, "should sit well under three quarters");
});
