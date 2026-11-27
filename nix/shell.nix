# joffice-shell (marathon 2 cycle 4, step 5): the Rust/GTK4
# shell, built with buildRustPackage and wrapped so the Nix
# runtime is its default (JOFFICE_RUNTIME_DIR — an explicit
# environment setting still wins, which is how the staged
# runtime stays usable for A/B drives).
{ pkgs, lib }:

let
  runtime = pkgs.callPackage ./runtime.nix { };
  rtDir = "${runtime}/share/joffice/runtime";
in
pkgs.rustPlatform.buildRustPackage {
  pname = "joffice-shell";
  version = "9.3.8";

  src = ../joffice-shell;
  cargoLock.lockFile = ../joffice-shell/Cargo.lock;

  nativeBuildInputs = [
    pkgs.pkg-config
    pkgs.wrapGAppsHook4
    pkgs.makeWrapper
  ];

  # The gapps hook still runs its setup (GIO modules,
  # schema paths), but its wrapping is disabled: see the
  # postFixup note below.
  dontWrapGApps = true;

  buildInputs = [
    pkgs.gtk4
    pkgs.webkitgtk_6_0
    pkgs.glib
    pkgs.openssl
    pkgs.libxkbcommon
    pkgs.wayland
    pkgs.fontconfig
    pkgs.freetype
  ];

  # The wrapper: runtime discovery + the font path variables
  # + qpdf on PATH (the print path's page surgery in the
  # staged runtime used pdfseparate/pdfunite from poppler;
  # qpdf is the spec's pin for the packaged build).
  #
  # NOTE (cycle 4 finisher, 2026-11-27): these args were
  # originally passed via gappsWrapperArgs and were
  # SILENTLY DROPPED: the nixos-25.05 wrap-gapps setup
  # hook opens with `gappsWrapperArgs=()`, wiping the
  # derivation value before its own args are appended, so
  # the built wrapper carried only the hook's XDG prefix
  # and a bare launch found no runtime at all (N2 failure:
  # x2t NOT FOUND, fonts None, scheme 404 on the editor
  # page). The wrapping therefore happens explicitly in
  # postFixup with makeWrapper, and the artifact check is:
  # strings on $out/bin/joffice-shell must show
  # JOFFICE_RUNTIME_DIR and the runtime store path.
  postFixup = ''
    wrapProgram $out/bin/joffice-shell \
      --set-default JOFFICE_RUNTIME_DIR "${rtDir}" \
      --set-default JOFFICE_FONTSDIR "${rtDir}/fonts" \
      --set-default JOFFICE_ALLFONTS "${rtDir}/fonts/AllFonts.js" \
      --set-default JOFFICE_FONTSROOT "${rtDir}/fonts" \
      --prefix PATH : ${lib.makeBinPath [ pkgs.qpdf pkgs.poppler-utils ]} \
      --prefix XDG_DATA_DIRS : "$out/share''${GSETTINGS_SCHEMAS_PATH:+:$GSETTINGS_SCHEMAS_PATH}"
  '';

  postInstall = ''
    install -Dm644 ${./joffice.desktop} \
      $out/share/applications/joffice.desktop
  '';

  passthru = { inherit runtime; };

  meta = {
    description = "jOffice shell: ONLYOFFICE/Euro-Office editors in a native Rust/GTK4 window";
    license = lib.licenses.agpl3Only;
    platforms = [ "x86_64-linux" ];
    mainProgram = "joffice-shell";
  };
}
