#!/usr/bin/env python3
"""Local search UI that shows every hit as its page in the ORIGINAL PDF.

Searches a corelamo database filled by pdf_uploader.py and shows each hit as the real page of the
source PDF, rendered by PyMuPDF, with the search words highlighted on it. Any PDF works: slides,
portrait or landscape documents, scans. The dialog walks through the whole document and can
open the PDF itself at that page in the browser's PDF viewer.

The parsed JSON folder is optional. With --json you also get a text view (tables, lists, speaker
notes) and pages of PowerPoint files, which have no PDF to render.
Searches go through this server, so the browser needs no corelamo key and no CORS.

    pip install pymupdf
    python3 slide_viewer.py ~/Downloads/lectures                       # folder of the original PDFs
    python3 slide_viewer.py ~/Downloads/lectures --json ~/Desktop/slides_json
    open http://localhost:8080

Options: --db slaidi  --port 8080  --corelamo http://localhost:6006
Login: CORELAMO_USER / CORELAMO_PASS (default admin / secret); skipped when the server has
auth = "false".
"""

import argparse
import json
import mimetypes
import os
import re
import shutil
import sys
import threading
import unicodedata
import urllib.error
import urllib.request
from collections import OrderedDict
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, quote, unquote, urlparse

try:
    import pymupdf
except ImportError:  # older PyMuPDF releases
    try:
        import fitz as pymupdf
    except ImportError:
        pymupdf = None

USERNAME = os.environ.get("CORELAMO_USER", "admin")
PASSWORD = os.environ.get("CORELAMO_PASS", "secret")
MAX_BODY = 64 * 1024
SLIDE_SUFFIX = re.compile(r"_slide(\d+)$")  # same rule as pdf_uploader.py

RENDER_WIDTHS = (320, 480, 640, 960, 1280, 1600, 2000)  # a few fixed sizes, so the cache gets hits
MAX_RENDER_HEIGHT = 6000              # px; very tall pages are rendered smaller instead
RENDER_CACHE_BYTES = 256 * 1024 * 1024
MAX_OPEN_DOCUMENTS = 16
MAX_MARK_WORDS = 12
MAX_MARKS = 400

# MuPDF is not thread-safe: every PyMuPDF call in this server runs under this one lock.
MUPDF = threading.Lock()


def nfc(text):
    """One Unicode form for Latvian letters: macOS file names often store "ā" as "a" + macron."""
    return unicodedata.normalize("NFC", text)


def clean_text(value):
    """The same cleaning pdf_uploader.py applies, so names match the indexed deck names."""
    if value is None:
        return ""
    return " ".join(nfc(str(value)).replace("­", "").split())


def deck_name(file_name):
    """The `deck` pdf_uploader.py indexes for a source file: its name without the extension."""
    return clean_text(os.path.splitext(os.path.basename(file_name))[0])


def search_stems(query):
    """Word stems to mark on the page; the same stemming the page uses for its <mark> highlights."""
    stems = []
    for word in re.findall(r"\w+", nfc(query).lower()):
        if len(word) > 2:
            stem = word[:max(3, len(word) - 2)]
            if stem not in stems:
                stems.append(stem)
    return stems[:MAX_MARK_WORDS]


def render_width(requested):
    """The smallest fixed render width that is at least the requested one."""
    for width in RENDER_WIDTHS:
        if width >= requested:
            return width
    return RENDER_WIDTHS[-1]


class ByteCache:
    """Least-recently-used cache of rendered pages, bounded by total bytes."""

    def __init__(self, limit):
        self.limit = limit
        self.size = 0
        self.items = OrderedDict()
        self.lock = threading.Lock()

    def get(self, key):
        with self.lock:
            value = self.items.get(key)
            if value is not None:
                self.items.move_to_end(key)
            return value

    def put(self, key, value):
        if len(value) > self.limit:
            return
        with self.lock:
            old = self.items.pop(key, None)
            if old is not None:
                self.size -= len(old)
            self.items[key] = value
            self.size += len(value)
            while self.size > self.limit:
                _, dropped = self.items.popitem(last=False)
                self.size -= len(dropped)


