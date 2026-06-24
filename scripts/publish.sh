#!/usr/bin/env bash
# Publish this single-crate repo to crates.io.
#
# crates.io publishes are IRREVERSIBLE — a version can be yanked, never deleted.
# Bump [package] version before re-publishing a changed crate.
#
# Usage:
#   ./scripts/publish.sh            # real publish (uploads to crates.io)
#   ./scripts/publish.sh --dry-run  # package + verify without uploading
set -euo pipefail
cd "$(dirname "$0")/.."

DRY="${1:-}"

# --allow-dirty: cargo publish regenerates Cargo.lock during its verify build (deps drift between
# releases), which trips the git-clean check on a fresh CI checkout. The lock is not part of a
# library's published package, so the upload still matches the tagged source.
if [ "$DRY" = "--dry-run" ]; then
  cargo publish --dry-run --allow-dirty
  exit 0
fi

# Resumable + rate-limit-aware: skip if this version is already on crates.io (so a re-run resumes a
# partial publish), and retry through crates.io's NEW-crate rate limit (~1 per 10 min).
published=0
for attempt in $(seq 1 30); do
  if cargo publish --allow-dirty 2>/tmp/publish.err; then published=1; break; fi
  if grep -qiE "already uploaded|already exists|crate version .* is already uploaded" /tmp/publish.err; then
    echo "    already published — skipping"; published=1; break
  fi
  if grep -qi "429 Too Many Requests" /tmp/publish.err; then
    echo "    rate-limited (attempt $attempt) — waiting 120s"; sleep 120
  else
    echo "    ERROR publishing:"; cat /tmp/publish.err; exit 1
  fi
done
[ "$published" -eq 1 ] || { echo "    gave up after 30 retries"; exit 1; }

echo "done."
