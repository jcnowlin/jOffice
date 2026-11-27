# jOffice runtime (marathon 2 cycle 4, step 5): the pinned
# Euro-Office 9.3.8 build, repackaged from the official deb.
#
# Layout produced:
#   $out/share/joffice/runtime/
#     converter/     x2t + its bundled libs (patchelfed)
#     sdkjs/         editors/sdkjs from the deb, sdk-all
#                    caches deleted, common/AllFonts.js =
#                    the converter map (see fonts note)
#     sdkjs-deploy/  the renderer tree (a second copy)
#     web-apps/      editors/web-apps from the deb
#     fonts/         deb fonts + nix/fonts.nix files +
#                    AllFonts.js + font_selection.bin
#     dictionaries/  from the deb
#     runtime-manifest.json
#
# DoctRenderer.config is NOT shipped: the shell generates it
# per run with absolute paths into this tree (cycle 3's
# design — the same code path serves the staged runtime and
# the Nix one).
#
# Fonts note: the deb's converter/AllFonts.js references a
# core-fonts tree the deb does not ship, and regenerating the
# map for this font set needs allfontsgen (see
# allfontsgen.nix). This derivation therefore ships the deb's
# converter artifacts as-is; the editor-facing map is the
# cycle's known gap (graded in N2).
{ pkgs, lib }:

let
  fonts = pkgs.callPackage ./fonts.nix { };
  deb = pkgs.fetchurl {
    url = "https://github.com/Euro-Office/DesktopEditors/releases/download/v9.3.8/euro-office-euro-office_9.3.8-0_amd64.deb";
    # sha256 of the official release asset, verified three
    # ways in cycle 4: the published value, the marathon-1
    # copy staged in the runtime (identical), and this hash.
    sha256 = "sha256-GBbEzV1YBrdXRow9xX7HbCOs0eoIUtRB5Q1qVJ0hfaE=";
  };
  # The editor-facing sdkjs is the DEVELOP tree (the Local
  # overrides — common/Local/common.js, word/Local/api.js —
  # exist only there; the deb's editors/sdkjs is a minimal
  # deploy tree and serves as sdkjs-deploy, the renderer's).
  # Pinned at the repo's sdkjs submodule commit. The
  # manifest's sdk_hash (FNV of common/Local/common.js) must
  # come out as the staged runtime's 36712ae529e6b556 — a
  # wrong pin fails that check loudly at build time.
  sdkjsSrc = pkgs.fetchFromGitHub {
    owner = "Euro-Office";
    repo = "sdkjs";
    rev = "0cde84e8dbfb299b53098532c8af142712830cc6";
    hash = "sha256-mbucMiTSgdp5QVpX9j52/dWmCXgHBLrTzjB5L226D8k=";
  };