class PdfLibrary:
    """The original PDFs, found by the deck name the uploader gave their pages."""

    def __init__(self, root):
        self.paths = {}       # deck -> absolute path
        self.pages = {}       # deck -> page count
        self.skipped = []
        self.duplicates = []
        self.open_documents = OrderedDict()  # deck -> (mtime_ns, document); used under MUPDF only
        self.cache = ByteCache(RENDER_CACHE_BYTES)
        for path in self._pdf_files(root):
            deck = deck_name(path)
            if not deck:
                continue
            if deck in self.paths:
                self.duplicates.append(f"{path} (same name as {self.paths[deck]})")
                continue
            try:
                count = self._page_count(path)
            except (RuntimeError, ValueError, OSError) as e:
                self.skipped.append(f"{path}: {e}")
                continue
            self.paths[deck] = path
            self.pages[deck] = count

    @staticmethod
    def _pdf_files(root):
        if os.path.isfile(root):
            return [root]
        found = []
        for folder, subfolders, files in os.walk(root):
            subfolders[:] = sorted(d for d in subfolders if not d.startswith("."))
            found.extend(os.path.join(folder, f) for f in sorted(files)
                         if f.lower().endswith(".pdf") and not f.startswith("."))
        return found

    @staticmethod
    def _page_count(path):
        with MUPDF:
            document = pymupdf.open(path)
            try:
                if document.needs_pass:
                    raise ValueError("the PDF is password protected")
                return document.page_count
            finally:
                document.close()

    def version(self, deck):
        """Changes whenever the PDF file changes; part of every cache key and image URL."""
        try:
            return os.stat(self.paths[deck]).st_mtime_ns
        except OSError:
            return 0

    def _document(self, deck):
        """An open document, reopened when the file changed. The caller must hold MUPDF."""
        path = self.paths[deck]
        mtime = os.stat(path).st_mtime_ns
        cached = self.open_documents.get(deck)
        if cached is not None and cached[0] == mtime:
            self.open_documents.move_to_end(deck)
            return cached[1]
        if cached is not None:
            cached[1].close()
        document = pymupdf.open(path)
        self.open_documents[deck] = (mtime, document)
        self.pages[deck] = document.page_count
        while len(self.open_documents) > MAX_OPEN_DOCUMENTS:
            _, (_, old) = self.open_documents.popitem(last=False)
            old.close()
        return document

    def _page(self, deck, number):
        """The page (1-based) or None. The caller must hold MUPDF."""
        document = self._document(deck)
        if not 1 <= number <= document.page_count:
            return None
        return document.load_page(number - 1)

    def render(self, deck, number, requested_width):
        """PNG bytes of the page at a fixed width, or None when the page does not exist."""
        if deck not in self.paths:
            return None
        width = render_width(requested_width)
        key = (deck, number, width, self.version(deck))
        png = self.cache.get(key)
        if png is not None:
            return png
        try:
            with MUPDF:
                page = self._page(deck, number)
                if page is None or page.rect.width <= 0 or page.rect.height <= 0:
                    return None
                zoom = width / page.rect.width
                if page.rect.height * zoom > MAX_RENDER_HEIGHT:
                    zoom = MAX_RENDER_HEIGHT / page.rect.height
                pixmap = page.get_pixmap(matrix=pymupdf.Matrix(zoom, zoom), alpha=False)
                png = pixmap.tobytes("png")
        except (RuntimeError, ValueError, OSError):
            return None
        self.cache.put(key, png)
        return png

    def marks(self, deck, number, query):
        """Boxes of the search words on the page, in page points, or None for a missing page."""
        if deck not in self.paths:
            return None
        stems = search_stems(query)
        try:
            with MUPDF:
                page = self._page(deck, number)
                if page is None:
                    return None
                rects = []
                for stem in stems:
                    for rect in page.search_for(stem):
                        # search_for answers in unrotated coordinates; the picture is rotated
                        box = rect * page.rotation_matrix
                        rects.append([round(box.x0, 1), round(box.y0, 1), round(box.x1, 1), round(box.y1, 1)])
                        if len(rects) >= MAX_MARKS:
                            break
                    if len(rects) >= MAX_MARKS:
                        break
                return {"width": page.rect.width, "height": page.rect.height, "rects": rects}
        except (RuntimeError, ValueError, OSError):
            return None


class JsonLibrary:
    """The page JSON files made by pdf_parse.py, keyed by (deck, page number) like the index."""

    def __init__(self, folder):
        self.folder = folder
        self.files = {}   # (deck, number) -> file name in the folder
        self.pages = {}   # deck -> highest page number
        self.skipped = []
        for name in sorted(os.listdir(folder)):
            stem, ext = os.path.splitext(name)
            if ext.lower() != ".json":
                continue
            try:
                with open(os.path.join(folder, name), encoding="utf-8") as f:
                    page = json.load(f)
                deck, number = self._place(nfc(stem), page)
            except (OSError, ValueError, TypeError) as e:
                self.skipped.append(f"{name}: {e}")
                continue
            self.files.setdefault((deck, number), name)
            self.pages[deck] = max(number, self.pages.get(deck, 0))

    @staticmethod
    def _place(stem, page):
        """(deck, number) exactly as pdf_uploader.slide_document derives them."""
        if not isinstance(page, dict) or not ({"shapes", "text_blocks", "slide_number"} & page.keys()):
            raise ValueError("not a page JSON file")
        suffix = SLIDE_SUFFIX.search(stem)
        deck = clean_text(os.path.splitext(page.get("deck") or "")[0])
        if not deck:
            deck = stem[:suffix.start()] if suffix else stem
        raw_number = page.get("slide_number")
        if raw_number in (None, "") and suffix:
            raw_number = suffix.group(1)
        return deck, int(raw_number)

    def page(self, deck, number):
        name = self.files.get((deck, number))
        if name is None:
            return None
        try:
            with open(os.path.join(self.folder, name), encoding="utf-8") as f:
                return json.load(f)
        except (OSError, ValueError):
            return None


class Corelamo:
    """Forwards searches; logs in on first use and again when the key expires."""

    def __init__(self, base_url, db):
        self.base_url = base_url.rstrip("/")
        self.db = db
        self.token = None
        self.lock = threading.Lock()

    def _post(self, path, body, token):
        req = urllib.request.Request(
            f"{self.base_url}{path}",
            data=json.dumps(body, ensure_ascii=False).encode("utf-8"),
            method="POST",
        )
        req.add_header("Content-Type", "application/json")
        req.add_header("Accept", "application/json")
        if token:
            req.add_header("X-Corelamo-Key", token)
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except urllib.error.URLError as e:
            message = {"detail": f"corelamo is not reachable at {self.base_url}: {e.reason}"}
            return HTTPStatus.BAD_GATEWAY, json.dumps(message).encode("utf-8")

    def _login(self):
        _status, raw = self._post("/api/login", {"username": USERNAME, "password": PASSWORD}, None)
        try:
            self.token = json.loads(raw)["data"]["token"]
        except (ValueError, KeyError, TypeError):
            # auth = "false" servers accept requests without a key
            self.token = None

    def search(self, body):
        path = f"/api/databases/{self.db}/search"
        with self.lock:
            if self.token is None:
                self._login()
            token = self.token
        status, raw = self._post(path, body, token)
        if status == HTTPStatus.UNAUTHORIZED:
            with self.lock:
                self._login()
                token = self.token
            status, raw = self._post(path, body, token)
        return status, raw


