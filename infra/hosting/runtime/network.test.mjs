import { test } from "node:test";
import assert from "node:assert/strict";
import {
  SLOT_COUNT, SUBNET_PREFIX, HOST_VPC_CIDR,
  slotSubnet, tapName, netnsName, guestMac, slotPlan, bootArgs,
  upstreamFor, renderNginxRoute,
} from "./network.mjs";

const ALL = Array.from({ length: SLOT_COUNT }, (_, i) => i);

test("every interface name fits linux's 15-character limit", () => {
  // IFNAMSIZ is 16 including the NUL. Over it, the device silently fails to
  // create -- which is why slots are numbered rather than named after the
  // project id, since ids run to 16 characters on their own.
  for (const s of ALL) {
    assert.ok(tapName(s).length <= 15, `${tapName(s)} is ${tapName(s).length} chars`);
    assert.ok(netnsName(s).length <= 15, netnsName(s));
  }
});

test("no two slots share an address, a device or a MAC", () => {
  const seen = { host: new Set(), guest: new Set(), tap: new Set(), mac: new Set(), ns: new Set() };
  for (const s of ALL) {
    const p = slotPlan(s);
    for (const [k, v] of [["host", p.hostIp], ["guest", p.guestIp], ["tap", p.tap], ["mac", p.mac], ["ns", p.netns]]) {
      assert.ok(!seen[k].has(v), `${k} ${v} is used by more than one slot`);
      seen[k].add(v);
    }
  }
});

test("no guest subnet can overlap the VPC", () => {
  // A guest network colliding with 172.31.0.0/16 would take out the host's own
  // route to the auth box at 172.31.22.160 -- and would do it at whichever slot
  // number first reached the collision, not on day one.
  assert.equal(HOST_VPC_CIDR, "172.31.0.0/16");
  assert.notEqual(SUBNET_PREFIX, "172.31");
  for (const s of ALL) {
    assert.ok(slotSubnet(s).cidr.startsWith("10.200."), slotSubnet(s).cidr);
    assert.ok(!slotSubnet(s).cidr.startsWith("172.31."));
  }
});

test("a /30 gives exactly one host and one guest address", () => {
  const n = slotSubnet(3);
  assert.equal(n.cidr, "10.200.3.0/30");
  assert.equal(n.network, "10.200.3.0");
  assert.equal(n.hostIp, "10.200.3.1");
  assert.equal(n.guestIp, "10.200.3.2");
  assert.equal(n.broadcast, "10.200.3.3");
  assert.notEqual(n.hostIp, n.guestIp);
});

test("a guest has no route to another guest's subnet", () => {
  // The property that makes tenant isolation structural rather than a rule:
  // each link is a /30, so slot 0's netmask cannot reach slot 1 at all.
  const a = slotSubnet(0), b = slotSubnet(1);
  assert.notEqual(a.cidr, b.cidr);
  // 255.255.255.252 covers four addresses. Third octets differ, so they are
  // 256 addresses apart -- far outside each other's mask.
  assert.notEqual(a.guestIp.split(".")[2], b.guestIp.split(".")[2]);
});

test("MACs are locally administered", () => {
  for (const s of ALL) {
    const first = parseInt(guestMac(s).split(":")[0], 16);
    assert.equal(first & 0x02, 0x02, `${guestMac(s)} is not locally administered`);
    assert.equal(first & 0x01, 0, `${guestMac(s)} is a multicast address`);
    assert.match(guestMac(s), /^([0-9A-F]{2}:){5}[0-9A-F]{2}$/);
  }
});

test("an out-of-range or non-integer slot is refused", () => {
  for (const bad of [-1, SLOT_COUNT, SLOT_COUNT + 1, 1.5, "0", null, undefined, NaN, {}, []]) {
    for (const fn of [slotSubnet, tapName, netnsName, guestMac, slotPlan]) {
      assert.throws(() => fn(bad), /slot must be an integer/, `${fn.name}(${JSON.stringify(bad)})`);
    }
  }
});

test("boot args configure the guest interface without a dhcp client", () => {
  const a = bootArgs(5);
  const n = slotSubnet(5);
  assert.match(a, new RegExp(`ip=${n.guestIp}::${n.hostIp}:255\\.255\\.255\\.252`));
  assert.match(a, /console=ttyS0/);
  // panic=1 and reboot=k mean a guest that panics dies rather than sitting
  // there holding its memory.
  assert.match(a, /panic=1/);
  assert.match(a, /pci=off/);
});

test("boot args cannot be used to smuggle a second line", () => {
  assert.throws(() => bootArgs(0, "quiet\ninit=/bin/sh"), /newline/);
  assert.throws(() => bootArgs(0, "a\rb"), /newline/);
  assert.equal(bootArgs(0, "  "), bootArgs(0), "blank extra should change nothing");
  assert.match(bootArgs(0, "quiet"), /quiet$/);
});

test("there are more slots than the box is sized for", () => {
  // A reaper that is briefly behind must not block a deploy.
  assert.ok(SLOT_COUNT > 10, `${SLOT_COUNT} slots for 10 projects leaves no slack`);
});

// ---- routing -------------------------------------------------------------

test("nginx is pointed straight at the guest, not at a host port", () => {
  // Only possible because app TAPs live in the host namespace. In a per-app
  // namespace this address is unreachable from nginx and nothing would serve.
  for (const s of ALL) {
    assert.equal(upstreamFor(s), `${slotSubnet(s).guestIp}:8080`);
  }
  const r = renderNginxRoute("k9depef6", 3);
  assert.match(r, /proxy_pass http:\/\/10\.200\.3\.2:8080\/;/);
});

test("the route strips its own prefix", () => {
  // Without the trailing slash on proxy_pass, every route inside the app would
  // have to know its own project id.
  assert.match(renderNginxRoute("k9depef6", 0), /proxy_pass http:\/\/[\d.]+:\d+\/;/);
  assert.match(renderNginxRoute("k9depef6", 0), /location \^~ \/api\/k9depef6\//);
});

test("the route carries websockets", () => {
  const r = renderNginxRoute("k9depef6", 0);
  assert.match(r, /proxy_http_version 1\.1;/);
  assert.match(r, /proxy_set_header Upgrade \$http_upgrade;/);
  assert.match(r, /proxy_set_header Connection \$connection_upgrade;/);
});

test("two projects never share an upstream", () => {
  const seen = new Set();
  for (const s of ALL) {
    const u = upstreamFor(s);
    assert.ok(!seen.has(u), `${u} used twice`);
    seen.add(u);
  }
});

test("a hostile id or port is refused", () => {
  for (const bad of ["../../etc", "a b", "", "A9depef6", "x".repeat(17), null, 42]) {
    assert.throws(() => renderNginxRoute(bad, 0), /refusing to route invalid id/);
  }
  for (const bad of [0, 65536, -1, 1.5, "8080", null]) {
    assert.throws(() => upstreamFor(0, bad), /refusing upstream port/);
  }
});
