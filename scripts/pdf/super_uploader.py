#!/usr/bin/env python3
"""Upload slide JSON files (made by pdf_parse.py) to corelamo.

Pass 1 - scan. Every JSON file becomes one document per slide. The uploader then walks every
document, nested objects and arrays included, and records which fields exist, how often and with
which type. The policy is generated from that scan:
  * a field that never occurs is not declared;
  * known slide fields get tuned settings (FIELD_POLICY), any other field a default for its type;
  * a field whose type the policy cannot express (arrays of arrays, arrays that mix objects and
    values, only empty arrays or nulls) is left out of the policy, so the server stores it unindexed;
  * a field seen with mixed value types (e.g. 3 and "3") is declared as text and converted.

Pass 2 - upload. Create and start the database, set the policy, upsert in batches, then wait until
the server has embedded the new slides.

Each slide document:
    _id            JSON file name without .json   e.g. Sistemu_modelesana_slide017 (always unique)
    deck           deck file name without extension, exact - for filters
    slide_number   int - for filters / sort
    title          slide title
    content        title + text + tables + charts + notes as sentences; "$vector": embedded
    notes          speaker notes (PowerPoint) or comments (PDF)
    source_format  "pdf" | "pptx" | "md"
    pictures       [picture asset paths]          stored, not ranked
    warnings       [parser warnings]              stored, not ranked
    page_image     legacy parser field            stored, not ranked
    ...            any other top-level field of the JSON files, typed by the scan

Usage:
    python3 upload_slides.py ~/Downloads/slides_json
    python3 upload_slides.py ~/Downloads/slides_json --dry-run        # scan and show the policy only
    python3 upload_slides.py ~/Downloads/slides_json --db slaidi --shards 1 --no-wait

Server from CORELAMO_URL (default http://localhost:6006); login from CORELAMO_USER / CORELAMO_PASS
(default admin / secret) - a server running without auth is used without a key.
Python 3.9+, standard library only.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
import re
import sys
import time
import unicodedata
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path
from typing import Any

BASE_URL = os.environ.get("CORELAMO_URL", "http://localhost:6006").rstrip("/")
USERNAME = os.environ.get("CORELAMO_USER", "admin")
PASSWORD = os.environ.get("CORELAMO_PASS", "secret")

DEFAULT_BATCH = 200
POLL_SECONDS = 10
RECOUNT_GRACE_SECONDS = 75          # segments flush ~2 s after the last write, the worker re-counts once a minute
STATUS_MISSES_ALLOWED = 5           # consecutive failed status polls before giving up
RETRIES = 5                         # attempts per request for network errors and RETRY_STATUSES
RETRY_STATUSES = {408, 425, 429, 500, 502, 503, 504}
ID_TYPE = "id"                      # we send our own _id values, so a re-run replaces the same slides

EXIT_OK, EXIT_UPLOAD_FAILED, EXIT_BAD_INPUT, EXIT_WAIT_FAILED = 0, 1, 2, 3

# ---------------------------------------------------------------------------------------- policy

# No "stemming": corelamo has no Latvian stemmer and the English one damages Latvian words; the
# vector field covers meaning instead. ascii_folding lets "modelesana" find "modelēšana", because
# people often type Latvian without diacritics.
STOPWORDS_LV = [
    "un", "ir", "ar", "no", "uz", "par", "pie", "pēc", "līdz", "starp", "caur", "bez", "aiz", "zem",
    "virs", "priekš", "pa", "kas", "ka", "kā", "vai", "bet", "arī", "lai", "jo", "ja", "tad", "kad",
    "kur", "gan", "vēl", "jau", "tikai", "ne", "nav", "būt", "bija", "būs", "tas", "tā", "to", "tie",
    "tās", "šis", "šī", "šo", "šie", "šīs", "tik", "cik", "kurš", "kura", "kuri", "kuras", "savu", "savs",
]
STOPWORDS_EN = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "if", "in", "into", "is", "it", "of",
    "on", "or", "the", "this", "that", "to", "was", "with", "from",
]

FIELD_POLICY: dict[str, Any] = {
    "_id": {"$type": ID_TYPE, "$searchable": True, "$weight": {"min": 90, "max": 95}},
    "deck": {"$type": "text", "$analyzer": "label", "$exact": True, "$weight": {"min": 1, "max": 20}},
    "slide_number": {"$type": "int", "$searchable": True, "$weight": {"min": 1, "max": 10}},
    "title": {"$type": "text", "$analyzer": "label", "$exact": True, "$weight": {"min": 40, "max": 80}},
    "content": {"$type": "text", "$vector": True, "$weight": {"min": 1, "max": 100}},
    "notes": {"$type": "text", "$weight": {"min": 1, "max": 30}},
    "source_format": {"$type": "text", "$analyzer": "label", "$exact": True, "$weight": {"min": 0, "max": 0}},
    "pictures[]": {"$type": "text", "$analyzer": "path", "$weight": {"min": 0, "max": 0}},
    "warnings[]": {"$type": "text", "$weight": {"min": 0, "max": 0}},
    "page_image": {"$type": "text", "$analyzer": "path", "$weight": {"min": 0, "max": 0}},
    "links[].url": {"$type": "text", "$analyzer": "path", "$weight": {"min": 0, "max": 0}},
    "links[].text": {"$type": "text", "$weight": {"min": 1, "max": 20}},
}

DEFAULT_POLICY: dict[str, Any] = {
    "text": {"$type": "text", "$weight": {"min": 10, "max": 30}},
    "int": {"$type": "int", "$searchable": True, "$weight": {"min": 1, "max": 10}},
    "float": {"$type": "float", "$searchable": True, "$weight": {"min": 1, "max": 10}},
    "bool": "bool",
}


def fold(word: str) -> str:
    return unicodedata.normalize("NFKD", word).encode("ascii", "ignore").decode("ascii")


def build_analyzers(stemming: str | None) -> dict:
    words = STOPWORDS_LV + STOPWORDS_EN
    default: dict[str, Any] = {
        "lowercase": True,
        "ascii_folding": True,
        # both spellings, whichever order the server applies folding and stopwords in
        "stopwords": sorted(set(words) | {fold(w) for w in words}),
    }
    if stemming:
        default["stemming"] = stemming
    return {
        "$default": default,
        "label": {"lowercase": True, "ascii_folding": True},  # titles and names: no stopwords
        "path": {"tokenizer": "url", "ascii_folding": True},  # file paths
    }


# ---------------------------------------------------------------------------------------- text

BULLET_LINE = re.compile(r"^[•\-–▪◦●]\s*$")
# a line that is only a number or a marker: the numbered circles and arrows of slide diagrams
NOISE_LINE = re.compile(r"^(\d{1,2}[.)]?|[↺→←↑↓►▶•\-–▪◦●])$")
SENTENCE_END = (".", "!", "?", ":", ";", "…")
SLIDE_SUFFIX = re.compile(r"_slide(\d+)$")
CONSUMED_KEYS = {"deck", "slide_number", "title", "shapes", "text_blocks", "notes", "warnings",
                 "source_format", "page_image"}


def nfc(text: str) -> str:
    """One Unicode form for Latvian letters: macOS file names often store "ā" as "a" + macron."""
    return unicodedata.normalize("NFC", text)


def clean_text(text: Any) -> str:
    if text is None:
        return ""
    return " ".join(nfc(str(text)).replace("­", "").split())


def clean_block(text: str) -> str:
    """Legacy text_blocks: one block as flowing text. Visual line breaks become spaces (a word split
    with a hyphen is joined back), lone numbers and markers are dropped."""
    out = ""
    for line in nfc(text).splitlines():
        line = " ".join(line.split())
        if not line or BULLET_LINE.match(line) or NOISE_LINE.match(line):
            continue
        line = line.lstrip("•▪◦●↺→► ").strip()
        if not line:
            continue
        if not out:
            out = line
        elif out.endswith("-") and line[:1].islower():
            out = out[:-1] + line
        elif line[:1].isupper() and not out.endswith(SENTENCE_END + ("—", "–", "(", "/", ",")):
            out = f"{out}. {line}"  # a wrapped line continues in lowercase; a capital starts a new item
        else:
            out = f"{out} {line}"
    return out


def join_blocks(parts: list[str]) -> str:
    """Parts become sentences of one paragraph: a part that does not end like a sentence gets a full
    stop, so headings and list items do not run into each other."""
    merged: list[str] = []
    for part in parts:
        # a lowercase tail like "perspective)" is the end of a box the PDF split in two:
        # it belongs to the earliest part still waiting for its ")"
        if part[:1].islower() and part.count(")") > part.count("("):
            owner = next((i for i, p in enumerate(merged) if p.count("(") > p.count(")")), None)
            if owner is not None:
                merged[owner] = f"{merged[owner]} {part}"
                continue
        merged.append(part)
    return " ".join(p if p.endswith(SENTENCE_END) else f"{p}." for p in merged)


def table_sentences(rows: list) -> list[str]:
    """A table as sentences an embedding model understands: "Header: value; Header: value"."""
    cleaned = [[clean_text(c) for c in row] for row in rows if isinstance(row, list)]
    cleaned = [row for row in cleaned if any(row)]
    if not cleaned:
        return []
    header = cleaned[0]
    if len(cleaned) >= 2 and all(header) and len(set(header)) == len(header):
        out = []
        for row in cleaned[1:]:
            pairs = [f"{h}: {v}" for h, v in zip(header, row) if v]
            if pairs:
                out.append("; ".join(pairs))
        return out
    return ["; ".join(c for c in row if c) for row in cleaned]


def chart_sentences(shape: dict) -> list[str]:
    categories = [clean_text(c) for c in shape.get("categories") or []]
    out = []
    for series in shape.get("series") or []:
        name = clean_text(series.get("name")) or "Series"
        values = series.get("values") or []
        if categories and len(categories) == len(values):
            pairs = ", ".join(f"{c} {v}" for c, v in zip(categories, values))
        else:
            pairs = ", ".join(str(v) for v in values)
        if pairs:
            out.append(f"{name}: {pairs}")
    return out


def data_sentences(data: Any, path: tuple[str, ...] = ()) -> list[str]:
    """Converted XML/JSON as "path: value" sentences, e.g. "index status: index state: normal".
    A single wrapper element such as cps:content is left out of the path."""
    if not path and isinstance(data, dict) and len(data) == 1:
        (only,) = data.values()
        if isinstance(only, (dict, list)):
            data = only
    out: list[str] = []
    if isinstance(data, dict):
        for key, value in data.items():
            label = str(key).lstrip("@")
            out.extend(data_sentences(value, path if key == "#text" else path + (label,)))
    elif isinstance(data, list):
        for value in data:
            out.extend(data_sentences(value, path))
    elif data is not None:
        value = clean_text(json.dumps(data) if isinstance(data, bool) else data)
        if value:
            label = clean_text(" ".join(path))
            out.append(f"{label}: {value}" if label else value)
    return out


def flatten_shapes(shapes: list, parts: list[str], pictures: list[str]) -> None:
    for shape in shapes:
        if not isinstance(shape, dict):
            continue
        kind = shape.get("kind")
        if kind == "heading":
            text = clean_text(shape.get("text"))
            if text:
                parts.append(text)
        elif kind == "code":
            # parsed XML/JSON reads better as sentences; other code goes in as written (names are searchable)
            if shape.get("data") is not None:
                parts.extend(data_sentences(shape["data"]))
            else:
                code = clean_text(shape.get("text"))
                if code:
                    parts.append(code)
        elif kind == "text":
            for p in shape.get("paragraphs") or []:
                text = clean_text(p.get("text") if isinstance(p, dict) else p)
                if text and not NOISE_LINE.match(text):
                    parts.append(text)
        elif kind == "table":
            parts.extend(table_sentences(shape.get("rows") or []))
        elif kind == "chart":
            parts.extend(chart_sentences(shape))
        elif kind == "group":
            flatten_shapes(shape.get("children") or [], parts, pictures)
        elif kind == "picture" and shape.get("file"):
            pictures.append(nfc(str(shape["file"])))


def strip_nulls(value: Any) -> Any:
    if isinstance(value, dict):
        return {nfc(k): strip_nulls(v) for k, v in value.items() if v is not None}
    if isinstance(value, list):
        return [strip_nulls(v) for v in value if v is not None]
    if isinstance(value, str):
        return nfc(value)
    return value


def slide_document(path: Path) -> dict:
    with path.open(encoding="utf-8") as fh:
        slide = json.load(fh)
    if not isinstance(slide, dict) or not ({"shapes", "text_blocks", "slide_number"} & slide.keys()):
        raise ValueError("not a slide JSON file")

    stem = nfc(path.stem)
    suffix = SLIDE_SUFFIX.search(stem)
    deck = clean_text(os.path.splitext(slide.get("deck") or "")[0]) or (stem[:suffix.start()] if suffix else stem)
    raw_number = slide.get("slide_number")
    if raw_number in (None, "") and suffix:
        raw_number = suffix.group(1)
    number = int(raw_number)  # ValueError for nonsense, reported by the caller
    title = clean_text(slide.get("title"))

    parts = [title] if title else []
    pictures: list[str] = []
    if isinstance(slide.get("shapes"), list):
        flatten_shapes(slide["shapes"], parts, pictures)
    for block in slide.get("text_blocks") or []:  # legacy parser output
        text = clean_block((block or {}).get("text") or "")
        if text and text != title:  # the title is usually also the first block; keep it once
            parts.append(text)
    notes = clean_text(slide.get("notes"))
    if notes:
        parts.append(notes)

    doc: dict[str, Any] = {
        "_id": stem,
        "deck": deck,
        "slide_number": number,
        "title": title,
        "content": join_blocks(parts),
    }
    if notes:
        doc["notes"] = notes
    if slide.get("source_format"):
        doc["source_format"] = clean_text(slide["source_format"])
    if pictures:
        doc["pictures"] = pictures
    warnings = [clean_text(w) for w in slide.get("warnings") or [] if clean_text(w)]
    if warnings:
        doc["warnings"] = warnings
    if slide.get("page_image"):
        doc["page_image"] = nfc(str(slide["page_image"]))
    for key, value in slide.items():  # any other field the parser writes travels along
        if key not in CONSUMED_KEYS and not key.startswith(("$", "_")) and value is not None:
            doc[nfc(key)] = strip_nulls(value)
    return doc


# ---------------------------------------------------------------------------------------- scan

class Node:
    """What the scan learned about one field: value kinds seen, children (object) or items (array)."""

    def __init__(self) -> None:
        self.present = 0
        self.scalars: Counter[str] = Counter()
        self.objects = 0
        self.arrays = 0
        self.fields: dict[str, Node] = {}
        self.item: Node | None = None

    def add(self, value: Any) -> None:
        self.present += 1
        if value is None:
            self.scalars["null"] += 1
        elif isinstance(value, bool):
            self.scalars["bool"] += 1
        elif isinstance(value, int):
            self.scalars["int"] += 1
        elif isinstance(value, float):
            self.scalars["float"] += 1
        elif isinstance(value, dict):
            self.objects += 1
            for key, child in value.items():
                self.fields.setdefault(key, Node()).add(child)
        elif isinstance(value, list):
            self.arrays += 1
            self.item = self.item or Node()
            for child in value:
                self.item.add(child)
        else:
            self.scalars["text"] += 1

    @property
    def scalar_count(self) -> int:
        return sum(n for kind, n in self.scalars.items() if kind != "null")

    def scalar_type(self) -> str | None:
        kinds = {k for k, n in self.scalars.items() if n and k != "null"}
        if not kinds:
            return None
        if kinds == {"int"}:
            return "int"
        if kinds <= {"int", "float"}:
            return "float"
        if kinds == {"bool"}:
            return "bool"
        return "text"


class Report:
    def __init__(self, total: int) -> None:
        self.total = total
        self.rows: list[tuple[str, str, str, str]] = []

    def add(self, path: str, kind: str, node: Node, note: str = "") -> None:
        seen = f"{node.present}/{self.total}" if "." not in path and "[]" not in path else f"{node.present}x"
        self.rows.append((path, kind, seen, note))

    def print(self) -> None:
        width = max([len(r[0]) for r in self.rows] + [5])
        print(f"[SCAN] {'field'.ljust(width)}  {'type':8} {'seen':>9}  note")
        for path, kind, seen, note in self.rows:
            print(f"[SCAN] {path.ljust(width)}  {kind:8} {seen:>9}  {note}")


def policy_for(node: Node, path: str, report: Report) -> Any | None:
    kinds = [k for k, n in (("value", node.scalar_count), ("object", node.objects), ("array", node.arrays)) if n]
    if not kinds:
        report.add(path, "-", node, "only null values: not declared")
        return None
    if len(kinds) > 1:
        report.add(path, "mixed", node, f"mixes {' / '.join(kinds)}: not declared, stored unindexed")
        return None

    if kinds == ["value"]:
        kind = node.scalar_type()
        mixed = len({k for k in node.scalars if k != "null"}) > 1
        override = FIELD_POLICY.get(path)
        if override is not None and kind in ("int", "float") and override.get("$type") == "text":
            kind = "text"
        entry = copy.deepcopy(override if override is not None else DEFAULT_POLICY[kind])
        note = "tuned" if override is not None else "default"
        if mixed:
            note += f"; mixed {dict(node.scalars)} -> {kind}"
        report.add(path, entry.get("$type", kind) if isinstance(entry, dict) else entry, node, note)
        return entry

    if kinds == ["object"]:
        if path:
            report.add(path, "object", node)
        children = {}
        for name in sorted(node.fields, key=lambda n: (n != "_id", n)):
            child_path = f"{path}.{name}" if path else name
            if name.startswith("$"):
                report.add(child_path, "-", node.fields[name], "reserved name: not declared")
                continue
            sub = policy_for(node.fields[name], child_path, report)
            if sub is not None:
                children[name] = sub
        return children or None

    item = node.item
    if item is None or item.present == 0:
        report.add(path, "array", node, "only empty arrays: not declared")
        return None
    if item.arrays:
        report.add(path, "array", node, "arrays of arrays are not supported: not declared, stored unindexed")
        return None
    report.add(path, "array", node)
    sub = policy_for(item, f"{path}[]", report)
    return [sub] if sub is not None else None


def coerce(value: Any, node: Node | None) -> Any:
    """Bring a value in line with the declared type: mixed fields become text, int becomes float."""
    if node is None or value is None:
        return value
    if isinstance(value, dict):
        return {k: coerce(v, node.fields.get(k)) for k, v in value.items()}
    if isinstance(value, list):
        return [coerce(v, node.item) for v in value]
    kind = node.scalar_type()
    if kind == "text" and not isinstance(value, str):
        return json.dumps(value) if isinstance(value, bool) else str(value)
    if kind == "float" and isinstance(value, int) and not isinstance(value, bool):
        return float(value)
    return value


def collect_files(inputs: list[str]) -> list[Path]:
    files: list[Path] = []
    for raw in inputs:
        path = Path(raw).expanduser()
        if path.is_dir():
            files.extend(sorted(p for p in path.glob("*.json") if p.is_file()))
        elif path.is_file() and path.suffix.lower() == ".json":
            files.append(path)
        else:
            print(f"[WARN] not a folder or .json file: {path}")
    return files


def scan(files: list[Path]) -> tuple[list[dict], Node]:
    docs: list[dict] = []
    seen_ids: dict[str, Path] = {}
    unreadable, empty = [], []
    for path in files:
        try:
            doc = slide_document(path)
        except (OSError, ValueError, TypeError, KeyError) as exc:
            unreadable.append(f"{path.name} ({exc})")
            continue
        if not doc["content"]:
            reason = "; ".join(doc.get("warnings", [])) or "no text"
            empty.append(f"{path.name} ({reason})")
            continue
        if doc["_id"] in seen_ids:
            unreadable.append(f"{path.name} (same _id as {seen_ids[doc['_id']].name})")
            continue
        seen_ids[doc["_id"]] = path
        docs.append(doc)

    root = Node()
    for doc in docs:
        root.add(doc)
    print(f"[INFO] {len(docs)} slides with text from {len(files)} files")
    for line in unreadable:
        print(f"[WARN] skipped {line}")
    for line in empty:
        print(f"[INFO] nothing to index in {line}")
    return [coerce(doc, root) for doc in docs], root


# ---------------------------------------------------------------------------------------- HTTP

class Client:
    def __init__(self, base_url: str) -> None:
        self.base_url = base_url
        self.token: str | None = None

    def call(self, method: str, path: str, body: Any = None, timeout: float = 300) -> tuple[int, Any]:
        """JSON request with retries. Returns (HTTP status, parsed body); status 0 = no answer."""
        data = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
        last: tuple[int, Any] = (0, "no attempt")
        for attempt in range(1, RETRIES + 1):
            req = urllib.request.Request(f"{self.base_url}{path}", data=data, method=method)
            req.add_header("Accept", "application/json")
            if data is not None:
                req.add_header("Content-Type", "application/json; charset=utf-8")
            if self.token:
                req.add_header("X-Corelamo-Key", self.token)
            try:
                with urllib.request.urlopen(req, timeout=timeout) as resp:
                    status, raw = resp.status, resp.read().decode("utf-8", "replace")
            except urllib.error.HTTPError as exc:
                status, raw = exc.code, exc.read().decode("utf-8", "replace")
            except (urllib.error.URLError, TimeoutError, ConnectionError, OSError) as exc:
                status, raw = 0, f"cannot reach {self.base_url}: {getattr(exc, 'reason', exc)}"
            try:
                parsed: Any = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                parsed = raw
            last = (status, parsed)
            if status != 0 and status not in RETRY_STATUSES:
                return last
            if attempt < RETRIES:
                pause = min(2 ** (attempt - 1), 30)
                print(f"[RETRY] {method} {path}: {short(status, parsed)} - retrying in {pause}s")
                time.sleep(pause)
        return last


def succeeded(status: int, body: Any) -> bool:
    if not 200 <= status < 300:
        return False
    if isinstance(body, dict):
        if body.get("error"):
            return False
        inner = body.get("status")
        if isinstance(inner, int) and inner >= 400:
            return False
    return True


def partial_failure(body: Any) -> bool:
    data = body.get("data") if isinstance(body, dict) else None
    if not isinstance(data, dict):
        return False
    failed = data.get("failed")
    errors = data.get("errors")
    return (isinstance(failed, int) and failed > 0) or (isinstance(errors, list) and bool(errors))


def short(status: int, body: Any, limit: int = 300) -> str:
    if isinstance(body, dict):
        text = body.get("title") or body.get("error") or json.dumps(body, ensure_ascii=False)
    else:
        text = str(body)
    text = " ".join(str(text).split())
    return f"HTTP {status}: {text[:limit]}" if status else text[:limit]


def login(client: Client) -> None:
    status, body = client.call("POST", "/api/login", {"username": USERNAME, "password": PASSWORD}, timeout=30)
    try:
        client.token = body["data"]["token"]
    except (KeyError, TypeError):
        # a server started with auth = "false" accepts requests without a key
        print(f"[WARN] login failed ({short(status, body)}), continuing without a key")


def step(client: Client, label: str, path: str, body: Any = None, fatal: bool = True) -> bool:
    status, out = client.call("POST", path, body)
    good = succeeded(status, out)
    print(f"[{'OK' if good else 'FATAL' if fatal else 'WARN'}] {label} ({short(status, out)})")
    if not good and fatal:
        sys.exit(EXIT_UPLOAD_FAILED)
    return good


def send(client: Client, path: str, docs: list[dict], can_resend: bool, failures: list[tuple[str, str]]) -> int:
    """Send docs; when the server rejects a batch, split it so only the bad slides fail."""
    status, body = client.call("POST", path, docs)
    good = succeeded(status, body)
    if good and not partial_failure(body):
        return len(docs)
    server_problem = status == 0 or status >= 500
    if len(docs) == 1 or server_problem or (good and not can_resend):
        reason = short(status, body)
        failures.extend((doc["_id"], reason) for doc in docs)
        return 0
    middle = len(docs) // 2
    return (send(client, path, docs[:middle], can_resend, failures)
            + send(client, path, docs[middle:], can_resend, failures))


# ---------------------------------------------------------------------------------------- embedding

def find_embedding(stats: Any) -> dict | None:
    """The embedding block of the status response, wherever the handler nests it."""
    if isinstance(stats, dict):
        for key, value in stats.items():
            if key == "embedding" and isinstance(value, dict):
                return value
            found = find_embedding(value)
            if found is not None:
                return found
    elif isinstance(stats, list):
        for value in stats:
            found = find_embedding(value)
            if found is not None:
                return found
    return None


def as_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def embedding_total(client: Client, db: str) -> int:
    status, body = client.call("GET", f"{db}/status", timeout=60)
    embedding = find_embedding(body) if succeeded(status, body) else None
    return as_int(embedding.get("vectors_total")) if embedding else 0


def wait_for_embedding(client: Client, db: str, expected: int, baseline: int, timeout: float) -> int:
    """Wait until the server counts at least `expected` documents and all of them are embedded.

    Right after an upload the slides are still in memory: they reach disk segments ~2 s after the
    last write and the worker re-counts segments once a minute, so until then the server can report
    "complete" for the old count. "Complete" is trusted only once the total grew past what it was
    before the upload, or after RECOUNT_GRACE_SECONDS (a re-run that replaced the same slides).
    """
    limit = f", giving up after {timeout:.0f}s" if timeout else ""
    print(f"[INFO] waiting for {expected} slides to be embedded{limit} "
          "(Ctrl+C stops waiting; embedding continues on the server)")
    started = time.time()
    last_line = last_error = None
    misses = 0
    while True:
        elapsed = time.time() - started
        if timeout and elapsed > timeout:
            print(f"[ERROR] embedding not finished after {elapsed:.0f}s")
            return EXIT_WAIT_FAILED
        status, stats = client.call("GET", f"{db}/status", timeout=60)
        if not succeeded(status, stats):
            misses += 1
            print(f"[WARN] status check failed ({short(status, stats)}), {misses}/{STATUS_MISSES_ALLOWED}")
            if misses >= STATUS_MISSES_ALLOWED:
                return EXIT_WAIT_FAILED
            time.sleep(POLL_SECONDS)
            continue
        misses = 0
        embedding = find_embedding(stats)
        if embedding is None:
            print("[WARN] the status has no embedding info; is a field marked $vector?")
            return EXIT_OK

        state = str(embedding.get("status", "")).lower()
        total = as_int(embedding.get("vectors_total"))
        done = as_int(embedding.get("vectors_done"))
        line = f"status={state} progress={embedding.get('progress')}% " + " ".join(
            f"{k}={v}" for k, v in embedding.items() if k not in ("status", "progress", "error"))
        if line != last_line:
            print(f"[{elapsed:6.0f}s] {line}")
            last_line = line
        error = embedding.get("error")
        if error and error != last_error:
            print(f"[WARN] server reports an embedding error: {error}")
        last_error = error

        if state == "disabled":
            print("[WARN] embedding is disabled for this database")
            return EXIT_OK
        trusted = total > baseline or elapsed >= RECOUNT_GRACE_SECONDS
        if trusted and total >= expected and done >= total:
            print(f"[DONE] {done} slides embedded in {elapsed:.0f}s")
            return EXIT_OK
        time.sleep(POLL_SECONDS)


# ---------------------------------------------------------------------------------------- main

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("inputs", nargs="+", help="folder(s) with *_slideNN.json files, or single .json files")
    parser.add_argument("--db", default="slaidi", help="database name (default: slaidi)")
    parser.add_argument("--shards", type=int, default=1, help="shards for a new database (default: 1)")
    parser.add_argument("--batch", type=int, default=DEFAULT_BATCH, help=f"slides per request (default: {DEFAULT_BATCH})")
    parser.add_argument("--mode", choices=("upsert", "insert"), default="upsert",
                        help="upsert replaces slides on a re-run (default); insert fails on existing _id")
    parser.add_argument("--stemming", default=None,
                        help="stemmer for the default analyzer, e.g. english (default: none, right for Latvian)")
    parser.add_argument("--no-wait", action="store_true", help="do not wait for embedding to finish")
    parser.add_argument("--wait-timeout", type=float, default=7200,
                        help="seconds to wait for embedding, 0 = no limit (default: 7200)")
    parser.add_argument("--dry-run", action="store_true", help="scan and print the policy and one document; upload nothing")
    args = parser.parse_args()
    if args.batch < 1:
        parser.error("--batch must be at least 1")

    files = collect_files(args.inputs)
    if not files:
        print("[FATAL] no .json files found")
        return EXIT_BAD_INPUT
    docs, root = scan(files)
    if not docs:
        print("[FATAL] no slide has text to upload")
        return EXIT_BAD_INPUT

    report = Report(len(docs))
    document_policy = policy_for(root, "", report) or {}
    report.print()
    policy = {"analyzers": build_analyzers(args.stemming), "document": document_policy}

    if args.dry_run:
        print("[DRY-RUN] policy:")
        print(json.dumps(policy, ensure_ascii=False, indent=2))
        print("[DRY-RUN] first document:")
        print(json.dumps(docs[0], ensure_ascii=False, indent=2))
        return EXIT_OK

    client = Client(BASE_URL)
    login(client)
    db = f"/api/databases/{args.db}"

    # both may fail harmlessly when the database already exists / already runs
    step(client, f"create database '{args.db}'", f"{db}/create-database", {"shard_count": args.shards}, fatal=False)
    step(client, f"start database '{args.db}'", f"{db}/start-database", fatal=False)
    step(client, "set policy", f"{db}/set-policy", policy)  # never insert without the policy

    baseline = 0 if args.no_wait else embedding_total(client, db)
    started = time.time()
    failures: list[tuple[str, str]] = []
    uploaded = 0
    for start in range(0, len(docs), args.batch):
        batch = docs[start:start + args.batch]
        uploaded += send(client, f"{db}/{args.mode}", batch, args.mode == "upsert", failures)
        print(f"[INFO] sent {start + len(batch)}/{len(docs)}, accepted {uploaded}")
    print(f"[INFO] upload finished in {time.time() - started:.1f}s: {uploaded} accepted, {len(failures)} failed")
    for doc_id, reason in failures[:20]:
        print(f"[ERROR] {doc_id}: {reason}")
    if len(failures) > 20:
        print(f"[ERROR] ... and {len(failures) - 20} more")

    code = EXIT_UPLOAD_FAILED if failures else EXIT_OK
    if not args.no_wait and uploaded:
        wait_code = wait_for_embedding(client, db, uploaded, baseline, args.wait_timeout)
        code = code or wait_code
    return code


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\n[INFO] stopped; anything already uploaded stays on the server")
        sys.exit(130)