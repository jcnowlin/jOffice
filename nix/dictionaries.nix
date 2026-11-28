# Spellcheck dictionaries (marathon 2 cycle 7): the pinned
# Euro-Office/dictionaries tree, English only.
#
# Provenance (KAO cycle 7 round 2 §1): core's records.h
# names 49 dictionary folders; Euro-Office/dictionaries is
# the upstream that matches those names exactly, pinned
# here at the commit James approved processing
# (2026-11-27). The shell's spell service reads ONLY this
# tree (JOFFICE_DICT_DIR): each folder ships <name>.aff +
# <name>.dic, and manifest.json — generated here from each
# folder's own <name>.json LCID list — is the registry the
# service refuses to start without. A dictionary that fails
# its load-time self-test (DT1) is reported BrokenDict and
# its LCIDs are never advertised; a missing folder is a
# different, silent-by-design state (NoDict).
#
# English only by default (round 1 §1D): every extra
# dictionary makes wrong-language-tag squiggles worse, and
# the editor stops checking entirely after 2,000 errors.
# Adding a language later = add its folder name here.
{ pkgs, lib }:

let
  src = pkgs.fetchFromGitHub {
    owner = "Euro-Office";
    repo = "dictionaries";
    rev = "0020f5ed5174e2a213171c4c7da9ec06ca140869";
    hash = "sha256-SjnwicmDXW2J/PvmZlLQW+Qt2MeE1mMDetdOx5uasfA=";
  };

  english = [ "en_AU" "en_CA" "en_GB" "en_US" "en_ZA" ];
in
pkgs.runCommand "joffice-spell-dictionaries"
{
  nativeBuildInputs = [ pkgs.python3 ];
} ''
  mkdir -p $out
  for d in ${lib.concatStringsSep " " english}; do
    mkdir -p $out/$d
    cp ${src}/$d/$d.aff ${src}/$d/$d.dic ${src}/$d/$d.json $out/$d/
  done
  # The manifest is generated from the pinned folders' own
  # <name>.json LCID lists.
  python3 - "$out" <<'PYEOF'
  import json, os, sys
  out = sys.argv[1]
  entries = []
  for name in sorted(os.listdir(out)):
      folder = os.path.join(out, name)
      if not os.path.isdir(folder):
          continue
      with open(os.path.join(folder, name + ".json")) as f:
          codes = json.load(f)["codes"]
      entries.append({"name": name, "lcids": codes})
  with open(os.path.join(out, "manifest.json"), "w") as f:
      json.dump({"version": 1, "dictionaries": entries}, f, indent=1)
  PYEOF
''
