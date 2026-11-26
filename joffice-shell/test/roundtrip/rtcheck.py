#!/usr/bin/env python3
"""Round-trip checker for the jOffice cycle-2 suite.

Implements the Kagi round 1 §1 semantic checklists and the
round 2 restatement-2 media classification:

  facts <file>            semantic facts as JSON
  diff <a> <b>            classified diffs as JSON
  markerdiff <a> <b> <m>  PASS/FAIL: b differs from a only by
                          the marker edit + allowed noise

Media are classified BY POSITION (docx: Nth a:blip in document
order; pptx: (slide, Nth picture); xlsx: (drawing part, Nth
picture)): missing -> LOST (fail); identical hash -> OK;
container format changed with equal decoded size -> CONVERTED;
same format, pixels identical -> RE-ENCODED; anything else ->
CHANGED (fail).

Every diff carries a change CLASS; the three-way suite checks
N-vs-R0 classes against the versioned allowlist.
"""
import hashlib
import io
import json
import sys
import zipfile

from lxml import etree

NS = {
    "w": "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    "a": "http://schemas.openxmlformats.org/drawingml/2006/main",
    "p": "http://schemas.openxmlformats.org/presentationml/2006/main",
    "r": "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    "rel": "http://schemas.openxmlformats.org/package/2006/relationships",
    "x": "http://schemas.openxmlformats.org/spreadsheetml/2006/main",
}


def sha(b):
    return hashlib.sha256(b).hexdigest()


def open_zip(path):
    z = zipfile.ZipFile(path)
    return z, {n: z.read(n) for n in z.namelist()}


def parse(xml_bytes):
    return etree.fromstring(xml_bytes)


def texts_of(elem, tag="w:t"):
    return "".join(t.text or "" for t in elem.iter(f"{{{NS['w']}}}t")) if tag == "w:t" else ""


def decoded_size(data):
    try:
        from PIL import Image

        img = Image.open(io.BytesIO(data))
        img.load()
        return img.size, img.convert("RGB").tobytes()
    except Exception:
        return None, None


# ------------------------------------------------------------------- docx
def docx_facts(path):
    z, parts = open_zip(path)
    facts = {"format": "docx", "parts": sorted(parts.keys())}
    doc = parse(parts["word/document.xml"])
    body = doc.find("w:body", NS)
    paras = []
    for p in body.iter(f"{{{NS['w']}}}p"):
        paras.append("".join(t.text or "" for t in p.iter(f"{{{NS['w']}}}t")))
    facts["paragraphs"] = paras
    tables = []
    for tbl in body.iter(f"{{{NS['w']}}}tbl"):
        rows = []
        merges = 0
        for tr in tbl.iter(f"{{{NS['w']}}}tr"):
            row = []
            for tc in tr.iter(f"{{{NS['w']}}}tc"):
                row.append("".join(t.text or "" for t in tc.iter(f"{{{NS['w']}}}t")))
                gs = tc.find("w:tcPr/w:gridSpan", NS)
                if gs is not None:
                    merges += 1
            rows.append(row)
        tables.append({"rows": rows, "merges": merges})
    facts["tables"] = tables
    # media by blip position
    rels = {}
    if "word/_rels/document.xml.rels" in parts:
        for rel in parse(parts["word/_rels/document.xml.rels"]):
            rels[rel.get("Id")] = rel.get("Target")
    media = []
    idx = 0
    for blip in doc.iter(f"{{{NS['a']}}}blip"):
        rid = blip.get(f"{{{NS['r']}}}embed")
        target = rels.get(rid, "")
        part = "word/" + target if not target.startswith("/") else target[1:]
        data = parts.get(part, b"")
        media.append(
            {
                "pos": idx,
                "part": part,
                "sha": sha(data) if data else None,
                "ext": part.rsplit(".", 1)[-1].lower() if "." in part else "",
                "bytes": len(data),
            }
        )
        idx += 1
    facts["media"] = media
    facts["revisions"] = {
        "ins": len(doc.findall(".//w:ins", NS)),
        "del": len(doc.findall(".//w:del", NS)),
    }
    facts["fields"] = len(doc.findall(".//w:fldSimple", NS)) + len(
        doc.findall(".//w:instrText", NS)
    )
    styles = set()
    for s in doc.iter(f"{{{NS['w']}}}pStyle"):
        styles.add(s.get(f"{{{NS['w']}}}val"))
    facts["styles_used"] = sorted(x for x in styles if x)
    facts["sections"] = len(doc.findall(".//w:sectPr", NS))
    facts["comments"] = []
    if "word/comments.xml" in parts:
        croot = parse(parts["word/comments.xml"])
        for c in croot.findall("w:comment", NS):
            facts["comments"].append(
                {
                    "author": c.get(f"{{{NS['w']}}}author"),
                    "text": "".join(t.text or "" for t in c.iter(f"{{{NS['w']}}}t")),
                }
            )
    facts["footnotes"] = []
    if "word/footnotes.xml" in parts:
        froot = parse(parts["word/footnotes.xml"])
        for fn in froot.findall("w:footnote", NS):
            if fn.get(f"{{{NS['w']}}}type"):
                continue
            facts["footnotes"].append(
                "".join(t.text or "" for t in fn.iter(f"{{{NS['w']}}}t"))
            )
    facts["headers"] = []
    facts["footers"] = []
    for name in sorted(parts):
        if name.startswith("word/header"):
            facts["headers"].append(
                "".join(t.text or "" for t in parse(parts[name]).iter(f"{{{NS['w']}}}t"))
            )
        if name.startswith("word/footer"):
            facts["footers"].append(
                "".join(t.text or "" for t in parse(parts[name]).iter(f"{{{NS['w']}}}t"))
            )
    if "word/vbaProject.bin" in parts:
        facts["vba_sha"] = sha(parts["word/vbaProject.bin"])
    facts["content_types"] = parts.get("[Content_Types].xml", b"").decode("utf8", "replace")
    return facts


