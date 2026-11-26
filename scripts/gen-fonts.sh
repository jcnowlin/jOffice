#!/usr/bin/env bash
# gen-fonts.sh — generate the complete jOffice font set from ONE
# allfontsgen run (Kagi marathon-2 cycle-1 design, round 2 §1).
#
# One run produces every font artifact the shell serves, so the web
# editor and x2t can never disagree about the font set:
#   <out>/web/AllFonts.js          server-style font map (numeric
#                                  file entries; kept as an artifact
#                                  of the run — NOT what the desktop
#                                  shell stages, see desktop/)
#   <out>/web/fonts/               web font files (numbered)
#   <out>/web/images/              fonts_thumbnail*.png sprites
#   <out>/desktop/AllFonts.js      THE editor font map for the
#                                  desktop shell (sdkjs/common): the
#                                  path-keyed file (a copy of the
#                                  x2t output). The desktop font
#                                  manager joins the selection bin's
#                                  path-keyed records against
#                                  __fonts_files; the numeric web
#                                  entries break substitution
#                                  lookups (Calibri->Carlito) and
#                                  the text shaper dies on a null
#                                  FontId. Proven by drive bisect,
#                                  marathon2/CYCLES.md.
#   <out>/x2t/AllFonts.js          native font map for x2t
#   <out>/x2t/font_selection.bin   x2t font selection (beside the
#                                  native AllFonts.js — x2t finds it
#                                  there; there is no XML element)
#   <out>/x2t/fonts.log            generation record of the font set
#
# Usage:
#   gen-fonts.sh --allfontsgen <bin> --input <fonts-root> --out <dir>
#
# --input may be several directories separated by semicolons.
# Generation is cached: a stamp of the input listing (paths, sizes,
# mtimes) is kept in <out>/.gen-stamp; if it matches and all outputs
# exist, the run is skipped. Set GEN_FONTS_FORCE=1 to regenerate.
set -euo pipefail

ALLFONTSGEN=""
INPUT=""
OUT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --allfontsgen) ALLFONTSGEN="$2"; shift 2 ;;
        --input)       INPUT="$2";       shift 2 ;;
        --out)         OUT="$2";         shift 2 ;;
        *) echo "gen-fonts: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ -n "$ALLFONTSGEN" ] && [ -n "$INPUT" ] && [ -n "$OUT" ] || {
    echo "usage: gen-fonts.sh --allfontsgen <bin> --input <fonts-root> --out <dir>" >&2
    exit 2
}
[ -x "$ALLFONTSGEN" ] || { echo "gen-fonts: allfontsgen not executable: $ALLFONTSGEN" >&2; exit 1; }

mkdir -p "$OUT/web/images" "$OUT/web/fonts" "$OUT/x2t"

# Cache stamp: the full input listing. allfontsgen accepts several
# input dirs separated by semicolons — hash them all.
stamp_inputs() {
    local IFS=';'
    for d in $INPUT; do
        [ -d "$d" ] || { echo "gen-fonts: input dir missing: $d" >&2; exit 1; }
        find "$d" -type f -printf '%p %s %T@\n'
    done
}
STAMP=$(stamp_inputs | sort | sha256sum | cut -d' ' -f1)
if [ "${GEN_FONTS_FORCE:-0}" != "1" ] \
    && [ -f "$OUT/.gen-stamp" ] \
    && [ "$(cat "$OUT/.gen-stamp")" = "$STAMP" ] \
    && [ -f "$OUT/web/AllFonts.js" ] && [ -f "$OUT/x2t/AllFonts.js" ] \
    && [ -f "$OUT/desktop/AllFonts.js" ] \
    && [ -f "$OUT/x2t/font_selection.bin" ]; then
    echo "gen-fonts: cache hit (stamp $STAMP), skipping"
    exit 0
fi

# The one generation run. Absolute paths throughout: the native
# AllFonts.js stores file paths verbatim.
ABS_INPUT=""
IFS=';' read -ra DIRS <<< "$INPUT"
for d in "${DIRS[@]}"; do
    ABS=$(cd "$d" && pwd)
    ABS_INPUT="${ABS_INPUT:+$ABS_INPUT;}$ABS"
done
ABS_OUT=$(cd "$OUT" && pwd)

"$ALLFONTSGEN" \
    --input="$ABS_INPUT" \
    --allfonts-web="$ABS_OUT/web/AllFonts.js" \
    --allfonts="$ABS_OUT/x2t/AllFonts.js" \
    --selection="$ABS_OUT/x2t/font_selection.bin" \
    --images="$ABS_OUT/web/images" \
    --output-web="$ABS_OUT/web/fonts"

# Sanity: every promised output must exist and be non-empty.
for f in web/AllFonts.js x2t/AllFonts.js x2t/font_selection.bin; do
    [ -s "$OUT/$f" ] || { echo "gen-fonts: expected output missing or empty: $OUT/$f" >&2; exit 1; }
done

# x2t looks for font_selection.bin in m_sFontDir FIRST (strace,
# marathon 2 cycle 1: it probes <fontdir>/font_selection.bin,
# then falls back to the copy beside the x2t binary — the deb's
# stale build-machine selection). The generated bin therefore
# also belongs in the fonts root itself, not only beside the
# native AllFonts.js. Copy it into the (first) input directory.
FIRST_INPUT=$(printf '%s' "$INPUT" | cut -d';' -f1)
cp "$OUT/x2t/font_selection.bin" "$FIRST_INPUT/font_selection.bin"

# The desktop shell stages the path-keyed map (see header): it is
# byte-identical to the x2t map, staged under its own name so the
# runtime assembly cannot grab the numeric web map by mistake.
mkdir -p "$OUT/desktop"
cp "$OUT/x2t/AllFonts.js" "$OUT/desktop/AllFonts.js"

echo "$STAMP" > "$OUT/.gen-stamp"
echo "gen-fonts: generated from $ABS_INPUT"
echo "gen-fonts: web map $OUT/web/AllFonts.js, desktop map $OUT/desktop/AllFonts.js, native map $OUT/x2t/AllFonts.js, stamp $STAMP"
