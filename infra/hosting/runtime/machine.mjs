// What a microVM is configured with, and how its VMM process is confined.
//
// Two separate things live here and they protect against different attackers:
//
//   machineConfig()  -- what the GUEST gets. Its kernel, its disk, its NIC, and
//                       ceilings on all of them. This bounds a hostile tenant.
//
//   jailerArgs()     -- how the HOST runs the VMM. Firecracker ships a `jailer`
//                       binary that chroots the VMM, drops it to an
//                       unprivileged uid, puts it in a cgroup and a network
//                       namespace, and installs a seccomp filter. This bounds a
//                       tenant who has already broken *out* of the guest.
//
// The second is the one worth being careful about. A microVM is a strong
// boundary, but the VMM process sits on the host side of it, and running that
// process as root would mean a Firecracker vulnerability lands as host root.
// The jailer is not optional hardening; it is the documented way to run this in
// production, and everything here assumes it.
//
// Pure: builds configuration and argv arrays, touches nothing.

import { slotPlan, bootArgs } from "./network.mjs";

export const ID_RE = /^[a-z2-9]{4,16}$/;

/// One vCPU per project. The box has two, so ten microVMs oversubscribe
/// heavily -- correct for demo backends, which are idle almost all the time,
/// and the reason a 2 vCPU box can host ten of them.
export const VM_VCPUS = 1;

/// Memory is NOT shared or reclaimable: a microVM has its own kernel, and what
/// it is given is spent from the host the moment it boots. Ten of these is
/// 2.5 GB of the 8 GB box, which is where the ten comes from.
export const VM_MEM_MIB = 256;

/// Per-VM disk and network ceilings, enforced by Firecracker itself rather than
/// by anything in the guest -- so a tenant cannot lift them.
export const DISK_BANDWIDTH_MB_S = 32;
export const NET_BANDWIDTH_MB_S = 16;

/// Base uid for jailed VMM processes; each slot gets its own. One shared uid
/// would mean a VMM that escaped its chroot could signal or ptrace the other
/// nine VMMs, which is most of what escaping a chroot is good for.
export const JAILER_UID_BASE = 30000;

export const JAIL_ROOT = "/srv/jailer";

function mustId(id) {
  if (typeof id !== "string" || !ID_RE.test(id)) {
    throw new Error(`refusing to configure a microVM for invalid id ${JSON.stringify(id)}`);
  }
  return id;
}

function mib(v, what) {
  if (!Number.isInteger(v) || v < 128 || v > 4096) {
    throw new Error(`refusing ${what} of ${JSON.stringify(v)} MiB`);
  }
  return v;
}

/// A token bucket in the shape Firecracker expects. `refill_time` is in ms, so
/// a bucket of N bytes refilled every 1000 ms is N bytes/second.
function bucket(bytesPerSecond) {
  return { bandwidth: { size: bytesPerSecond, refill_time: 1000 } };
}

/// The Firecracker configuration for one project's microVM.
export function machineConfig({ id, slot, memMib = VM_MEM_MIB, vcpus = VM_VCPUS, kernel, rootfs, extraBootArgs = "" }) {
  mustId(id);
  mib(memMib, "memory");
  if (!Number.isInteger(vcpus) || vcpus < 1 || vcpus > 2) {
    throw new Error(`refusing vcpu count ${JSON.stringify(vcpus)}`);
  }
  for (const [k, v] of [["kernel", kernel], ["rootfs", rootfs]]) {
    if (typeof v !== "string" || !v.startsWith("/") || v.includes("..")) {
      throw new Error(`refusing ${k} path ${JSON.stringify(v)}`);
    }
  }

  const net = slotPlan(slot);

  return {
    "boot-source": {
      kernel_image_path: kernel,
      boot_args: bootArgs(slot, extraBootArgs),
    },
    drives: [
      {
        drive_id: "rootfs",
        path_on_host: rootfs,
        is_root_device: true,
        // Writable, because an app writes logs and temp files. It is a private
        // copy per project and it is discarded at expiry, so nothing written
        // here outlives the project or is visible to another one.
        is_read_only: false,
        rate_limiter: bucket(DISK_BANDWIDTH_MB_S * 1024 * 1024),
      },
    ],
    "machine-config": {
      vcpu_count: vcpus,
      mem_size_mib: memMib,
      // Hyperthread siblings share microarchitectural state, which is the
      // substrate for most cross-VM side-channel work. Off.
      smt: false,
      track_dirty_pages: false,
    },
    "network-interfaces": [
      {
        iface_id: "eth0",
        guest_mac: net.mac,
        host_dev_name: net.tap,
        rx_rate_limiter: bucket(NET_BANDWIDTH_MB_S * 1024 * 1024),
        tx_rate_limiter: bucket(NET_BANDWIDTH_MB_S * 1024 * 1024),
      },
    ],
    // No balloon device. Ballooning would let the host reclaim guest memory,
    // which sounds useful and means a tenant's memory ceiling stops being a
    // ceiling -- the accounting the 10-project figure rests on would drift.
  };
}

/// uid/gid the jailed VMM for this slot runs as.
export function jailerIds(slot) {
  const { slot: s } = slotPlan(slot);
  return { uid: JAILER_UID_BASE + s, gid: JAILER_UID_BASE + s };
}

/// argv for the jailer. It execs firecracker itself, so nothing here is ever
/// handed to a shell -- and the id is validated regardless, because an id that
/// is itself a flag would still arrive as its own argument.
export function jailerArgs({ id, slot, firecracker = "/usr/bin/firecracker", configFile = "vm.json", netns = false }) {
  mustId(id);
  const net = slotPlan(slot);
  const { uid, gid } = jailerIds(slot);

  if (typeof configFile !== "string" || configFile.includes("/") || configFile.includes("..")) {
    // Resolved inside the chroot, so it must be a bare filename.
    throw new Error(`config file must be a plain name inside the jail, got ${JSON.stringify(configFile)}`);
  }

  const args = [
    "--id", id,
    "--exec-file", firecracker,
    "--uid", String(uid),
    "--gid", String(gid),
    "--chroot-base-dir", JAIL_ROOT,
  ];

  // Only the build VM gets a network namespace, and the reason is worth
  // recording because the first version of this gave one to every VM.
  //
  // A namespace hides the TAP device from the host, which sounds strictly
  // better -- and it also hides the guest from nginx, which runs in the host
  // namespace and has to reach the app to serve it. There is no route in. The
  // design could not have served a single request.
  //
  // App TAPs therefore live in the host namespace, where nginx can reach the
  // guest and the guest can reach Postgres. What the guest may do from there is
  // controlled by ip_forward=0 (so it cannot reach another guest or the
  // internet, both of which would be forwarding) and by INPUT rules limiting it
  // to Postgres. That is a firewall guarantee rather than a topological one --
  // weaker in kind, so setup.sh asserts both rather than assuming them.
  //
  // The build VM keeps its namespace, because it genuinely needs forwarding and
  // NAT that must not exist anywhere else on the box.
  if (netns) {
    args.push("--netns", `/var/run/netns/${net.netns}`);
  }

  // Everything after -- is firecracker's own.
  args.push(
    "--",
    "--config-file", configFile,
    // No API socket. The VM is fully described by its config file at boot, and
    // a live API socket is a control channel into the VMM that nothing needs.
    "--no-api",
  );
  return args;
}

/// Where the jailer will place this VM's chroot. The control-plane copies the
/// kernel and rootfs in here before starting it.
export function jailPath(id, slot) {
  mustId(id);
  slotPlan(slot);
  return `${JAIL_ROOT}/firecracker/${id}/root`;
}