# ------------------------------------------------------------------- xlsx
def xlsx_facts(path):
    import openpyxl

    facts = {"format": "xlsx"}
    z, parts = open_zip(path)
    facts["parts"] = sorted(parts.keys())
    facts["has_calcchain"] = "xl/calcChain.xml" in parts
    wb_f = openpyxl.load_workbook(path, data_only=False)
    wb_v = openpyxl.load_workbook(path, data_only=True)
    sheets = {}
    for name in wb_f.sheetnames:
        wsf = wb_f[name]
        wsv = wb_v[name]
        cells = {}
        for row in wsf.iter_rows():
            for c in row:
                if c.value is not None:
                    cached = wsv[c.coordinate].value
                    cells[c.coordinate] = {
                        "formula": c.value if isinstance(c.value, str) and c.value.startswith("=") else None,
                        "value": None if (isinstance(c.value, str) and c.value.startswith("=")) else c.value,
                        "cached": cached if (isinstance(c.value, str) and c.value.startswith("=")) else None,
                    }
        sheets[name] = {
            "dims": wsf.dimensions,
            "cells": {k: cells[k] for k in sorted(cells)},
            "merges": sorted(str(m) for m in wsf.merged_cells.ranges),
            "charts": len(getattr(wsf, "_charts", [])),
        }
    facts["sheets"] = sheets
    try:
        facts["defined_names"] = sorted(wb_f.defined_names.keys())
    except Exception:
        facts["defined_names"] = []
    # drawing media by (drawing part, position)
    media = []
    for name in sorted(parts):
        if name.startswith("xl/drawings/drawing") and name.endswith(".xml"):
            root = parse(parts[name])
            rels_name = name.replace("xl/drawings/", "xl/drawings/_rels/") + ".rels"
            rels = {}
            if rels_name in parts:
                for rel in parse(parts[rels_name]):
                    rels[rel.get("Id")] = rel.get("Target")
            idx = 0
            for blip in root.iter(f"{{{NS['a']}}}blip"):
                rid = blip.get(f"{{{NS['r']}}}embed")
                target = rels.get(rid, "")
                part = "xl/" + target.replace("../", "") if target else ""
                data = parts.get(part, b"")
                media.append(
                    {
                        "pos": f"{name}#{idx}",
                        "part": part,
                        "sha": sha(data) if data else None,
                        "ext": part.rsplit(".", 1)[-1].lower() if "." in part else "",
                        "bytes": len(data),
                    }
                )
                idx += 1
    facts["media"] = media
    return facts


# ------------------------------------------------------------------- pptx
def pptx_facts(path):
    import pptx

    facts = {"format": "pptx"}
    z, parts = open_zip(path)
    facts["parts"] = sorted(parts.keys())
    prs = pptx.Presentation(path)
    slides = []
    for i, slide in enumerate(prs.slides):
        shapes = []
        for shape in slide.shapes:
            entry = {"type": str(shape.shape_type), "name": shape.name}
            if shape.has_text_frame:
                entry["text"] = shape.text_frame.text
            if shape.has_chart:
                entry["chart_type"] = str(shape.chart.chart_type)
            shapes.append(entry)
        notes = ""
        if slide.has_notes_slide:
            notes = slide.notes_slide.notes_text_frame.text
        # pictures by position via slide XML blips
        slide_part = f"ppt/slides/slide{i + 1}.xml"
        media = []
        if slide_part in parts:
            root = parse(parts[slide_part])
            rels = {}
            rels_name = f"ppt/slides/_rels/slide{i + 1}.xml.rels"
            if rels_name in parts:
                for rel in parse(parts[rels_name]):
                    rels[rel.get("Id")] = rel.get("Target")
            idx = 0
            for blip in root.iter(f"{{{NS['a']}}}blip"):
                rid = blip.get(f"{{{NS['r']}}}embed")
                target = rels.get(rid, "")
                part = "ppt/" + target.replace("../", "") if target else ""
                data = parts.get(part, b"")
                media.append(
                    {
                        "pos": idx,
                        "part": part,
                        "sha": sha(data) if data else None,
                        "ext": part.rsplit(".", 1)[-1].lower() if "." in part else "",
                        "bytes": len(data),
                    }
                )
                idx += 1
        slides.append({"shapes": shapes, "notes": notes, "media": media})
    facts["slides"] = slides
    return facts


