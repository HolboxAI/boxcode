// How a hosted app is actually run, as data rather than as a shell string.
//
// Every containment control from the hosting design lands here as a docker
// flag, which makes this the file where forgetting one line silently removes a
// security property and nothing looks wrong. So the flags are built by pure
// functions with tests that assert each one is present, rather than being
// typed into a shell command in the control-plane where nothing checks them.
//
// The network shape is the part worth understanding, and it was verified
// against a real Docker rather than reasoned about:
//
//   * ONE --internal network PER APP, containing exactly that app and Postgres.
//     --internal is what blocks egress; there is no default route on the bridge
//     at all, so a miner cannot reach a pool, a bot cannot reach C2, and DNS
//     for external names returns SERVFAIL.
//
//   * App-to-app is impossible BY CONSTRUCTION, not by a flag: two apps are
//     never on the same network, so there is nothing to misconfigure. An
//     earlier draft used one shared network with icc=false, which also works
//     but depends on a single option staying set.
//
//   * Postgres is attached to each app network as an additional interface, so
//     each app reaches it by name over its own private bridge.
//
// Reproduce the verification any time:
//   node --test infra/hosting/runtime/container.test.mjs   (the flags)
//   infra/hosting/runtime/verify-containment.sh            (a real Docker)

/// Project ids, same shape every other control-plane already validates.
export const ID_RE = /^[a-z2-9]{4,16}$/;

/// Per-app ceilings. Ten apps at 384 MB plus Postgres plus a build fits an
/// 8 GiB box at about 77% committed -- that arithmetic is where the cap of ten
/// comes from, rather than ten being picked and the sizes fitted to it.
export const APP_MEMORY_MB = 384;
export const APP_CPUS = "0.25";
export const APP_PIDS = 256;
export const APP_TMPFS_MB = 64;

/// Containers run as this uid. It exists in no image's /etc/passwd, which is
/// the point: nothing inside is running as a user the image was built to trust.
export const APP_UID = 10001;

/// The build sandbox is the one place with network, because `npm install`
/// cannot work without it. It is also the one place that executes arbitrary
/// postinstall scripts, so it gets a hard wall clock and no route to Postgres.
export const BUILD_MEMORY_MB = 1024;
export const BUILD_TIMEOUT_S = 300;

export function appNetworkName(id) {
  return `boxcode-app-${must(id)}`;
}

export function appContainerName(id) {
  // Deliberately the same string as the network. The kill switch matches
  // ^boxcode-app-[a-z2-9]{4,16}$ on container names, and keeping the two
  // identical means there is one naming rule on this box, not two that can
  // drift until the kill switch quietly stops matching anything.
  return `boxcode-app-${must(id)}`;
}

/// Throws on anything that is not a valid id. Fail closed, and loudly: an id
/// is attacker-supplied, and while these functions return argv arrays rather
/// than shell strings -- so classic shell injection does not apply -- an id
/// like "x --privileged" would still arrive at docker as its own argument.
function must(id) {
  if (typeof id !== "string" || !ID_RE.test(id)) {
    throw new Error(`refusing to build container arguments for invalid id ${JSON.stringify(id)}`);
  }
  return id;
}

/// `docker network create` for one app.
export function createAppNetworkArgs(id) {
  return ["network", "create", "--internal", appNetworkName(id)];
}

/// `docker network connect`, to give Postgres an interface on that app's
/// network. Without this the app has a database URL it cannot reach.
export function connectPostgresArgs(id, postgresContainer = "boxcode-postgres") {
  return ["network", "connect", appNetworkName(id), postgresContainer];
}

/// `docker run` for a hosted app.
///
/// `appDir` is the built tree on the data volume. It is mounted read-only:
/// an app that wants to write gets the tmpfs at /tmp and nothing else, so a
/// compromised app cannot modify the code that runs on the next restart.
export function runAppArgs({ id, image, port, appDir, command = [], env = {} }) {
  const name = appContainerName(id);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    throw new Error(`refusing to publish app ${id} on invalid port ${port}`);
  }

  const args = [
    "run", "-d",
    "--name", name,
    "--runtime", "runsc",
    "--network", appNetworkName(id),

    // Bound to loopback, not 0.0.0.0. nginx is the only thing that may reach
    // an app; publishing on all interfaces would put ten demo apps directly on
    // the box's public address, behind nothing.
    "-p", `127.0.0.1:${port}:8080`,

    "--user", `${APP_UID}:${APP_UID}`,
    "--cap-drop", "ALL",
    "--security-opt", "no-new-privileges",
    "--read-only",
    "--tmpfs", `/tmp:rw,noexec,nosuid,size=${APP_TMPFS_MB}m`,

    // memory-swap equal to memory means no swap at all. Without it a leaking
    // app swaps instead of being killed, and drags the other nine down with it
    // by way of the disk.
    "--memory", `${APP_MEMORY_MB}m`,
    "--memory-swap", `${APP_MEMORY_MB}m`,
    "--cpus", APP_CPUS,
    "--pids-limit", String(APP_PIDS),

    "--restart", "unless-stopped",
    "--label", "boxcode:hosting=true",
    "--label", `boxcode:id=${id}`,

    "-v", `${appDir}:/app:ro`,
    "-w", "/app",
  ];

  for (const [k, v] of Object.entries(env)) {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(k)) {
      throw new Error(`refusing to set environment variable with invalid name ${JSON.stringify(k)}`);
    }
    args.push("-e", `${k}=${v}`);
  }

  args.push(image, ...command);
  return args;
}

/// `docker run` for a build.
///
/// Has network, because installing dependencies needs it. Has nothing else:
/// no app network, so no route to Postgres or to any running app; a wall clock
/// so a hung or deliberate infinite install cannot hold a slot forever; and it
/// is removed the moment it exits.
export function runBuildArgs({ id, image, srcDir, command }) {
  must(id);
  if (!Array.isArray(command) || command.length === 0) {
    throw new Error("a build needs a command");
  }
  return [
    "run", "--rm",
    "--name", `boxcode-build-${id}`,
    "--runtime", "runsc",

    // The default bridge, NOT this app's network. Egress to a package registry
    // is the entire reason this container exists; reaching Postgres or another
    // tenant is not, and being on a different network makes it impossible
    // rather than merely disallowed.
    "--network", "bridge",

    "--user", `${APP_UID}:${APP_UID}`,
    "--cap-drop", "ALL",
    "--security-opt", "no-new-privileges",
    "--memory", `${BUILD_MEMORY_MB}m`,
    "--memory-swap", `${BUILD_MEMORY_MB}m`,
    "--cpus", "1.0",
    "--pids-limit", "512",
    "--stop-timeout", "10",
    "--label", "boxcode:hosting=true",

    // Writable, unlike the app mount: the whole job is to produce node_modules.
    "-v", `${srcDir}:/src`,
    "-w", "/src",

    image,
    // timeout(1) inside the container rather than a timer outside it, so the
    // limit holds even if whatever launched the build has itself died.
    "timeout", "-s", "KILL", String(BUILD_TIMEOUT_S), ...command,
  ];
}
