#!/usr/bin/env python3
"""Split every PDF and PowerPoint file in a folder into one JSON file per page/slide.

Usage:
    python deck_to_slide_json.py INPUT [OUTPUT_DIR] [--min-image-px N] [--verbose]

    INPUT is one file (~/Downloads/the_file.pdf) or a folder of files.
    OUTPUT_DIR defaults to a "slides_json" folder next to the input file (or inside the input folder).

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
      "notes": "...",        # speaker notes (PowerPoint) or comment annotations (PDF)
      "warnings": ["..."]
    }

Requirements: Python 3.10+.
    PDF files:         pip install pymupdf        (1.23 or newer)
    .pptx / .potx:     pip install python-pptx
    legacy .ppt:       python-pptx plus LibreOffice (soffice on PATH)
Each library is only needed if the folder actually contains that kind of file.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import logging
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
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
CANDIDATE_SUFFIXES = {".pptx", ".ppt", ".potx", ".pdf"}

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


# --------------------------------------------------------------------------- CLI

def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("input", type=Path, help="a PDF / PowerPoint file, or a folder of them")
    parser.add_argument("output_dir", type=Path, nargs="?", default=None,
                        help="where to write the JSON (default: slides_json next to the input)")
    parser.add_argument("--min-image-px", type=int, default=DEFAULT_MIN_IMAGE_PX,
                        help=f"skip PDF images smaller than this in either dimension (default {DEFAULT_MIN_IMAGE_PX})")
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
            log.warning("Skipping %s: not a PDF or PowerPoint file", path.name)
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