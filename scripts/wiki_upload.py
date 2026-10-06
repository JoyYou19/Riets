#!/usr/bin/env python3
"""
Chunk-aware Corelamo uploader for Wikipedia-style datasets.

For every chunk file the uploader:
  1. reads the chunk and measures its average document size,
  2. derives the batch size (docs per request) from a target payload size,
  3. admits requests under a byte budget, so the number of concurrent inserts
     adapts automatically: small docs -> many in flight, heavy docs -> few.

The database is NOT created, started or configured; it must already exist.

Usage
-----
python wiki_uploader.py \
    --input wikipedia/chunks \
    --db wiki \
    --target-payload-mb 60 \
    --inflight-budget-mb 256 \
    --max-in-flight 16
"""

import argparse
import glob
import http.client
import json
import math
import os
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from dataclasses import dataclass, field
from typing import Iterator, List, Tuple

MB = 1024 * 1024

DEFAULT_BASE_URL = "http://localhost:6006"
DEFAULT_PATTERNS = ("*.json", "*.jsonl", "*.ndjson")


# ─────────────────────────────────────────────────────────────────────────────
# CLI
# ─────────────────────────────────────────────────────────────────────────────

def parse_args():
    parser = argparse.ArgumentParser(
        description="Upload chunked documents to Corelamo, sizing batches and "
                    "concurrency from each chunk's average document size.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--input", "-i", required=True,
                        help="Directory of chunk files (NDJSON or JSON array), or a single chunk file.")
    parser.add_argument("--db", "-d", required=True, help="Existing database name.")
    parser.add_argument("--base-url", "-u", default=DEFAULT_BASE_URL,
                        help=f"Server base URL (default: {DEFAULT_BASE_URL}).")
    parser.add_argument("--pattern", default=None,
                        help="Glob for chunk files inside --input "
                             f"(default: {', '.join(DEFAULT_PATTERNS)}).")
    parser.add_argument("--max-chunks", type=int, default=0,
                        help="Upload only the first N chunk files (0 = all).")

    parser.add_argument("--target-payload-mb", type=float, default=32.0,
                        help="Target request payload size in MB (default: 32).")
    parser.add_argument("--min-batch-docs", type=int, default=1,
                        help="Lower clamp for docs per request (default: 1).")
    parser.add_argument("--max-batch-docs", type=int, default=250_000,
                        help="Upper clamp for docs per request (default: 250,000).")

    parser.add_argument("--inflight-budget-mb", type=float, default=256.0,
                        help="Max total payload MB allowed in flight at once (default: 256).")
    parser.add_argument("--min-in-flight", type=int, default=1,
                        help="Concurrency floor, regardless of budget (default: 1).")
    parser.add_argument("--max-in-flight", type=int, default=16,
                        help="Concurrency ceiling (default: 16).")

    parser.add_argument("--target-latency", type=float, default=30.0,
                        help="Per-request latency (s) the budget is tuned against (default: 30).")
    parser.add_argument("--request-timeout", type=float, default=600.0,
                        help="HTTP request timeout in seconds (default: 600).")
    parser.add_argument("--report-interval", type=float, default=5.0,
                        help="Seconds between progress lines (default: 5).")
    parser.add_argument("--dry-run", action="store_true",
                        help="Read chunks and plan batches, but do not POST anything.")

    args = parser.parse_args()

    if args.target_payload_mb <= 0 or args.inflight_budget_mb <= 0:
        parser.error("--target-payload-mb and --inflight-budget-mb must be > 0")
    if args.min_batch_docs < 1 or args.max_batch_docs < args.min_batch_docs:
        parser.error("require 1 <= --min-batch-docs <= --max-batch-docs")
    if args.min_in_flight < 1 or args.max_in_flight < args.min_in_flight:
        parser.error("require 1 <= --min-in-flight <= --max-in-flight")
    if args.target_latency <= 0 or args.request_timeout <= 0 or args.report_interval <= 0:
        parser.error("--target-latency, --request-timeout and --report-interval must be > 0")
    if args.max_chunks < 0:
        parser.error("--max-chunks must be >= 0")
    return args


# ─────────────────────────────────────────────────────────────────────────────
# HTTP
# ─────────────────────────────────────────────────────────────────────────────

