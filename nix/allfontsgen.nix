# allfontsgen — NOT YET PACKAGED (marathon 2 cycle 4).
#
# Why this is a documented stub rather than a derivation:
#
# allfontsgen is built from ONLYOFFICE core (the repo's core
# submodule, pinned at 5a33ce6c4a63387e8576c7e763b70cd971988b24)
# via core's build_tools: build_3rdparty.py downloads boost,
# icu, harfbuzz, brotli, hyphen and the katana html parser
# from the network, then AllFontsGen links core's libkernel +
# libgraphics against them (cycle 1 built it exactly this way
# on jNix; see marathon2/CYCLES.md).
#
# A Nix derivation build is offline. Packaging allfontsgen
# therefore means vendoring every one of those 3rd-party
# sources as fixed-output fetches and driving core's qmake
# build by hand — a project of its own, not a step inside
# this cycle. Until that lands:
#
#   * nix/runtime.nix ships the deb's own converter font
#     artifacts (AllFonts.js + font_selection.bin) so x2t
#     conversions run, and the font FILES from nix/fonts.nix;
#   * the editor-facing generated map (the path-keyed
#     AllFonts.js cycle 1 produced with this tool) is NOT
#     regenerated in the Nix build — the known gap graded in
#     cycle 4's N2.
#
# The follow-up is scoped: vendor the six 3rd-party sources,
# build libkernel/libgraphics/AllFontsGen from the pinned
# core with THIRD_PARTY_PREPARED=ON (cycle 1's recipe), then
# wire scripts/gen-fonts.sh into runtime.nix's build with
# fonts.nix as its input.
{ }:
throw "allfontsgen is not packaged yet — see nix/allfontsgen.nix for the blocker and the scoped follow-up"
