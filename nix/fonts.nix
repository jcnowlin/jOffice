# jOffice font FILES (marathon 2 cycle 4, step 5).
#
# Copies (never symlinks — the path-keyed font map keys on
# real paths, and cycle 1 showed symlink farms break the
# font loader's bookkeeping) of the font families the
# editor's substitution table expects, from nixpkgs font
# packages. This is the INPUT set for scripts/gen-fonts.sh;
# the generated map (AllFonts.js / font_selection.bin) is
# produced from exactly this set by allfontsgen — see
# allfontsgen.nix for why that generation is not yet part of
# the Nix build.
{ pkgs, lib }:

let
  nanumPkg =
    if pkgs ? nanum then pkgs.nanum else pkgs.nanum-gothic-coding;
in
pkgs.stdenv.mkDerivation {
  pname = "joffice-fonts";
  version = "9.3.8";

  dontUnpack = true;

  buildInputs = [
    pkgs.liberation_ttf
    pkgs.carlito
    pkgs.caladea
    pkgs.dejavu_fonts
    pkgs.noto-fonts-cjk-sans
    nanumPkg
  ];

  installPhase = ''
    runHook preInstall
    mkdir -p $out/fonts
    copy_fonts() {
      # $1 = package, remaining args = share/fonts subdirs
      local pkg=$1; shift
      for sub in "$@"; do
        if [ -d "$pkg/share/fonts/$sub" ]; then
          find "$pkg/share/fonts/$sub" -type f \
            \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) \
            -exec cp -v {} $out/fonts/ \;
        fi
      done
    }
    copy_fonts ${pkgs.liberation_ttf} truetype
    copy_fonts ${pkgs.carlito} truetype
    copy_fonts ${pkgs.caladea} truetype
    copy_fonts ${pkgs.dejavu_fonts} truetype
    copy_fonts ${pkgs.noto-fonts-cjk-sans} opentype/noto-cjk truetype/noto-cjk
    copy_fonts ${nanumPkg} truetype
    # Flat layout is what gen-fonts.sh consumed in cycle 1
    # (fonts-src); duplicate basenames would collide — fail
    # loudly instead of silently dropping a family.
    test -n "$(ls -A $out/fonts)" || { echo "no fonts copied"; exit 1; }
    runHook postInstall
  '';

  meta = {
    description = "Font files for the jOffice runtime (gen-fonts.sh input set)";
    license = lib.licenses.free;
  };
}
