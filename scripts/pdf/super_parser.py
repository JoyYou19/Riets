#!/usr/bin/env python3
"""Split PDF, PowerPoint and Markdown files into one JSON file per page/slide/document.

Usage:
    python deck_to_slide_json.py INPUT [OUTPUT_DIR] [--min-image-px N] [--md-split-level N] [--verbose]

    INPUT is one file (~/Downloads/the_file.pdf, ~/docs/XML_status.md) or a folder of files.
    OUTPUT_DIR defaults to a "slides_json" folder next to the input file (or inside the input folder).

Markdown (.md / .markdown): one JSON per file, or one per section with --md-split-level 1 or 2.
Headings, paragraphs, lists, tables, images and links become shapes; fenced code blocks become
"code" shapes, and an XML or JSON block also gets its content as JSON under "data":
    <cps:content><index><status> normal </status></index></cps:content>
    -> "data": {"cps:content": {"index": {"status": "normal"}}}
XML conversion: attributes become "@name", text next to child elements "#text", repeated elements
lists; namespace prefixes are kept even when the snippet never declares them (cps:).

Example output for "Sistēmu modelēšana.pdf" (page 2):
    OUTPUT_DIR/Sistēmu_modelēšana_slide02.json
    OUTPUT_DIR/assets/Sistēmu_modelēšana/slide02_image01.png   (only if the page has pictures)

Every JSON file has the same shape, whatever the source format:
    {
      "deck": "Sistēmu modelēšana.pdf",
      "source_format": "pdf" | "pptx",
      "slide_number": 2,
      "title": "...",
      "shapes": [ {"name", "kind": "text", "paragraphs": [{"level", "text"}]},
                  {"name", "kind": "table", "rows": [[...]]},
                  {"name", "kind": "picture", "file": "assets/..."},
                  {"name", "kind": "chart", ...},          # PowerPoint only
                  {"name", "kind": "group", "children"} ], # PowerPoint only
                  {"name", "kind": "heading", "level", "text"},             # Markdown only
                  {"name", "kind": "code", "language", "text", "data"?} ],  # Markdown only
      "notes": "...",        # speaker notes (PowerPoint) or comment annotations (PDF)
      "warnings": ["..."],
      "links": [{"text", "url"}],    # Markdown only, when the section has links
      "metadata": {...}              # Markdown only, from YAML front matter
    }

Requirements: Python 3.10+.
    PDF files:         pip install pymupdf        (1.23 or newer)
    .pptx / .potx:     pip install python-pptx
    legacy .ppt:       python-pptx plus LibreOffice (soffice on PATH)
    Markdown:          nothing extra (standard library only)
Each library is only needed if the folder actually contains that kind of file.
"""
from __future__ import annotations

import argparse
import hashlib
import html
import io
import json
import logging
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
import urllib.parse
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

try:  # PowerPoint support is optional
    from pptx import Presentation
    from pptx.enum.shapes import MSO_SHAPE_TYPE
except ImportError:  # pragma: no cover - depends on environment
    Presentation = None
    MSO_SHAPE_TYPE = None

try:  # PDF support is optional
    import pymupdf
except ImportError:  # pragma: no cover - depends on environment
    try:
        import fitz as pymupdf  # older PyMuPDF releases
    except ImportError:
        pymupdf = None

ZIP_MAGIC = b"PK\x03\x04"                        # .pptx / .potx
OLE_MAGIC = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"  # legacy .ppt
PDF_MAGIC = b"%PDF-"                             # may legally appear anywhere in the first 1 KiB
SNIFF_BYTES = 1024
MD_SUFFIXES = {".md", ".markdown", ".mdown", ".mkd"}
CANDIDATE_SUFFIXES = {".pptx", ".ppt", ".potx", ".pdf", *MD_SUFFIXES}

ROW_BAND_EMU = 274320     # 0.3 inch in EMU: shapes whose tops fall in the same band form one row
ROW_BAND_PT = 21.6        # the same 0.3 inch in PDF points
INDENT_TOLERANCE_PT = 6.0 # left edges within this distance count as the same bullet level
AVG_CHAR_WIDTH_EM = 0.5   # average glyph width as a share of font size, for "did this line wrap?"
MAX_LEVEL = 8
TITLE_MIN_RATIO = 1.15    # a title must be at least 15 % larger than the page's body text
TITLE_ZONE = 0.40         # ... and start in the top 40 % of the page
MARGIN_ZONE = 0.08        # top/bottom 8 % of the page, where page numbers live
DEFAULT_MIN_IMAGE_PX = 24 # images narrower or shorter than this are decorative (bullets, rules)
BROWSER_SAFE_EXT = {"png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif"}

BULLET_RE = re.compile(
    r"^[•●○◦▪▫■□‣⁃∙·➢→"
    r"\-–—*](?:\s+|$)"
)
PAGE_NUMBER_RE = re.compile(r"^\d{1,4}(\s*/\s*\d{1,4})?$")

log = logging.getLogger("deck_to_slide_json")


# --------------------------------------------------------------------------- shared helpers

def safe_name(text: str) -> str:
    cleaned = re.sub(r"[^\w\-]+", "_", unicodedata.normalize("NFC", text)).strip("_")
    return cleaned or "deck"


def clean_text(text: str) -> str:
    text = unicodedata.normalize("NFC", text).replace("­", "")
    return re.sub(r"\s+", " ", text).strip()


def sniff(path: Path) -> str | None:
    try:
        with path.open("rb") as fh:
            head = fh.read(SNIFF_BYTES)
    except OSError:
        return None
    if head.startswith(ZIP_MAGIC):
        return "pptx"
    if head.startswith(OLE_MAGIC):
        return "ppt"
    # checked before the PDF test: a Markdown page may well mention "%PDF-" in its first lines
    if path.suffix.lower() in MD_SUFFIXES and not head.startswith(PDF_MAGIC) and b"\x00" not in head:
        return "md"
    if PDF_MAGIC in head:
        return "pdf"
    return None


def write_json(target: Path, data: dict) -> None:
    """Write atomically so an interrupted run never leaves half a JSON file behind."""
    tmp = target.with_name(target.name + ".tmp")
    tmp.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(target)


def slide_record(deck: str, fmt: str, number: int, title: str, shapes: list[dict],
                 notes: str, warnings: list[str]) -> dict:
    return {
        "deck": deck,
        "source_format": fmt,
        "slide_number": number,
        "title": title,
        "shapes": shapes,
        "notes": notes,
        "warnings": warnings,
    }


def require(module: Any, what: str, install: str) -> None:
    if module is None:
        raise RuntimeError(f"{what} support needs '{install}' (pip install {install})")


# --------------------------------------------------------------------------- PowerPoint

