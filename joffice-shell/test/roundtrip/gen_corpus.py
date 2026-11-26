#!/usr/bin/env python3
"""Corpus generator for the jOffice marathon-2 cycle-2 round-trip suite.

Builds the corpus Kagi round 1 §1/§5 + round 2 §E specified, at
the smallest viable scale: semantic fixtures (docx/xlsx/pptx/
docm/dotx/legacy/odt/txt/csv/encrypted), the measurement shapes,
and the realistic worst cases (tracked-change contract,
cross-sheet financial model, merged deck).

Usage: gen_corpus.py <outdir>
Everything is script-generated; no fixture is hand-made, so the
corpus is reproducible byte-for-byte apart from zip timestamps.
"""
import io
import json
import os
import struct
import subprocess
import sys
import zipfile

OUT = sys.argv[1] if len(sys.argv) > 1 else "corpus"
os.makedirs(OUT, exist_ok=True)
os.makedirs(os.path.join(OUT, "measure"), exist_ok=True)

from PIL import Image  # noqa: E402


def make_jpeg(path, w=320, h=240, seed=7):
    """A real JPEG with actual image content (noise-ish gradient)."""
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = ((x * seed) % 256, (y * seed * 3) % 256, ((x + y) * seed) % 256)
    img.save(path, "JPEG", quality=88)
    return path


def make_png(path, w=200, h=150):
    img = Image.new("RGB", (w, h), (40, 90, 160))
    img.save(path, "PNG")
    return path


TMPIMG = os.path.join(OUT, "_img")
os.makedirs(TMPIMG, exist_ok=True)
JPEG_A = make_jpeg(os.path.join(TMPIMG, "a.jpg"), 320, 240, 7)
JPEG_B = make_jpeg(os.path.join(TMPIMG, "b.jpg"), 640, 480, 13)
PNG_A = make_png(os.path.join(TMPIMG, "a.png"))

# ---------------------------------------------------------------- docx basic
import docx  # noqa: E402
from docx.shared import Inches, Pt  # noqa: E402


def build_basic_docx(path):
    d = docx.Document()
    d.add_heading("Round Trip Fixture", level=1)
    p = d.add_paragraph()
    r = p.add_run("Plain sentence with ")
    r.bold = False
    r2 = p.add_run("bold")
    r2.bold = True
    r3 = p.add_run(" and ")
    r3.italic = False
    r4 = p.add_run("italic")
    r4.italic = True
    r5 = p.add_run(" runs.")
    d.add_paragraph("Second paragraph, numbered list follows:")
    d.add_paragraph("list item one", style="List Number")
    d.add_paragraph("list item two", style="List Number")
    t = d.add_table(rows=3, cols=3)
    t.style = "Table Grid"
    vals = [["A1", "B1", "C1"], ["A2", "B2", "C2"], ["A3", "B3", "C3"]]
    for i, row in enumerate(vals):
        for j, v in enumerate(row):
            t.rows[i].cells[j].text = v
    t.rows[0].cells[0].merge(t.rows[0].cells[1])
    d.add_paragraph("Image below:")
    d.add_picture(JPEG_A, width=Inches(2.0))
    sec = d.sections[0]
    hdr = sec.header.paragraphs[0]
    hdr.text = "Fixture Header"
    ftr = sec.footer.paragraphs[0]
    ftr.text = "Fixture Footer"
    d.save(path)
    add_footnote(path, "A fixture footnote.")
    return path


