#!/usr/bin/env python3
"""Local search UI for the lecture slides in corelamo.

Serves one page that searches the `slaidi` database and shows every hit as a slide: either the
rendered page picture, or the slide rebuilt from its text blocks and images at their original
positions. Searches go through this server, so the browser needs no corelamo key and no CORS.

    python3 slide_viewer.py ~/Desktop/slaidi_json
    open http://localhost:8080

Options: --db slaidi  --port 8080  --corelamo http://localhost:6006
Login: CORELAMO_USER / CORELAMO_PASS (default admin / secret); skipped when the server has
auth = "false".
"""

import argparse
import json
import mimetypes
import os
import sys
import threading
import urllib.error
import urllib.request
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlparse

USERNAME = os.environ.get("CORELAMO_USER", "admin")
PASSWORD = os.environ.get("CORELAMO_PASS", "secret")
MAX_BODY = 64 * 1024


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
        status, raw = self._post("/api/login", {"username": USERNAME, "password": PASSWORD}, None)
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
            "_id": True, "deck": True, "slide_number": True,
            "title": True, "content": True, "page_image": True,
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


def make_handler(folder, corelamo):
    folder = os.path.realpath(folder)
    # (deck, slide number) -> file name; lectures with 100+ slides pad numbers to three digits
    slides = {}
    for name in os.listdir(folder):
        stem, ext = os.path.splitext(name)
        deck, sep, number = stem.rpartition("_slide")
        if ext == ".json" and sep and number.isdigit():
            slides[(deck, int(number))] = name
    decks = sorted({deck for deck, _ in slides})

    class Handler(BaseHTTPRequestHandler):
        server_version = "SlideViewer"

        def log_message(self, fmt, *args):
            sys.stderr.write("[viewer] " + (fmt % args) + "\n")

        def send_bytes(self, status, data, content_type, cache=False):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "max-age=3600" if cache else "no-store")
            self.end_headers()
            self.wfile.write(data)

        def send_json(self, status, value):
            data = json.dumps(value, ensure_ascii=False).encode("utf-8")
            self.send_bytes(status, data, "application/json; charset=utf-8")

        def local_file(self, relative):
            """A file inside the slide folder, or None for anything outside it."""
            path = os.path.realpath(os.path.join(folder, relative))
            if not path.startswith(folder + os.sep) or not os.path.isfile(path):
                return None
            return path

        def do_GET(self):
            path = unquote(urlparse(self.path).path)
            if path in ("/", "/index.html"):
                self.send_bytes(HTTPStatus.OK, PAGE.encode("utf-8"), "text/html; charset=utf-8")
            elif path == "/decks":
                self.send_json(HTTPStatus.OK, decks)
            elif path.startswith("/slide/"):
                # /slide/<deck>/<number>
                deck, _, number = path[len("/slide/"):].rpartition("/")
                name = slides.get((deck, int(number))) if number.isdigit() else None
                file = self.local_file(name) if name else None
                if file is None:
                    self.send_json(HTTPStatus.NOT_FOUND, {"detail": "no such slide"})
                    return
                with open(file, "rb") as f:
                    self.send_bytes(HTTPStatus.OK, f.read(), "application/json; charset=utf-8", cache=True)
            elif path.startswith("/assets/"):
                file = self.local_file(path.lstrip("/"))
                if file is None:
                    self.send_json(HTTPStatus.NOT_FOUND, {"detail": "no such file"})
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
            except (ValueError, TypeError) as e:
                self.send_json(HTTPStatus.BAD_REQUEST, {"detail": f"bad request: {e}"})
                return
            if not (body["query"] if isinstance(body["query"], str) else body["query"]["vector"]):
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
<title>Slaidu meklētājs</title>
<style>
:root {
  --bg: #f4f2ee; --panel: #ffffff; --ink: #1d1b18; --muted: #6d675f; --line: #e2ddd5;
  --accent: #2f5d50; --accent-ink: #ffffff; --mark: #f3e3a6; --shadow: 0 1px 2px rgba(0,0,0,.06), 0 4px 16px rgba(0,0,0,.06);
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #161514; --panel: #201f1d; --ink: #ece8e1; --muted: #a39d94; --line: #34312d;
    --accent: #7fb8a6; --accent-ink: #10201b; --mark: #5a4d1c; --shadow: 0 1px 2px rgba(0,0,0,.4), 0 4px 16px rgba(0,0,0,.3);
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink);
  font: 15px/1.45 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
header { position: sticky; top: 0; z-index: 5; background: var(--bg); border-bottom: 1px solid var(--line); }
.bar { max-width: 1280px; margin: 0 auto; padding: 14px 16px; display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }
.bar h1 { font-size: 17px; margin: 0 8px 0 0; letter-spacing: .2px; white-space: nowrap; }
#q { flex: 1 1 280px; min-width: 0; padding: 10px 12px; font: inherit; color: var(--ink);
  background: var(--panel); border: 1px solid var(--line); border-radius: 8px; }
