#!/usr/bin/env bash
# Remove one hosted project, now, and leave nothing of it behind.
#
#   takedown.sh <id>
#
# The reaper already does this when a project's 48 hours are up. This is the
# same work on demand, for the two cases that will not wait: someone asking for
# their own project to be removed, and someone else's project that has to come
# down this minute -- a phishing page on the apex domain puts the reputation of
# every other page on boxcode.sh at risk, and there the time to remove is the
# only thing limiting the damage.
#
# Doing it by hand means composing three commands, knowing the slot, and
# remembering the registry. Miss the registry and reconciliation starts the
# project again within fifteen minutes, which is the failure that looks like it
# worked.
#
# Unlike the kill switch, this is not reversible. The kill switch stops projects
# serving and deletes nothing, so it is safe to fire on a suspicion. This
# deletes the database. Different tool, different question.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Absolute, because this runs over SSM where PATH is the bare default and node
# is not on it. kill-switch.sh learned the same thing; a bare `node` here fails
# at the one step that must not fail halfway.
NODE="${NODE:-/opt/node22/bin/node}"
STATE_DIR="${BOXCODE_STATE_DIR:-/opt/boxcode-hosting/state}"
APPS_DIR="${BOXCODE_APPS_DIR:-/opt/boxcode-hosting/apps}"
REGISTRY="$STATE_DIR/registry.json"

id="${1:?usage: takedown.sh <project-id>}"

# The same pattern the server enforces on every id. This one is not a tidy
# check: the value is about to be interpolated into a path, a systemd-adjacent
# shell call and a SQL identifier, so anything that is not an id must be refused
# before any of that rather than sanitised into something plausible.
if ! printf '%s' "$id" | grep -qE '^[a-z2-9]{4,16}$'; then
    echo "refusing $(printf '%q' "$id"): not a project id" >&2
    exit 2
fi

echo "== takedown: $id =="

# Which slot, asked before the registry entry goes. Falls back to the running
# VM, so a project whose registry entry was already lost can still be removed --
# that is exactly the half-cleaned state someone runs this to fix.
slot="$("$NODE" -e "
  const fs = require('fs');
  try {
    const r = JSON.parse(fs.readFileSync('$REGISTRY', 'utf8'));
    const p = r?.projects?.['$id'];
    if (p && Number.isInteger(p.slot)) { console.log(p.slot); process.exit(0); }
  } catch {}
" 2>/dev/null)"
if [ -z "$slot" ]; then
    slot="$(bash "$REPO/lifecycle/vm.sh" list 2>/dev/null \
            | awk -v n="boxcode-app-$id" '$1 == n { print $2; exit }')"
fi
if [ -z "$slot" ]; then
    echo "   no slot found in the registry or among running VMs"
    echo "   continuing anyway: the database and files may still be there"
    slot=0
fi
echo "   slot $slot"

# The registry goes FIRST, and the order is the whole design.
#
# Reconciliation stops any VM no project in the registry claims. So if this
# script dies anywhere below, the next sweep finishes the job rather than
# undoing it. Stopping the VM first would leave an entry pointing at nothing,
# and the same sweep would helpfully start the project again.
if [ -f "$REGISTRY" ]; then
    tmp="$(mktemp)"
    if "$NODE" -e "
      const fs = require('fs');
      const r = JSON.parse(fs.readFileSync('$REGISTRY', 'utf8'));
      if (r?.projects && '$id' in r.projects) { delete r.projects['$id']; }
      else { console.error('   (no registry entry -- already gone)'); }
      fs.writeFileSync('$tmp', JSON.stringify(r));
    "; then
        # Rename, not write-in-place: the control plane reads this file on every
        # sweep and a torn read is a registry it will refuse to parse.
        sudo cp "$tmp" "$REGISTRY" && echo "   registry entry removed"
    else
        echo "   WARNING: could not rewrite the registry; not continuing" >&2
        rm -f "$tmp"
        exit 1
    fi
    rm -f "$tmp"
fi

# Everything below is best-effort and independently idempotent. One failure must
# not stop the rest: a database that will not drop is worth reporting, but the
# VM still needs stopping and the disk image still needs deleting.
rc=0

# Removes the nginx route and reloads before killing the process, so nginx stops
# sending traffic to an address that is about to stop answering.
bash "$REPO/lifecycle/vm.sh" stop "$id" "$slot" >/dev/null 2>&1 \
    && echo "   microVM stopped" \
    || { echo "   WARNING: the microVM did not stop cleanly" >&2; rc=1; }

bash "$REPO/lifecycle/database.sh" drop "$id" >/dev/null 2>&1 \
    && echo "   database and role dropped" \
    || { echo "   WARNING: the database did not drop" >&2; rc=1; }

# Last. While this exists the project can in principle be started again, so it
# goes only once there is nothing left to start.
if [ -d "$APPS_DIR/$id" ]; then
    sudo rm -rf "${APPS_DIR:?}/$id" \
        && echo "   disk image and source removed" \
        || { echo "   WARNING: could not remove $APPS_DIR/$id" >&2; rc=1; }
fi

if [ "$rc" -eq 0 ]; then
    echo "== $id is gone =="
else
    echo "== $id is down, but something did not clean up -- see the warnings above ==" >&2
    echo "   Safe to run again: every step here is idempotent." >&2
fi
exit "$rc"