def facts_for(path):
    z = zipfile.ZipFile(path)
    names = z.namelist()
    if "word/document.xml" in names:
        return docx_facts(path)
    if "xl/workbook.xml" in names:
        return xlsx_facts(path)
    if "ppt/presentation.xml" in names:
        return pptx_facts(path)
    return {"format": "unknown", "parts": sorted(names)}


# ------------------------------------------------------------------- diff
def classify_media(a_list, b_list, parts_a, parts_b):
    out = []
    b_by_pos = {m["pos"]: m for m in b_list}
    a_by_pos = {m["pos"]: m for m in a_list}
    for pos, am in a_by_pos.items():
        bm = b_by_pos.get(pos)
        if bm is None:
            out.append({"pos": pos, "verdict": "LOST", "class": "media-lost"})
            continue
        if am["sha"] == bm["sha"]:
            out.append({"pos": pos, "verdict": "OK", "class": "media-ok"})
            continue
        # format changed?
        data_a = parts_a.get(am["part"], b"")
        data_b = parts_b.get(bm["part"], b"")
        size_a, pix_a = decoded_size(data_a)
        size_b, pix_b = decoded_size(data_b)
        if am["ext"] != bm["ext"]:
            if size_a is not None and size_a == size_b:
                out.append(
                    {
                        "pos": pos,
                        "verdict": "CONVERTED",
                        "class": f"media-converted:{am['ext']}>{bm['ext']}",
                    }
                )
            else:
                out.append({"pos": pos, "verdict": "CHANGED", "class": "media-changed"})
        else:
            if pix_a is not None and pix_a == pix_b:
                out.append({"pos": pos, "verdict": "RE-ENCODED", "class": "media-reencoded"})
            else:
                out.append({"pos": pos, "verdict": "CHANGED", "class": "media-changed"})
    for pos in b_by_pos:
        if pos not in a_by_pos:
            out.append({"pos": pos, "verdict": "ADDED", "class": "media-added"})
    return out


def part_class(name):
    import re

    return re.sub(r"\d+", "N", name)