def post(base_url: str, path: str, body: bytes, timeout: float) -> dict:
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=body,
        method="POST",
        headers={"Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            text = response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", errors="replace")
    except urllib.error.URLError as e:
        return {"error": str(e.reason)}
    except TimeoutError:
        return {"error": f"request timed out after {timeout}s"}
    except (http.client.HTTPException, OSError) as e:
        return {"error": f"{type(e).__name__}: {e}"}

    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        return {"error": text}
    return parsed if isinstance(parsed, dict) else {"error": text}


def timed_post(base_url: str, path: str, body: bytes, timeout: float) -> Tuple[dict, float]:
    t0 = time.monotonic()
    reply = post(base_url, path, body, timeout)
    return reply, time.monotonic() - t0


# ─────────────────────────────────────────────────────────────────────────────
# Chunk reading and batch planning
# ─────────────────────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class Batch:
    chunk: str
    part: int
    parts: int
    docs: int
    payload: bytes
    chunk_avg_doc_bytes: float
    planned_batch_docs: int

    @property
    def label(self) -> str:
        return f"{self.chunk}[{self.part}/{self.parts}]"


@dataclass
class Stats:
    sent_docs: int = 0
    sent_bytes: int = 0
    inserted: int = 0
    failed_docs: int = 0
    failed_batches: List[str] = field(default_factory=list)
    failed_chunks: List[str] = field(default_factory=list)
    requests_done: int = 0


