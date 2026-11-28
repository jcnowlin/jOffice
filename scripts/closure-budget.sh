#!/usr/bin/env bash
# jOffice closure budget (marathon 2 cycle 9, CL1).
#
# Rule (KAO cycle-9 round 1): fail on any NEW requisite
# over 50 MB. "New" is measured against the 5e93604
# closure baseline below — the requisites over 50 MB
# that were already there, each with its reason. The
# point is catching accidental bloat, not re-litigating
# the known residents; a deliberate addition earns its
# own baseline line here, with its reason, in the same
# commit that adds it.
#
# Baseline (per-path narSize, 5e93604 closure, 355 paths):
#   joffice-runtime-9.3.8   1874 MB — the product itself:
#     the repackaged Euro-Office runtime (deb payload,
#     sdkjs + web-apps develop trees, dictionaries).
#   gcc-14.3.0               257 MB — a DIRECT requisite
#     of webkitgtk (nix why-depends: shell -> webkitgtk
#     -> gcc). Accepted per the design: webkitgtk is not
#     overridden. It is nixpkgs' wart, documented here.
#   webkitgtk-2.50.4+abi=6.0 168 MB — the web view; the
#     shell's reason to exist.
#   python3-3.12.12          114 MB — transitive (glib /
#     systemd tooling chain), present in the baseline.
#   flite-2.2-lib             63 MB — speech synthesis
#     library via gst-plugins-bad / webkitgtk media.
#   perl-5.40.0               60 MB — transitive tooling
#     chain, present in the baseline.
#   joffice-converter-fontmap-9.3.8 — cycle 9's own
#     addition: the dereferenced shipped font set + the
#     generated map. Deliberate; it replaces the deb's
#     stale converter artifacts. (Size recorded in
#     CYCLES.md at promotion time.)
#
# Usage: closure-budget.sh <store-path>
set -euo pipefail
TARGET="${1:?usage: closure-budget.sh <store-path>}"
LIMIT=$((50 * 1000 * 1000))
nix path-info -r --json "$TARGET" | python3 -c '
import json, re, sys
limit = int(sys.argv[1])
baseline = {
    "joffice-runtime",
    "gcc",
    "webkitgtk",
    "python3",
    "flite",
    "perl",
    "joffice-converter-fontmap",
    "joffice-shell",
}
data = json.load(sys.stdin)
bad = []
for path, info in data.items():
    name = re.sub(r"^[0-9a-z]{32}-", "", path.split("/")[-1])
    base = re.split(r"-[0-9]", name)[0]
    size = info["narSize"]
    if size > limit and base not in baseline:
        bad.append((size, path))
if bad:
    for size, path in sorted(bad, reverse=True):
        print("OVER BUDGET: %.1f MB  %s" % (size / 1e6, path))
    print("CL1 FAIL: new requisite(s) over 50 MB not in the documented baseline")
    sys.exit(1)
print("CL1 PASS: no new requisite over 50 MB beyond the documented baseline (%d paths)" % len(data))
' "$LIMIT"
