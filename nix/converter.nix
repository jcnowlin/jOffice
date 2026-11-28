# jOffice converter font map (marathon 2 cycle 9): the
# generated x2t font map for the pinned Euro-Office 9.3.8
# converter, built where the build graph forces it.
#
# Why this exists: the .deb ships NO allfontsgen, and the
# map it ships in converter/AllFonts.js names 207
# root-relative font paths for a vendor layout we do not
# run — the stale-artifact disease behind the broken
# print/PDF-export path (KAO cycle-9 rounds 1+2). The map
# must be generated from the exact font set the runtime
# ships, by a generator whose version constants match the
# pinned core, and proven by a REAL conversion inside this
# build: if any of that fails, the build fails, and a
# build that cannot convert never becomes a candidate.
#
# Generator route (round 2's d1/d2, resolved by evidence):
# nixpkgs' x2t allfontsgen (8.3.2) writes
# ONLYOFFICE_FONTS_VERSION_12; the pinned core header
# (DesktopEditor/fontengine/ApplicationFontsWorker.h)
# requires ONLYOFFICE_FONTS_VERSION 15 /
# ONLYOFFICE_ALL_FONTS_VERSION 2 — d1 fails the design's
# own version-constant gate. The .deb ships no generator
# at all. What remains is the pinned core's own
# DesktopEditor/AllFontsGen/main.cpp (a single file),
# compiled here against the .deb's prebuilt converter
# libraries: it writes ONLYOFFICE_FONTS_VERSION_15 and its
# output over the dereferenced fonts tree is byte-stable
# across runs. The .deb is a derivation input, so a silent
# .deb change fails this build, as the design requires.
#
# Outputs:
#   $out/fonts/            the dereferenced merged fonts
#                          tree (deb fonts + nix/fonts.nix),
#                          scanned IN PLACE so the map's
#                          paths are the final store paths;
#                          carries AllFonts.js,
#                          font_selection.bin, fonts.log
#   $out/AllFonts.js       the generated map
#   $out/font_selection.bin
#   $out/fonts.log         version line + scanned paths
#                          (the shell's FM6 launch check
#                          reads this file)
{ pkgs, lib }:

let
  fonts = pkgs.callPackage ./fonts.nix { };
  deb = pkgs.fetchurl {
    url = "https://github.com/Euro-Office/DesktopEditors/releases/download/v9.3.8/euro-office-euro-office_9.3.8-0_amd64.deb";
    sha256 = "sha256-GBbEzV1YBrdXRow9xX7HbCOs0eoIUtRB5Q1qVJ0hfaE=";
  };
  # The pinned core (the repo's core submodule pin; the
  # same pin the staged allfontsgen was built from in
  # cycle 1). Only DesktopEditor/AllFontsGen/main.cpp and
  # the fontengine header's constants are consumed.
  coreTarball = pkgs.fetchurl {
    url = "https://github.com/Euro-Office/core/archive/5a33ce6c4a63387e8576c7e763b70cd971988b24.tar.gz";
    hash = "sha256-9cEXuRE549ZwK/aAoQDf8utrMNEMGtvDzb58ssl9vTY=";
  };
