# jOffice runtime (marathon 2 cycle 4, step 5): the pinned
# Euro-Office 9.3.8 build, repackaged from the official deb.
#
# Layout produced:
#   $out/share/joffice/runtime/
#     converter/     x2t + its bundled libs (patchelfed)
#     sdkjs/         the DEVELOP sdkjs (GitHub pin below),
#                    sdk-all caches deleted, common/
#                    AllFonts.js = the converter map
#                    (see fonts note)
#     sdkjs-deploy/  the deb's editors/sdkjs (renderer tree)
#     web-apps/      the DEVELOP web-apps (GitHub pin
#                    below — NOT the deb's deploy build)
#     fonts/         deb fonts + nix/fonts.nix files +
#                    AllFonts.js + font_selection.bin
#     fonts/core-fonts/ + fonts/desktopeditors/
#                    the editor-facing font trees the deb's
#                    path-keyed AllFonts.js names, resolved
#                    by the shell against the fonts root
#                    (see the assembly note in the install
#                    phase)
#     dictionaries/  from the deb
#     spell-dictionaries/  the pinned English hunspell tree
#                    + manifest.json the shell's spell
#                    service reads (cycle 7; nix/
#                    dictionaries.nix)
#     runtime-manifest.json
#
# DoctRenderer.config is NOT shipped: the shell generates it
# per run with absolute paths into this tree (cycle 3's
# design — the same code path serves the staged runtime and
# the Nix one).
#
# Fonts note (rewritten cycle 9): two maps ship, for two
# consumers. The EDITOR-facing map stays the deb's
# converter/AllFonts.js (path-keyed; the trees its paths
# name ship below) in sdkjs/common. The CONVERTER-facing
# map — converter/AllFonts.js, sdkjs-deploy/common/
# AllFonts.js (the file DoctRenderer.config names),
# fonts/AllFonts.js + font_selection.bin, and
# converter/font_selection.bin + fonts.log — is the
# GENERATED map from nix/converter.nix: produced by the
# pinned core's own allfontsgen over the exact shipped
# font set and proven by a real conversion inside that
# build. The deb ships no generator and its converter map
# names a vendor layout we do not run; that stale map was
# the print/PDF-export disease (KAO cycle-9 rounds 1+2).
{ pkgs, lib }:

