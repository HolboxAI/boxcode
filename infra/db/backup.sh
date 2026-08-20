#!/usr/bin/env bash
# Nightly snapshot of every project's SQLite file to S3.
#
# Until this existed there was no backup of any kind: durability was "the EBS
# volume is still there". One terminated instance, one corrupted filesystem, or
# one `DROP TABLE` a developer regrets, and every project's data was gone with
# no way back. That was survivable while the databases held toy demos; it stops
# being survivable the moment anything is built on top of them.
#
# Deliberately dumb: no incremental logic, no manifest, no restore tooling. A
# dated copy of each file, kept by an S3 lifecycle rule, restorable with `aws
# s3 cp`. A backup nobody can explain is a backup nobody will trust at 3am.
#
# Run from cron/systemd-timer on the box (see setup.sh). Idempotent, and safe
# to run by hand at any time.

set -euo pipefail

DATA_DIR="${DATA_DIR:-/opt/boxcode-db/data}"
BUCKET="${BACKUP_BUCKET:-boxcode-artifacts}"
PREFIX="${BACKUP_PREFIX:-backups/db}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

if [ ! -d "$DATA_DIR" ]; then
  echo "no data directory at $DATA_DIR -- nothing to back up"
  exit 0
fi

stamp="$(date -u +%Y-%m-%d)"
count=0

for db in "$DATA_DIR"/*.sqlite; do
  [ -e "$db" ] || break
  name="$(basename "$db")"
  # `.backup` rather than `cp`: SQLite may be mid-write when this runs, and a
  # byte copy of a file with a live journal restores as a corrupt database.
  # The backup API takes a consistent snapshot while other connections keep
  # working -- which matters because this runs against a live service.
  if sqlite3 "$db" ".backup '$STAGE/$name'" 2>/dev/null; then
    count=$((count + 1))
  else
    echo "WARNING: could not snapshot $name -- skipped" >&2
  fi
done

if [ "$count" -eq 0 ]; then
  echo "no databases to back up"
  exit 0
fi

# Compressed as one object per night rather than N: these files are mostly
# empty pages and compress hard, and one object per night is what makes the
# lifecycle rule and any restore obvious.
tar -czf "$STAGE/db-$stamp.tar.gz" -C "$STAGE" --exclude='*.tar.gz' .
aws s3 cp "$STAGE/db-$stamp.tar.gz" "s3://$BUCKET/$PREFIX/db-$stamp.tar.gz" \
  --only-show-errors

echo "backed up $count database(s) to s3://$BUCKET/$PREFIX/db-$stamp.tar.gz"

# To restore one project:
#   aws s3 cp s3://BUCKET/backups/db/db-YYYY-MM-DD.tar.gz .
#   tar -xzf db-YYYY-MM-DD.tar.gz ./<project_id>.sqlite
#   systemctl stop boxcode-db-control-plane
#   cp <project_id>.sqlite /opt/boxcode-db/data/
#   systemctl start boxcode-db-control-plane
