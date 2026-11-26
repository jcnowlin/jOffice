#!/usr/bin/env python3
"""R0 baseline runner (runs on jNix, beside the runtime).

For each corpus document: x2t file -> Editor.bin -> file, with
NO editor involved — x2t's own round-trip losses, which
calibrate everything else (Kagi round 1 §5). The params mirror
the shell's exactly (font dir, native AllFonts.js, theme dir,
CSV block, per-family canvas code as m_nFormatFrom on the way
back) so R0 measures the same converter the shell drives.

Usage: run_r0.py <corpus_dir> <out_dir> <runtime_dir>
Writes <out_dir>/<name>.r0.<ext> per input + r0-report.json.
"""
import json
import os
import subprocess
import sys
import xml.sax.saxutils as sx

CORPUS, OUT, RT = sys.argv[1], sys.argv[2], sys.argv[3]
X2T = os.path.join(RT, "converter", "x2t")
FONT_DIR = os.path.join(RT, "fonts-src")
ALLFONTS = os.path.join(RT, "fonts-gen", "x2t", "AllFonts.js")
THEME_DIR = os.path.join(RT, "sdkjs", "slide", "themes")

CANVAS = {"docx": 8193, "docm": 8193, "dotx": 8193, "xlsx": 8194, "csv": 8194,
          "pptx": 8195, "odt": 8193, "ods": 8194, "odp": 8195, "doc": 8193,
          "rtf": 8193, "txt": 8193}
FMT_TO = {"docx": 65, "docm": 75, "dotx": 76, "xlsx": 257, "pptx": 129,
          "odt": 67, "ods": 259, "odp": 131, "doc": 65, "rtf": 68, "txt": 69,
          "csv": 260}


def convert(src, dst, fmt_to, fmt_from=None, csv_delim=None):
    # PRIVATE temp dir per conversion: x2t unpacks into a
    # fixed-name "<m_sTempDir>/<family>_unpacked/" tree and
    # merges with stale content — a shared _tmp made media.docx
    # inherit basic.docx's headers/footnotes (cycle-2 finding).
    import tempfile

    tmp = tempfile.mkdtemp(prefix="r0-", dir=OUT)
    extra = ""
    if fmt_from is not None:
        extra += f"  <m_nFormatFrom>{fmt_from}</m_nFormatFrom>\n"
    extra += f"  <m_sFontDir>{sx.escape(FONT_DIR)}</m_sFontDir>\n"
    if os.path.exists(ALLFONTS):
        extra += f"  <m_sAllFontsPath>{sx.escape(ALLFONTS)}</m_sAllFontsPath>\n"
    if os.path.isdir(THEME_DIR):
        extra += f"  <m_sThemeDir>{sx.escape(THEME_DIR)}</m_sThemeDir>\n"
    if fmt_to in (260, 69) or src.endswith((".csv", ".txt")):
        extra += "  <m_nCsvTxtEncoding>46</m_nCsvTxtEncoding>\n"
        extra += f"  <m_nCsvDelimiter>{csv_delim or 4}</m_nCsvDelimiter>\n"
    params = f"""<?xml version="1.0" encoding="utf-8"?>
<TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <m_sFileFrom>{sx.escape(src)}</m_sFileFrom>
  <m_sFileTo>{sx.escape(dst)}</m_sFileTo>
  <m_nFormatTo>{fmt_to}</m_nFormatTo>
  <m_sTempDir>{sx.escape(tmp)}</m_sTempDir>
  <m_sTitle>jOffice</m_sTitle>
  <m_bIsNoBase64>true</m_bIsNoBase64>
{extra}</TaskQueueDataConvert>"""
    ppath = os.path.join(tmp, "params.xml")
    with open(ppath, "w") as f:
        f.write(params)
    r = subprocess.run([X2T, ppath], capture_output=True, text=True, timeout=300)
    return r.returncode, r.stderr.strip()


def sniff_delim(path):
    with open(path, "rb") as f:
        first = f.read(8192).split(b"\n")[0]
    semi, comma, tab = first.count(b";"), first.count(b","), first.count(b"\t")
    if semi > comma and semi >= tab:
        return 2
    if tab > comma:
        return 1
    return 4


report = {}
for name in sorted(os.listdir(CORPUS)):
    if name.startswith(("_", "measure", "manifest")):
        continue
    src = os.path.join(CORPUS, name)
    if not os.path.isfile(src):
        continue
    ext = name.rsplit(".", 1)[-1].lower()
    if ext not in FMT_TO:
        report[name] = {"skip": "no r0 mapping"}
        continue
    if name == "encrypted.docx":
        report[name] = {"skip": "encrypted (E1 territory)"}
        continue
    binp = os.path.join(OUT, name + ".bin")
    delim = sniff_delim(src) if ext == "csv" else None
    rc1, err1 = convert(src, binp, 8192, csv_delim=delim)
    if rc1 != 0:
        report[name] = {"open_rc": rc1, "error": err1[:300]}
        continue
    outname = name + ".r0." + ext if ext != "doc" else "legacy.r0.docx"
    outp = os.path.join(OUT, outname)
    rc2, err2 = convert(binp, outp, FMT_TO[ext], fmt_from=CANVAS.get(ext, 8192))
    report[name] = {
        "open_rc": rc1,
        "save_rc": rc2,
        "bin_bytes": os.path.getsize(binp) if os.path.exists(binp) else None,
        "out": outname if rc2 == 0 else None,
        "error": (err2[:300] if rc2 != 0 else ""),
    }
    print(name, report[name], flush=True)

with open(os.path.join(OUT, "r0-report.json"), "w") as f:
    json.dump(report, f, indent=1)
print("R0 DONE")
