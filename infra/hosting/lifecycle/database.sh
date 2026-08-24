#!/usr/bin/env bash
# Create or remove one project's database and login role.
#
#   database.sh provision <id> <slot>   # prints the DATABASE_URL on stdout
#   database.sh drop <id>
#   database.sh harden                  # once, when the box is provisioned
#
# The SQL comes from runtime/database.mjs, which is where the reasoning and the
# tests live. This script runs it and manages the password.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NODE="${NODE:-/opt/node22/bin/node}"
APPS_DIR="${APPS_DIR:-/opt/boxcode-apps}"

action="${1:?usage: database.sh provision|drop|harden [id] [slot]}"

# Every statement as the superuser goes over the Unix socket with peer auth, so
# there is no superuser password anywhere on this box to leak.
psql_super() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qtA "$@"; }

sql_from() {
    "$NODE" -e "
      import('file://$REPO/runtime/database.mjs').then(m => {
        const out = m[process.argv[1]](JSON.parse(process.argv[2] || 'null'));
        process.stdout.write(Array.isArray(out) ? out.join('\n') : String(out));
      });
    " "$@"
}

if [ "$action" = "harden" ]; then
    echo "== closing the PUBLIC defaults =="
    # PostgreSQL grants CONNECT on every database to PUBLIC. Without this,
    # creating a role per project buys nothing -- any role could open any other
    # project's database the moment it reached the port.
    sql_from harden | psql_super
    echo "postgres and template1 no longer grant CONNECT to PUBLIC"
    exit 0
fi

id="${2:?project id}"
case "$id" in *[!a-z2-9]* | "" ) echo "invalid project id: $id" >&2; exit 2 ;; esac
[ "${#id}" -ge 4 ] && [ "${#id}" -le 16 ] || { echo "invalid project id length: $id" >&2; exit 2; }

db="app_$id"

case "$action" in
drop)
    "$NODE" -e "
      import('file://$REPO/runtime/database.mjs').then(m =>
        process.stdout.write(m.dropSql(process.argv[1]).join('\n')));
    " "$id" | psql_super
    sudo rm -f "$APPS_DIR/$id/db.url"
    echo "dropped $db"
    ;;

provision)
    slot="${3:?slot number}"
    case "$slot" in *[!0-9]* | "" ) echo "invalid slot: $slot" >&2; exit 2 ;; esac

    # Rotated on every deploy rather than stored and reused. A password that
    # never changes is one that outlives the project it belonged to, sitting in
    # an image and a shell history for as long as anyone keeps either.
    password="$(openssl rand -hex 32)"

    payload="$("$NODE" -e "
      console.log(JSON.stringify({ id: process.argv[1], password: process.argv[2] }));
    " "$id" "$password")"

    plan="$("$NODE" -e "
      import('file://$REPO/runtime/database.mjs').then(m =>
        console.log(JSON.stringify(m.provisionSql(JSON.parse(process.argv[1])))));
    " "$payload")"

    part() { printf '%s' "$plan" | "$NODE" -e "
      let s=''; process.stdin.on('data',d=>s+=d).on('end',()=>{
        const p = JSON.parse(s)[process.argv[1]];
        process.stdout.write(Array.isArray(p) ? p.join('\n') : String(p));
      });" "$1"; }

    echo "== $id: role ==" >&2
    part cluster | psql_super

    # CREATE DATABASE has no IF NOT EXISTS and cannot run inside a transaction,
    # so existence is checked first rather than relying on the error.
    if [ -z "$(psql_super -c "$(part databaseExists)")" ]; then
        echo "== $id: database ==" >&2
        psql_super -c "$(part createDatabase)"
    else
        echo "== $id: database already exists ==" >&2
    fi

    echo "== $id: grants ==" >&2
    part grants | psql_super
    part inDatabase | psql_super -d "$db"

    url="$("$NODE" -e "
      import('file://$REPO/runtime/database.mjs').then(m =>
        process.stdout.write(m.databaseUrl({
          id: process.argv[1], password: process.argv[2], slot: Number(process.argv[3]),
        })));
    " "$id" "$password" "$slot")"

    # Kept so a redeploy that skips provisioning can still find the URL, and so
    # an operator can reach a project's database during an incident. 0600 and
    # owned by root: the guest never reads this file, it gets the URL baked into
    # its image at build time.
    sudo mkdir -p "$APPS_DIR/$id"
    printf '%s\n' "$url" | sudo tee "$APPS_DIR/$id/db.url" >/dev/null
    sudo chmod 600 "$APPS_DIR/$id/db.url"

    # The URL contains the password, so it goes to stdout for the caller to
    # capture and never into the log stream on stderr above.
    printf '%s\n' "$url"
    ;;

*)
    echo "usage: database.sh provision|drop|harden [id] [slot]" >&2
    exit 2
    ;;
esac
