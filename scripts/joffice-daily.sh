#!/usr/bin/env bash
# jOffice daily launcher (marathon 2 cycle 9): execs ONLY
# the store path recorded in the promotion manifest,
# after checking the path still exists and is the very
# path the promotion gate GC-rooted. A missing or
# mismatched manifest is a refusal, never a fallback —
# the daily driver is a promoted build or it is nothing.
set -euo pipefail
PROMO_DIR="${JOFFICE_PROMO_DIR:-$HOME/OpenCloud/Development/joffice-promoted}"
MANIFEST="$PROMO_DIR/manifest.json"
[ -f "$MANIFEST" ] || { echo "joffice-daily: no promotion manifest at $MANIFEST — run scripts/promote.sh" >&2; exit 1; }
STORE_PATH="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["store_path"])' "$MANIFEST")"
[ -n "$STORE_PATH" ] && [ -x "$STORE_PATH/bin/joffice-shell" ] \
  || { echo "joffice-daily: promoted path $STORE_PATH is gone — re-run scripts/promote.sh" >&2; exit 1; }
ROOT_TARGET="$(readlink "$PROMO_DIR/current" || true)"
[ "$ROOT_TARGET" = "$STORE_PATH" ] \
  || { echo "joffice-daily: manifest path does not match the promotion GC root ($ROOT_TARGET) — refusing" >&2; exit 1; }
exec "$STORE_PATH/bin/joffice-shell" "$@"
