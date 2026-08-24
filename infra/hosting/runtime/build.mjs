// Installing a project's dependencies, inside a microVM.
//
// `npm install` runs arbitrary postinstall code. That is true of every CI system
// ever built and it is not preventable -- containment is the only answer, and
// the containment has to be at least as good as what the app itself gets, or the
// build becomes the soft way in to a platform whose whole premise is per-tenant
// hardware isolation. So a build is a microVM too.
//
// It differs from an app microVM in exactly three ways:
//
//   1. It has network. Installing dependencies needs a package registry, and
//      that is the entire reason this VM exists. Its network namespace has
//      forwarding and NAT; an app's does not. ip_forward is per-namespace in
//      Linux, so switching it on for the build slot leaves the guarantee that
//      app VMs have no route off the box completely untouched.
//
//   2. It has more memory. `npm install` peaks far above what the app it
//      produces will ever use.
//
//   3. It boots a different init. Same image, `init=/sbin/build-init` on the
//      kernel command line -- which is what makes this cheap: the image already
//      contains the app's tree, so there is nothing to copy in, and the built
//      node_modules are simply there when the app boots the same disk normally.
//      No mounting, no extraction, no second block device.
//
// Pure.

/// The last slot, reserved. Builds are serialised through it: two at once would
/// each want 1 GiB, and the box is sized for ten apps plus one build, not ten
/// plus several.
export const BUILD_SLOT = 15;

/// Peak, not steady state. A large `npm install` will use most of this.
export const BUILD_MEM_MIB = 1024;

/// Wall clock. A hostile or merely broken postinstall must not hold the build
/// slot open forever, and a project that cannot install in five minutes is not
/// one this platform is for.
export const BUILD_TIMEOUT_S = 300;

/// Where the guest leaves its exit code. Read back afterwards with `debugfs -R`,
/// which reads an ext4 image without mounting it -- so nothing on the host ever
/// mounts a filesystem a stranger's build has just written to.
export const STATUS_PATH = "/build-status";

/// Marker the guest writes before anything else, so a build VM that never got
/// as far as running the install is distinguishable from one whose install
/// failed. Without it both look like a missing status file.
export const STARTED_PATH = "/build-started";

function shellSafe(what, v) {
  const s = String(v);
  if (/[\n\r]/.test(s)) throw new Error(`refusing ${what}: contains a newline`);
  if (s.includes("'")) throw new Error(`refusing ${what}: contains a single quote`);
  if (s.includes("\0")) throw new Error(`refusing ${what}: contains a null byte`);
  return s;
}

/// What to run for a given runtime.
///
/// A lockfile is honoured when present: `npm ci` installs exactly what is
/// pinned and fails if the lockfile and manifest disagree, which is both faster
/// and the difference between deploying what was tested and deploying whatever
/// the registry served today.
export function installCommands(runtime, { lockfile = false, manifest = null } = {}) {
  if (runtime === "node") {
    return [lockfile ? "npm ci --omit=dev --no-audit --no-fund"
      : "npm install --omit=dev --no-audit --no-fund"];
  }
  if (runtime === "python") {
    if (manifest === "requirements.txt") {
      return ["pip install --no-cache-dir --disable-pip-version-check -r requirements.txt"];
    }
    if (manifest === "pyproject.toml" || manifest === "setup.py") {
      return ["pip install --no-cache-dir --disable-pip-version-check ."];
    }
    // Nothing to install is a valid outcome, not an error: a single-file Flask
    // app with no dependencies is a perfectly good project.
    return [];
  }
  throw new Error(`no install command for runtime ${JSON.stringify(runtime)}`);
}

