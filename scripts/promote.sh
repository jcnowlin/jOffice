#!/usr/bin/env bash
# jOffice promotion gate (marathon 2 cycle 9; KAO rounds
# 1+2). The ONLY route by which a build becomes the daily
# driver:
#
#   "A build becomes the daily driver only by passing the
#    promotion gate as the exact store path that gets
#    installed; any check that nothing forces to run
#    counts as not run."
#
# What this script does, in order:
#   1. Refuses a dirty tree (tracked modifications, or
#      untracked files other than nix build result links):
#      what gets promoted is a commit, never a worktree.
#   2. Builds the candidate from the committed tree and
#      takes its exact store path.
#   3. Runs the machine-checkable gate battery against
#      THAT path: EX0 (direct-x2t conversion through the
#      candidate's own runtime + generated font map), FM4
#      (the runtime's converter map IS the converter
#      derivation's map; fonts.log version line matches
#      the pinned core constant), the ST1a export-ledger
#      self-test and the shell's built-in self-test suite,
#      all under a scratch HOME.
#   4. Requires GUI-gate evidence (G-PRINT: EX1/EX2/EX5
#      and the cycles 5-8 battery, G-PASS) recorded against
#      the SAME store path in an evidence JSON less than
#      48 hours old. GUI legs cannot run inside this
#      script; evidence that names another path, or none,
#      does not promote this one.
#   5. On pass: writes the launch manifest + a GC root at
#      stable paths under the promotion dir, and installs
#      the daily launcher + desktop entry pointing at the
#      manifest-checked path.
#
# Usage: promote.sh <evidence.json> [promotion-dir]
#   promotion-dir defaults to
#   ~/OpenCloud/Development/joffice-promoted
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE="${1:?usage: promote.sh <evidence.json> [promotion-dir]}"
PROMO_DIR="${2:-$HOME/OpenCloud/Development/joffice-promoted}"
DAILY_BIN_DIR="$HOME/OpenCloud/Development/ProjectBinaries"
APPS_DIR="$HOME/.local/share/applications"

fail() { echo "PROMOTE REFUSED: $*" >&2; exit 1; }