def ensure_pptx(path: Path, work_dir: Path, kind: str) -> Path:
    if kind == "pptx":
        return path
    soffice = shutil.which("soffice") or shutil.which("libreoffice")
    if soffice is None:
        raise RuntimeError("legacy .ppt needs LibreOffice (soffice) on PATH")
    staged = work_dir / "input.ppt"  # ASCII name: avoids LibreOffice issues with unusual file names
    shutil.copyfile(path, staged)
    subprocess.run(
        [soffice, "--headless", "--convert-to", "pptx", "--outdir", str(work_dir), str(staged)],
        check=True,
        capture_output=True,
        timeout=300,
    )
    converted = work_dir / "input.pptx"
    if not converted.exists():
        raise RuntimeError("LibreOffice did not produce a .pptx file")
    return converted


def pptx_reading_order(shapes: Iterable) -> list:
    return sorted(shapes, key=lambda s: ((s.top or 0) // ROW_BAND_EMU, s.left or 0))


class PptxSlideExtractor:
    def __init__(self, slide_number: int, width: int, asset_dir: Path, asset_prefix: str):
        self.slide_number = slide_number
        self.width = width
        self.asset_dir = asset_dir
        self.asset_prefix = asset_prefix
        self.image_count = 0
        self.warnings: list[str] = []

    def extract_all(self, shapes: Iterable) -> list[dict]:
        items = (self.extract(s) for s in pptx_reading_order(shapes))
        return [item for item in items if item]

    def extract(self, shape) -> dict | None:
        base = {"name": shape.name}

        if shape.shape_type == MSO_SHAPE_TYPE.GROUP:
            children = self.extract_all(shape.shapes)
            return {**base, "kind": "group", "children": children} if children else None

        try:
            image = shape.image  # pictures and filled picture placeholders
        except (AttributeError, ValueError, KeyError):
            image = None          # not a picture, empty placeholder, or linked (not embedded) image
        if image is not None:
            return self._picture(image, base)

        if getattr(shape, "has_table", False):
            rows = [[clean_text(cell.text) for cell in row.cells] for row in shape.table.rows]
            return {**base, "kind": "table", "rows": rows}

        if getattr(shape, "has_chart", False):
            return self._chart(shape.chart, base)

        if getattr(shape, "has_text_frame", False) and shape.has_text_frame:
            paragraphs = [
                {"level": p.level, "text": clean_text(p.text)}
                for p in shape.text_frame.paragraphs
                if clean_text(p.text)
            ]
            if paragraphs:
                return {**base, "kind": "text", "paragraphs": paragraphs}

        return None

    def _chart(self, chart, base: dict) -> dict:
        try:
            categories = [str(c) for c in chart.plots[0].categories] if len(chart.plots) else []
        except Exception:  # XY / bubble charts have no category axis
            categories = []
        series = []
        for s in chart.series:
            try:
                values = list(s.values)
            except Exception:
                values = []
            series.append({"name": s.name, "values": values})
        return {**base, "kind": "chart", "categories": categories, "series": series}

    def _picture(self, image, base: dict) -> dict:
        self.image_count += 1
        filename = f"slide{self.slide_number:0{self.width}d}_image{self.image_count:02d}.{image.ext}"
        self.asset_dir.mkdir(parents=True, exist_ok=True)
        (self.asset_dir / filename).write_bytes(image.blob)
        return {**base, "kind": "picture", "file": f"{self.asset_prefix}/{filename}"}


def build_pptx_slide(slide, number: int, deck: str, extractor: PptxSlideExtractor) -> dict:
    title_shape = slide.shapes.title
    title = ""
    if title_shape is not None and title_shape.has_text_frame:
        title = clean_text(title_shape.text_frame.text)

    body_shapes = [
        s for s in slide.shapes
        if title_shape is None or s.shape_id != title_shape.shape_id
    ]
    shapes = extractor.extract_all(body_shapes)

    notes = ""
    if slide.has_notes_slide:
        notes_frame = slide.notes_slide.notes_text_frame
        if notes_frame is not None:
            notes = notes_frame.text.strip()

    return slide_record(deck, "pptx", number, title, shapes, notes, extractor.warnings)


def convert_pptx(path: Path, kind: str, out_dir: Path, stem: str) -> tuple[int, int]:
    require(Presentation, "PowerPoint", "python-pptx")
    asset_dir = out_dir / "assets" / stem
    with tempfile.TemporaryDirectory() as tmp:
        prs = Presentation(str(ensure_pptx(path, Path(tmp), kind)))  # loads the whole package into memory
    slides = list(prs.slides)
    width = max(2, len(str(len(slides))))

    page_failures = 0
    for number, slide in enumerate(slides, 1):
        extractor = PptxSlideExtractor(number, width, asset_dir, f"assets/{stem}")
        try:
            data = build_pptx_slide(slide, number, path.name, extractor)
        except Exception as exc:
            page_failures += 1
            log.error("%s slide %d: %s", path.name, number, exc)
            data = slide_record(path.name, "pptx", number, "", [], "", [f"extraction failed: {exc}"])
        write_json(out_dir / f"{stem}_slide{number:0{width}d}.json", data)
    return len(slides), page_failures


# --------------------------------------------------------------------------- PDF

@dataclass
class PdfLine:
    text: str
    x0: float
    y0: float
    x1: float
    y1: float
    size: float

    @property
    def cx(self) -> float:
        return (self.x0 + self.x1) / 2

    @property
    def cy(self) -> float:
        return (self.y0 + self.y1) / 2


@dataclass
class PdfBlock:
    lines: list[PdfLine]

    @property
    def x0(self) -> float:
        return min(ln.x0 for ln in self.lines)

    @property
    def x1(self) -> float:
        return max(ln.x1 for ln in self.lines)

    @property
    def y0(self) -> float:
        return min(ln.y0 for ln in self.lines)


def weighted_median_size(lines: list[PdfLine]) -> float:
    """Font size that covers the middle character of the page: the body-text size."""
    pairs = sorted((ln.size, len(ln.text)) for ln in lines)
    total = sum(n for _, n in pairs)
    running = 0
    for size, n in pairs:
        running += n
        if running * 2 >= total:
            return size
    return pairs[-1][0]


def inside(cx: float, cy: float, rect) -> bool:
    return rect[0] <= cx <= rect[2] and rect[1] <= cy <= rect[3]


class PdfDeckExtractor:
    """Holds per-document state: the outline, and images already written (logos repeat on every page)."""

    def __init__(self, doc, deck: str, width: int, asset_dir: Path, asset_prefix: str, min_image_px: int):
        self.doc = doc
        self.deck = deck
        self.width = width
        self.asset_dir = asset_dir
        self.asset_prefix = asset_prefix
        self.min_image_px = min_image_px
        self.written: dict[str, str] = {}  # content hash -> relative asset path
        self.outline = self._outline_titles()
        self.text_flags = (
            pymupdf.TEXTFLAGS_DICT
            & ~pymupdf.TEXT_PRESERVE_LIGATURES   # "ﬁ" -> "fi"
            & ~pymupdf.TEXT_PRESERVE_IMAGES      # images are read separately, by xref
        )

    def _outline_titles(self) -> dict[int, str]:
        titles: dict[int, tuple[int, str]] = {}
        try:
            toc = self.doc.get_toc(simple=True)
        except Exception:
            return {}
        for level, title, page in toc:
            title = clean_text(title)
            if page >= 1 and title and (page not in titles or level < titles[page][0]):
                titles[page] = (level, title)
        return {page: title for page, (_, title) in titles.items()}

    # ---- page

    def build_page(self, page, number: int) -> dict:
        warnings: list[str] = []
        height = page.rect.height or 1.0

        tables, table_rects = self._tables(page, warnings)
        blocks = self._text_blocks(page, table_rects, height)
        all_lines = [ln for b in blocks for ln in b.lines]

        title_lines = self._title_lines(all_lines, height)
        if not title_lines and number in self.outline:
            title_lines = self._lines_matching(all_lines, self.outline[number])
        title = " ".join(ln.text for ln in title_lines)
        if not title and number in self.outline:
            title = self.outline[number]  # bookmark title that is not printed on the page
        if title_lines:
            dropped = {id(ln) for ln in title_lines}
            blocks = [b for b in (PdfBlock([ln for ln in b.lines if id(ln) not in dropped]) for b in blocks) if b.lines]

        items: list[tuple[float, float, dict]] = []
        for group in self._merge_columns(blocks):
            paragraphs = self._paragraphs(group)
            if paragraphs:
                items.append((group.y0, group.x0, {"kind": "text", "paragraphs": paragraphs}))
        for rect, rows in tables:
            items.append((rect[1], rect[0], {"kind": "table", "rows": rows}))
        pictures = self._pictures(page, number, warnings)
        items.extend(pictures)

        items.sort(key=lambda it: (it[0] // ROW_BAND_PT, it[1]))
        counters: dict[str, int] = {}
        shapes = []
        for _, _, item in items:
            counters[item["kind"]] = counters.get(item["kind"], 0) + 1
            shapes.append({"name": f"{item['kind'].title()} {counters[item['kind']]}", **item})

        if not all_lines and not tables:
            if pictures:
                warnings.append("no extractable text; page is probably scanned - run OCR first (e.g. ocrmypdf)")
            else:
                warnings.append("page has no text, tables or pictures")

        return slide_record(self.deck, "pdf", number, title, shapes, self._notes(page), warnings)

    # ---- text

    def _text_blocks(self, page, table_rects: list, height: float) -> list[PdfBlock]:
        raw = page.get_text("dict", flags=self.text_flags, sort=True)
        blocks: list[PdfBlock] = []
        for block in raw.get("blocks", []):
            if block.get("type") != 0:
                continue
            lines: list[PdfLine] = []
            for line in block.get("lines", []):
                spans = [s for s in line.get("spans", []) if s.get("text", "").strip()]
                if not spans:
                    continue
                text = clean_text("".join(s["text"] for s in line["spans"]))
                x0, y0, x1, y1 = line["bbox"]
                ln = PdfLine(text, x0, y0, x1, y1, round(max(s["size"] for s in spans), 1))
                if any(inside(ln.cx, ln.cy, r) for r in table_rects):
                    continue  # already captured as table cells
                in_margin = ln.y1 < height * MARGIN_ZONE or ln.y0 > height * (1 - MARGIN_ZONE)
                if in_margin and PAGE_NUMBER_RE.match(ln.text):
                    continue  # page / slide number
                lines.append(ln)
            if lines:
                blocks.append(PdfBlock(lines))
        return blocks

    def _title_lines(self, lines: list[PdfLine], height: float) -> list[PdfLine]:
        if not lines:
            return []
        if len(lines) == 1:  # a lone heading on an otherwise text-free page (title + table/picture)
            only = lines[0]
            return [only] if only.y0 < height * TITLE_ZONE else []
        body = weighted_median_size(lines)
        candidates = [ln for ln in lines if ln.y0 < height * TITLE_ZONE and ln.size >= body * TITLE_MIN_RATIO]
        if not candidates:
            return []
        top_size = max(ln.size for ln in candidates)
        chosen = sorted((ln for ln in candidates if ln.size >= top_size - 0.5), key=lambda ln: (ln.y0, ln.x0))
        # keep only the first visual run of lines (a title wrapped over 2-3 lines), not a second heading lower down
        run = [chosen[0]]
        for ln in chosen[1:]:
            if ln.y0 - run[-1].y1 <= top_size * 0.8:
                run.append(ln)
            else:
                break
        return run

    @staticmethod
    def _lines_matching(lines: list[PdfLine], title: str) -> list[PdfLine]:
        """Consecutive lines (in reading order) whose joined text equals the bookmark title."""
        target = title.casefold()
        ordered = sorted(lines, key=lambda ln: (ln.y0, ln.x0))
        for start in range(len(ordered)):
            joined = ""
            for end in range(start, min(start + 4, len(ordered))):
                joined = f"{joined} {ordered[end].text}".strip().casefold()
                if joined == target:
                    return ordered[start:end + 1]
                if not target.startswith(joined):
                    break
        return []

    @staticmethod
    def _merge_columns(blocks: list[PdfBlock]) -> list[PdfBlock]:
        """Join text blocks that sit in the same column with no big vertical gap into one text shape.
        PDF producers often emit every paragraph as its own block; this restores the text box.
        Blocks that do not overlap horizontally (columns separated by a gutter) stay apart."""
        groups: list[PdfBlock] = []
        for block in sorted(blocks, key=lambda b: (b.y0, b.x0)):
            size = max(ln.size for ln in block.lines)
            for group in groups:
                last = group.lines[-1]
                overlaps = block.x0 < group.x1 and group.x0 < block.x1
                gap = block.y0 - max(ln.y1 for ln in group.lines)
                if overlaps and -size < gap <= 2.0 * max(size, last.size):
                    group.lines.extend(block.lines)
                    break
            else:
                groups.append(PdfBlock(list(block.lines)))
        return groups

    @staticmethod
    def _assign_levels(paragraphs: list[dict]) -> None:
        """Bullet level = rank of the paragraph's indent among the distinct indents in its text shape.
        Ranking adapts to any template's indent widths instead of assuming a fixed step."""
        starts = sorted({p["x"] for p in paragraphs})
        clusters: list[float] = []
        for x in starts:
            if not clusters or x - clusters[-1] > INDENT_TOLERANCE_PT:
                clusters.append(x)
        for p in paragraphs:
            level = sum(1 for c in clusters if c <= p["x"] + INDENT_TOLERANCE_PT) - 1
            p["level"] = min(max(level, 0), MAX_LEVEL)
            del p["x"], p["bulleted"]

    @classmethod
    def _paragraphs(cls, block: PdfBlock) -> list[dict]:
        paragraphs: list[dict] = []
        current: dict | None = None
        prev: PdfLine | None = None
        pending_bullet_x: float | None = None  # bullet glyph exported as its own line
        right_edge = block.x1

        def wrapped(prev_line: PdfLine, next_text: str, size: float) -> bool:
            """True if the previous line ended only because the next word did not fit on it."""
            first_word = next_text.split(" ", 1)[0]
            estimated = (len(first_word) + 1) * AVG_CHAR_WIDTH_EM * size
            return prev_line.x1 + estimated >= right_edge - 0.5 * size

        for ln in block.lines:
            match = BULLET_RE.match(ln.text)
            text = ln.text[match.end():].strip() if match else ln.text
            if match and not text:
                pending_bullet_x = ln.x0
                continue

            bulleted = bool(match) or pending_bullet_x is not None
            start_x = pending_bullet_x if pending_bullet_x is not None else ln.x0
            pending_bullet_x = None

            same_row = prev is not None and ln.y0 < prev.cy < ln.y1
            new_paragraph = (
                current is None
                or prev is None
                or bulleted
                or abs(ln.size - prev.size) > 1.0
                or (not same_row and ln.y0 - prev.y1 > 0.5 * prev.size)
                or (not same_row and not wrapped(prev, text, ln.size))
                # only bullets have hanging indents: a further-indented line under plain text is a sub-item
                or (not same_row and not current["bulleted"] and ln.x0 > current["x"] + INDENT_TOLERANCE_PT)
            )

            if new_paragraph:
                current = {"level": 0, "text": text, "x": start_x, "bulleted": bulleted}
                paragraphs.append(current)
            elif current["text"].endswith("-") and len(current["text"]) > 1 \
                    and current["text"][-2].isalpha() and text[:1].islower():
                current["text"] = current["text"][:-1] + text  # word hyphenated across a line break
            else:
                current["text"] = f"{current['text']} {text}"
            prev = ln

        paragraphs = [p for p in paragraphs if p["text"]]
        if paragraphs:
            cls._assign_levels(paragraphs)
        return paragraphs

    # ---- tables

    def _tables(self, page, warnings: list[str]) -> tuple[list[tuple[tuple, list]], list[tuple]]:
        if not hasattr(page, "find_tables"):
            return [], []
        try:
            found = page.find_tables()
        except Exception as exc:
            warnings.append(f"table detection failed: {exc}")
            return [], []
        tables, rects = [], []
        for table in found.tables:
            rows = [[clean_text(cell or "") for cell in row] for row in table.extract()]
            rows = [row for row in rows if any(row)]
            if len(rows) < 2 or max(len(r) for r in rows) < 2:
                continue  # a single framed box is not a table
            rect = tuple(table.bbox)
            tables.append((rect, rows))
            rects.append(rect)
        return tables, rects

    # ---- pictures

    def _pictures(self, page, number: int, warnings: list[str]) -> list[tuple[float, float, dict]]:
        results: list[tuple[float, float, dict]] = []
        seen_on_page: set[int] = set()
        index = 0
        for info in page.get_images(full=True):
            xref, smask, width_px, height_px = info[0], info[1], info[2], info[3]
            if xref in seen_on_page or width_px < self.min_image_px or height_px < self.min_image_px:
                continue
            seen_on_page.add(xref)
            try:
                rects = page.get_image_rects(xref)
            except Exception:
                rects = []
            rects = [r for r in rects if r.is_valid and not r.is_empty and r.intersects(page.rect)]
            if not rects:
                continue  # listed in the page resources but never drawn
            try:
                data, ext = self._image_bytes(xref, smask)
            except Exception as exc:
                warnings.append(f"could not extract image xref {xref}: {exc}")
                continue
            index += 1
            relative = self._store(data, ext, number, index)
            rect = min(rects, key=lambda r: (r.y0, r.x0))
            results.append((rect.y0, rect.x0, {"kind": "picture", "file": relative}))
        return results

    def _image_bytes(self, xref: int, smask: int) -> tuple[bytes, str]:
        if not smask:
            info = self.doc.extract_image(xref)
            if info and info.get("image") and info.get("ext", "").lower() in BROWSER_SAFE_EXT:
                return info["image"], info["ext"].lower()
        # transparency mask, CMYK, JPX/JBIG2 or other formats that viewers handle badly -> render to RGB(A) PNG
        pix = pymupdf.Pixmap(self.doc, xref)
        if pix.colorspace is not None and pix.colorspace.n not in (1, 3):
            pix = pymupdf.Pixmap(pymupdf.csRGB, pix)
        if smask:
            try:
                pix = pymupdf.Pixmap(pix, pymupdf.Pixmap(self.doc, smask))
            except Exception:
                pass  # mask size or type mismatch: keep the opaque image rather than lose it
        return pix.tobytes("png"), "png"

    def _store(self, data: bytes, ext: str, number: int, index: int) -> str:
        digest = hashlib.sha1(data).hexdigest()
        if digest in self.written:
            return self.written[digest]
        filename = f"slide{number:0{self.width}d}_image{index:02d}.{ext}"
        self.asset_dir.mkdir(parents=True, exist_ok=True)
        (self.asset_dir / filename).write_bytes(data)
        relative = f"{self.asset_prefix}/{filename}"
        self.written[digest] = relative
        return relative

    # ---- notes

    @staticmethod
    def _notes(page) -> str:
        notes = []
        for annot in page.annots() or []:
            content = clean_text((annot.info or {}).get("content", "") or "")
            if content:
                notes.append(content)
        return "\n".join(notes)


def convert_pdf(path: Path, out_dir: Path, stem: str, min_image_px: int) -> tuple[int, int]:
    require(pymupdf, "PDF", "pymupdf")
    doc = pymupdf.open(str(path))
    try:
        if doc.needs_pass and not doc.authenticate(""):
            raise RuntimeError("PDF is password-protected")
        if doc.page_count == 0:
            raise RuntimeError("PDF has no pages")
        width = max(2, len(str(doc.page_count)))
        extractor = PdfDeckExtractor(doc, path.name, width, out_dir / "assets" / stem, f"assets/{stem}", min_image_px)

        page_failures = 0
        for index in range(doc.page_count):
            number = index + 1
            try:
                data = extractor.build_page(doc.load_page(index), number)
            except Exception as exc:
                page_failures += 1
                log.error("%s page %d: %s", path.name, number, exc)
                data = slide_record(path.name, "pdf", number, "", [], "", [f"extraction failed: {exc}"])
            write_json(out_dir / f"{stem}_slide{number:0{width}d}.json", data)
        return doc.page_count, page_failures
    finally:
        doc.close()


# --------------------------------------------------------------------------- XML / JSON code blocks

XML_LANGUAGES = {"xml", "xsd", "wsdl", "xslt", "xhtml", "rss", "atom"}
JSON_LANGUAGES = {"json", "jsonc", "json5", "geojson"}
WRAPPER_TAG = "md_snippet_root"
BARE_AMPERSAND = re.compile(r"&(?!#\d+;|#x[0-9a-fA-F]+;|[A-Za-z][\w.-]*;)")
XML_DECLARATION = re.compile(r"^\s*<\?xml[^>]*\?>", re.IGNORECASE)
USED_PREFIX = re.compile(r"</?([A-Za-z_][\w.-]*):[A-Za-z_]")
ATTR_PREFIX = re.compile(r"\s([A-Za-z_][\w.-]*):[A-Za-z_][\w.-]*\s*=")
DECLARED_PREFIX = re.compile(r"xmlns:([A-Za-z_][\w.-]*)\s*=")


def _qualified(tag: str, prefixes: dict[str, str]) -> str:
    """'{urn:undeclared:cps}content' -> 'cps:content'; the default namespace drops away."""
    if tag.startswith("{"):
        uri, local = tag[1:].split("}", 1)
        prefix = prefixes.get(uri, "")
        return f"{prefix}:{local}" if prefix else local
    return tag


def _element_value(element: ET.Element, prefixes: dict[str, str]) -> Any:
    result: dict[str, Any] = {}
    repeated: set[str] = set()
    for name, value in element.attrib.items():
        result[f"@{_qualified(name, prefixes)}"] = clean_text(value)
    for child in element:
        if not isinstance(child.tag, str):  # comments / processing instructions
            continue
        key = _qualified(child.tag, prefixes)
        value = _element_value(child, prefixes)
        if key in result:
            if key not in repeated:
                result[key] = [result[key]]
                repeated.add(key)
            result[key].append(value)
        else:
            result[key] = value
    pieces = [element.text] + [child.tail for child in element]
    text = clean_text(" ".join(p for p in pieces if p and p.strip()))
    if not result:
        return text or None
    if text:
        result["#text"] = text
    return result


def xml_to_json(source: str) -> Any:
    """Convert an XML snippet from documentation into JSON-ready data.

    Documentation snippets are rarely well-formed documents: they use prefixes such as cps: without
    declaring them, contain several top-level elements, or a bare "&". All three are repaired.
    DOCTYPE / ENTITY declarations are refused, so a snippet can never expand entities."""
    if re.search(r"<!(DOCTYPE|ENTITY)", source, re.IGNORECASE):
        raise ValueError("DOCTYPE / ENTITY declarations are not allowed")
    body = BARE_AMPERSAND.sub("&amp;", XML_DECLARATION.sub("", source.strip()))
    used = (set(USED_PREFIX.findall(body)) | set(ATTR_PREFIX.findall(body))) - {"xml", "xmlns"}
    missing = sorted(used - set(DECLARED_PREFIX.findall(body)))
    declarations = "".join(f' xmlns:{p}="urn:undeclared:{p}"' for p in missing)
    wrapped = f"<{WRAPPER_TAG}{declarations}>{body}</{WRAPPER_TAG}>"

    prefixes: dict[str, str] = {}
    events = ET.iterparse(io.BytesIO(wrapped.encode("utf-8")), events=("start-ns",))
    for _, (prefix, uri) in events:
        prefixes.setdefault(uri, prefix)
    value = _element_value(events.root, prefixes)
    if not isinstance(value, dict) or set(value) == {"#text"}:
        raise ValueError("no XML elements found")
    return value


def looks_like_xml(text: str) -> bool:
    stripped = text.strip()
    return stripped.startswith("<") and stripped.endswith(">") and not stripped.lower().startswith("<!doctype html")


def looks_like_json(text: str) -> bool:
    stripped = text.strip()
    return (stripped[:1], stripped[-1:]) in (("{", "}"), ("[", "]"))


def code_data(language: str, text: str, warnings: list[str], where: str) -> tuple[bool, Any]:
    """(parsed?, data) for a code block. Labelled XML/JSON that fails to parse is reported;
    unlabelled blocks are only tried and left as plain code when they do not parse."""
    lang = language.lower()
    if lang in XML_LANGUAGES or (not lang and looks_like_xml(text)):
        try:
            return True, xml_to_json(text)
        except (ET.ParseError, ValueError) as exc:
            if lang:
                warnings.append(f"{where}: XML block kept as text, it does not parse ({exc})")
    elif lang in JSON_LANGUAGES or (not lang and looks_like_json(text)):
        try:
            return True, json.loads(text)
        except json.JSONDecodeError as exc:
            if lang:
                warnings.append(f"{where}: JSON block kept as text, it does not parse ({exc.msg}, line {exc.lineno})")
    return False, None


# --------------------------------------------------------------------------- Markdown

FENCE_RE = re.compile(r"^([ \t]*)(`{3,}|~{3,})[ \t]*([^\s`{]*)")
ATX_RE = re.compile(r"^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$")
SETEXT_RE = re.compile(r"^ {0,3}(=+|-+)[ \t]*$")
HR_RE = re.compile(r"^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$")
LIST_RE = re.compile(r"^([ \t]*)([-*+]|\d{1,9}[.)])(?:[ \t]+(.*)|[ \t]*$)")
TABLE_SEP_RE = re.compile(r"^[ \t]*\|?[ \t]*:?-+:?[ \t]*(\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$")
QUOTE_RE = re.compile(r"^ {0,3}>[ ]?(.*)$")
REF_DEF_RE = re.compile(r"^ {0,3}\[([^\]]+)\]:[ \t]*<?(\S+?)>?(?:[ \t]+[\"'(].*[\"')])?[ \t]*$")
IMAGE_RE = re.compile(r"!\[([^\]]*)\]\(\s*<?([^)\s>]+)>?(?:\s+[\"'][^\"']*[\"'])?\s*\)")
LINK_RE = re.compile(r"(?<!!)\[([^\]]+)\]\(\s*<?([^)\s>]*)>?(?:\s+[\"'][^\"']*[\"'])?\s*\)")
REF_LINK_RE = re.compile(r"(?<!!)\[([^\]]+)\]\[([^\]]*)\]")
AUTOLINK_RE = re.compile(r"<((?:https?|ftp|mailto):[^>\s]+)>")
CODE_SPAN_RE = re.compile(r"(`+)(.+?)\1", re.DOTALL)
ESCAPE_RE = re.compile(r"\\([\\`*_{}\[\]()#+\-.!|<>~])")
EMPHASIS_RES = (
    re.compile(r"\*\*(?=\S)(.+?)(?<=\S)\*\*"),
    re.compile(r"(?<!\w)__(?=\S)(.+?)(?<=\S)__(?!\w)"),
    re.compile(r"(?<![\w*])\*(?=\S)(.+?)(?<=\S)\*(?![\w*])"),
    re.compile(r"(?<!\w)_(?=\S)(.+?)(?<=\S)_(?!\w)"),  # snake_case words stay intact
    re.compile(r"~~(?=\S)(.+?)(?<=\S)~~"),
)
LINE_BREAK_TAG = re.compile(r"<br\s*/?>", re.IGNORECASE)
IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".bmp", ".tif", ".tiff"}
TAB_WIDTH = 4


def expand_indent(line: str) -> int:
    width = 0
    for char in line:
        if char == " ":
            width += 1
        elif char == "\t":
            width += TAB_WIDTH - width % TAB_WIDTH
        else:
            break
    return width


def read_text_file(path: Path, warnings: list[str]) -> str:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8-sig")
    except UnicodeDecodeError:
        try:
            text = raw.decode("cp1257")  # Baltic Windows code page, the usual non-UTF-8 Latvian encoding
            warnings.append("file is not UTF-8; read as Windows-1257 (Baltic)")
        except UnicodeDecodeError:
            text = raw.decode("latin-1")
            warnings.append("file is not UTF-8; read as Latin-1, some letters may be wrong")
    return text.replace("\r\n", "\n").replace("\r", "\n")


@dataclass
class MdSection:
    title: str
    shapes: list[dict]
    links: list[dict]
    warnings: list[str]


class MarkdownExtractor:
    """A small CommonMark-style block parser: enough for documentation pages, no dependencies."""

    def __init__(self, path: Path, asset_dir: Path, asset_prefix: str, warnings: list[str]):
        self.path = path
        self.width = 2
        self.asset_dir = asset_dir
        self.asset_prefix = asset_prefix
        self.warnings = warnings  # file-level: encoding, unclosed code blocks
        self.references: dict[str, str] = {}
        self.copied: dict[Path, str] = {}
        self.image_count = 0

    # ---- inline

    def inline(self, text: str, links: list[dict], images: list[dict] | None = None) -> str:
        protected: list[str] = []

        def protect(value: str) -> str:
            protected.append(value)
            return f"{len(protected) - 1}"

        text = CODE_SPAN_RE.sub(lambda m: protect(m.group(2).strip()), text)
        text = ESCAPE_RE.sub(lambda m: protect(m.group(1)), text)

        def image(m: re.Match) -> str:
            if images is not None:
                images.append({"alt": clean_text(m.group(1)), "src": m.group(2)})
            return m.group(1)

        def link(m: re.Match) -> str:
            label = m.group(1)
            if m.group(2):
                links.append({"text": clean_text(label), "url": m.group(2)})
            return label

        def reference(m: re.Match) -> str:
            label, key = m.group(1), (m.group(2) or m.group(1)).casefold()
            if key in self.references:
                links.append({"text": clean_text(label), "url": self.references[key]})
            return label

        def autolink(m: re.Match) -> str:
            links.append({"text": m.group(1), "url": m.group(1)})
            return protect(m.group(1))

        text = IMAGE_RE.sub(image, text)
        text = LINK_RE.sub(link, text)
        text = REF_LINK_RE.sub(reference, text)
        text = AUTOLINK_RE.sub(autolink, text)
        for pattern in EMPHASIS_RES:
            text = pattern.sub(r"\1", text)
        text = LINE_BREAK_TAG.sub(" ", text)
        text = html.unescape(text)  # &lt;cps:content&gt; -> <cps:content>; code spans stay literal
        text = re.sub("(\\d+)", lambda m: protected[int(m.group(1))], text)
        return clean_text(text)

    # ---- pictures

    def picture(self, number: int, image: dict, warnings: list[str]) -> dict:
        src = image["src"]
        item: dict[str, Any] = {"kind": "picture", "file": src}
        if image.get("alt"):
            item["alt"] = image["alt"]
        parsed = urllib.parse.urlparse(src)
        if parsed.scheme or src.startswith("//"):
            return item  # remote image: keep the URL
        local = (self.path.parent / urllib.parse.unquote(parsed.path)).resolve()
        if local.suffix.lower() not in IMAGE_SUFFIXES or not local.is_file():
            warnings.append(f"image not found next to the Markdown file: {src}")
            return item
        if local not in self.copied:
            self.image_count += 1
            filename = f"slide{number:0{self.width}d}_image{self.image_count:02d}{local.suffix.lower()}"
            self.asset_dir.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(local, self.asset_dir / filename)
            self.copied[local] = f"{self.asset_prefix}/{filename}"
        item["file"] = self.copied[local]
        return item

    # ---- blocks

    @staticmethod
    def front_matter(lines: list[str]) -> tuple[dict[str, str], list[str]]:
        if not lines or lines[0].strip() != "---":
            return {}, lines
        for end in range(1, min(len(lines), 200)):
            if lines[end].strip() in ("---", "..."):
                meta: dict[str, str] = {}
                for line in lines[1:end]:
                    match = re.match(r"^([A-Za-z_][\w-]*)\s*:\s*(.*?)\s*$", line)
                    if match and match.group(2):
                        meta[match.group(1)] = clean_text(match.group(2).strip("'\""))
                return meta, lines[end + 1:]
        return {}, lines

    def strip_comments_and_refs(self, lines: list[str]) -> list[str]:
        out: list[str] = []
        in_fence: str | None = None
        in_comment = False
        for line in lines:
            fence = FENCE_RE.match(line)
            if in_fence is None and not in_comment and fence:
                in_fence = fence.group(2)[0] * len(fence.group(2))
            elif in_fence is not None:
                if line.strip().startswith(in_fence) and set(line.strip()) == {in_fence[0]}:
                    in_fence = None
            elif in_comment or line.lstrip().startswith("<!--"):
                in_comment = "-->" not in line
                continue
            elif REF_DEF_RE.match(line):
                match = REF_DEF_RE.match(line)
                self.references[match.group(1).casefold()] = match.group(2)
                continue
            out.append(line)
        return out

    def blocks(self, lines: list[str]) -> list[dict]:
        out: list[dict] = []
        i, n = 0, len(lines)
        previous_was_list = False

        def starts_block(line: str) -> bool:
            if FENCE_RE.match(line) or ATX_RE.match(line) or QUOTE_RE.match(line) or HR_RE.match(line):
                return True
            item = LIST_RE.match(line)
            # only bullets and "1." may interrupt a paragraph, so "25. novembrī ..." stays text
            return bool(item) and (item.group(2)[0] in "-*+" or item.group(2)[:-1] == "1") and bool(item.group(3))

        while i < n:
            line = lines[i]
            if not line.strip():
                i += 1
                continue
            line_no = i + 1

            fence = FENCE_RE.match(line)
            if fence:
                indent, marker, language = expand_indent(fence.group(1)), fence.group(2), fence.group(3)
                body: list[str] = []
                i += 1
                closed = False
                while i < n:
                    stripped = lines[i].strip()
                    if stripped.startswith(marker[0] * len(marker)) and set(stripped) == {marker[0]}:
                        closed = True
                        i += 1
                        break
                    body.append(self._dedent(lines[i], indent))
                    i += 1
                if not closed:
                    self.warnings.append(f"line {line_no}: code block is never closed; it runs to the end of the file")
                out.append({"kind": "code", "language": language.lower(), "text": "\n".join(body).strip("\n"),
                            "line": line_no})
                previous_was_list = False
                continue

            heading = ATX_RE.match(line)
            if heading:
                out.append({"kind": "heading", "level": len(heading.group(1)), "raw": heading.group(2) or ""})
                i += 1
                previous_was_list = False
                continue

            if HR_RE.match(line):  # "- - -" is a rule, not a list (CommonMark)
                i += 1
                continue

            if "|" in line and i + 1 < n and "|" in lines[i + 1] and TABLE_SEP_RE.match(lines[i + 1]) and "-" in lines[i + 1]:
                rows = [self._cells(line)]
                i += 2
                while i < n and lines[i].strip() and "|" in lines[i]:
                    rows.append(self._cells(lines[i]))
                    i += 1
                out.append({"kind": "table", "raw_rows": rows})
                previous_was_list = False
                continue

            list_item = LIST_RE.match(line)
            if list_item and list_item.group(3) is not None:
                start = i
                items, i = self._list(lines, i)
                lone = items[0] if len(items) == 1 else None
                if lone and lone["marker"][0].isdigit() and lone["marker"][:-1] != "1" \
                        and (i >= n or not FENCE_RE.match(lines[i])):
                    # a single "25. novembrī ..." item is a Latvian date opening a sentence, not a list
                    out.append({"kind": "paragraph", "raw": f"{lone['marker']} {lone['raw']}"})
                    previous_was_list = False
                    continue
                if not items:  # defensive: never loop on a line the list parser did not consume
                    i = start + 1
                    continue
                out.append({"kind": "list", "items": items})
                previous_was_list = True
                continue

            if QUOTE_RE.match(line):
                quoted: list[str] = []
                while i < n and lines[i].strip() and (QUOTE_RE.match(lines[i]) or not starts_block(lines[i])):
                    match = QUOTE_RE.match(lines[i])
                    quoted.append(match.group(1) if match else lines[i])
                    i += 1
                out.extend(dict(block, quote=True) if block["kind"] == "paragraph" else block
                           for block in self.blocks(quoted))
                previous_was_list = False
                continue

            if expand_indent(line) >= 4 and not previous_was_list:
                body = []
                while i < n and (not lines[i].strip() or expand_indent(lines[i]) >= 4):
                    body.append(self._dedent(lines[i], 4))
                    i += 1
                out.append({"kind": "code", "language": "", "text": "\n".join(body).strip("\n"), "line": line_no})
                continue

            paragraph = [line.strip()]
            i += 1
            while i < n and lines[i].strip():
                if SETEXT_RE.match(lines[i]):
                    level = 1 if lines[i].strip()[0] == "=" else 2
                    out.append({"kind": "heading", "level": level, "raw": " ".join(paragraph)})
                    paragraph = []
                    i += 1
                    break
                if starts_block(lines[i]) or ("|" in lines[i] and i + 1 < n and TABLE_SEP_RE.match(lines[i + 1])
                                              and "-" in lines[i + 1]):
                    break
                paragraph.append(lines[i].strip())
                i += 1
            if paragraph:
                out.append({"kind": "paragraph", "raw": " ".join(paragraph)})
            previous_was_list = False
        return out

    @staticmethod
    def _dedent(line: str, width: int) -> str:
        removed = 0
        position = 0
        while position < len(line) and removed < width and line[position] in " \t":
            removed += TAB_WIDTH - removed % TAB_WIDTH if line[position] == "\t" else 1
            position += 1
        return line[position:].rstrip()

    @staticmethod
    def _cells(line: str) -> list[str]:
        protected = re.sub(r"\\\|", "", line.strip())
        protected = CODE_SPAN_RE.sub(lambda m: m.group(0).replace("|", ""), protected)
        if protected.startswith("|"):
            protected = protected[1:]
        if protected.endswith("|"):
            protected = protected[:-1]
        return [cell.replace("", "|").strip() for cell in protected.split("|")]

    def _list(self, lines: list[str], i: int) -> tuple[list[dict], int]:
        items: list[dict] = []
        stack: list[int] = []
        n = len(lines)
        while i < n:
            line = lines[i]
            if not line.strip():
                ahead = i + 1
                while ahead < n and not lines[ahead].strip():
                    ahead += 1
                if ahead < n and (LIST_RE.match(lines[ahead]) or expand_indent(lines[ahead]) >= 2) \
                        and not FENCE_RE.match(lines[ahead]):
                    i = ahead
                    continue
                break
            if FENCE_RE.match(line) or ATX_RE.match(line):
                break  # code inside a list item becomes its own code shape, in order
            item = LIST_RE.match(line)
            if item and item.group(3) is not None:
                indent = expand_indent(item.group(1))
                while stack and indent < stack[-1]:
                    stack.pop()
                if not stack or indent > stack[-1] + 1:
                    stack.append(indent)
                items.append({"level": min(len(stack) - 1, MAX_LEVEL), "marker": item.group(2), "raw": item.group(3)})
            elif items and (expand_indent(line) >= 2 or not (HR_RE.match(line) or QUOTE_RE.match(line))):
                items[-1]["raw"] = f"{items[-1]['raw']} {line.strip()}"  # continuation / lazy line
            else:
                break
            i += 1
        return items, i

    # ---- document

    def sections(self, text: str, split_level: int) -> tuple[dict[str, str], list[MdSection]]:
        meta, lines = self.front_matter(text.split("\n"))
        blocks = self.blocks(self.strip_comments_and_refs(lines))

        # title: front matter "title", else the first level-1 heading, else a heading that opens the
        # file, else the file name. A heading used as the title is not repeated in the shapes.
        doc_title_block = None
        doc_title = meta.get("title", "")
        if not doc_title:
            headings = [b for b in blocks if b["kind"] == "heading"]
            doc_title_block = next((b for b in headings if b["level"] == 1), None)
            if doc_title_block is None and blocks and blocks[0]["kind"] == "heading":
                doc_title_block = blocks[0]
            if doc_title_block is not None:
                doc_title = self.inline(doc_title_block["raw"], [])
        doc_title = doc_title or clean_text(self.path.stem.replace("_", " "))

        sections: list[tuple[str, list[dict]]] = []
        current_title, current = doc_title, []
        for block in blocks:
            if block is doc_title_block:
                continue
            if split_level and block["kind"] == "heading" and block["level"] <= split_level:
                if current or current_title != doc_title:  # an empty preface before the first heading is dropped
                    sections.append((current_title, current))
                current_title, current = self.inline(block["raw"], []), []
                continue
            current.append(block)
        if current or current_title != doc_title or not sections:
            sections.append((current_title, current))

        self.width = max(2, len(str(len(sections))))  # picture names use the same numbering as the JSON files
        result = []
        for number, (title, section_blocks) in enumerate(sections, 1):
            result.append(self._render(number, title, section_blocks))
        return meta, result

    def _render(self, number: int, title: str, blocks: list[dict]) -> MdSection:
        shapes: list[dict] = []
        links: list[dict] = []
        warnings: list[str] = []
        counters: dict[str, int] = {}

        def add(item: dict) -> None:
            counters[item["kind"]] = counters.get(item["kind"], 0) + 1
            shapes.append({"name": f"{item['kind'].title()} {counters[item['kind']]}", **item})

        for block in blocks:
            kind = block["kind"]
            if kind == "heading":
                text = self.inline(block["raw"], links)
                if text:
                    add({"kind": "heading", "level": block["level"], "text": text})
            elif kind == "paragraph":
                images: list[dict] = []
                text = self.inline(block["raw"], links, images)
                only_images = images and text == clean_text(" ".join(img["alt"] for img in images))
                if text and not only_images:
                    item: dict[str, Any] = {"kind": "text", "paragraphs": [{"level": 0, "text": text}]}
                    if block.get("quote"):
                        item["quote"] = True
                    add(item)
                for image in images:
                    add(self.picture(number, image, warnings))
            elif kind == "list":
                paragraphs = []
                for entry in block["items"]:
                    text = self.inline(entry["raw"], links)
                    if text:
                        paragraphs.append({"level": entry["level"], "marker": entry["marker"], "text": text})
                if paragraphs:
                    add({"kind": "text", "paragraphs": paragraphs})
            elif kind == "table":
                rows = [[self.inline(cell, links) for cell in row] for row in block["raw_rows"]]
                width = max(len(r) for r in rows)
                add({"kind": "table", "rows": [r + [""] * (width - len(r)) for r in rows]})
            elif kind == "code":
                item = {"kind": "code", "language": block["language"], "text": block["text"]}
                parsed, data = code_data(block["language"], block["text"], warnings, f"line {block['line']}")
                if parsed:
                    item["data"] = data
                if block["text"]:
                    add(item)

        unique_links = list({(link["text"], link["url"]): link for link in links}.values())
        return MdSection(title, shapes, unique_links, warnings)


def convert_markdown(path: Path, out_dir: Path, stem: str, split_level: int) -> tuple[int, int]:
    file_warnings: list[str] = []
    text = read_text_file(path, file_warnings)
    if not text.strip():
        raise RuntimeError("Markdown file is empty")

    extractor = MarkdownExtractor(path, out_dir / "assets" / stem, f"assets/{stem}", file_warnings)
    meta, sections = extractor.sections(text, split_level)

    for number, section in enumerate(sections, 1):
        warnings = file_warnings + section.warnings
        if not section.shapes:
            warnings.append("section has a heading but no content")
        data = slide_record(path.name, "md", number, section.title, section.shapes, "", warnings)
        if section.links:
            data["links"] = section.links
        if meta:
            data["metadata"] = meta
        write_json(out_dir / f"{stem}_slide{number:0{extractor.width}d}.json", data)
    return len(sections), 0


# --------------------------------------------------------------------------- CLI

def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("input", type=Path, help="a PDF / PowerPoint file, or a folder of them")
    parser.add_argument("output_dir", type=Path, nargs="?", default=None,
                        help="where to write the JSON (default: slides_json next to the input)")
    parser.add_argument("--min-image-px", type=int, default=DEFAULT_MIN_IMAGE_PX,
                        help=f"skip PDF images smaller than this in either dimension (default {DEFAULT_MIN_IMAGE_PX})")
    parser.add_argument("--md-split-level", type=int, default=0, choices=range(0, 7), metavar="N",
                        help="Markdown: start a new JSON file at every heading of level 1..N "
                             "(default 0 = one JSON per file)")
    parser.add_argument("--verbose", action="store_true", help="log debug details")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO, format="%(levelname)s: %(message)s")
    if pymupdf is not None:
        pymupdf.TOOLS.mupdf_display_errors(False)  # MuPDF warnings go to the per-page "warnings" instead of stderr

    source = args.input.expanduser().resolve()
    if source.is_file():
        files = [source]  # a single file is converted whatever its extension; sniff() decides what it is
        default_out = source.parent / "slides_json"
    elif source.is_dir():
        files = sorted(
            p for p in source.iterdir()
            if p.is_file()
            and p.suffix.lower() in CANDIDATE_SUFFIXES
            and not p.name.startswith(("~$", "."))
        )
        default_out = source / "slides_json"
    else:
        log.error("Input not found: %s", source)
        return 2
    out_dir = (args.output_dir or default_out).expanduser().resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    used_stems: set[str] = set()
    converted_decks = total_slides = failures = 0

    for path in files:
        kind = sniff(path)
        if kind is None:
            log.warning("Skipping %s: not a PDF, PowerPoint or Markdown file", path.name)
            continue

        stem = safe_name(path.stem)
        unique, n = stem, 2
        while unique.casefold() in used_stems:  # case-insensitive file systems (Windows, macOS)
            unique = f"{stem}_{n}"
            n += 1
        used_stems.add(unique.casefold())

        try:
            if kind == "pdf":
                count, page_failures = convert_pdf(path, out_dir, unique, args.min_image_px)
            elif kind == "md":
                count, page_failures = convert_markdown(path, out_dir, unique, args.md_split_level)
            else:
                count, page_failures = convert_pptx(path, kind, out_dir, unique)
            suffix = f" ({page_failures} pages failed)" if page_failures else ""
            log.info("%s: %d pages%s", path.name, count, suffix)
            converted_decks += 1
            total_slides += count
            failures += page_failures
        except Exception as exc:
            failures += 1
            log.error("Failed %s: %s", path.name, exc)

    print(f"Done: {converted_decks} files, {total_slides} pages, {failures} failures -> {out_dir}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())