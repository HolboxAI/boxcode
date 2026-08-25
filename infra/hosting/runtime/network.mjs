// Host networking for microVMs.
//
// Firecracker gives a microVM a virtual NIC and nothing on the other end of it.
// Everything past that is ours: a TAP device on the host, an address on each
// side, and rules deciding what the guest may reach. This module allocates
// those, and does it from a slot number rather than from a project id, for two
// reasons that are not obvious:
//
//   1. Linux caps interface names at 15 characters (IFNAMSIZ is 16, including
//      the terminating NUL). Project ids run to 16 characters, so `fc-tap-` plus
//      an id does not fit and the device silently fails to create. A slot index
//      always fits.
//
//   2. Addresses have to be reused as projects come and go. Numbering by slot
//      makes reuse the normal case instead of something to garbage-collect.
//
// Pure. The registry maps project id to slot; this module only knows slots.

/// More slots than the ten projects the box is sized for, so a reaper that is
/// briefly behind does not block a deploy, and bounded so the address plan
/// cannot grow into anything else.
export const SLOT_COUNT = 16;

/// 10.200.x.x. NOT 172.31.x.x, which is this account's VPC -- a guest network
/// overlapping the VPC would take out the host's own route to the auth box at
/// 172.31.22.160, and it would happen at whichever slot number reached the
/// collision rather than on day one.
export const SUBNET_PREFIX = "10.200";

/// The VPC this box lives in. Kept here so the test can assert no guest subnet
/// can ever overlap it, rather than that being something someone remembers.
export const HOST_VPC_CIDR = "172.31.0.0/16";

function must(slot) {
  if (!Number.isInteger(slot) || slot < 0 || slot >= SLOT_COUNT) {
    throw new Error(`slot must be an integer 0..${SLOT_COUNT - 1}, got ${JSON.stringify(slot)}`);
  }
  return slot;
}

/// A /30 per microVM: network, host, guest, broadcast. The smallest subnet that
/// carries a point-to-point link, so no two guests ever share one -- a guest
/// cannot reach a neighbour it has no route to.
export function slotSubnet(slot) {
  must(slot);
  return {
    cidr: `${SUBNET_PREFIX}.${slot}.0/30`,
    network: `${SUBNET_PREFIX}.${slot}.0`,
    hostIp: `${SUBNET_PREFIX}.${slot}.1`,
    guestIp: `${SUBNET_PREFIX}.${slot}.2`,
    broadcast: `${SUBNET_PREFIX}.${slot}.3`,
    prefixLen: 30,
  };
}

/// Host-side TAP device. Must stay under 15 characters -- see the header.
export function tapName(slot) {
  return `fc-tap${must(slot)}`;
}

/// Network namespace the jailer puts this microVM's VMM process into, so the
/// TAP device is not visible in the host's namespace at all.
export function netnsName(slot) {
  return `fcns${must(slot)}`;
}

/// Guest MAC. The 0x02 in the first octet marks it locally administered, which
/// is what stops it colliding with a real vendor address.
export function guestMac(slot) {
  must(slot);
  return `02:FC:00:00:00:${slot.toString(16).padStart(2, "0").toUpperCase()}`;
}

/// Everything the host needs to set up one slot, in one object.
export function slotPlan(slot) {
  const net = slotSubnet(slot);
  return {
    slot,
    tap: tapName(slot),
    netns: netnsName(slot),
    mac: guestMac(slot),
    ...net,
  };
}

/// Where nginx sends a request for this slot.
///
/// Straight at the guest's own address. There is no host port to allocate and
/// nothing to collide over, because every guest has an address of its own --
/// which is only true because app TAPs live in the host network namespace. In a
/// per-app namespace this address would be unreachable from nginx entirely.
export function upstreamFor(slot, port = 8080) {
  const n = slotSubnet(slot);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`refusing upstream port ${JSON.stringify(port)}`);
  }
  return `${n.guestIp}:${port}`;
}

/// The nginx location block for one project.
export function renderNginxRoute(id, slot, port = 8080) {
  if (typeof id !== "string" || !/^[a-z2-9]{4,16}$/.test(id)) {
    throw new Error(`refusing to route invalid id ${JSON.stringify(id)}`);
  }
  const upstream = upstreamFor(slot, port);
  return `# Generated for ${id} on slot ${slot}. Regenerated on every deploy.
location ^~ /api/${id}/ {
    # ^~ so a longer regex location elsewhere cannot steal this prefix, and the
    # trailing slash on proxy_pass is what strips /api/<id>/ before the app sees
    # the path -- without it every route inside the app would need to know its
    # own project id.
    proxy_pass http://${upstream}/;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

    # WebSockets, which Lambda could not do at all.
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;

    # A microVM boots in well under a second, but a cold app behind it may not.
    proxy_connect_timeout 5s;
    proxy_read_timeout 60s;
}
`;
}

/// The kernel command line the guest boots with.
///
/// `ip=` configures the guest's interface at boot without a DHCP client in the
/// image -- one less moving part inside a rootfs we build ourselves. There is
/// deliberately no default gateway beyond the host side of the point-to-point
/// link, and the host does not forward, so the guest has no route off the box.
export function bootArgs(slot, extra = "") {
  const n = slotSubnet(slot);
  if (typeof extra !== "string" || /[\n\r]/.test(extra)) {
    throw new Error("refusing boot arguments containing a newline");
  }
  const base = [
    "console=ttyS0",
    "reboot=k",
    "panic=1",
    "pci=off",
    // Named explicitly. The default would be /sbin/init, which in an Alpine
    // rootfs is a symlink to busybox -- the guest boots Alpine's init, reads
    // /etc/inittab and dies looking for openrc. A build overrides this with its
    // own init= in extraBootArgs; the kernel takes the last one given.
    "init=/sbin/boxcode-init",
    // No hardware to probe and no init system to wait for: this is most of why
    // a microVM boots in about a tenth of a second.
    `ip=${n.guestIp}::${n.hostIp}:255.255.255.252::eth0:off`,
  ].join(" ");
  return extra.trim() ? `${base} ${extra.trim()}` : base;
}