in
pkgs.stdenv.mkDerivation {
  pname = "joffice-runtime";
  version = "9.3.8";

  dontUnpack = true;

  nativeBuildInputs = [
    pkgs.dpkg
    pkgs.autoPatchelfHook
    pkgs.python3
  ];

  # Libraries x2t + the converter .so files resolve against
  # the system. The converter's own bundled libs (icu 74,
  # graphics, doctrenderer…) are found FIRST because
  # autoPatchelfSearchPath puts $out/.../converter ahead —
  # a bundled soname must never be rebound to a nixpkgs copy
  # of a different vintage (round 2 §E). icu is deliberately
  # NOT here: the converter carries its own.
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

  autoPatchelfSearchPath = [
    "$out/share/joffice/runtime/converter"
  ];

  installPhase = ''
    runHook preInstall
    RT=$out/share/joffice/runtime
    mkdir -p $RT
    WORK=$(mktemp -d)
    dpkg-deb -x ${deb} $WORK/deb
    APP=$WORK/deb/opt/euro-office/euro-office

    cp -r $APP/converter $RT/converter
    cp -r ${sdkjsSrc} $RT/sdkjs
    cp -r $APP/editors/sdkjs $RT/sdkjs-deploy
    cp -r $APP/editors/web-apps $RT/web-apps
    cp -r $APP/fonts $RT/fonts
    cp -r $APP/dictionaries $RT/dictionaries
    # Fetched sources land read-only; the copies below write
    # INTO these trees.
    chmod -R u+w $RT

    # The editor-facing font map: the converter's path-keyed
    # file is the same form the desktop shell consumes
    # (cycle 1). Paths inside it name the vendor layout; the
    # shell's font index tolerates missing entries (they fail
    # soft per cycle 1's loader fix) — regeneration for this
    # exact set is the allfontsgen follow-up.
    cp $APP/converter/AllFonts.js $RT/sdkjs/common/AllFonts.js
    cp $APP/converter/AllFonts.js $RT/sdkjs-deploy/common/AllFonts.js
    cp $APP/converter/AllFonts.js $RT/fonts/AllFonts.js
    cp $APP/converter/font_selection.bin $RT/fonts/font_selection.bin

    # DoctRenderer.config, pre-generated with store paths:
    # the shell generates this beside x2t at run time, which
    # a read-only store tree forbids (cycle 4 N3 finding —
    # the shell's own log line anticipates it: "a packaged
    # runtime must ship its own"). The content matches the
    # shell's template byte for byte, so its freshness check
    # reads it as current and never tries to rewrite it.
    printf '<Settings>\n<file>%s/sdkjs-deploy/common/Native/native.js</file>\n<file>%s/sdkjs-deploy/common/Native/jquery_native.js</file>\n<allfonts>%s/sdkjs-deploy/common/AllFonts.js</allfonts>\n<file>%s/web-apps/vendor/xregexp/xregexp-all-min.js</file>\n<sdkjs>%s/sdkjs-deploy</sdkjs>\n<dictionaries>%s/dictionaries</dictionaries>\n</Settings>\n' \
      "$RT" "$RT" "$RT" "$RT" "$RT" "$RT" > $RT/converter/DoctRenderer.config

    # The nixpkgs font files join the deb's in one flat dir
    # (gen-fonts.sh's input layout).
    cp ${fonts}/fonts/* $RT/fonts/ || true

    # sdkjs caches are fingerprint-invalidated by the shell
    # at runtime; shipping the deb's prebuilt ones would
    # serve a stale engine (cycle 3 finding).
    find $RT/sdkjs $RT/sdkjs-deploy -name 'sdk-all.cache' -delete
    find $RT/sdkjs $RT/sdkjs-deploy -name 'sdk-all.bin' -delete

    # runtime-manifest.json: sdk_hash is the shell's own
    # FNV-1a-64 of sdkjs/common/Local/common.js (the shell
    # recomputes it at startup and logs a mismatch).
    python3 - "$RT" <<'PYEOF'
    import json, sys
    rt = sys.argv[1]
    data = open(rt + "/sdkjs/common/Local/common.js", "rb").read()
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    manifest = {
        "name": "joffice-runtime",
        "version": "9.3.8",
        "sdk_hash": format(h, "016x"),
        "deb_sha256": "1816c4cd5d5806b757468c3dc57ec76c23acd1ea0852d441e50d6a549d217da1",
    }
    with open(rt + "/runtime-manifest.json", "w") as f:
        json.dump(manifest, f, indent=2)
    print("manifest sdk_hash", manifest["sdk_hash"])
    # Pin check: the develop tree at the pinned commit hashes
    # to the staged runtime's value (cycle 4 build log).
    assert manifest["sdk_hash"] == "36712ae529e6b556", \
        "sdkjs pin mismatch: " + manifest["sdk_hash"]
    PYEOF
    runHook postInstall
  '';

  meta = {
    description = "jOffice runtime: Euro-Office 9.3.8 converter + sdkjs + web-apps, repackaged from the official deb";
    license = lib.licenses.agpl3Only;
    platforms = [ "x86_64-linux" ];
  };
}
