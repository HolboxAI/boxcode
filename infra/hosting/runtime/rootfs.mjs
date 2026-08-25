// Turning an uploaded project into a bootable disk.
//
// Firecracker boots a kernel and hands it a block device. Neither exists on its
// own -- this is the part the project's README means when it says image
// management is "an external concern users must address separately".
//
// The shape:
//
//   base rootfs (one per runtime, built once at setup and reused)
//     + the project's built tree
//     + an init that starts it
//     = one ext4 file per project
//
// Built with `mke2fs -d`, which populates a filesystem image from a directory
// **without mounting it**. That matters more than it sounds: the alternative is
// a loop device and a real mount, which needs CAP_SYS_ADMIN in the host mount
// namespace, on the one box where handing out that capability is least
// appealing. mke2fs needs neither.
//
// Pure. Produces text and argument arrays; the shell script drives them.

export const ID_RE = /^[a-z2-9]{4,16}$/;

/// The guest runs the app as this uid. The microVM boundary is the isolation
/// that matters, but a guest kernel exploit is easier from root than from
/// nobody, and not being root costs nothing.
export const APP_UID = 1000;
export const APP_GID = 1000;

/// Where the app's init goes, and why it is not /sbin/init.
///
/// alpine-baselayout ships /sbin/init as a symlink to busybox. Writing there
/// follows the symlink and lands on busybox itself, and the guest then boots
/// Alpine's own init, reads /etc/inittab, and dies looking for openrc -- which
/// is exactly what happened on the first real boot. Its own name has no symlink
/// to follow, and makes the app path symmetric with the build path, which uses
/// /sbin/build-init and worked first time for the same reason.
export const APP_INIT_PATH = "/sbin/boxcode-init";

/// Base rootfs per runtime, built once at setup and copied per project.
export const BASES = {
  node: "node22",
  python: "python312",
};

/// Floor, ceiling, and how much room to leave above the tree.
///
/// The ceiling is the one that matters: ten projects at 1 GiB is 10 GiB of a
/// 50 GiB disk, and a full disk takes every microVM and Postgres down together.
export const MIN_IMAGE_MIB = 256;
export const MAX_IMAGE_MIB = 1024;
export const HEADROOM_MIB = 128;

function mustId(id) {
  if (typeof id !== "string" || !ID_RE.test(id)) {
    throw new Error(`refusing to build a rootfs for invalid id ${JSON.stringify(id)}`);
  }
  return id;
}

/// A value that will be written into a shell script. A newline would end the
/// line and start a command; a quote would end the string. Both are refused
/// rather than escaped -- these come from a project's own configuration, so
/// they are attacker-influenced, and escaping is the thing people get subtly
/// wrong.
function shellSafe(what, v) {
  const s = String(v);
  if (/[\n\r]/.test(s)) throw new Error(`refusing ${what}: contains a newline`);
  if (s.includes("'")) throw new Error(`refusing ${what}: contains a single quote`);
  if (s.includes("\0")) throw new Error(`refusing ${what}: contains a null byte`);
  return s;
}

export function baseFor(runtime) {
  const b = BASES[runtime];
  if (!b) throw new Error(`no base image for runtime ${JSON.stringify(runtime)}`);
  return b;
}

/// How big the image needs to be, from the size of the built tree.
///
/// Rounded up to a whole MiB and clamped. A tree that would need more than the
/// ceiling is refused by the caller rather than silently truncated -- an ext4
/// image too small for its contents fails during `mke2fs -d`, and it is much
/// better to say "your project is too large" than to ship that error.
export function imageSizeMib(treeBytes, baseMib = 192) {
  const t = strictNumber("tree bytes", treeBytes);
  const b = strictNumber("base size", baseMib);
  const needed = Math.ceil(t / (1024 * 1024)) + b + HEADROOM_MIB;
  return Math.min(MAX_IMAGE_MIB, Math.max(MIN_IMAGE_MIB, needed));
}

/// Validate BEFORE coercing, never after.
///
/// `Number(null)`, `Number("")`, `Number(false)` and `Number([])` are all 0, and
/// `Number("512")` is a perfectly good 512 -- so a check written as
/// `const n = Number(x); if (!Number.isFinite(n))` accepts every one of them.
/// Every value reaching this module describes an image that is about to hold a
/// stranger's code, so a string that happens to parse is not the same as a
/// number and is refused.
function strictNumber(what, v) {
  if (typeof v !== "number" || !Number.isFinite(v) || v < 0) {
    throw new Error(`refusing to size an image for ${what} ${JSON.stringify(v)}`);
  }
  return v;
}