def read_chunk_docs(path: str) -> List[bytes]:
    """Return one compact JSON object (bytes) per document in the chunk.

    Supports NDJSON (one object per line) and a single top-level JSON array.
    Malformed NDJSON lines are skipped with a warning.
    """
    with open(path, "rb") as f:
        raw = f.read()

    if raw.lstrip()[:1] == b"[":
        items = json.loads(raw)
        if not isinstance(items, list):
            raise ValueError("top-level JSON value is not an array")
        docs = []
        for index, item in enumerate(items):
            if not isinstance(item, dict):
                print(f"[WARN] {os.path.basename(path)}: item {index} is not a JSON object, skipped")
                continue
            docs.append(json.dumps(item, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
        return docs

    docs = []
    for line_no, line in enumerate(raw.splitlines(), start=1):
        line = line.strip()
        if not line:
            continue
        if not (line.startswith(b"{") and line.endswith(b"}")):
            print(f"[WARN] {os.path.basename(path)}: line {line_no} is not a JSON object, skipped")
            continue
        docs.append(line)
    return docs


def plan_batch_docs(n_docs: int, avg_doc_bytes: float, target_payload_bytes: float,
                    min_docs: int, max_docs: int) -> int:
    """Docs per request so that payload ~= target, clamped, and evenly balanced
    across the chunk so the last request is not a tiny remainder."""
    per_batch = int(target_payload_bytes // max(avg_doc_bytes, 1.0))
    per_batch = max(min_docs, min(max_docs, per_batch))
    per_batch = min(per_batch, n_docs)
    parts = math.ceil(n_docs / per_batch)
    return math.ceil(n_docs / parts)


def human_bytes(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if abs(n) < 1024:
            return f"{n:.2f} {unit}"
        n /= 1024
    return f"{n:.2f} TB"


def generate_batches(files: List[str], args, flow: "FlowController", stats: Stats) -> Iterator[Batch]:
    target_payload_bytes = args.target_payload_mb * MB
    total_files = len(files)

    for file_index, path in enumerate(files, start=1):
        name = os.path.basename(path)
        try:
            docs = read_chunk_docs(path)
        except (OSError, ValueError, json.JSONDecodeError) as e:
            print(f"[ERROR] chunk {name}: cannot read ({e})", flush=True)
            stats.failed_chunks.append(name)
            continue

        if not docs:
            print(f"[WARN] chunk {name}: no valid documents, skipped", flush=True)
            continue

        n_docs = len(docs)
        avg_doc_bytes = sum(len(d) for d in docs) / n_docs
        batch_docs = plan_batch_docs(
            n_docs, avg_doc_bytes, target_payload_bytes,
            args.min_batch_docs, args.max_batch_docs,
        )
        parts = math.ceil(n_docs / batch_docs)
        est_payload = avg_doc_bytes * batch_docs
        print(
            f"[chunk {file_index}/{total_files}] {name}: {n_docs:,} docs, "
            f"avg {human_bytes(avg_doc_bytes)} -> batch={batch_docs:,} docs "
            f"(~{human_bytes(est_payload)}) x {parts} request(s), "
            f"in-flight limit ~{flow.limit_for(est_payload)}",
            flush=True,
        )

        for part in range(parts):
            start = part * batch_docs
            piece = docs[start:start + batch_docs]
            payload = b"[" + b",".join(piece) + b"]"
            yield Batch(
                chunk=name,
                part=part + 1,
                parts=parts,
                docs=len(piece),
                payload=payload,
                chunk_avg_doc_bytes=avg_doc_bytes,
                planned_batch_docs=batch_docs,
            )
        del docs


# ─────────────────────────────────────────────────────────────────────────────
# Concurrency control
# ─────────────────────────────────────────────────────────────────────────────

class FlowController:
    """Byte-budget admission control with AIMD feedback.

    A request is admitted if the total bytes already in flight plus its own
    payload fit in the (scaled) budget. Heavy payloads therefore get fewer
    concurrent requests and light payloads get more. Failures or very slow
    responses halve the scale; healthy responses restore it gradually.
    Only ever used from the main thread.
    """

    MIN_SCALE = 0.125
    RECOVERY_STEP = 0.05

    def __init__(self, budget_bytes: float, min_in_flight: int, max_in_flight: int,
                 target_latency: float):
        self.budget_bytes = budget_bytes
        self.min_in_flight = min_in_flight
        self.max_in_flight = max_in_flight
        self.target_latency = target_latency
        self.scale = 1.0

    @property
    def budget(self) -> float:
        return self.budget_bytes * self.scale

    def limit_for(self, payload_bytes: float) -> int:
        by_budget = int(self.budget // max(payload_bytes, 1.0))
        return max(self.min_in_flight, min(self.max_in_flight, by_budget))

    def can_submit(self, in_flight_count: int, in_flight_bytes: int, payload_bytes: int) -> bool:
        if in_flight_count == 0:
            return True
        if in_flight_count >= self.max_in_flight:
            return False
        if in_flight_count < self.min_in_flight:
            return True
        return in_flight_bytes + payload_bytes <= self.budget

    def observe(self, duration: float, ok: bool) -> None:
        if not ok or duration > self.target_latency * 2:
            self.scale = max(self.MIN_SCALE, self.scale * 0.5)
        elif duration <= self.target_latency:
            self.scale = min(1.0, self.scale + self.RECOVERY_STEP)


# ─────────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────────

def collect_files(args) -> List[str]:
    if os.path.isfile(args.input):
        return [args.input]
    if not os.path.isdir(args.input):
        print(f"[ERROR] Input path does not exist: {args.input}")
        sys.exit(1)

    patterns = (args.pattern,) if args.pattern else DEFAULT_PATTERNS
    found = set()
    for pattern in patterns:
        found.update(glob.glob(os.path.join(args.input, pattern)))
    files = sorted(found)
    if args.max_chunks > 0:
        files = files[:args.max_chunks]
    return files


def main() -> int:
    args = parse_args()

    files = collect_files(args)
    if not files:
        print(f"[ERROR] No chunk files found in {args.input}.")
        return 1

    flow = FlowController(
        budget_bytes=args.inflight_budget_mb * MB,
        min_in_flight=args.min_in_flight,
        max_in_flight=args.max_in_flight,
        target_latency=args.target_latency,
    )
    stats = Stats()
    insert_path = f"/api/databases/{args.db}/insert"

    print(
        f"[INFO] {len(files)} chunk file(s) -> {args.base_url}{insert_path} | "
        f"target payload {args.target_payload_mb:g} MB, in-flight budget "
        f"{args.inflight_budget_mb:g} MB, in-flight {args.min_in_flight}..{args.max_in_flight}"
        f"{' | DRY RUN' if args.dry_run else ''}",
        flush=True,
    )

    start = time.monotonic()
    last_report = start
    batches = generate_batches(files, args, flow, stats)
    next_batch = None
    exhausted = False
    pending = {}            # future -> Batch
    in_flight_bytes = 0
    last_avg_doc_bytes = 0.0
    last_batch_docs = 0

    def report(force: bool = False) -> None:
        nonlocal last_report
        now = time.monotonic()
        if not force and now - last_report < args.report_interval:
            return
        last_report = now
        elapsed = max(now - start, 1e-9)
        print(
            f"[progress] inserted {stats.inserted:>12,} / sent {stats.sent_docs:>12,} "
            f"| {stats.inserted / elapsed:>10,.0f} docs/s "
            f"| {stats.sent_bytes / MB / elapsed:>7.2f} MB/s "
            f"| flight={len(pending):>2} ({human_bytes(in_flight_bytes)}) "
            f"budget={human_bytes(flow.budget)} "
            f"| avg doc {human_bytes(last_avg_doc_bytes)} batch={last_batch_docs:,} "
            f"| {elapsed:>7.1f}s",
            flush=True,
        )

    with ThreadPoolExecutor(max_workers=args.max_in_flight) as pool:
        while True:
            # Admit as many batches as the byte budget allows.
            while not exhausted:
                if next_batch is None:
                    try:
                        next_batch = next(batches)
                    except StopIteration:
                        exhausted = True
                        break

                if not flow.can_submit(len(pending), in_flight_bytes, len(next_batch.payload)):
                    break

                batch = next_batch
                next_batch = None
                last_avg_doc_bytes = batch.chunk_avg_doc_bytes
                last_batch_docs = batch.planned_batch_docs
                stats.sent_docs += batch.docs
                stats.sent_bytes += len(batch.payload)

                if args.dry_run:
                    print(f"[DRY RUN] {batch.label}: {batch.docs:,} docs, {human_bytes(len(batch.payload))}")
                    continue

                future = pool.submit(timed_post, args.base_url, insert_path,
                                     batch.payload, args.request_timeout)
                pending[future] = batch
                in_flight_bytes += len(batch.payload)

            if not pending:
                if exhausted:
                    break
                continue

            done, _ = wait(list(pending), timeout=args.report_interval,
                           return_when=FIRST_COMPLETED)

            for future in done:
                batch = pending.pop(future)
                in_flight_bytes -= len(batch.payload)
                stats.requests_done += 1

                try:
                    reply, duration = future.result()
                except Exception as e:  # defensive: worker raised unexpectedly
                    reply, duration = {"error": f"{type(e).__name__}: {e}"}, 0.0

                data = reply.get("data") or {}
                ok = "error" not in reply and "inserted" in data
                flow.observe(duration, ok)

                if not ok:
                    stats.failed_docs += batch.docs
                    stats.failed_batches.append(batch.label)
                    print(f"[ERROR] {batch.label} ({duration:.1f}s): {reply}", flush=True)
                    continue

                got = int(data["inserted"])
                stats.inserted += got
                missing = batch.docs - got
                if missing > 0:
                    stats.failed_docs += missing
                    stats.failed_batches.append(batch.label)
                    print(f"[WARN] {batch.label}: server inserted {got:,} of "
                          f"{batch.docs:,} docs ({missing:,} failed)", flush=True)

            report()

    report(force=True)
    duration = max(time.monotonic() - start, 1e-9)

    print(
        f"\n[INFO] Done: {stats.inserted:,} documents inserted in {duration:.2f}s "
        f"({stats.inserted / duration:,.0f} docs/s, "
        f"{stats.sent_bytes / MB / duration:.2f} MB/s), "
        f"{stats.requests_done} request(s)"
    )

    if stats.failed_chunks:
        print(f"[ERROR] {len(stats.failed_chunks)} unreadable chunk(s): "
              + ", ".join(stats.failed_chunks))
    if stats.failed_batches:
        print(f"[ERROR] {stats.failed_docs:,} document(s) failed in "
              f"{len(stats.failed_batches)} batch(es), no automatic retry (a timed-out request may "
              f"have been applied): " + ", ".join(stats.failed_batches))
    return 1 if (stats.failed_batches or stats.failed_chunks) else 0


if __name__ == "__main__":
    sys.exit(main())