def add_footnote(docx_path, text):
    """Append a real footnotes part + reference via direct XML surgery."""
    from lxml import etree

    W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    ns = {"w": W, "r": R}
    zin = zipfile.ZipFile(docx_path, "r")
    parts = {n: zin.read(n) for n in zin.namelist()}
    zin.close()

    footnotes = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<w:footnotes xmlns:w="{W}">'
        f'<w:footnote w:type="separator" w:id="0"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>'
        f'<w:footnote w:type="continuationSeparator" w:id="1"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>'
        f'<w:footnote w:id="2"><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:footnote>'
        "</w:footnotes>"
    )
    parts["word/footnotes.xml"] = footnotes.encode()

    rels = etree.fromstring(parts["word/_rels/document.xml.rels"])
    maxid = 0
    for rel in rels:
        rid = rel.get("Id", "rId0")
        if rid.startswith("rId"):
            maxid = max(maxid, int(rid[3:]))
    new_rel = etree.SubElement(rels, "Relationship")
    new_rel.set("Id", f"rId{maxid + 1}")
    new_rel.set(
        "Type",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes",
    )
    new_rel.set("Target", "footnotes.xml")
    parts["word/_rels/document.xml.rels"] = etree.tostring(rels, xml_declaration=True, encoding="UTF-8", standalone=True)

    ct = etree.fromstring(parts["[Content_Types].xml"])
    ov = etree.SubElement(ct, "Override")
    ov.set("PartName", "/word/footnotes.xml")
    ov.set(
        "ContentType",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml",
    )
    parts["[Content_Types].xml"] = etree.tostring(ct, xml_declaration=True, encoding="UTF-8", standalone=True)

    doc = etree.fromstring(parts["word/document.xml"])
    body = doc.find("w:body", ns)
    first_p = body.find("w:p", ns)
    run = etree.SubElement(first_p, f"{{{W}}}r")
    ref = etree.SubElement(run, f"{{{W}}}footnoteReference")
    ref.set(f"{{{W}}}id", "2")
    parts["word/document.xml"] = etree.tostring(doc, xml_declaration=True, encoding="UTF-8", standalone=True)

    zout = zipfile.ZipFile(docx_path, "w", zipfile.ZIP_DEFLATED)
    for name, data in parts.items():
        zout.writestr(name, data)
    zout.close()


basic = build_basic_docx(os.path.join(OUT, "basic.docx"))

# ---------------------------------------------------------------- docx media
d = docx.Document()
d.add_paragraph("Media fixture: three JPEGs and a PNG.")
d.add_picture(JPEG_A, width=Inches(1.5))
d.add_paragraph("between images")
d.add_picture(JPEG_B, width=Inches(2.5))
t = d.add_table(rows=1, cols=1)
t.rows[0].cells[0].paragraphs[0].add_run().add_picture(PNG_A, width=Inches(1.0))
d.add_paragraph("last image:")
d.add_picture(JPEG_A, width=Inches(1.0))
d.save(os.path.join(OUT, "media.docx"))

# ---------------------------------------------------------------- xlsx
import openpyxl  # noqa: E402
from openpyxl.chart import BarChart, Reference  # noqa: E402
from openpyxl.workbook.defined_name import DefinedName  # noqa: E402

wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Calc"
ws["A1"] = 2
ws["B1"] = "=A1*3"
ws["C1"] = "=SUM(B1:B10)"
ws["D1"] = "label"
ws["E1"] = "2026-01-15"
ws["A2"] = 10
ws["B2"] = 20
chart = BarChart()
data = Reference(ws, min_col=1, min_row=1, max_row=2)
chart.add_data(data)
ws.add_chart(chart, "A5")
ws2 = wb.create_sheet("Second")
ws2["A1"] = "other sheet"
ws2["B1"] = "=Calc!A1+1"
wb.defined_names["FixtureName"] = DefinedName("FixtureName", attr_text="Calc!$A$1")
wb.save(os.path.join(OUT, "formulas.xlsx"))

# ---------------------------------------------------------------- pptx
import pptx  # noqa: E402
from pptx.chart.data import ChartData  # noqa: E402
from pptx.enum.chart import XL_CHART_TYPE  # noqa: E402
from pptx.util import Inches as PInches  # noqa: E402