in
pkgs.stdenv.mkDerivation {
  pname = "joffice-converter-fontmap";
  version = "9.3.8";

  dontUnpack = true;

  nativeBuildInputs = [
    pkgs.dpkg
    pkgs.gcc
    pkgs.pkg-config
    pkgs.icu.dev
    pkgs.poppler-utils
    pkgs.python3
    pkgs.patchelf
  ];

  # The libraries the .deb's x2t resolves against the
  # system — the same set runtime.nix declares for the
  # shipped converter (its bundled libs still win: the
  # converter dir stays first on the library path).
  buildInputs = [
    pkgs.stdenv.cc.cc.lib
    pkgs.glib
    pkgs.fontconfig
    pkgs.freetype
    pkgs.zlib
    pkgs.libpng
    pkgs.libjpeg
    pkgs.curl
    pkgs.openssl
  ];

  buildPhase = ''
    runHook preBuild
    WORK=$(mktemp -d)
    dpkg-deb -x ${deb} $WORK/deb
    APP=$WORK/deb/opt/euro-office/euro-office
    CONV=$APP/converter

    mkdir -p core
    tar xzf ${coreTarball} -C core --strip-components=1

    # The version-constant gate (FM1's precondition): the
    # pinned header's constants, read from the pinned
    # source itself — never a number typed here.
    HDR=core/DesktopEditor/fontengine/ApplicationFontsWorker.h
    VER=$(grep -o 'ONLYOFFICE_FONTS_VERSION[[:space:]]*[0-9]*' $HDR | grep -o '[0-9]*$')
    ALLVER=$(grep -o 'ONLYOFFICE_ALL_FONTS_VERSION[[:space:]]*[0-9]*' $HDR | grep -o '[0-9]*$')
    test -n "$VER" && test -n "$ALLVER"
    echo "pinned core constants: FONTS_VERSION=$VER ALL_FONTS_VERSION=$ALLVER"

    # Compile the pinned core's own generator against the
    # .deb's prebuilt converter libraries (the b-prime
    # recipe, proven by hand before it was encoded here).
    g++ -O2 -std=c++17 \
      -Icore -Icore/Common -Icore/DesktopEditor \
      -Icore/DesktopEditor/common -Icore/DesktopEditor/fontengine \
      -Icore/DesktopEditor/graphics/pro \
      $(pkg-config --cflags icu-uc) \
      core/DesktopEditor/AllFontsGen/main.cpp \
      -o $WORK/allfontsgen \
      -L$CONV -Wl,-rpath,$CONV \
      -lgraphics -lkernel -lUnicodeConverter \
      $(pkg-config --libs icu-uc) -lpthread -ldl
    echo "generator built: $($WORK/allfontsgen 2>/dev/null | head -1 || true)"

    # The fonts tree, dereferenced (cp -rL): ONLYOFFICE's
    # scan skips symlinks, and nix/fonts.nix is a
    # symlinkJoin — scanning it raw was a live suspect for
    # the empty map. The scan runs over $out/fonts itself
    # so every path the map names is the final store path.
    mkdir -p $out/fonts
    cp -rL $APP/fonts/. $out/fonts/
    cp -rL ${fonts}/fonts/. $out/fonts/
    chmod -R u+w $out/fonts
    # Pristine input: a stale selection file in the scan
    # dir would list itself (cycle 1's generator did
    # exactly that when the selection was copied in first).
    rm -f $out/fonts/font_selection.bin $out/fonts/AllFonts.js

    # Generate (the pinned main.cpp's named-argument form;
    # fonts.log lands beside the selection file). System
    # fonts are OFF: the map covers exactly the shipped
    # set, nothing the build host happens to carry.
    $WORK/allfontsgen --input=$out/fonts \
      --selection=$out/fonts/font_selection.bin \
      --allfonts=$out/fonts/AllFonts.js \
      --use-system=0 --use-system-user-fonts=0
    cp $out/fonts/AllFonts.js $out/AllFonts.js
    cp $out/fonts/font_selection.bin $out/font_selection.bin
    # The x2t-side fonts.log sits beside the selection in
    # the fonts dir (allfontsgen wrote it there); keep a
    # copy at $out for the shell's launch check.
    cp $out/fonts/fonts.log $out/fonts.log

    # ---- FM validation (build fails on any miss) ----
    # FM1: the log's version line equals the pinned
    # header's constant.
    HEAD_LINE=$(head -1 $out/fonts.log)
    test "$HEAD_LINE" = "ONLYOFFICE_FONTS_VERSION_$VER" || {
      echo "FM1 FAIL: fonts.log version line '$HEAD_LINE' != ONLYOFFICE_FONTS_VERSION_$VER"; exit 1; }
    echo "FM1 ok: $HEAD_LINE"
    # FM3: no listed path dangles; the selection file is
    # not self-listed; the map names only files under
    # $out/fonts.
    python3 - "$out" <<'PYEOF'
    import os, sys
    out = sys.argv[1]
    lines = open(out + "/fonts/fonts.log").read().splitlines()
    assert lines and lines[0].startswith("ONLYOFFICE_FONTS_VERSION_"), lines[:1]
    paths = [l for l in lines[1:] if l.strip()]
    assert paths, "FM3 FAIL: empty font list"
    for p in paths:
        assert os.path.isfile(p), "FM3 FAIL: dangling path " + p
        assert p.startswith(out + "/fonts/"), "FM3 FAIL: path outside fonts tree " + p
        assert not p.endswith("font_selection.bin"), "FM3 FAIL: selection self-listed"
    print("FM3 ok:", len(paths), "fonts listed, none dangling")
    PYEOF
    # FM5 (map hygiene): the generated map must not name
    # the .deb's install layout anywhere.
    ! grep -q '/opt/euro-office' $out/AllFonts.js || {
      echo "FM5 FAIL: generated map names the deb layout"; exit 1; }
    echo "FM5 ok: no deb-layout paths in the generated map"

    # ---- The in-graph conversion (the design's forcing
    # function): a real docx -> PDF with the .deb's x2t,
    # the generated selection beside it, the generated
    # fonts dir as m_sFontDir. Exit 0, non-empty PDF, and
    # the fixture's text extracted, or the build fails.
    mkdir -p $WORK/conv $WORK/cwork
    cp -r $CONV $WORK/conv/converter
    chmod -R u+w $WORK/conv/converter
    # The .deb's x2t carries a /lib64 interpreter path the
    # sandbox does not provide (the runtime derivation
    # fixes the shipped copy with autoPatchelfHook; the
    # scratch copy here gets the same treatment by hand).
    patchelf --set-interpreter $(cat $NIX_CC/nix-support/dynamic-linker) \
      $WORK/conv/converter/x2t
    cp $out/font_selection.bin $WORK/conv/converter/font_selection.bin
    cp $out/fonts.log $WORK/conv/converter/fonts.log
    # DoctRenderer.config for the gate: the deb's own
    # renderer tree (its editors/sdkjs + web-apps vendor
    # files) with the allfonts slot pointing at the
    # GENERATED map — the exact consumer relationship the
    # shipped runtime has (runtime.nix generates the same
    # shape with store paths).
    printf '<Settings>\n<file>%s/editors/sdkjs/common/Native/native.js</file>\n<file>%s/editors/sdkjs/common/Native/jquery_native.js</file>\n<allfonts>%s/AllFonts.js</allfonts>\n<file>%s/editors/web-apps/vendor/xregexp/xregexp-all-min.js</file>\n<sdkjs>%s/editors/sdkjs</sdkjs>\n<dictionaries>%s/dictionaries</dictionaries>\n</Settings>\n' \
      "$APP" "$APP" "$out" "$APP" "$APP" "$APP" > $WORK/conv/converter/DoctRenderer.config
    cat > $WORK/params.xml <<XEOF
    <?xml version="1.0" encoding="utf-8"?>
    <TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
      <m_sFileFrom>${./assets/fontmap-probe.docx}</m_sFileFrom>
      <m_sFileTo>$WORK/probe.pdf</m_sFileTo>
      <m_nFormatTo>513</m_nFormatTo>
      <m_sTempDir>$WORK/cwork</m_sTempDir>
      <m_sTitle>jOffice</m_sTitle>
      <m_bIsNoBase64>true</m_bIsNoBase64>
      <m_sFontDir>$out/fonts</m_sFontDir>
    </TaskQueueDataConvert>
    XEOF
    LD_LIBRARY_PATH=$WORK/conv/converter:${lib.makeLibraryPath [
      pkgs.stdenv.cc.cc.lib
      pkgs.glib
      pkgs.fontconfig
      pkgs.freetype
      pkgs.zlib
      pkgs.libpng
      pkgs.libjpeg
      pkgs.curl
      pkgs.openssl
    ]} TMPDIR=$WORK/cwork \
      $WORK/conv/converter/x2t $WORK/params.xml
    test -s $WORK/probe.pdf || { echo "CONVERSION FAIL: no PDF produced"; exit 1; }
    pdftotext $WORK/probe.pdf $WORK/probe.txt
    grep -q 'FontMapProbe conversion gate' $WORK/probe.txt || {
      echo "CONVERSION FAIL: fixture text missing from the PDF"; exit 1; }
    grep -q 'quick brown fox' $WORK/probe.txt || {
      echo "CONVERSION FAIL: body text missing from the PDF"; exit 1; }
    echo "CONVERSION ok: docx -> PDF in-graph, fixture text extracted"
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    # Everything was written straight into $out during the
    # build phase (the map must name final store paths).
    runHook postInstall
  '';

  meta = {
    description = "jOffice converter font map: generated from the pinned core's allfontsgen over the shipped font set, proven by an in-graph conversion";
    license = lib.licenses.agpl3Only;
    platforms = [ "x86_64-linux" ];
  };
}