/// PID 1 for a build.
///
/// Never fails the shell on a failed install -- the exit code is the product
/// here, and `set -e` would kill the guest before it could record why. The
/// timeout is belt and braces with the host's: if this one works the host never
/// has to kill anything, and a status file is much easier to act on than a
/// process that vanished.
export function renderBuildInit({ commands, env = {}, timeoutSeconds = BUILD_TIMEOUT_S }) {
  if (!Array.isArray(commands)) throw new Error("commands must be an array");
  const cmds = commands.map((c, i) => shellSafe(`install command ${i}`, c));

  if (!Number.isInteger(timeoutSeconds) || timeoutSeconds < 10 || timeoutSeconds > 1800) {
    throw new Error(`refusing build timeout ${JSON.stringify(timeoutSeconds)}`);
  }

  const exports = Object.entries(env).map(([k, v]) => {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(k)) {
      throw new Error(`refusing environment variable with invalid name ${JSON.stringify(k)}`);
    }
    return `export ${k}='${shellSafe(`value of ${k}`, v)}'`;
  });

  // Nothing to do is a success, and saying so here keeps the caller from having
  // to special-case a project with no dependencies.
  const body = cmds.length === 0
    ? `echo "no dependencies to install"\nrc=0`
    : `rc=0\n${cmds.map((c) =>
        `echo "+ ${c}"\ntimeout -s KILL ${timeoutSeconds} su-exec 1000:1000 sh -c '${c}' || rc=$?\n` +
        `[ "$rc" = 0 ] || { echo "FAILED: ${c} (exit $rc)"; }`).join("\n")}`;

  return `#!/bin/sh
# PID 1 for a boxcode dependency build. Generated -- regenerated every deploy.
#
# Selected with init=/sbin/build-init on the kernel command line. The app's own
# init is in the same image untouched, so once this finishes the disk boots
# normally with its dependencies already in place. Nothing is copied out.
#
# No set -e: the exit code IS the product, and dying before recording it would
# turn every failed install into an indistinguishable silent one.

mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev 2>/dev/null || true
ip link set lo up 2>/dev/null || true

# Written before anything else, so a VM that died during boot is
# distinguishable from one whose install failed. Otherwise both look the same
# from outside: a missing status file.
echo "1" > ${STARTED_PATH}
sync

${exports.join("\n")}

cd /app || { echo "no /app in the image"; echo 90 > ${STATUS_PATH}; sync; poweroff -f; }

${body}

echo "$rc" > ${STATUS_PATH}

# Without this the status file can still be in the page cache when the VM
# stops, and the host reads an image that never received it.
sync

# -f because there is no init system to ask politely.
poweroff -f
`;
}

/// Read back what the guest recorded.
///
/// Both files come from `debugfs -R cat`, which prints nothing for a file that
/// does not exist -- so absence is the normal signal here, not an error.
export function parseStatus({ started, status }) {
  const s = (status ?? "").trim();
  const began = (started ?? "").trim() !== "";

  if (!began) {
    return { ok: false, code: null, reason: "the build VM never started its init -- the image may not be bootable" };
  }
  if (s === "") {
    return { ok: false, code: null, reason: `the build did not finish within ${BUILD_TIMEOUT_S}s and was stopped` };
  }
  if (!/^\d{1,3}$/.test(s)) {
    return { ok: false, code: null, reason: `unreadable build status ${JSON.stringify(s.slice(0, 40))}` };
  }
  const code = Number(s);
  if (code === 0) return { ok: true, code: 0, reason: "dependencies installed" };
  if (code === 90) return { ok: false, code, reason: "the image had no /app directory" };
  if (code === 137) {
    return { ok: false, code, reason: `installing dependencies exceeded ${BUILD_TIMEOUT_S}s and was killed` };
  }
  return { ok: false, code, reason: `installing dependencies failed with exit code ${code}` };
}

/// `debugfs` arguments to read one path out of an image.
///
/// Read-only and without mounting, which is the point: the host must never
/// mount a filesystem a stranger's build has just written to.
export function debugfsReadArgs(imagePath, guestPath) {
  if (typeof imagePath !== "string" || !imagePath.startsWith("/") || imagePath.includes("..")) {
    throw new Error(`refusing image path ${JSON.stringify(imagePath)}`);
  }
  if (typeof guestPath !== "string" || !guestPath.startsWith("/") || guestPath.includes("..")) {
    throw new Error(`refusing guest path ${JSON.stringify(guestPath)}`);
  }
  return ["-R", `cat ${guestPath}`, imagePath];
}