prs = pptx.Presentation()
s1 = prs.slides.add_slide(prs.slide_layouts[0])
s1.shapes.title.text = "Deck Fixture"
s1.placeholders[1].text = "subtitle here"
s2 = prs.slides.add_slide(prs.slide_layouts[1])
s2.shapes.title.text = "Bullets"
tf = s2.placeholders[1].text_frame
tf.text = "first bullet"
tf.add_paragraph().text = "second bullet"
s2.notes_slide.notes_text_frame.text = "Speaker notes for slide two."
s3 = prs.slides.add_slide(prs.slide_layouts[5])
s3.shapes.title.text = "Picture slide"
s3.shapes.add_picture(JPEG_A, PInches(1), PInches(1.5), width=PInches(3))
s4 = prs.slides.add_slide(prs.slide_layouts[5])
s4.shapes.title.text = "Chart slide"
cd = ChartData()
cd.categories = ["Q1", "Q2"]
cd.add_series("S", (3, 4))
s4.shapes.add_chart(XL_CHART_TYPE.COLUMN_CLUSTERED, PInches(0.5), PInches(1.5), PInches(5), PInches(3), cd)
prs.save(os.path.join(OUT, "basic.pptx"))

# ---------------------------------------------------------------- docm / dotx
def retype_docx(src, dst, main_ct, extra_parts=None):
    zin = zipfile.ZipFile(src)
    parts = {n: zin.read(n) for n in zin.namelist()}
    zin.close()
    ct = parts["[Content_Types].xml"].decode()
    ct = ct.replace(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        main_ct,
    )
    if extra_parts:
        for name, data, ctype in extra_parts:
            parts[name] = data
            ct = ct.replace(
                "</Types>",
                f'<Override PartName="/{name}" ContentType="{ctype}"/></Types>',
            )
    parts["[Content_Types].xml"] = ct.encode()
    if extra_parts:
        rels = parts["word/_rels/document.xml.rels"].decode()
        rid = 900
        for name, _data, _ctype in extra_parts:
            base = name.split("/")[-1]
            reltype = (
                "http://schemas.microsoft.com/office/2006/relationships/vbaProject"
                if base == "vbaProject.bin"
                else "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vbaData"
            )
            rels = rels.replace(
                "</Relationships>",
                f'<Relationship Id="rId{rid}" Type="{reltype}" Target="{base}"/></Relationships>',
            )
            rid += 1
        parts["word/_rels/document.xml.rels"] = rels.encode()
    z = zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED)
    for n, data in parts.items():
        z.writestr(n, data)
    z.close()


# A stand-in vbaProject.bin: the shell and checker treat it as
# opaque bytes (presence + hash). The editor drive grades
# whether sdkjs/x2t do the same.
fake_vba = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1" + b"JOFFICE-FIXTURE-VBA" + bytes(range(256))
retype_docx(
    basic,
    os.path.join(OUT, "macro.docm"),
    "application/vnd.ms-word.document.macroEnabled.main+xml",
    extra_parts=[
        ("word/vbaProject.bin", fake_vba, "application/vnd.ms-office.vbaProject"),
        (
            "word/vbaData.xml",
            b'<?xml version="1.0"?><wne:vbaSuppData xmlns:wne="http://schemas.microsoft.com/office/word/2006/wordml"><wne:mcds/></wne:vbaSuppData>',
            "application/vnd.ms-office.vbaData+xml",
        ),
    ],
)
retype_docx(
    basic,
    os.path.join(OUT, "template.dotx"),
    "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
)

# ---------------------------------------------------------------- legacy / odt / text
def soffice_convert(src, fmt, outdir):
    subprocess.run(
        ["soffice", "--headless", "--convert-to", fmt, "--outdir", outdir, src],
        check=True,
        capture_output=True,
        timeout=120,
    )


soffice_convert(basic, "doc", OUT)  # legacy.doc
soffice_convert(basic, "odt", OUT)  # basic.odt
os.rename(os.path.join(OUT, "basic.doc"), os.path.join(OUT, "legacy.doc"))
os.rename(os.path.join(OUT, "basic.odt"), os.path.join(OUT, "simple.odt"))

with open(os.path.join(OUT, "plain.txt"), "w") as f:
    f.write("Plain text fixture.\nSecond line, with a comma, and a semicolon; inside.\n")
with open(os.path.join(OUT, "data.csv"), "w") as f:
    f.write("name,value,note\nalpha,1,first\nbeta,2,second\n")
with open(os.path.join(OUT, "data-semi.csv"), "w") as f:
    f.write("name;value;note\nalpha;1;first\nbeta;2;second\n")

# ---------------------------------------------------------------- encrypted
import msoffcrypto  # noqa: E402