def build_query(request):
    """Turns the page's request into a corelamo search body."""
    text = str(request.get("text", "")).strip()
    mode = request.get("mode", "hybrid")
    docs = max(1, min(int(request.get("docs", 24)), 100))
    offset = max(0, int(request.get("offset", 0)))
    body = {
        "docs": docs,
        "offset": offset,
        "return_fields": {
            "_id": True, "deck": True, "slide_number": True, "title": True,
            "content": True, "source_format": True,
        },
    }
    if mode == "meaning":
        body["query"] = {"vector": text}
    else:
        body["query"] = text
        body["semantic"] = mode == "hybrid"
    deck = str(request.get("deck", "")).strip()
    if deck:
        body["filters"] = {"deck": deck}
    return body


def deck_list(pdfs, texts):
    """Every known deck: whether its PDF is here, how many pages, and the PDF's version."""
    names = set(pdfs.paths if pdfs else ()) | set(texts.pages if texts else ())
    decks = []
    for deck in sorted(names):
        has_pdf = bool(pdfs and deck in pdfs.paths)
        pages = pdfs.pages[deck] if has_pdf else texts.pages.get(deck, 0)
        decks.append({"deck": deck, "pdf": has_pdf, "pages": pages,
                      "v": pdfs.version(deck) if has_pdf else 0})
    return decks


def split_place(rest):
    """'<deck>/<number>' -> (deck, number), or None."""
    deck, _, number = rest.rpartition("/")
    if not deck or not (number.isascii() and number.isdigit()):
        return None
    return nfc(deck), int(number)


def make_handler(pdfs, texts, corelamo):

    class Handler(BaseHTTPRequestHandler):
        server_version = "SlideViewer"

        def log_message(self, fmt, *args):
            sys.stderr.write("[viewer] " + (fmt % args) + "\n")

        def send_bytes(self, status, data, content_type, cache=False):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "max-age=86400" if cache else "no-store")
            self.end_headers()
            self.wfile.write(data)

        def send_json(self, status, value):
            data = json.dumps(value, ensure_ascii=False).encode("utf-8")
            self.send_bytes(status, data, "application/json; charset=utf-8")

        def not_found(self, what):
            self.send_json(HTTPStatus.NOT_FOUND, {"detail": f"no such {what}"})

        def send_pdf(self, path):
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "application/pdf")
            self.send_header("Content-Length", str(os.path.getsize(path)))
            self.send_header("Content-Disposition", f"inline; filename*=UTF-8''{quote(os.path.basename(path))}")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            with open(path, "rb") as f:
                shutil.copyfileobj(f, self.wfile)

        def json_asset(self, relative):
            """A picture inside the JSON folder, or None for anything outside it."""
            if texts is None:
                return None
            folder = texts.folder
            path = os.path.realpath(os.path.join(folder, relative))
            if not path.startswith(folder + os.sep) or not os.path.isfile(path):
                return None
            return path

        def do_GET(self):
            url = urlparse(self.path)
            path = unquote(url.path)
            params = parse_qs(url.query)
            try:
                self.route(path, params)
            except (BrokenPipeError, ConnectionResetError):
                pass  # the browser went away mid-transfer, e.g. the user stepped to the next page

        def route(self, path, params):
            if path in ("/", "/index.html"):
                self.send_bytes(HTTPStatus.OK, PAGE.encode("utf-8"), "text/html; charset=utf-8")
            elif path == "/decks":
                self.send_json(HTTPStatus.OK, deck_list(pdfs, texts))
            elif path.startswith("/render/"):
                place = split_place(path[len("/render/"):])
                try:
                    width = int(params.get("w", ["960"])[0])
                except ValueError:
                    width = 960
                png = pdfs.render(*place, width) if pdfs and place else None
                if png is None:
                    self.not_found("page")
                else:
                    self.send_bytes(HTTPStatus.OK, png, "image/png", cache=True)
            elif path.startswith("/marks/"):
                place = split_place(path[len("/marks/"):])
                query = params.get("q", [""])[0][:500]
                marks = pdfs.marks(*place, query) if pdfs and place else None
                if marks is None:
                    self.not_found("page")
                else:
                    self.send_json(HTTPStatus.OK, marks)
            elif path.startswith("/pdf/"):
                deck = nfc(path[len("/pdf/"):])
                file = pdfs.paths.get(deck) if pdfs else None
                if file is None or not os.path.isfile(file):
                    self.not_found("document")
                else:
                    self.send_pdf(file)
            elif path.startswith("/text/"):
                place = split_place(path[len("/text/"):])
                page = texts.page(*place) if texts and place else None
                if page is None:
                    self.not_found("page")
                else:
                    self.send_json(HTTPStatus.OK, {"page": page})
            elif path.startswith("/assets/"):
                file = self.json_asset(path.lstrip("/"))
                if file is None:
                    self.not_found("file")
                    return
                content_type = mimetypes.guess_type(file)[0] or "application/octet-stream"
                with open(file, "rb") as f:
                    self.send_bytes(HTTPStatus.OK, f.read(), content_type, cache=True)
            else:
                self.send_json(HTTPStatus.NOT_FOUND, {"detail": "not found"})

        def do_POST(self):
            if urlparse(self.path).path != "/search":
                self.send_json(HTTPStatus.NOT_FOUND, {"detail": "not found"})
                return
            length = int(self.headers.get("Content-Length") or 0)
            if length > MAX_BODY:
                self.send_json(HTTPStatus.REQUEST_ENTITY_TOO_LARGE, {"detail": "request too large"})
                return
            try:
                request = json.loads(self.rfile.read(length) or b"{}")
                body = build_query(request)
            except (ValueError, TypeError, AttributeError) as e:
                self.send_json(HTTPStatus.BAD_REQUEST, {"detail": f"bad request: {e}"})
                return
            query = body["query"]
            if not (query if isinstance(query, str) else query["vector"]):
                self.send_json(HTTPStatus.BAD_REQUEST, {"detail": "type something to search"})
                return
            status, raw = corelamo.search(body)
            self.send_bytes(status, raw, "application/json; charset=utf-8")

    return Handler