# --- 1. clean tree -------------------------------------
# Tracked modifications refuse outright. Untracked files
# refuse too, EXCEPT nix build result links: a symlink
# pointing into /nix/store is a build output, not a
# worktree change.
cd "$REPO"
DIRTY=""
while IFS= read -r line; do
  [ -z "$line" ] && continue
  if [ "${line:0:2}" = "??" ]; then
    p="${line:3}"
    if [ -L "$p" ] && case "$(readlink "$p")" in /nix/store/*) true;; *) false;; esac; then
      continue
    fi
  fi
  DIRTY="$DIRTY
$line"
done < <(git status --porcelain --untracked-files=all)
[ -z "$DIRTY" ] || fail "dirty tree:$DIRTY
Commit first; a promotion is a commit, not a worktree."
HEAD_REV="$(git rev-parse HEAD)"
echo "promote: tree clean at $HEAD_REV"

# --- 2. candidate --------------------------------------
CANDIDATE="$(nix build .#joffice --no-link --print-out-paths 2>/dev/null | tail -1)"
[ -n "$CANDIDATE" ] && [ -x "$CANDIDATE/bin/joffice-shell" ] \
  || fail "candidate build produced no shell at '$CANDIDATE'"
RUNTIME="$(nix build .#joffice-runtime --no-link --print-out-paths 2>/dev/null | tail -1)"
CONVERTER="$(nix build .#joffice-converter --no-link --print-out-paths 2>/dev/null | tail -1)"
RT="$RUNTIME/share/joffice/runtime"
echo "promote: candidate $CANDIDATE"

# --- 3. machine battery against the candidate ----------
# FM4: the runtime's converter map is the generated map,
# and its fonts.log carries the pinned version line.
cmp -s "$RT/converter/AllFonts.js" "$CONVERTER/AllFonts.js" \
  || fail "FM4: runtime converter map differs from the generated map"
cmp -s "$RT/converter/font_selection.bin" "$CONVERTER/font_selection.bin" \
  || fail "FM4: runtime font_selection.bin differs from the generated one"
head -1 "$RT/converter/fonts.log" | grep -qx 'ONLYOFFICE_FONTS_VERSION_15' \
  || fail "FM4: runtime fonts.log version line is not ONLYOFFICE_FONTS_VERSION_15"
echo "promote: FM4 ok"

# EX0: direct-x2t conversion through the candidate's own
# converter + generated font map (no shell involved).
EX0_DIR="$(mktemp -d)"
trap 'rm -rf "$EX0_DIR"' EXIT
mkdir -p "$EX0_DIR/work"
cat > "$EX0_DIR/params.xml" <<XEOF
<?xml version="1.0" encoding="utf-8"?>
<TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <m_sFileFrom>$REPO/nix/assets/fontmap-probe.docx</m_sFileFrom>
  <m_sFileTo>$EX0_DIR/probe.pdf</m_sFileTo>
  <m_nFormatTo>513</m_nFormatTo>
  <m_sTempDir>$EX0_DIR/work</m_sTempDir>
  <m_sTitle>jOffice</m_sTitle>
  <m_bIsNoBase64>true</m_bIsNoBase64>
  <m_sFontDir>$CONVERTER/fonts</m_sFontDir>
</TaskQueueDataConvert>
XEOF
LD_LIBRARY_PATH="$RT/converter" TMPDIR="$EX0_DIR/work" \
  "$RT/converter/x2t" "$EX0_DIR/params.xml" >/dev/null 2>&1 \
  || fail "EX0: candidate x2t conversion exited non-zero"
[ -s "$EX0_DIR/probe.pdf" ] || fail "EX0: candidate conversion produced no PDF"
echo "promote: EX0 ok"

# ST1a + the shell self-test suite under a scratch HOME.
SCRATCH_HOME="$(mktemp -d)"
HOME="$SCRATCH_HOME" "$CANDIDATE/bin/joffice-shell" --export-selftest \
  > "$EX0_DIR/selftest.log" 2>&1 \
  || fail "ST1a: export self-test failed (see $EX0_DIR/selftest.log)"
grep -q 'SELFTEST export ST1a: PASS' "$EX0_DIR/selftest.log" \
  || fail "ST1a: PASS line missing"
HOME="$SCRATCH_HOME" "$CANDIDATE/bin/joffice-shell" --selftest \
  >> "$EX0_DIR/selftest.log" 2>&1 \
  || fail "selftest: the shell self-test suite failed"
rm -rf "$SCRATCH_HOME"
echo "promote: ST1a + selftest ok"

# --- 4. GUI-gate evidence for THIS path -----------------
[ -f "$EVIDENCE" ] || fail "evidence file not found: $EVIDENCE"
python3 - "$EVIDENCE" "$CANDIDATE" <<'PYEOF' \
  || fail "evidence does not cover this candidate (path, G-PRINT/G-PASS, or freshness)"
import json, sys, time
ev = json.load(open(sys.argv[1]))
assert ev.get("store_path") == sys.argv[2], "store path mismatch"
assert ev.get("g_print") == "PASS", "G-PRINT not PASS"
assert ev.get("g_pass") == "PASS", "G-PASS not PASS"
age = time.time() - float(ev.get("when", 0))
assert age < 48 * 3600, "evidence stale: %.1f h old" % (age / 3600)
print("promote: evidence ok (%.1f h old)" % (age / 3600))
PYEOF

# --- 5. manifest + GC root + daily entry ----------------
mkdir -p "$PROMO_DIR" "$DAILY_BIN_DIR" "$APPS_DIR"
nix-store --add-root "$PROMO_DIR/current" -r "$CANDIDATE" >/dev/null
python3 - "$PROMO_DIR/manifest.json" "$CANDIDATE" "$HEAD_REV" <<'PYEOF'
import json, sys, time
manifest = {
    "store_path": sys.argv[2],
    "flake_rev": sys.argv[3],
    "promoted_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    "gate": "G-PROMOTE PASS (promote.sh: FM4 + EX0 + ST1a + selftest + evidence)",
}
with open(sys.argv[1], "w") as f:
    json.dump(manifest, f, indent=2)
print("promote: manifest written")
PYEOF
install -m755 "$REPO/scripts/joffice-daily.sh" "$DAILY_BIN_DIR/joffice-daily"
# The daily desktop entry: same identity as the packaged
# entry, but its Exec is the manifest-checking launcher,
# never a store path directly and never the clone's
# result link.
sed "s|^Exec=.*|Exec=$DAILY_BIN_DIR/joffice-daily %f|" \
  "$REPO/nix/joffice.desktop" > "$APPS_DIR/joffice.desktop"
chmod 644 "$APPS_DIR/joffice.desktop"
echo "PROMOTE PASS: $CANDIDATE is the daily driver (manifest: $PROMO_DIR/manifest.json)"