with open(basic, "rb") as fin, open(os.path.join(OUT, "encrypted.docx"), "wb") as fout:
    off = msoffcrypto.OfficeFile(fin)
    off.encrypt("joffice-test", fout)

# ================================================================ measurement
M = os.path.join(OUT, "measure")

# T: long text documents (500 / 2000 pages of body text).
for pages, name in [(25, "t500.docx"), (100, "t2000.docx")]:
    # ~20 paragraphs/page at this density; page count is nominal
    # (the shape is what matters: many paragraphs, one style).
    d = docx.Document()
    para = (
        "The committee considered the proposal at length and resolved "
        "to record its findings in the minutes of the meeting, with "
        "the full text of the motion appended for the record."
    )
    for _ in range(pages * 20):
        d.add_paragraph(para)
    d.save(os.path.join(M, name))

# M: media-heavy documents (~20MB / ~100MB of JPEG). The
# images must be DISTINCT — python-docx dedupes identical image
# parts, so one repeated JPEG would measure nothing. True-noise
# JPEGs (incompressible, photo-weight) at ~1 MB each.
def noise_jpeg(path, w=1200, h=900):
    raw = os.urandom(w * h * 3)
    img = Image.frombytes("RGB", (w, h), raw)
    img.save(path, "JPEG", quality=92)
    return os.path.getsize(path)


probe = noise_jpeg(os.path.join(TMPIMG, "noise-probe.jpg"))
os.remove(os.path.join(TMPIMG, "noise-probe.jpg"))
for target_mb, name in [(20, "m20.docx"), (100, "m100.docx")]:
    d = docx.Document()
    d.add_paragraph(f"Media measurement fixture (~{target_mb} MB).")
    n = max(1, round(target_mb * 1024 * 1024 / probe))
    for i in range(n):
        p = os.path.join(TMPIMG, f"noise-{name}-{i}.jpg")
        noise_jpeg(p)
        d.add_picture(p, width=Inches(3.0))
        os.remove(p)
    d.save(os.path.join(M, name))

# X1: big value grids (write-only for speed).
for rows, name in [(100_000, "x1-100k.xlsx"), (1_000_000, "x1-1m.xlsx")]:
    wb = openpyxl.Workbook(write_only=True)
    ws = wb.create_sheet("Grid")
    for r in range(rows):
        ws.append([r, r * 2, f"row{r}", r % 97, r * 0.5, "x", r + 1, r - 1, r % 13, r * 3])
    wb.save(os.path.join(M, name))

# X2: formula chain.
wb = openpyxl.Workbook(write_only=True)
ws = wb.create_sheet("Chain")
ws.append([1])
for r in range(2, 200_001):
    ws.append([f"=A{r - 1}+1"])
wb.save(os.path.join(M, "x2-200k.xlsx"))

# P: 100-slide deck.
prs = pptx.Presentation()
for i in range(100):
    s = prs.slides.add_slide(prs.slide_layouts[5])
    s.shapes.title.text = f"Slide {i + 1}"
    s.shapes.add_picture(JPEG_A, PInches(1), PInches(1.5), width=PInches(3))
prs.save(os.path.join(M, "p100.pptx"))