PAGE = r"""<!doctype html>
<html lang="lv">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Dokumentu meklētājs</title>
<style>
:root {
  --bg: #f4f2ee; --panel: #ffffff; --ink: #1d1b18; --muted: #6d675f; --line: #e2ddd5; --well: #e9e5de;
  --accent: #2f5d50; --accent-ink: #ffffff; --mark: #f3e3a6; --shadow: 0 1px 2px rgba(0,0,0,.06), 0 4px 16px rgba(0,0,0,.06);
  --paper: #ffffff; --paper-ink: #1b1b1b; --paper-line: #d9d4cb; --paper-head: #f3f0ea; --paper-title: #123d33;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #161514; --panel: #201f1d; --ink: #ece8e1; --muted: #a39d94; --line: #34312d; --well: #2a2826;
    --accent: #7fb8a6; --accent-ink: #10201b; --mark: #5a4d1c; --shadow: 0 1px 2px rgba(0,0,0,.4), 0 4px 16px rgba(0,0,0,.3);
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink);
  font: 15px/1.45 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
header { position: sticky; top: 0; z-index: 5; background: var(--bg); border-bottom: 1px solid var(--line); }
.bar { max-width: 1320px; margin: 0 auto; padding: 14px 16px; display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }
.bar h1 { font-size: 17px; margin: 0 8px 0 0; letter-spacing: .2px; white-space: nowrap; }
#q { flex: 1 1 280px; min-width: 0; padding: 10px 12px; font: inherit; color: var(--ink);
  background: var(--panel); border: 1px solid var(--line); border-radius: 8px; }
#q:focus, select:focus, button:focus-visible, a.btn:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
select, button, a.btn { font: inherit; color: var(--ink); background: var(--panel); border: 1px solid var(--line);
  border-radius: 8px; padding: 9px 10px; }
a.btn { text-decoration: none; white-space: nowrap; }
button { cursor: pointer; }
button:disabled { opacity: .4; cursor: default; }
button.primary { background: var(--accent); color: var(--accent-ink); border-color: var(--accent); }
.seg { display: inline-flex; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
.seg button { border: 0; border-radius: 0; background: var(--panel); }
.seg button[aria-pressed="true"] { background: var(--accent); color: var(--accent-ink); }
main { max-width: 1320px; margin: 0 auto; padding: 16px; }
#status { color: var(--muted); min-height: 22px; margin-bottom: 10px; }
#status.error { color: #b3261e; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 16px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; box-shadow: var(--shadow);
  overflow: hidden; cursor: pointer; display: flex; flex-direction: column; text-align: left; padding: 0; color: inherit; }
.card:hover { border-color: var(--accent); }
.meta { padding: 10px 12px 12px; display: grid; gap: 4px; }
.meta .title { font-weight: 600; }
.meta .sub { color: var(--muted); font-size: 13px; display: flex; justify-content: space-between; gap: 8px; }
.snippet { font-size: 13px; color: var(--muted); display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden; }
mark { background: var(--mark); color: inherit; border-radius: 2px; }

/* The original page: a sheet with the page's own proportions, search words marked on top. */
.sheet { position: relative; aspect-ratio: var(--ar, 0.7071); background: #fff;
  box-shadow: 0 1px 3px rgba(0,0,0,.18); opacity: .35; transition: opacity .15s; }
.sheet.ready { opacity: 1; }
.sheet > img { position: absolute; inset: 0; width: 100%; height: 100%; display: block; }
.hit-mark { position: absolute; background: rgba(255, 196, 0, .42); mix-blend-mode: multiply;
  border-radius: 2px; outline: 1px solid rgba(214, 150, 0, .55); pointer-events: none; }

/* Card preview: every page fits whole, portrait or landscape. */
.preview { position: relative; width: 100%; aspect-ratio: 4 / 3; overflow: hidden; background: var(--well);
  border-bottom: 1px solid var(--line); container-type: size; display: flex; align-items: center;
  justify-content: center; padding: 8px; }
.preview .sheet { width: min(100cqw, calc(100cqh * var(--ar, 0.7071))); }
.preview.as-doc { display: block; padding: 0; background: var(--paper); color: var(--paper-ink); }
.preview .doc { font-size: 9.5px; padding: 12px 14px; pointer-events: none; }
.preview.as-doc::after { content: ""; position: absolute; inset: auto 0 0 0; height: 34%;
  background: linear-gradient(to bottom, transparent, var(--paper)); }
.preview .doc img.pic { max-height: 14em; }

/* Text view: a page rebuilt from the parsed JSON. */
.doc { line-height: 1.4; overflow-wrap: anywhere; }
.doc h2 { font-size: 1.55em; line-height: 1.2; margin: 0 0 .6em; color: var(--paper-title); }
.doc .para { margin: .35em 0; white-space: pre-line; position: relative; padding-left: calc(var(--lvl, 0) * 1.4em); }
.doc .para.sub::before { content: "•"; position: absolute; left: calc(var(--lvl) * 1.4em - 1em); }
.doc .table-wrap { overflow-x: auto; margin: .6em 0; }
.doc table { border-collapse: collapse; font-size: .95em; }
.doc td { border: 1px solid var(--paper-line); padding: .25em .5em; vertical-align: top; white-space: pre-line; }
.doc tr:first-child td { font-weight: 600; background: var(--paper-head); }
.doc img.pic { display: block; max-width: 100%; max-height: 32em; margin: .6em auto; object-fit: contain; }
.doc mark { background: #f3e3a6; }
.doc .blank { color: #77716a; font-style: italic; }
.doc .notes { margin-top: 1.2em; padding-top: .6em; border-top: 1px solid var(--paper-line); }
.doc .notes summary { cursor: pointer; font-weight: 600; }
.doc .warn { margin-top: .8em; font-size: .85em; color: #8a5a00; }

.more { display: flex; justify-content: center; margin: 22px 0 8px; }
dialog { width: min(1400px, 97vw); max-height: 96vh; border: 0; border-radius: 12px; padding: 0;
  background: var(--panel); color: var(--ink); box-shadow: 0 10px 40px rgba(0,0,0,.35); }
dialog::backdrop { background: rgba(0,0,0,.6); }
.dlg-head { display: flex; gap: 8px; align-items: center; padding: 10px 12px; border-bottom: 1px solid var(--line); }
.dlg-head .title { flex: 1; font-weight: 600; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dlg-body { padding: 14px; overflow: auto; max-height: calc(96vh - 58px); background: var(--well); }
.viewer { display: flex; justify-content: center; }
/* whole page visible by default; "fit width" lets tall pages scroll */
.viewer .sheet { width: min(100%, calc((96vh - 90px) * var(--ar, 0.7071))); }
dialog.fit-width .viewer .sheet { width: 100%; }
.paper { background: var(--paper); color: var(--paper-ink); border: 1px solid var(--line); border-radius: 6px;
  max-width: 880px; margin: 0 auto; padding: 28px 34px; font-size: 16px; }
.empty { text-align: center; color: var(--muted); padding: 60px 10px; }
@media (max-width: 560px) { .grid { grid-template-columns: 1fr; } .bar h1 { width: 100%; } .paper { padding: 18px; } }
</style>
</head>
<body>
<header>
  <form class="bar" id="form" autocomplete="off">
    <h1>Dokumentu meklētājs</h1>
    <input id="q" type="search" placeholder="Meklēt dokumentos… piem. „kā novērtēt projekta izmaksas”" autofocus>
    <select id="mode" title="Meklēšanas veids">
      <option value="hybrid">Vārdi + nozīme</option>
      <option value="meaning">Tikai nozīme</option>
      <option value="words">Tikai vārdi</option>
    </select>
    <select id="deck" title="Dokuments"><option value="">Visi dokumenti</option></select>
    <div class="seg" role="group" aria-label="Skats">
      <button type="button" data-view="original" aria-pressed="true" title="Lapa no oriģinālā PDF">Oriģināls</button>
      <button type="button" data-view="text" aria-pressed="false" title="Lapas teksts, tabulas un piezīmes">Teksts</button>
    </div>
    <button class="primary" type="submit">Meklēt</button>
  </form>
</header>
<main>
  <div id="status"></div>
  <div class="grid" id="results"></div>
  <div class="more"><button id="more" hidden>Rādīt vairāk</button></div>
</main>
<dialog id="dlg">
  <div class="dlg-head">
    <button id="prev" title="Iepriekšējā lapa (←)">←</button>
    <div class="title" id="dlg-title"></div>
    <button id="fit" title="Visa lapa / pa platumu (F)">⤢</button>
    <a id="open-pdf" class="btn" target="_blank" rel="noopener" title="Atvērt visu PDF šajā lapā">Atvērt PDF</a>
    <button id="next" title="Nākamā lapa (→)">→</button>
    <button id="close" title="Aizvērt (Esc)">✕</button>
  </div>
  <div class="dlg-body" id="dlg-body"></div>
</dialog>
<script>
const PAGE_SIZE = 24;
const VIEWS = ["original", "text"];
const WIDTHS = [320, 480, 640, 960, 1280, 1600, 2000];   // the server's render widths
const $ = (id) => document.getElementById(id);
const state = { view: pickView(readPref("view", "original")), fit: readPref("fit", "page"),
  text: "", offset: 0, hits: [], open: null };
const decks = new Map();          // deck -> {deck, pdf, pages, v}
const textCache = new Map();
const marksCache = new Map();

function pickView(view) { return VIEWS.includes(view) ? view : "original"; }
function readPref(key, fallback) { try { return localStorage.getItem(key) || fallback; } catch { return fallback; } }
function writePref(key, value) { try { localStorage.setItem(key, value); } catch {} }
function escapeHtml(s) { return String(s ?? "").replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c])); }
function prettyDeck(deck) { return String(deck || "").replace(/_lv$/, "").replace(/^(\d+\w?)_/, "$1 · ").replace(/[_-]/g, " "); }
function pageLabel(format, number) { return format === "pptx" ? `slaids ${number}` : `${number}. lapa`; }
function assetUrl(path) { return "/" + String(path).split("/").map(encodeURIComponent).join("/"); }

function highlight(text, query) {
  const words = [...new Set(query.toLowerCase().split(/\s+/).filter(w => w.length > 2))];
  let html = escapeHtml(text);
  for (const w of words) {
    const stem = w.slice(0, Math.max(3, w.length - 2)).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    html = html.replace(new RegExp(`(${stem}[\\p{L}]*)`, "giu"), "<mark>$1</mark>");
  }
  return html;
}

const decksReady = fetch("/decks").then(r => r.json()).then(list => {
  for (const info of list) {
    decks.set(info.deck, info);
    const option = document.createElement("option");
    option.value = info.deck; option.textContent = prettyDeck(info.deck);
    $("deck").appendChild(option);
  }
}).catch(() => {});

// ---- the original page

function hasPdf(deck, number) {
  const info = decks.get(deck);
  return Boolean(info && info.pdf && number >= 1 && number <= info.pages);
}

function renderUrl(deck, number, cssWidth) {
  const wanted = cssWidth * (window.devicePixelRatio || 1);
  const width = WIDTHS.find(w => w >= wanted) ?? WIDTHS[WIDTHS.length - 1];
  return `/render/${encodeURIComponent(deck)}/${number}?w=${width}&v=${decks.get(deck)?.v ?? 0}`;
}

function loadMarks(deck, number) {
  if (!state.text) return Promise.resolve(null);
  const key = `${deck}\n${number}\n${state.text}`;
  if (!marksCache.has(key)) {
    const url = `/marks/${encodeURIComponent(deck)}/${number}?q=${encodeURIComponent(state.text)}`;
    marksCache.set(key, fetch(url).then(r => r.ok ? r.json() : null).catch(() => null));
  }
  return marksCache.get(key);
}

function drawMarks(sheetEl, marks) {
  if (!marks || !marks.width || !marks.height || !Array.isArray(marks.rects)) return;
  for (const [x0, y0, x1, y1] of marks.rects) {
    const m = document.createElement("span");
    m.className = "hit-mark";
    Object.assign(m.style, {
      left: `${(x0 / marks.width) * 100}%`, top: `${(y0 / marks.height) * 100}%`,
      width: `${((x1 - x0) / marks.width) * 100}%`, height: `${((y1 - y0) / marks.height) * 100}%`,
    });
    sheetEl.appendChild(m);
  }
}

// A page of the original PDF at roughly `cssWidth` px; `onFail` runs when it cannot be rendered.
function sheet(deck, number, cssWidth, onFail) {
  const el = document.createElement("div");
  el.className = "sheet";
  const img = document.createElement("img");
  img.alt = ""; img.decoding = "async"; img.loading = "lazy";
  img.onload = () => {
    if (img.naturalWidth && img.naturalHeight) el.style.setProperty("--ar", String(img.naturalWidth / img.naturalHeight));
    el.classList.add("ready");
  };
  img.onerror = () => onFail();
  img.src = renderUrl(deck, number, cssWidth);
  el.appendChild(img);
  loadMarks(deck, number).then(marks => drawMarks(el, marks));
  return el;
}

function prefetch(deck, number, cssWidth) {
  if (hasPdf(deck, number)) new Image().src = renderUrl(deck, number, cssWidth);
}

// ---- the text view, from the parsed JSON (optional)

function loadText(deck, number) {
  const key = `${deck}\n${number}`;
  if (!textCache.has(key)) {
    const url = `/text/${encodeURIComponent(deck)}/${number}`;
    textCache.set(key, fetch(url).then(r => r.ok ? r.json() : null).then(j => j?.page ?? null).catch(() => null));
  }
  return textCache.get(key);
}

function tableHtml(rows) {
  if (!Array.isArray(rows) || !rows.length) return "";
  const body = rows.map(row => {
    const cells = Array.isArray(row) ? row : [row];
    return `<tr>${cells.map(cell => `<td>${highlight(cell ?? "", state.text)}</td>`).join("")}</tr>`;
  }).join("");
  return `<div class="table-wrap"><table>${body}</table></div>`;
}

function chartHtml(shape) {
  const series = Array.isArray(shape.series) ? shape.series : [];
  if (!series.length) return "";
  const categories = Array.isArray(shape.categories) ? shape.categories : [];
  return tableHtml([["", ...categories], ...series.map(s => [s?.name ?? "", ...(Array.isArray(s?.values) ? s.values : [])])]);
}

function pictureHtml(file) {
  return file ? `<img class="pic" loading="lazy" alt="" src="${assetUrl(file)}">` : "";
}

function shapesHtml(shapes, out) {
  for (const shape of Array.isArray(shapes) ? shapes : []) {
    if (!shape || typeof shape !== "object") continue;
    switch (shape.kind) {
      case "text":
        for (const p of Array.isArray(shape.paragraphs) ? shape.paragraphs : []) {
          const text = typeof p === "string" ? p : p?.text;
          if (!text || !String(text).trim()) continue;
          const level = Math.min(8, Math.max(0, Number(p?.level) || 0));
          out.push(`<p class="para${level ? " sub" : ""}" style="--lvl:${level}">${highlight(text, state.text)}</p>`);
        }
        break;
      case "table": out.push(tableHtml(shape.rows)); break;
      case "chart": out.push(chartHtml(shape)); break;
      case "picture": out.push(pictureHtml(shape.file)); break;
      case "group": shapesHtml(shape.children, out); break;
      default:
        if (typeof shape.text === "string" && shape.text.trim()) out.push(`<p class="para">${highlight(shape.text, state.text)}</p>`);
    }
  }
}

function legacyHtml(page, out) {
  const title = String(page.title || "").trim();
  (Array.isArray(page.text_blocks) ? page.text_blocks : []).forEach((block, i) => {
    const text = String(block?.text || "").replace(/^([•●▪◦])\s*\n/gm, "$1 ").trim();
    if (!text || (i === 0 && text === title)) return;
    out.push(`<p class="para">${highlight(text, state.text)}</p>`);
  });
  for (const image of Array.isArray(page.images) ? page.images : []) out.push(pictureHtml(image?.file));
}

function documentHtml(page, full) {
  const out = [];
  if (page.title) out.push(`<h2>${highlight(page.title, state.text)}</h2>`);
  const before = out.length;
  shapesHtml(page.shapes, out);
  legacyHtml(page, out);
  if (out.length === before) out.push(`<p class="blank">Šai lapai nav izvelkama teksta.</p>`);
  if (full && page.notes) {
    out.push(`<details class="notes"><summary>Piezīmes</summary><p class="para">${highlight(page.notes, state.text)}</p></details>`);
  }
  if (full && Array.isArray(page.warnings) && page.warnings.length) {
    out.push(`<p class="warn">${page.warnings.map(escapeHtml).join(" · ")}</p>`);
  }
  return `<div class="doc">${out.join("")}</div>`;
}

// What the index returned, for pages that have neither a PDF here nor a JSON file.
function plainHtml(hit) {
  return `<div class="doc">${hit.title ? `<h2>${highlight(hit.title, state.text)}</h2>` : ""}
    <p class="para">${highlight(hit.content || "", state.text)}</p></div>`;
}

// ---- cards

async function renderPreview(box, hit, view) {
  box.className = "preview";
  box.replaceChildren();
  if (view === "original" && hasPdf(hit.deck, hit.slide_number)) {
    box.appendChild(sheet(hit.deck, hit.slide_number, 420, () => renderPreview(box, hit, "text")));
    return;
  }
  box.classList.add("as-doc");
  const page = await loadText(hit.deck, hit.slide_number);
  box.innerHTML = page ? documentHtml(page, false) : plainHtml(hit);
}

function card(hit, index) {
  const el = document.createElement("button");
  el.type = "button"; el.className = "card";
  el.innerHTML = `
    <div class="preview"></div>
    <div class="meta">
      <div class="title">${highlight(hit.title || "(bez virsraksta)", state.text)}</div>
      <div class="sub"><span>${escapeHtml(prettyDeck(hit.deck))} · ${escapeHtml(pageLabel(hit.source_format, hit.slide_number))}</span>
        <span title="Rezultāta vērtējums">${Number(hit.score || 0).toFixed(3)}</span></div>
      <div class="snippet">${highlight(hit.content || "", state.text)}</div>
    </div>`;
  renderPreview(el.querySelector(".preview"), hit, state.view);
  el.addEventListener("click", () => openHit(index));
  return el;
}

function setStatus(text, isError = false) { $("status").textContent = text; $("status").className = isError ? "error" : ""; }

async function search(append) {
  const text = $("q").value.trim();
  if (!text) { setStatus("Ieraksti, ko meklēt."); return; }
  if (!append) { state.text = text; state.offset = 0; state.hits = []; $("results").innerHTML = ""; }
  setStatus("Meklē…");
  $("more").hidden = true;
  try {
    await decksReady;
    const response = await fetch("/search", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: state.text, mode: $("mode").value, deck: $("deck").value,
        docs: PAGE_SIZE, offset: state.offset }),
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) { setStatus(body.detail || body.title || `Kļūda (HTTP ${response.status})`, true); return; }
    const hits = (body.data || []).map(h => ({ ...h.data, _id: h.id ?? h.data?._id, score: h.score ?? 0 }));
    const start = state.hits.length;
    state.hits.push(...hits);
    state.offset += hits.length;
    hits.forEach((hit, i) => $("results").appendChild(card(hit, start + i)));
    const total = body.total_hits?.value ?? state.hits.length;
    const more = hits.length === PAGE_SIZE && (body.total_hits?.more || state.hits.length < total);
    if (!state.hits.length) {
      setStatus("");
      $("results").innerHTML = `<div class="empty">Nekas netika atrasts. Pamēģini citus vārdus vai „Tikai nozīme”.</div>`;
    } else {
      setStatus(`${state.hits.length}${more ? "+" : ""} lapas · ${body.time_taken || ""}`);
    }
    $("more").hidden = !more;
  } catch (e) {
    setStatus("Serveris neatbild: " + e.message, true);
  }
}

// ---- the dialog walks through the whole document, not only the hits

function openHit(index) {
  const hit = state.hits[index];
  state.open = { deck: hit.deck, number: hit.slide_number, format: hit.source_format, hit };
  showOpen();
  if (!$("dlg").open) $("dlg").showModal();
}

async function showOpen() {
  const open = state.open;
  const { deck, number, format, hit } = open;
  const pages = decks.get(deck)?.pages ?? 0;
  const onHitPage = hit.deck === deck && hit.slide_number === number;
  $("dlg-title").textContent = `${prettyDeck(deck)} · ${pageLabel(format, number)}${pages ? ` no ${pages}` : ""}`
    + (onHitPage && hit.title ? ` · ${hit.title}` : "");
  $("prev").disabled = number <= 1;
  $("next").disabled = !pages || number >= pages;
  const pdf = hasPdf(deck, number);
  $("open-pdf").hidden = !pdf;
  if (pdf) $("open-pdf").href = `/pdf/${encodeURIComponent(deck)}#page=${number}`;
  $("fit").hidden = !(pdf && state.view === "original");
  $("dlg").classList.toggle("fit-width", state.fit === "width");
  const body = $("dlg-body");
  if (pdf && state.view === "original") {
    const width = Math.min(body.clientWidth || 1200, 1400);
    const viewer = document.createElement("div");
    viewer.className = "viewer";
    viewer.appendChild(sheet(deck, number, width, () => showText(open)));
    body.replaceChildren(viewer);
    body.scrollTop = 0;
    prefetch(deck, number + 1, width);
    prefetch(deck, number - 1, width);
  } else {
    await showText(open);
  }
}

async function showText(open) {
  const { deck, number, hit } = open;
  const page = await loadText(deck, number);
  if (open !== state.open) return;  // the user stepped on while this page was loading
  let html;
  if (page) html = documentHtml(page, true);
  else if (hit.deck === deck && hit.slide_number === number) html = plainHtml(hit);
  else html = `<div class="doc"><p class="blank">Šai lapai teksta skats nav pieejams (palaid skatītāju ar --json).</p></div>`;
  $("dlg-body").innerHTML = `<div class="paper">${html}</div>`;
  $("dlg-body").scrollTop = 0;
}

function step(delta) {
  if (!state.open) return;
  const pages = decks.get(state.open.deck)?.pages ?? 0;
  const number = state.open.number + delta;
  if (number < 1 || number > pages) return;
  state.open = { ...state.open, number };
  showOpen();
}

function toggleFit() {
  state.fit = state.fit === "width" ? "page" : "width";
  writePref("fit", state.fit);
  $("dlg").classList.toggle("fit-width", state.fit === "width");
}

function setView(view) {
  state.view = pickView(view); writePref("view", state.view);
  document.querySelectorAll("[data-view]").forEach(b => b.setAttribute("aria-pressed", String(b.dataset.view === state.view)));
  document.querySelectorAll(".card").forEach((el, i) => renderPreview(el.querySelector(".preview"), state.hits[i], state.view));
  if ($("dlg").open) showOpen();
}

$("form").addEventListener("submit", (e) => { e.preventDefault(); search(false); });
$("more").addEventListener("click", () => search(true));
$("mode").value = readPref("mode", "hybrid");
$("mode").addEventListener("change", () => { writePref("mode", $("mode").value); if (state.text) search(false); });
$("deck").addEventListener("change", () => { if (state.text) search(false); });
document.querySelectorAll("[data-view]").forEach(b => b.addEventListener("click", () => setView(b.dataset.view)));
$("prev").addEventListener("click", () => step(-1));
$("next").addEventListener("click", () => step(1));
$("fit").addEventListener("click", toggleFit);
$("close").addEventListener("click", () => $("dlg").close());
$("dlg").addEventListener("click", (e) => { if (e.target === $("dlg")) $("dlg").close(); });
document.addEventListener("keydown", (e) => {
  if (!$("dlg").open) return;
  if (e.key === "ArrowLeft") step(-1);
  else if (e.key === "ArrowRight") step(1);
  else if (e.key === "f" || e.key === "F") toggleFit();
});
setView(state.view);
</script>
</body>
</html>
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("originals", nargs="?",
                        help="the original PDFs: one file or a folder (searched with its subfolders)")
    parser.add_argument("--json", help="optional: the folder pdf_parse.py made, for the text view and PowerPoint files")
    parser.add_argument("--db", default="slaidi", help="corelamo database (default: slaidi)")
    parser.add_argument("--port", type=int, default=8080, help="port of this viewer (default: 8080)")
    parser.add_argument("--corelamo", default=os.environ.get("CORELAMO_URL", "http://localhost:6006"),
                        help="corelamo address (default: http://localhost:6006)")
    args = parser.parse_args()
    if not args.originals and not args.json:
        parser.error("give the folder of the original PDFs, --json, or both")

    pdfs = None
    if args.originals:
        if pymupdf is None:
            sys.exit("[FATAL] showing the original PDFs needs PyMuPDF: pip install pymupdf")
        root = os.path.realpath(os.path.expanduser(args.originals))
        if not os.path.exists(root):
            sys.exit(f"[FATAL] {root} does not exist")
        pdfs = PdfLibrary(root)
        for problem in pdfs.skipped:
            print(f"[WARN] skipped {problem}")
        for duplicate in pdfs.duplicates:
            print(f"[WARN] ignored {duplicate}: the index cannot tell two documents with one name apart")
        print(f"[INFO] {len(pdfs.paths)} PDFs, {sum(pdfs.pages.values())} pages from {root}")

    texts = None
    if args.json:
        folder = os.path.realpath(os.path.expanduser(args.json))
        if not os.path.isdir(folder):
            sys.exit(f"[FATAL] {folder} is not a folder")
        texts = JsonLibrary(folder)
        for problem in texts.skipped:
            print(f"[WARN] skipped {problem}")
        print(f"[INFO] {len(texts.files)} parsed pages from {folder}")

    if not (pdfs and pdfs.paths) and not (texts and texts.files):
        sys.exit("[FATAL] nothing to show: no readable PDFs and no page JSON files")

    handler = make_handler(pdfs, texts, Corelamo(args.corelamo, args.db))
    server = ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    print(f"[INFO] viewer on http://localhost:{args.port}  (database '{args.db}' at {args.corelamo})")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[INFO] stopped")


if __name__ == "__main__":
    main()