/// True when a tree simply will not fit, whatever we do.
export function tooLarge(treeBytes, baseMib = 192) {
  const t = strictNumber("tree bytes", treeBytes);
  const b = strictNumber("base size", baseMib);
  return Math.ceil(t / (1024 * 1024)) + b + HEADROOM_MIB > MAX_IMAGE_MIB;
}

/// The guest's PID 1.
///
/// Deliberately not an init system. There is one process to run, no services to
/// order, and nothing to supervise -- if the app exits, PID 1 exits, and the
/// kernel panics. With `panic=1` in the boot arguments that stops the microVM
/// dead instead of leaving it holding 256 MiB doing nothing, which is exactly
/// what should happen: the control plane sees a stopped VM and decides.
export function renderInit({ startCommand, env = {} }) {
  if (!Array.isArray(startCommand) || startCommand.length === 0) {
    throw new Error("a rootfs needs a start command");
  }
  const argv = startCommand.map((a, i) => shellSafe(`start command word ${i}`, a));

  const exports = Object.entries(env).map(([k, v]) => {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(k)) {
      throw new Error(`refusing environment variable with invalid name ${JSON.stringify(k)}`);
    }
    return `export ${k}='${shellSafe(`value of ${k}`, v)}'`;
  });

  return `#!/bin/sh
# PID 1 inside a boxcode microVM. Generated -- the next deploy overwrites it.
#
# There is one process to run and nothing to supervise. If it exits, this exits,
# and the kernel panics; panic=1 in the boot arguments turns that into a stopped
# microVM rather than one holding memory and doing nothing.
set -e

# PID 1 gets no environment from the kernel -- not even PATH. Without this,
# every unqualified command in this script fails with "can't execute: No such
# file or directory", which reads as a missing binary rather than a missing
# PATH. su-exec lives in /sbin and was exactly that failure.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev 2>/dev/null || true

# The kernel already configured eth0 from the ip= boot argument, so there is no
# DHCP client in this image and nothing to wait for.
ip link set lo up 2>/dev/null || true

${exports.join("\n")}

# Set for the same reason the build init sets it: the app user has no home
# directory, and a library that writes to HOME gets EACCES rather than a
# fallback. /tmp is writable and empty at boot.
export HOME=/tmp

cd /app

# Not root. The microVM boundary is the isolation that matters, but a guest
# kernel exploit is easier from uid 0 than from uid ${APP_UID}, and dropping is free.
exec su-exec ${APP_UID}:${APP_GID} ${argv.join(" ")}
`;
}

/// `mke2fs` arguments to build the image from a staging directory.
///
/// -d populates from a directory with no mount and no loop device, so this
/// needs no CAP_SYS_ADMIN. -F because the target is a plain file rather than a
/// block device. Fixed 4096-byte blocks so the size arithmetic above is exact.
export function mke2fsArgs({ imagePath, stagingDir, sizeMib }) {
  for (const [k, v] of [["image path", imagePath], ["staging dir", stagingDir]]) {
    if (typeof v !== "string" || !v.startsWith("/") || v.includes("..")) {
      throw new Error(`refusing ${k} ${JSON.stringify(v)}`);
    }
  }
  // Checked on the raw value, not on Number(sizeMib) -- coercing first would
  // accept the string "512", and the block count below would then be built from
  // something that was never a number.
  if (!Number.isInteger(sizeMib) || sizeMib < MIN_IMAGE_MIB || sizeMib > MAX_IMAGE_MIB) {
    throw new Error(`refusing image size ${JSON.stringify(sizeMib)} MiB`);
  }
  const mib = sizeMib;
  return [
    "-t", "ext4",
    "-b", "4096",
    "-d", stagingDir,
    "-F",
    // No reserved blocks. The default holds 5% back for root, which is a
    // server-filesystem convention that only wastes space in a throwaway
    // single-app image.
    "-m", "0",
    imagePath,
    `${mib * 256}`, // 4096-byte blocks per MiB
  ];
}

/// Where a project's image and staging tree live.
export function paths(id, base = "/opt/boxcode-apps") {
  mustId(id);
  return {
    dir: `${base}/${id}`,
    staging: `${base}/${id}/staging`,
    image: `${base}/${id}/rootfs.ext4`,
    source: `${base}/${id}/src`,
  };
}
