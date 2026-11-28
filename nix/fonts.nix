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
          # -type f OR -type l, copied dereferenced: a
          # symlinked face is still a shipped face.
          # nixpkgs' dejavu_fonts symlinks DejaVuSans.ttf
          # (the regular face!) into dejavu-fonts-minimal;
          # a -type f find silently dropped it, and the
          # converter's face matcher then rendered regular
          # DejaVu text in Bold (cycle 9 EX1, goofy-PDF
          # rule fired; root cause by ls -la, not by
          # iteration).
          find "$pkg/share/fonts/$sub" \( -type f -o -type l \) \
            \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) \
            -exec cp -vL {} $out/fonts/ \;
          # Record what this package offered (basenames,
          # symlinks followed) for the drop guard below.
          find -L "$pkg/share/fonts/$sub" \
            \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) \
            -printf '%f\n' >> $TMPDIR/offered.txt
        fi
      done
    }
    copy_fonts ${pkgs.liberation_ttf} truetype
    copy_fonts ${pkgs.carlito} truetype
    copy_fonts ${pkgs.caladea} truetype
    copy_fonts ${pkgs.dejavu_fonts} truetype
    copy_fonts ${pkgs.noto-fonts-cjk-sans} opentype/noto-cjk truetype/noto-cjk
    # nanum ships its files directly under share/fonts (no
    # truetype/ subdir); the generic copy silently copied
    # nothing in the first build — the editor's Nanum faces
    # were missing from the set entirely (cycle 4 finisher,
    # Phase-3 regression).
    find ${nanumPkg}/share/fonts -maxdepth 1 \
      \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) \
      -exec cp -vL {} $out/fonts/ \;
    find -L ${nanumPkg}/share/fonts -maxdepth 1 \
      \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) \
      -printf '%f\n' >> $TMPDIR/offered.txt
    # Flat layout is what gen-fonts.sh consumed in cycle 1
    # (fonts-src); duplicate basenames would collide — fail
    # loudly instead of silently dropping a family.
    test -n "$(ls -A $out/fonts)" || { echo "no fonts copied"; exit 1; }
    # The drop guard: every face the inputs offered must
    # be present in the output. A silent drop is a build
    # failure from now on — this is the check that lives
    # where the build graph forces it.
    sort -u $TMPDIR/offered.txt > $TMPDIR/offered-sorted.txt
    ls $out/fonts | sort -u > $TMPDIR/present.txt
    DROPPED=$(comm -23 $TMPDIR/offered-sorted.txt $TMPDIR/present.txt)
    if [ -n "$DROPPED" ]; then
      echo "fonts.nix: input faces silently dropped: $DROPPED" >&2
      exit 1
    fi
    echo "fonts.nix: all $(wc -l < $TMPDIR/offered-sorted.txt) offered faces present"
    runHook postInstall
  '';

  meta = {
    description = "Font files for the jOffice runtime (gen-fonts.sh input set)";
    license = lib.licenses.free;
  };
}