# Realistic 1: negotiated contract — tracked changes + comments
# at reduced scale (round 2 §E at the smallest viable size:
# 200 tracked insertions/deletions by 2 authors, 40 comments).
# Hand-built XML: python-docx cannot author revisions.
def build_contract(path):
    paras = []
    for i in range(60):
        paras.append(
            f"<w:p><w:r><w:t>Clause {i + 1}. The parties agree to the "
            f"terms set out in schedule {i + 1} of this agreement.</w:t></w:r></w:p>"
        )
    for i in range(200):
        author = "Alice" if i % 2 == 0 else "Bob"
        paras.append(
            f'<w:p><w:ins w:id="{1000 + i}" w:author="{author}" w:date="2026-11-0{(i % 9) + 1}T10:00:00Z">'
            f"<w:r><w:t>Inserted clause {i}.</w:t></w:r></w:ins>"
            f'<w:r><w:t xml:space="preserve"> </w:t></w:r>'
            f'<w:del w:id="{2000 + i}" w:author="{author}" w:date="2026-11-0{(i % 9) + 1}T11:00:00Z">'
            f"<w:r><w:delText>Deleted clause {i}.</w:delText></w:r></w:del></w:p>"
        )
    comments = []
    for i in range(40):
        author = "Alice" if i % 2 == 0 else "Bob"
        comments.append(
            f'<w:comment w:id="{i}" w:author="{author}" w:date="2026-11-05T09:00:00Z">'
            f"<w:p><w:r><w:t>Comment {i}: please check this clause.</w:t></w:r></w:p></w:comment>"
        )
        paras.append(
            f"<w:p><w:commentRangeStart w:id=\"{i}\"/>"
            f"<w:r><w:t>Commented text {i}.</w:t></w:r>"
            f"<w:commentRangeEnd w:id=\"{i}\"/>"
            f"<w:r><w:commentReference w:id=\"{i}\"/></w:r></w:p>"
        )
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        f"<w:body>{''.join(paras)}"
        '<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:body></w:document>'
    )
    comments_xml = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f"{''.join(comments)}</w:comments>"
    )
    content_types = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
        '<Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>'
        '<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>'
        "</Types>"
    )
    rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
        "</Relationships>"
    )
    doc_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>'
        '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>'
        "</Relationships>"
    )
    styles = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        '<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>'
        "</w:styles>"
    )
    z = zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED)
    z.writestr("[Content_Types].xml", content_types)
    z.writestr("_rels/.rels", rels)
    z.writestr("word/document.xml", document)
    z.writestr("word/comments.xml", comments_xml)
    z.writestr("word/styles.xml", styles)
    z.writestr("word/_rels/document.xml.rels", doc_rels)
    z.close()


build_contract(os.path.join(M, "contract.docx"))

# Realistic 2: financial model at reduced scale — 10 sheets x
# 3000 cross-sheet formulas + conditional formatting rules.
wb = openpyxl.Workbook()
wb.remove(wb.active)
from openpyxl.formatting.rule import CellIsRule  # noqa: E402

for s in range(10):
    ws = wb.create_sheet(f"Model{s}")
    ws["A1"] = s + 1
    for r in range(2, 3002):
        if s == 0:
            ws.cell(row=r, column=1, value=r)
        else:
            ws.cell(row=r, column=1, value=f"=Model{s - 1}!A{r}+1")
        ws.cell(row=r, column=2, value=f"=A{r}*2")
    ws.conditional_formatting.add(
        f"A2:A3001", CellIsRule(operator="greaterThan", formula=["100"])
    )
wb.save(os.path.join(M, "finmodel.xlsx"))

# Realistic 3: merged deck at reduced scale — 30 slides across
# several layouts, pictures + a chart.
prs = pptx.Presentation()
layouts = [0, 1, 5, 6]
for i in range(30):
    layout = prs.slide_layouts[layouts[i % len(layouts)]]
    s = prs.slides.add_slide(layout)
    if s.shapes.title is not None:
        s.shapes.title.text = f"Merged deck slide {i + 1}"
    if i % 3 == 0:
        s.shapes.add_picture(JPEG_A, PInches(1), PInches(2), width=PInches(2.5))
    if i == 10:
        cd = ChartData()
        cd.categories = ["A", "B", "C"]
        cd.add_series("S", (1, 2, 3))
        s.shapes.add_chart(
            XL_CHART_TYPE.COLUMN_CLUSTERED, PInches(0.5), PInches(1.5), PInches(5), PInches(3), cd
        )
prs.save(os.path.join(M, "deck.pptx"))

manifest = {
    "basic": "basic.docx",
    "media": "media.docx",
    "formulas": "formulas.xlsx",
    "deck": "basic.pptx",
    "macro": "macro.docm",
    "template": "template.dotx",
    "legacy": "legacy.doc",
    "odt": "simple.odt",
    "txt": "plain.txt",
    "csv": "data.csv",
    "csv_semi": "data-semi.csv",
    "encrypted": "encrypted.docx",
}
with open(os.path.join(OUT, "manifest.json"), "w") as f:
    json.dump(manifest, f, indent=1)
print("corpus base written to", OUT)