#q:focus, select:focus, button:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
select, button { font: inherit; color: var(--ink); background: var(--panel); border: 1px solid var(--line);
  border-radius: 8px; padding: 9px 10px; }
button { cursor: pointer; }
button.primary { background: var(--accent); color: var(--accent-ink); border-color: var(--accent); }
.seg { display: inline-flex; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
.seg button { border: 0; border-radius: 0; background: var(--panel); }
.seg button[aria-pressed="true"] { background: var(--accent); color: var(--accent-ink); }
main { max-width: 1280px; margin: 0 auto; padding: 16px; }
#status { color: var(--muted); min-height: 22px; margin-bottom: 10px; }
#status.error { color: #b3261e; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 16px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; box-shadow: var(--shadow);
  overflow: hidden; cursor: pointer; display: flex; flex-direction: column; text-align: left; padding: 0; }
.card:hover { border-color: var(--accent); }
.meta { padding: 10px 12px 12px; display: grid; gap: 4px; }
.meta .title { font-weight: 600; }
.meta .sub { color: var(--muted); font-size: 13px; display: flex; justify-content: space-between; gap: 8px; }
.snippet { font-size: 13px; color: var(--muted); display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden; }
mark { background: var(--mark); color: inherit; border-radius: 2px; }
.slide { position: relative; width: 100%; aspect-ratio: 16 / 9; background: #fff; overflow: hidden; border-bottom: 1px solid var(--line); }
.slide > img.page { width: 100%; height: 100%; object-fit: contain; display: block; }
.slide .block { position: absolute; color: #1b1b1b; white-space: pre-wrap; overflow: hidden; line-height: 1.18; }
.slide .block.heading { font-weight: 700; color: #123d33; }
.slide .pic { position: absolute; object-fit: contain; }
.more { display: flex; justify-content: center; margin: 22px 0 8px; }
dialog { width: min(1180px, 96vw); max-height: 94vh; border: 0; border-radius: 12px; padding: 0;
  background: var(--panel); color: var(--ink); box-shadow: 0 10px 40px rgba(0,0,0,.35); }
dialog::backdrop { background: rgba(0,0,0,.55); }
.dlg-head { display: flex; gap: 10px; align-items: center; padding: 12px 14px; border-bottom: 1px solid var(--line); }
.dlg-head .title { flex: 1; font-weight: 600; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dlg-body { padding: 14px; overflow: auto; max-height: calc(94vh - 60px); }
.dlg-body .slide { border: 1px solid var(--line); border-radius: 6px; }
.dlg-text { margin-top: 12px; color: var(--muted); font-size: 14px; }
.empty { text-align: center; color: var(--muted); padding: 60px 10px; }
@media (max-width: 520px) { .grid { grid-template-columns: 1fr; } .bar h1 { width: 100%; } }
</style>
</head>
<body>
<header>
  <form class="bar" id="form" autocomplete="off">
    <h1>Slaidu meklētājs</h1>
    <input id="q" type="search" placeholder="Meklēt slaidos… piem. „kā novērtēt projekta izmaksas”" autofocus>
    <select id="mode" title="Meklēšanas veids">
      <option value="hybrid">Vārdi + nozīme</option>
      <option value="meaning">Tikai nozīme</option>
      <option value="words">Tikai vārdi</option>
    </select>
    <select id="deck" title="Lekcija"><option value="">Visas lekcijas</option></select>
    <div class="seg" role="group" aria-label="Skats">
      <button type="button" data-view="picture" aria-pressed="true">Attēls</button>
      <button type="button" data-view="rebuilt" aria-pressed="false">Atjaunots</button>
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
    <button id="prev" title="Iepriekšējais slaids (←)">←</button>
    <div class="title" id="dlg-title"></div>
    <button id="next" title="Nākamais slaids (→)">→</button>
    <button id="close" title="Aizvērt (Esc)">✕</button>
  </div>
  <div class="dlg-body">
    <div id="dlg-slide"></div>
    <div class="dlg-text" id="dlg-text"></div>
  </div>
</dialog>
<script>
const PAGE_WIDTH_PT = 960;          // PDF page width the text block positions are measured in
const PAGE_SIZE = 24;
const $ = (id) => document.getElementById(id);
const state = { view: readPref("view", "picture"), text: "", offset: 0, hits: [], open: null };
const slideCache = new Map();

function readPref(key, fallback) { try { return localStorage.getItem(key) || fallback; } catch { return fallback; } }
function writePref(key, value) { try { localStorage.setItem(key, value); } catch {} }
function escapeHtml(s) { return String(s ?? "").replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c])); }
function prettyDeck(deck) { return String(deck || "").replace(/_lv$/, "").replace(/^(\d+\w?)_/, "$1 · ").replace(/[_-]/g, " "); }

function highlight(text, query) {
  const words = [...new Set(query.toLowerCase().split(/\s+/).filter(w => w.length > 2))];
  let html = escapeHtml(text);
  for (const w of words) {
    const stem = w.slice(0, Math.max(3, w.length - 2)).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    html = html.replace(new RegExp(`(${stem}[\\p{L}]*)`, "giu"), "<mark>$1</mark>");
  }
  return html;
}

async function loadSlide(deck, number) {
  const key = `${deck}/${number}`;
  if (!slideCache.has(key)) {
    const url = `/slide/${encodeURIComponent(deck)}/${number}`;
    slideCache.set(key, fetch(url).then(r => r.ok ? r.json() : null).catch(() => null));
  }
  return slideCache.get(key);
}

// Width / height of a slide, read from its page picture (decks are 16:9 or 4:3).
const aspectCache = new Map();
function slideAspect(pageImage) {
  if (!pageImage) return Promise.resolve(16 / 9);
  if (!aspectCache.has(pageImage)) {
    aspectCache.set(pageImage, new Promise(resolve => {
      const img = new Image();
      img.onload = () => resolve(img.naturalWidth && img.naturalHeight ? img.naturalWidth / img.naturalHeight : 16 / 9);
      img.onerror = () => resolve(16 / 9);
      img.src = "/" + pageImage;
    }));
  }
  return aspectCache.get(pageImage);
}

// Largest font size (px) at which the block's text still fits its box.
function fitText(el, startPx) {
  let size = startPx;
  el.style.fontSize = `${size}px`;
  while (size > 4 && (el.scrollHeight > el.clientHeight + 1 || el.scrollWidth > el.clientWidth + 1)) {
    size *= 0.92;
    el.style.fontSize = `${size}px`;
  }
}

function blockText(text) {
  // the parser puts each bullet marker on its own line: "•\nitem" -> "• item"
  return text.replace(/^([•●▪◦])\s*\n/gm, "$1 ");
}

// Rebuilds a slide from its JSON: images and text blocks at their PDF positions, scaled to the box.
async function renderRebuilt(box, slide) {
  box.innerHTML = "";
  if (!slide) { box.textContent = "Slaids nav atrasts"; return; }
  box.style.aspectRatio = String(await slideAspect(slide.page_image));
  const draw = () => {
    if (!box.clientWidth) return;
    const scale = box.clientWidth / PAGE_WIDTH_PT;
    box.innerHTML = "";
    for (const img of slide.images || []) {
      const [x0, y0, x1, y1] = img.bbox;
      const el = document.createElement("img");
      el.className = "pic"; el.loading = "lazy"; el.alt = "";
      el.src = "/" + img.file;
      Object.assign(el.style, { left: `${x0 * scale}px`, top: `${y0 * scale}px`,
        width: `${(x1 - x0) * scale}px`, height: `${(y1 - y0) * scale}px` });
      box.appendChild(el);
    }
    (slide.text_blocks || []).forEach((block, i) => {
      const [x0, y0, x1, y1] = block.bbox;
      const el = document.createElement("div");
      const text = blockText(block.text);
      const lines = Math.max(1, text.split("\n").length);
      const isHeading = i === 0 && block.text.trim() === (slide.title || "").trim();
      el.className = "block" + (isHeading ? " heading" : "");
      el.textContent = text;
      Object.assign(el.style, { left: `${x0 * scale}px`, top: `${y0 * scale}px`,
        width: `${(x1 - x0 + 8) * scale}px`, height: `${(y1 - y0 + 6) * scale}px` });
      box.appendChild(el);
      fitText(el, Math.min(32, ((y1 - y0) / lines) * 0.9) * scale);
    });
  };
  draw();
  let lastWidth = box.clientWidth;
  new ResizeObserver(() => { if (box.clientWidth !== lastWidth) { lastWidth = box.clientWidth; draw(); } }).observe(box);
}

function renderPicture(box, hit) {
  box.innerHTML = "";
  slideAspect(hit.page_image).then(aspect => { box.style.aspectRatio = String(aspect); });
  const img = document.createElement("img");
  img.className = "page"; img.loading = "lazy"; img.alt = hit.title || "";
  img.src = "/" + hit.page_image;
  img.onerror = () => { renderSlideInto(box, hit, "rebuilt"); };
  box.appendChild(img);
}

async function renderSlideInto(box, hit, view) {
  if (view === "picture" && hit.page_image) { renderPicture(box, hit); return; }
  renderRebuilt(box, await loadSlide(hit.deck, hit.slide_number));
}

function card(hit, index) {
  const el = document.createElement("button");
  el.type = "button"; el.className = "card";
  el.innerHTML = `
    <div class="slide"></div>
    <div class="meta">
      <div class="title">${highlight(hit.title || "(bez virsraksta)", state.text)}</div>
      <div class="sub"><span>${escapeHtml(prettyDeck(hit.deck))} · slaids ${hit.slide_number}</span>
        <span title="Rezultāta vērtējums">${hit.score.toFixed(3)}</span></div>
      <div class="snippet">${highlight(hit.content || "", state.text)}</div>
    </div>`;
  renderSlideInto(el.querySelector(".slide"), hit, state.view);
  el.addEventListener("click", () => openSlide(index));
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
    const response = await fetch("/search", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: state.text, mode: $("mode").value, deck: $("deck").value,
        docs: PAGE_SIZE, offset: state.offset }),
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) { setStatus(body.detail || body.title || `Kļūda (HTTP ${response.status})`, true); return; }
    const hits = (body.data || []).map(h => ({ ...h.data, _id: h.id, score: h.score ?? 0 }));
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
      setStatus(`${state.hits.length}${more ? "+" : ""} slaidi · ${body.time_taken || ""}`);
    }
    $("more").hidden = !more;
  } catch (e) {
    setStatus("Serveris neatbild: " + e.message, true);
  }
}