let
  fonts = pkgs.callPackage ./fonts.nix { };
  converter = pkgs.callPackage ./converter.nix { };
  spellDicts = pkgs.callPackage ./dictionaries.nix { };
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
  # The web-apps is ALSO the develop tree, from its own
  # repo at the pin the shell's bootstrap stamp names
  # ("web-apps f50e7e32, sdkjs 0cde84e8" in main.rs). The
  # deb's editors/web-apps is the compiled deploy build:
  # its index.html loads the sdk-all bundles the develop
  # sdkjs does not carry, so an editor on the deb's
  # web-apps dies with a scheme 404 on
  # /sdkjs/word/sdk-all-min.js (cycle 4 finisher, N2:
  # the staged runtime works because it pairs develop
  # web-apps with develop sdkjs; the first Nix build
  # shipped the deb's web-apps and no editor ever
  # loaded).
  webappsSrc = pkgs.fetchFromGitHub {
    owner = "Euro-Office";
    repo = "web-apps";
    rev = "f50e7e32caba70bbe96f9f8b21ad4b7f4009884e";
    hash = "sha256-M1zzlbkkdhHWfA19RCIMQ0z0gEy08gQQkQNBBgIU/3w=";
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
    chmod -R u+w $RT/sdkjs
    # The develop-mode script manifests: the develop
    # web-apps loads the sdkjs through
    # sdkjs/develop/sdkjs/<editor>/scripts.js, a
    # generated file list the sdkjs build produces and
    # the raw repo does not carry. The four manifests
    # are vendored under nix/assets/sdkjs-develop/ —
    # generated 2026-11-25 against this exact sdkjs pin
    # (they enumerate the pin's own files; the staged
    # runtime's editor runs on them). Regenerate if the
    # sdkjs pin ever moves (cycle 4 finisher, N2).
    for ed in word cell slide visio; do
      mkdir -p $RT/sdkjs/develop/sdkjs/$ed
      cp ${./assets/sdkjs-develop}/$ed/scripts.js \
         $RT/sdkjs/develop/sdkjs/$ed/scripts.js
    done
    cp -r $APP/editors/sdkjs $RT/sdkjs-deploy
    cp -r ${webappsSrc}/. $RT/web-apps
    chmod -R u+w $RT/web-apps
    # The desktop entry contract: the shell loads
    # apps/api/documents/index.html, which the vendor
    # build "deploys" from index.html.desktop. The raw
    # repo has only the .desktop file; the staged runtime
    # carries the two byte-identical (verified). Deploy
    # it the same way (cycle 4 finisher, N2: without
    # this the entry page itself 404s and no editor
    # ever loads).
    cp $RT/web-apps/apps/api/documents/index.html.desktop \
       $RT/web-apps/apps/api/documents/index.html
    # Euro-Office theme glue the staged runtime carries
    # and the web-apps pin does not: a one-line @import
    # shim onto the fork's theme, and the mobile logo
    # overrides. Vendored under nix/assets/less/ with
    # their content unchanged (cycle 4 finisher, N2:
    # the develop page requests _theme-main.less by
    # name and 404s without it).
    cp ${./assets/less/_theme-main.less} \
       $RT/web-apps/apps/common/main/resources/less/_theme-main.less
    cp ${./assets/less/_theme-mobile-overrides.less} \
       $RT/web-apps/apps/common/mobile/resources/less/_theme-mobile-overrides.less
    cp -r $APP/fonts $RT/fonts
    cp -r $APP/dictionaries $RT/dictionaries
    # Cycle 7: the spell service's own pinned English tree
    # (nix/dictionaries.nix) — separate from the deb's
    # dictionaries/ above, which the generated
    # DoctRenderer.config references for the renderer.
    cp -r ${spellDicts} $RT/spell-dictionaries
    # Fetched sources land read-only; the copies below write
    # INTO these trees.
    chmod -R u+w $RT

    # The editor-facing font map: the converter's path-keyed
    # file is the same form the desktop shell consumes
    # (cycle 1). Paths inside it name the vendor layout.
    # Missing entries fail soft only for faces nothing shapes
    # with; a face a document actually lays out with resolves
    # to a null face and the text shaper dies (m_pFaceInfo) —
    # the trees below are what make the named faces
    # resolvable. This copy serves sdkjs/common ONLY.
    cp $APP/converter/AllFonts.js $RT/sdkjs/common/AllFonts.js
    # The converter-facing map is the GENERATED one (cycle
    # 9, nix/converter.nix): sdkjs-deploy/common (the file
    # DoctRenderer.config names), the converter dir itself
    # (x2t's own selection + log beside the binary), and
    # the fonts dir's copies.
    cp ${converter}/AllFonts.js $RT/sdkjs-deploy/common/AllFonts.js
    cp ${converter}/AllFonts.js $RT/converter/AllFonts.js
    cp ${converter}/font_selection.bin $RT/converter/font_selection.bin
    cp ${converter}/fonts.log $RT/converter/fonts.log
    cp ${converter}/AllFonts.js $RT/fonts/AllFonts.js
    cp ${converter}/font_selection.bin $RT/fonts/font_selection.bin

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

    # The editor-facing font trees (cycle 4 finisher, found
    # by the Phase-3 regression): the map copied above names
    # 184 root-relative core-fonts/<sub>/<file> ids and one
    # desktopeditors/fonts/ id. The loader fetches them
    # through ascdesktop://fonts/<id>, and the shell resolves
    # an id against the FONTS ROOT ($JOFFICE_FONTSROOT — this
    # tree's fonts/): root.join(id), served only from inside
    # the root. The first Nix build shipped neither tree
    # under fonts/: every editor font fetch missed, the text
    # shaper died on a null face, and no document rendered at
    # all. The staged runtime works because its fonts-root/
    # carries exactly these trees. Assemble them from the
    # same sources: the deb's own font files, plus the
    # nixpkgs families in fonts.nix.
    mkdir -p $RT/fonts/core-fonts/{asana,caladea,crosextra,liberation,nanum,openoffice,opensans}
    cp $APP/fonts/ASC.ttf $RT/fonts/core-fonts/ASC.ttf
    cp -r $APP/fonts/asana/. $RT/fonts/core-fonts/asana/
    cp -r $APP/fonts/caladea/. $RT/fonts/core-fonts/caladea/
    cp -r $APP/fonts/crosextra/. $RT/fonts/core-fonts/crosextra/
    cp ${fonts}/fonts/Liberation*.ttf $RT/fonts/core-fonts/liberation/
    cp ${fonts}/fonts/NanumBarunGothic.ttf \
       ${fonts}/fonts/NanumBarunGothicBold.ttf $RT/fonts/core-fonts/nanum/
    cp $APP/fonts/openoffice/opens___.ttf $RT/fonts/core-fonts/openoffice/
    cp $APP/fonts/OpenSans-*.ttf $RT/fonts/core-fonts/opensans/
    mkdir -p $RT/fonts/desktopeditors/fonts/caladea
    cp $RT/fonts/core-fonts/caladea/caladea-bolditalic.ttf \
       $RT/fonts/desktopeditors/fonts/caladea/caladea-bolditalic.ttf

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
