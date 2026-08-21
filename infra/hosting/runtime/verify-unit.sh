#!/usr/bin/env bash
# Checks a generated unit against real systemd, without needing the box.
#
# This exists because systemd does not fail on a directive it does not
# understand -- it logs "Unknown key ... ignoring" and starts the service
# anyway. A misspelled hardening directive, or one in the wrong section, is
# therefore invisible: the unit works, and the protection is simply not there.
#
# Both of those had already happened when this script was first run:
# StartLimitIntervalSec was in [Service] instead of [Unit], and SystemCallFilter
# used a ~ on every entry instead of one on the list. Neither was visible in the
# unit tests, which only assert the text we generate.
#
# Runs anywhere with Docker; needs no AWS and no Linux host.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

node -e "
import('file://$REPO/infra/hosting/runtime/unit.mjs').then(m => {
  const u = m.renderUnit({
    id: 'k9depef6', port: 10000, execStart: '/bin/sh server.js',
    env: { DATABASE_URL: 'postgresql://app:pw@127.0.0.1:5432/app_k9depef6' },
  });
  require('fs').writeFileSync('$OUT/boxcode-app-k9depef6.service', u);
});
"

docker run --rm -v "$OUT":/u:ro debian:12 sh -c '
    apt-get update -qq >/dev/null 2>&1
    apt-get install -y -qq systemd >/dev/null 2>&1

    echo "== systemd-analyze verify =="
    # "not found" lines are the units we declare a dependency on (postgresql)
    # not existing in this throwaway container, which is expected.
    WARN=$(systemd-analyze verify /u/boxcode-app-k9depef6.service 2>&1 | grep -v "not found" || true)
    if [ -n "$WARN" ]; then
        echo "$WARN"
        echo "FAIL: systemd would ignore the directives above, silently."
        exit 1
    fi
    echo "clean -- no ignored keys, no unparseable values"

    echo
    echo "== systemd-analyze security =="
    # Exposure is 0 (locked down) to 10 (unrestricted). A plain unhardened
    # service scores about 9.6; anything under 3 is "OK" by systemd own rating.
    systemd-analyze security --offline=true /u/boxcode-app-k9depef6.service 2>/dev/null \
        | tail -3
'