// The dialog walks through the whole lecture, not only the hits.
async function openSlide(index) {
  const hit = state.hits[index];
  state.open = { deck: hit.deck, number: hit.slide_number };
  await showOpen();
  if (!$("dlg").open) $("dlg").showModal();
}

async function showOpen() {
  const { deck, number } = state.open;
  const slide = await loadSlide(deck, number);
  $("dlg-title").textContent = slide
    ? `${prettyDeck(deck)} · ${number}. ${slide.title || ""}`
    : `${prettyDeck(deck)} · ${number}. slaids nav atrasts`;
  const box = document.createElement("div");
  box.className = "slide";
  $("dlg-slide").replaceChildren(box);
  const hit = { deck, slide_number: number, page_image: slide?.page_image, title: slide?.title };
  await renderSlideInto(box, hit, state.view);
  $("dlg-text").textContent = slide ? (slide.text_blocks || []).map(b => b.text.replace(/\s*\n\s*/g, " ")).join(" · ") : "";
  $("prev").disabled = number <= 1;
  // the last slide of a lecture has no next one: that lookup is expected to 404
  $("next").disabled = !(await loadSlide(deck, number + 1));
}

async function step(delta) {
  if (!state.open) return;
  const number = state.open.number + delta;
  if (number < 1 || !(await loadSlide(state.open.deck, number))) return;
  state.open.number = number;
  await showOpen();
}

