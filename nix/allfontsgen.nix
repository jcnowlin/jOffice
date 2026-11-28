# allfontsgen — SUPERSEDED (marathon 2 cycle 9).
#
# This file was cycle 4's documented stub: packaging the
# generator looked like it needed core's whole build_tools
# machinery (network-fetched 3rd-party sources, qmake),
# which a Nix sandbox cannot do.
#
# Cycle 9 found the shorter truth: the generator is ONE
# file — DesktopEditor/AllFontsGen/main.cpp in the pinned
# core — and it compiles directly against the .deb's
# prebuilt converter libraries (libgraphics, libkernel,
# libUnicodeConverter). nix/converter.nix does exactly
# that, generates the map over the shipped font set, and
# proves it with a real in-graph conversion. nixpkgs'
# x2t allfontsgen was evaluated and rejected on evidence:
# it writes ONLYOFFICE_FONTS_VERSION_12 while the pinned
# core requires 15.
#
# This stub stays so the pointer trail survives; the
# derivation to read is converter.nix.
{ }:
throw "allfontsgen.nix is a documentation stub; use converter.nix (joffice-converter)"
