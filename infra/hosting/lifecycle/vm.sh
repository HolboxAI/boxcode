#!/usr/bin/env bash
# Start or stop one project's microVM.
#
#   vm.sh start <id> <slot>
#   vm.sh stop  <id> <slot>
#   vm.sh list
#
# `list` prints what is actually running, in the shape reconcile.mjs expects:
# one `<name> <slot> <pid>` per line. That is deliberately the only way the
# control plane learns what exists -- the registry says what *should* be
# running, the process table says what *is*, and reconciliation is the only
# place the two are compared.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NODE="${NODE:-/opt/node22/bin/node}"
FC_DIR="${FC_DIR:-/opt/firecracker}"
APPS_DIR="${APPS_DIR:-/opt/boxcode-apps}"
NGINX_DIR="${NGINX_DIR:-/etc/nginx/conf.d/app-projects}"

action="${1:?usage: vm.sh start|stop|list [id] [slot]}"

# `list` needs no arguments and must work on an empty box, so it comes first.
if [ "$action" = "list" ]; then
    # Matched on the jailer's own argv rather than on a pid file. A pid file
    # written by something that then crashed is a lie; the process table is not.
    ps -eo pid=,args= | while read -r pid args; do
        case "$args" in
            */jailer*--id*)
                vm_id=$(printf '%s\n' "$args" | sed -n 's/.*--id[= ]\([a-z2-9]\{4,16\}\).*/\1/p')
                [ -n "$vm_id" ] || continue
                vm_uid=$(printf '%s\n' "$args" | sed -n 's/.*--uid[= ]\([0-9][0-9]*\).*/\1/p')
                # The slot is recoverable from the jailer uid, which is
                # 30000 + slot. Nothing else in the argv carries it.
                if [ -n "$vm_uid" ] && [ "$vm_uid" -ge 30000 ]; then
                    printf 'boxcode-app-%s %s %s\n' "$vm_id" "$((vm_uid - 30000))" "$pid"
                else
                    printf 'boxcode-app-%s - %s\n' "$vm_id" "$pid"
                fi
                ;;
        esac
    done
    exit 0
fi

id="${2:?project id}"
slot="${3:?slot number}"
case "$id" in *[!a-z2-9]* | "" ) echo "invalid project id: $id" >&2; exit 2 ;; esac
[ "${#id}" -ge 4 ] && [ "${#id}" -le 16 ] || { echo "invalid project id length: $id" >&2; exit 2; }
case "$slot" in *[!0-9]* | "" ) echo "invalid slot: $slot" >&2; exit 2 ;; esac

image="$APPS_DIR/$id/rootfs.ext4"
jail="$("$NODE" -e "
  import('file://$REPO/runtime/machine.mjs').then(m =>
    console.log(m.jailPath(process.argv[1], Number(process.argv[2]))));
" "$id" "$slot")"
uid=$((30000 + slot))

stop_vm() {
    # Firecracker has no graceful shutdown without the API socket, and the API
    # socket is deliberately not there -- it is a control channel into the VMM
    # that nothing needs. A guest is stateless by design: its disk is a
    # throwaway image and its database lives on the host. So SIGKILL is not
    # brutality, it is the defined way to stop one.
    pkill -f -- "--id ${id} " 2>/dev/null || true
    for _ in $(seq 1 20); do
        pgrep -f -- "--id ${id} " >/dev/null 2>&1 || break
        sleep 0.25
    done
    if pgrep -f -- "--id ${id} " >/dev/null 2>&1; then
        echo "VM for $id did not exit" >&2
        return 1
    fi
    sudo rm -f "$NGINX_DIR/$id.conf"
    sudo rm -rf "$jail"
    sudo /usr/local/sbin/boxcode-slot-net down "$slot" || true
}

case "$action" in
stop)
    echo "== $id: stopping =="
    stop_vm
    # Reloading after the route is gone, so nginx stops sending traffic to an
    # address with nothing behind it. A missed reload here is a 502 per request
    # rather than a clean 404.
    sudo nginx -t && sudo systemctl reload nginx
    echo "== $id: stopped =="
    ;;

start)
    [ -f "$image" ] || { echo "no image at $image" >&2; exit 1; }

    echo "== $id: slot $slot =="
    # Starting over a VM that is somehow already there would leave two
    # processes for one project, which reconcile.mjs would then have to clean
    # up. Cheaper to be idempotent here.
    stop_vm >/dev/null 2>&1 || true
    sudo /usr/local/sbin/boxcode-slot-net up "$slot"

    sudo rm -rf "$jail"
    sudo mkdir -p "$jail"
    sudo cp --reflink=auto "$image" "$jail/rootfs.ext4"
    sudo cp "$FC_DIR/vmlinux" "$jail/vmlinux"

    "$NODE" -e "
      import('file://$REPO/runtime/machine.mjs').then(m => {
        console.log(JSON.stringify(m.machineConfig({
          id: process.argv[1],
          slot: Number(process.argv[2]),
          kernel: '/vmlinux',
          rootfs: '/rootfs.ext4',
        }), null, 2));
      });
    " "$id" "$slot" | sudo tee "$jail/vm.json" >/dev/null

    sudo chown -R "$uid:$uid" "$jail"

    mapfile -t jailer_args < <("$NODE" -e "
      import('file://$REPO/runtime/machine.mjs').then(m =>
        console.log(m.jailerArgs({
          id: process.argv[1], slot: Number(process.argv[2]), configFile: 'vm.json',
        }).join('\n')));
    " "$id" "$slot")

    # Detached on purpose. The control plane is not what keeps a microVM alive
    # -- these processes outlive it, which is exactly why reconcile.mjs exists
    # and why restarting the control plane is safe.
    sudo setsid /usr/bin/jailer "${jailer_args[@]}" \
        >"/var/log/boxcode-app-$id.log" 2>&1 < /dev/null &
    disown || true

    # A microVM boots in about a tenth of a second, so anything that is going to
    # fail has failed well inside this. Checked rather than assumed: reporting a
    # successful deploy for a VM that died on boot is worse than a slow deploy.
    for _ in $(seq 1 20); do
        pgrep -f -- "--id ${id} " >/dev/null 2>&1 && break
        sleep 0.25
    done
    if ! pgrep -f -- "--id ${id} " >/dev/null 2>&1; then
        echo "the VM for $id exited immediately. Last of its log:" >&2
        tail -20 "/var/log/boxcode-app-$id.log" >&2 || true
        sudo /usr/local/sbin/boxcode-slot-net down "$slot" || true
        exit 1
    fi

    echo "== $id: routing =="
    "$NODE" -e "
      import('file://$REPO/runtime/network.mjs').then(m =>
        process.stdout.write(m.renderNginxRoute(process.argv[1], Number(process.argv[2]))));
    " "$id" "$slot" | sudo tee "$NGINX_DIR/$id.conf" >/dev/null
    # Tested before reloading: one bad generated file would otherwise take every
    # other project's route down with it.
    sudo nginx -t && sudo systemctl reload nginx

    echo "== $id: running on slot $slot =="
    ;;

*)
    echo "usage: vm.sh start|stop|list [id] [slot]" >&2
    exit 2
    ;;
esac
