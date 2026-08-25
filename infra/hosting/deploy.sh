#!/usr/bin/env bash
# Put this directory onto the runner box and provision it.
#
#   bash infra/hosting/deploy.sh                 # normal: bundle, upload, setup
#   SKIP_TLS=1 bash infra/hosting/deploy.sh      # before DNS points at the box
#   bash infra/hosting/deploy.sh smoke           # bundle, upload, run the smoke test
#
# Everything here was done by hand the first time and every step of it was got
# wrong at least once, which is why it is a script:
#
#   - `tar --exclude` after the path is not an exclude, it is a filename. The
#     first bundle was 163 MB of Terraform provider instead of 100 KB.
#   - the box has no route to a private git remote, so the source arrives over
#     S3, on the one key its instance role can read.
#   - SSM SendCommand returns a spurious AccessDeniedException often enough that
#     a single attempt is unreliable. Retried below.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUCKET="${BOOTSTRAP_BUCKET:-boxcode-artifacts}"
KEY="hosting/hosting.tgz"
MODE="${1:-setup}"
SKIP_TLS="${SKIP_TLS:-0}"

command -v aws >/dev/null || { echo "aws cli is required" >&2; exit 1; }

# The instance is found by tag, not by a hard-coded id: the id changes if the
# box is ever replaced, and a script that then provisions nothing while
# reporting success is worse than one that cannot find it.
INSTANCE="${RUNNER_INSTANCE_ID:-$(aws ec2 describe-instances \
    --filters "Name=tag:Name,Values=boxcode-runner" "Name=instance-state-name,Values=running,pending" \
    --query 'Reservations[0].Instances[0].InstanceId' --output text 2>/dev/null)}"
if [ -z "$INSTANCE" ] || [ "$INSTANCE" = "None" ]; then
    echo "no running instance tagged Name=boxcode-runner." >&2
    echo "If it is stopped:  aws ec2 start-instances --instance-ids <id>" >&2
    exit 1
fi
echo "runner: $INSTANCE"

echo "== bundling =="
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
# Excludes BEFORE the path. After it they are read as filenames and silently do
# nothing -- see the header.
tar --exclude='.terraform' --exclude='.terraform.lock.hcl' \
    --exclude='terraform.tfstate*' --exclude='terraform.tfvars' \
    --exclude='*.zip' --exclude='.terraform-*' \
    -czf "$TMP/hosting.tgz" -C "$(dirname "$HERE")" "$(basename "$HERE")"
SIZE="$(du -k "$TMP/hosting.tgz" | cut -f1)"
echo "   ${SIZE} KB, $(tar -tzf "$TMP/hosting.tgz" | wc -l | tr -d ' ') entries"
# A bundle that size is Terraform state or a provider binary, not source.
[ "$SIZE" -lt 5000 ] || { echo "bundle is ${SIZE} KB -- something is being included that should not be" >&2; exit 1; }

echo "== uploading =="
aws s3 cp "$TMP/hosting.tgz" "s3://${BUCKET}/${KEY}" --only-show-errors || exit 1

case "$MODE" in
    setup) REMOTE="SKIP_TLS=${SKIP_TLS} bash hosting/setup.sh" ;;
    smoke) REMOTE="bash hosting/smoke-test.sh" ;;
    *)     echo "usage: deploy.sh [setup|smoke]" >&2; exit 2 ;;
esac

cat > "$TMP/remote.sh" <<REMOTEEOF
set -uo pipefail
mkdir -p /opt/stage && cd /opt/stage
rm -rf hosting
aws s3 cp s3://${BUCKET}/${KEY} . --only-show-errors || exit 1
# macOS tar writes xattr headers GNU tar warns about, one line per file. They
# are harmless and they drown everything else.
tar -xzf hosting.tgz 2>/dev/null
chmod +x hosting/*.sh hosting/rootfs/*.sh hosting/lifecycle/*.sh 2>/dev/null || true
${REMOTE} > /var/log/boxcode-${MODE}.log 2>&1
rc=\$?
echo "${MODE} exit: \$rc"
tail -40 /var/log/boxcode-${MODE}.log
exit \$rc
REMOTEEOF

python3 -c "import json,sys; json.dump({'commands':[open('$TMP/remote.sh').read()]}, open('$TMP/p.json','w'))"

echo "== running ${MODE} on the box =="
ID=""
for try in 1 2 3 4 5; do
    ID=$(aws ssm send-command --instance-ids "$INSTANCE" --document-name AWS-RunShellScript \
         --parameters "file://$TMP/p.json" --timeout-seconds 3600 \
         --comment "boxcode hosting ${MODE}" --query 'Command.CommandId' --output text 2>/dev/null)
    case "$ID" in [0-9a-f]*-*-*) break;; *) ID=""; sleep 5;; esac
done
[ -n "$ID" ] || { echo "SendCommand failed after 5 tries" >&2; exit 1; }

STATUS=""
for _ in $(seq 1 240); do
    STATUS=$(aws ssm get-command-invocation --command-id "$ID" --instance-id "$INSTANCE" \
             --query Status --output text 2>/dev/null)
    case "$STATUS" in Success|Failed|Cancelled|TimedOut) break;; esac
    sleep 5
done

aws ssm get-command-invocation --command-id "$ID" --instance-id "$INSTANCE" \
    --query 'join(``,[StandardOutputContent,StandardErrorContent])' --output text 2>/dev/null \
    | grep -vE '^\[ *[0-9]+\.[0-9]+\]'

echo
echo "== ${MODE}: ${STATUS} =="
[ "$STATUS" = Success ]