function setView(view) {
  state.view = view; writePref("view", view);
  document.querySelectorAll("[data-view]").forEach(b => b.setAttribute("aria-pressed", String(b.dataset.view === view)));
  document.querySelectorAll(".card").forEach((el, i) => renderSlideInto(el.querySelector(".slide"), state.hits[i], view));
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
$("close").addEventListener("click", () => $("dlg").close());
$("dlg").addEventListener("click", (e) => { if (e.target === $("dlg")) $("dlg").close(); });
document.addEventListener("keydown", (e) => {
  if (!$("dlg").open) return;
  if (e.key === "ArrowLeft") step(-1);
  if (e.key === "ArrowRight") step(1);
});
setView(state.view);

fetch("/decks").then(r => r.json()).then(decks => {
  for (const deck of decks) {
    const option = document.createElement("option");
    option.value = deck; option.textContent = prettyDeck(deck);
    $("deck").appendChild(option);
  }
}).catch(() => {});
</script>
</body>
</html>
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("folder", help="slide folder (the *_slideNN.json files and assets/)")
    parser.add_argument("--db", default="slaidi", help="corelamo database (default: slaidi)")
    parser.add_argument("--port", type=int, default=8080, help="port of this viewer (default: 8080)")
    parser.add_argument("--corelamo", default=os.environ.get("CORELAMO_URL", "http://localhost:6006"),
                        help="corelamo address (default: http://localhost:6006)")
    args = parser.parse_args()

    folder = os.path.expanduser(args.folder)
    if not os.path.isdir(os.path.join(folder, "assets")):
        sys.exit(f"[FATAL] {folder} has no assets/ folder")
    handler = make_handler(folder, Corelamo(args.corelamo, args.db))
    server = ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    print(f"[INFO] slide viewer on http://localhost:{args.port}  (database '{args.db}' at {args.corelamo})")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[INFO] stopped")


if __name__ == "__main__":
    main()