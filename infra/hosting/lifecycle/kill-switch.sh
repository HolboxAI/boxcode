#!/usr/bin/env bash
# Stop every hosted project, or let them come back.
#
#   kill-switch.sh stop
#   kill-switch.sh restore
#   kill-switch.sh status
#
# Run on the box by the boxcode-kill-switch Lambda through SSM, or by hand.
#
# It stops projects serving. It deletes nothing: no image, no database, no
# registry entry. Everything it does is reversible in one command, which is the
# property that makes a kill switch one people dare to arm -- one that deleted
# would be one nobody ever fired.
#
# Which VMs it may stop is decided by kill-switch/scope.mjs, not by a pattern
# written here. That module is tested against the real names of the production
# functions in this account, and having one answer to "is this ours" is worth
# more than the convenience of a grep.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NODE="${NODE:-/opt/node22/bin/node}"
STATE_DIR="${BOXCODE_STATE_DIR:-/opt/boxcode-hosting/state}"
NGINX_DIR="${NGINX_DIR:-/etc/nginx/conf.d/app-projects}"

# The control plane checks for this before starting anything. Without it,
# reconciliation would see ten registry entries with nothing running and
# helpfully start them all again within fifteen minutes -- undoing the switch
# while it was still meant to be held.
FLAG="$STATE_DIR/killed"

# Returned to anyone who asks for a hosted project while the switch is on. A
# plain 503 with a reason beats a connection refused: it says the platform is
# deliberately stopped rather than broken.
BLOCK_CONF="$NGINX_DIR/_killed.conf"

action="${1:?usage: kill-switch.sh stop|restore|status}"

case "$action" in
stop)
    echo "== kill switch: stopping =="
    sudo mkdir -p "$STATE_DIR"
    # Written first. If this script dies halfway through stopping VMs, the
    # flag is already there and reconciliation will not restart the ones it
    # managed to stop.
    date -Is | sudo tee "$FLAG" >/dev/null

    sudo tee "$BLOCK_CONF" >/dev/null <<'EOF'
# Written by kill-switch.sh. Removed by `kill-switch.sh restore`.
location ^~ /api/ {
    return 503 "boxcode hosting is temporarily stopped\n";
    add_header Content-Type text/plain always;
}
EOF
    # ^~ so it beats every per-project location, which are also ^~ and would
    # otherwise win on being longer.
    # -s reload, not systemctl: this runs from the control plane's own service
    # context via SSM, where systemctl silently does not reload nginx.
    sudo nginx -t && sudo nginx -s reload
    echo "   /api/* now returns 503"

    running="$(bash "$REPO/lifecycle/vm.sh" list || true)"
    if [ -z "$running" ]; then
        echo "   no microVMs running"
        exit 0
    fi

    plan="$(printf '%s\n' "$running" | "$NODE" -e "
      let s='';
      process.stdin.on('data', d => s += d).on('end', async () => {
        const scope = await import('file://$REPO/kill-switch/scope.mjs');
        const items = s.trim().split('\n').filter(Boolean).map(line => {
          const [name, slot] = line.split(/\s+/);
          return { name, slot };
        });
        const { allowed, skipped } = scope.partition(items);
        const bySlot = Object.fromEntries(items.map(i => [i.name, i.slot]));
        // Logged in full, every time. During an incident the question asked at
        // speed is 'what did it touch', and the answer has to be in the log
        // rather than reconstructed afterwards.
        console.error(JSON.stringify({ willStop: allowed, leftAlone: skipped }));
        for (const name of allowed) console.log(name + ' ' + (bySlot[name] ?? '0'));
      });
    ")"

    while read -r name slot; do
        [ -n "$name" ] || continue
        id="${name#boxcode-app-}"
        echo "   stopping $id"
        # One failure must not stop the rest: a VM that will not die is worth
        # reporting, but the other nine still need stopping.
        bash "$REPO/lifecycle/vm.sh" stop "$id" "$slot" >/dev/null 2>&1 \
            || echo "   WARNING: $id did not stop" >&2
    done <<<"$plan"

    echo "== kill switch: stopped =="
    ;;

restore)
    echo "== kill switch: restoring =="
    sudo rm -f "$FLAG" "$BLOCK_CONF"
    sudo nginx -t && sudo nginx -s reload
    # Nothing is started here on purpose. The control plane's reconciliation
    # already knows what should be running -- it is the registry -- and starting
    # things from two places is how a box ends up with two VMs for one project.
    echo "   /api/* is live again; the control plane will restart projects on"
    echo "   its next sweep, within 15 minutes. To not wait:"
    echo "     systemctl restart boxcode-hosting-control-plane"
    ;;

status)
    if [ -f "$FLAG" ]; then
        echo "STOPPED since $(cat "$FLAG")"
        exit 0
    fi
    echo "running"
    ;;

*)
    echo "usage: kill-switch.sh stop|restore|status" >&2
    exit 2
    ;;
esac