def diff_facts(fa, fb, parts_a, parts_b):
    diffs = []
    classes = set()

    def add(path, kind, cls, detail=""):
        diffs.append({"path": path, "kind": kind, "class": cls, "detail": detail})
        classes.add(cls)

    # parts added/removed
    for p in sorted(set(fa["parts"]) - set(fb["parts"])):
        if p.startswith("docProps/"):
            add(p, "part-removed", "metadata-part")
        else:
            add(p, "part-removed", f"part-removed:{part_class(p)}")
    for p in sorted(set(fb["parts"]) - set(fa["parts"])):
        if p.startswith("docProps/"):
            add(p, "part-added", "metadata-part")
        else:
            add(p, "part-added", f"part-added:{part_class(p)}")

    fmt = fa["format"]
    if fmt == "docx":
        if fa["paragraphs"] != fb["paragraphs"]:
            for i, (x, y) in enumerate(zip(fa["paragraphs"], fb["paragraphs"])):
                if x != y:
                    add(f"paragraphs[{i}]", "text", "content-text-changed", f"{x!r} -> {y!r}")
            if len(fa["paragraphs"]) != len(fb["paragraphs"]):
                add("paragraphs", "count", "structure-changed",
                    f"{len(fa['paragraphs'])} -> {len(fb['paragraphs'])}")
        if fa["tables"] != fb["tables"]:
            add("tables", "structure", "tables-changed")
        if fa["revisions"] != fb["revisions"]:
            add("revisions", "count", "revisions-changed", f"{fa['revisions']} -> {fb['revisions']}")
        if fa["fields"] != fb["fields"]:
            add("fields", "count", "fields-changed")
        if fa["comments"] != fb["comments"]:
            add("comments", "content", "comments-changed")
        if fa["footnotes"] != fb["footnotes"]:
            add("footnotes", "content", "footnotes-changed")
        if fa["headers"] != fb["headers"] or fa["footers"] != fb["footers"]:
            add("headers/footers", "content", "headers-changed")
        if fa["styles_used"] != fb["styles_used"]:
            add("styles_used", "set", "styles-changed")
        if fa.get("vba_sha") != fb.get("vba_sha"):
            add("vbaProject.bin", "hash", "vba-changed")
    elif fmt == "xlsx":
        if set(fa["sheets"]) != set(fb["sheets"]):
            add("sheets", "set", "structure-changed",
                f"{sorted(fa['sheets'])} -> {sorted(fb['sheets'])}")
        for name in fa["sheets"]:
            if name not in fb["sheets"]:
                continue
            sa, sb = fa["sheets"][name], fb["sheets"][name]
            if sa["cells"] != sb["cells"]:
                for coord in sorted(set(sa["cells"]) | set(sb["cells"])):
                    ca, cb = sa["cells"].get(coord), sb["cells"].get(coord)
                    if ca == cb:
                        continue
                    if ca is None or cb is None:
                        add(f"{name}!{coord}", "cell", "cell-added-removed", f"{ca} -> {cb}")
                    elif ca["formula"] != cb["formula"]:
                        add(f"{name}!{coord}", "cell", "formula-changed", f"{ca} -> {cb}")
                    elif ca["value"] != cb["value"]:
                        add(f"{name}!{coord}", "cell", "cell-value-changed", f"{ca} -> {cb}")
                    else:
                        # Only the cached result of a formula
                        # moved: recalculation evidence, tracked
                        # separately from content changes.
                        add(f"{name}!{coord}", "cell", "cached-value-changed", f"{ca} -> {cb}")
            if sa["merges"] != sb["merges"]:
                add(f"{name}.merges", "set", "merges-changed")
            if sa["charts"] != sb["charts"]:
                add(f"{name}.charts", "count", "charts-changed")
        if fa["defined_names"] != fb["defined_names"]:
            add("defined_names", "set", "defined-names-changed")
        if fa["has_calcchain"] != fb["has_calcchain"]:
            add("calcChain", "presence", "calcchain-changed")
    elif fmt == "pptx":
        if len(fa["slides"]) != len(fb["slides"]):
            add("slides", "count", "structure-changed",
                f"{len(fa['slides'])} -> {len(fb['slides'])}")
        for i, (sa, sb) in enumerate(zip(fa["slides"], fb["slides"])):
            ta = [(s.get("text"), s.get("chart_type")) for s in sa["shapes"]]
            tb = [(s.get("text"), s.get("chart_type")) for s in sb["shapes"]]
            if ta != tb:
                add(f"slide[{i}]", "shapes", "slide-content-changed", f"{ta} -> {tb}")
            if sa["notes"] != sb["notes"]:
                add(f"slide[{i}].notes", "text", "notes-changed")

    # media (docx global list; pptx per slide; xlsx drawing list)
    media_a, media_b = [], []
    if fmt == "docx" or fmt == "xlsx":
        media_a, media_b = fa["media"], fb["media"]
        for m in classify_media(media_a, media_b, parts_a, parts_b):
            if m["verdict"] != "OK":
                add(f"media[{m['pos']}]", "media", m["class"], m["verdict"])
    elif fmt == "pptx":
        for i, (sa, sb) in enumerate(zip(fa["slides"], fb["slides"])):
            for m in classify_media(sa["media"], sb["media"], parts_a, parts_b):
                if m["verdict"] != "OK":
                    add(f"slide[{i}].media[{m['pos']}]", "media", m["class"], m["verdict"])
    return {"diffs": diffs, "classes": sorted(classes)}


def main():
    cmd = sys.argv[1]
    if cmd == "facts":
        print(json.dumps(facts_for(sys.argv[2]), indent=1, default=str))
    elif cmd == "diff":
        a, b = sys.argv[2], sys.argv[3]
        fa, fb = facts_for(a), facts_for(b)
        _, pa = open_zip(a)
        _, pb = open_zip(b)
        print(json.dumps(diff_facts(fa, fb, pa, pb), indent=1, default=str))
    elif cmd == "markerdiff":
        a, b, marker = sys.argv[2], sys.argv[3], sys.argv[4]
        fa, fb = facts_for(a), facts_for(b)
        _, pa = open_zip(a)
        _, pb = open_zip(b)
        d = diff_facts(fa, fb, pa, pb)
        bad = []
        for diff in d["diffs"]:
            if diff["class"] in ("metadata-part",):
                continue
            if marker in diff["detail"]:
                continue
            bad.append(diff)
        if bad:
            print("FAIL")
            print(json.dumps(bad, indent=1, default=str))
            sys.exit(1)
        print("PASS")
    else:
        print(f"unknown command {cmd}", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
