#!/usr/bin/env python3
"""Split every PowerPoint file in a folder into one JSON file per slide.

Usage:
    python pptx_to_slide_json.py INPUT_DIR [OUTPUT_DIR]

Example output for "Sistēmu modelēšana.pdf" (slide 2):
    OUTPUT_DIR/Sistēmu_modelēšana_slide02.json
    OUTPUT_DIR/assets/Sistēmu_modelēšana/slide02_image01.png   (only if the slide has pictures)

Requirements: Python 3.10+, python-pptx (pip install python-pptx).
Legacy .ppt files also need LibreOffice (soffice on PATH).
"""
from __future__ import annotations

import json
import logging
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from pptx import Presentation
from pptx.enum.shapes import MSO_SHAPE_TYPE

ZIP_MAGIC = b"PK\x03\x04"                      # .pptx / .potx
OLE_MAGIC = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"  # legacy .ppt
CANDIDATE_SUFFIXES = {".pptx", ".ppt", ".potx", ".pdf"}
ROW_BAND = 274320  # 0.3 inch in EMU: shapes with tops in the same band count as one row

log = logging.getLogger("pptx_to_slide_json")


def safe_name(text: str) -> str:
    cleaned = re.sub(r"[^\w\-]+", "_", text).strip("_")
    return cleaned or "deck"


def sniff(path: Path) -> str | None:
    with path.open("rb") as fh:
        head = fh.read(8)
    if head.startswith(ZIP_MAGIC):
        return "pptx"
    if head == OLE_MAGIC:
        return "ppt"
    return None


def ensure_pptx(path: Path, work_dir: Path, kind: str) -> Path:
    if kind == "pptx":
        return path
    staged = work_dir / f"{path.stem}.ppt"
    shutil.copyfile(path, staged)
    subprocess.run(
        ["soffice", "--headless", "--convert-to", "pptx", "--outdir", str(work_dir), str(staged)],
        check=True,
        capture_output=True,
        timeout=300,
    )
    converted = work_dir / f"{path.stem}.pptx"
    if not converted.exists():
        raise RuntimeError("LibreOffice did not produce a .pptx file")
    return converted


def reading_order(shapes) -> list:
    return sorted(shapes, key=lambda s: ((s.top or 0) // ROW_BAND, s.left or 0))


class SlideExtractor:
    def __init__(self, slide_number: int, asset_dir: Path, asset_prefix: str):
        self.slide_number = slide_number
        self.asset_dir = asset_dir
        self.asset_prefix = asset_prefix
        self.images: list[str] = []

    def extract_all(self, shapes) -> list[dict]:
        items = (self.extract(s) for s in reading_order(shapes))
        return [item for item in items if item]

    def extract(self, shape) -> dict | None:
        base = {"name": shape.name}

        if shape.shape_type == MSO_SHAPE_TYPE.GROUP:
            return {**base, "kind": "group", "children": self.extract_all(shape.shapes)}

        if hasattr(shape, "image"):
            return self._picture(shape, base)

        if getattr(shape, "has_table", False):
            rows = [[cell.text for cell in row.cells] for row in shape.table.rows]
            return {**base, "kind": "table", "rows": rows}

        if getattr(shape, "has_chart", False):
            chart = shape.chart
            return {
                **base,
                "kind": "chart",
                "categories": [str(c) for c in chart.plots[0].categories],
                "series": [{"name": s.name, "values": list(s.values)} for s in chart.series],
            }

        if shape.has_text_frame:
            paragraphs = [
                {"level": p.level, "text": p.text.strip()}
                for p in shape.text_frame.paragraphs
                if p.text.strip()
            ]
            if paragraphs:
                return {**base, "kind": "text", "paragraphs": paragraphs}

        return None

    def _picture(self, shape, base: dict) -> dict:
        image = shape.image
        filename = f"slide{self.slide_number:02d}_image{len(self.images) + 1:02d}.{image.ext}"
        self.asset_dir.mkdir(parents=True, exist_ok=True)
        (self.asset_dir / filename).write_bytes(image.blob)
        relative = f"{self.asset_prefix}/{filename}"
        self.images.append(relative)
        return {**base, "kind": "picture", "file": relative}


def build_slide(slide, slide_number: int, deck_name: str, extractor: SlideExtractor) -> dict:
    title_shape = slide.shapes.title
    title = ""
    if title_shape is not None and title_shape.has_text_frame:
        title = title_shape.text_frame.text.strip()

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

    return {
        "deck": deck_name,
        "slide_number": slide_number,
        "title": title,
        "shapes": shapes,
        "notes": notes,
    }


def convert_deck(path: Path, out_dir: Path, stem: str) -> int:
    kind = sniff(path)
    asset_dir = out_dir / "assets" / stem
    with tempfile.TemporaryDirectory() as tmp:
        pptx_path = ensure_pptx(path, Path(tmp), kind)
        prs = Presentation(str(pptx_path))
        slides = list(prs.slides)

    width = max(2, len(str(len(slides))))
    with tempfile.TemporaryDirectory():
        pass  # presentation is fully loaded above; the temp file is no longer needed

    for number, slide in enumerate(slides, 1):
        extractor = SlideExtractor(number, asset_dir, f"assets/{stem}")
        data = build_slide(slide, number, path.name, extractor)
        target = out_dir / f"{stem}_slide{number:0{width}d}.json"
        target.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    return len(slides)


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2

    in_dir = Path(sys.argv[1]).expanduser().resolve()
    out_dir = Path(sys.argv[2]).expanduser().resolve() if len(sys.argv) > 2 else Path("slides_json").resolve()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")
    out_dir.mkdir(parents=True, exist_ok=True)

    files = sorted(
        p for p in in_dir.iterdir()
        if p.is_file()
        and p.suffix.lower() in CANDIDATE_SUFFIXES
        and not p.name.startswith(("~$", "."))
    )

    used_stems: set[str] = set()
    converted_decks = total_slides = failures = 0

    for path in files:
        if sniff(path) is None:
            log.warning("Skipping %s: not a PowerPoint file", path.name)
            continue

        stem = safe_name(path.stem)
        unique, n = stem, 2
        while unique in used_stems:
            unique = f"{stem}_{n}"
            n += 1
        used_stems.add(unique)

        try:
            count = convert_deck(path, out_dir, unique)
            log.info("%s: %d slides", path.name, count)
            converted_decks += 1
            total_slides += count
        except Exception as exc:
            failures += 1
            log.error("Failed %s: %s", path.name, exc)

    print(f"Done: {converted_decks} decks, {total_slides} slides, {failures} failures -> {out_dir}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())