#!/usr/bin/env python3
"""
Adaptive Corelamo uploader with predictive + aggressive parameter search.

Usage
-----
python uploader.py \
    --input fever/corpus.jsonl \
    --db fever \
    --policy-file policies/fever.toml \
    --add-random-fields \
    --adaptive \
    --max-in-flight 32 \
    --max-batch-size 150000
"""

import argparse
import glob
import json
import math
import os
import random
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import deque
from concurrent.futures import ThreadPoolExecutor

DEFAULT_BASE_URL = "http://localhost:6006"
DEFAULT_SHARD_COUNT = 4

DEFAULT_MIN_BATCH_SIZE = 500
DEFAULT_MAX_BATCH_SIZE = 250_000
DEFAULT_MAX_IN_FLIGHT = 64

DEFAULT_TARGET_PAYLOAD_MB = 5.0
DEFAULT_ADAPT_INTERVAL = 4.0
DEFAULT_TARGET_LATENCY = 8.0
DEFAULT_REQUEST_TIMEOUT = 600

IMPROVEMENT_PCT = 3.0          # need 3% improvement to keep a change
EXPLORATION_PCT = 10.0         # random perturbation magnitude around best
MIN_SAMPLES = 4


def parse_args():
    parser = argparse.ArgumentParser(
        description="Upload documents to Corelamo, optionally auto-tuning for speed.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )

    parser.add_argument("--input", "-i", required=True,
                        help="JSONL file OR directory containing *.json chunk files.")
    parser.add_argument("--db", "-d", required=True, help="Database name.")
    parser.add_argument("--base-url", "-u", default=DEFAULT_BASE_URL,
                        help=f"Server base URL (default: {DEFAULT_BASE_URL}).")

    parser.add_argument("--policy", "-p", default=None,
                        help="Policy as an inline TOML string.")
    parser.add_argument("--policy-file", "-P", default=None,
                        help="Path to a TOML policy file.")

    parser.add_argument("--shard-count", "-s", type=int, default=DEFAULT_SHARD_COUNT,
                        help=f"Number of shards (default: {DEFAULT_SHARD_COUNT}).")
    parser.add_argument("--skip-db-setup", action="store_true",
                        help="Skip create/start/policy steps and only upload data.")

    parser.add_argument("--add-random-fields", action="store_true",
                        help="Add random_year and random_float fields to every JSONL document.")
    parser.add_argument("--max-chunks", type=int, default=0,
                        help="For directory input: upload only first N chunk files (0 = all).")

    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--fixed", action="store_true",
                      help="Use fixed batch size and in-flight (default when --adaptive is omitted).")
    mode.add_argument("--adaptive", action="store_true",
                      help="Auto-tune batch size and/or concurrency for maximum insert speed.")

    parser.add_argument("--batch-size", "-b", type=int, default=None,
                        help="Initial batch size for JSONL (default: auto from file size).")
    parser.add_argument("--in-flight", "-f", type=int, default=None,
                        help="Initial concurrent requests (default: auto from file size).")

    parser.add_argument("--min-batch-size", type=int, default=DEFAULT_MIN_BATCH_SIZE,
                        help=f"Minimum JSONL batch size (default: {DEFAULT_MIN_BATCH_SIZE:,}).")
    parser.add_argument("--max-batch-size", type=int, default=DEFAULT_MAX_BATCH_SIZE,
                        help=f"Maximum JSONL batch size (default: {DEFAULT_MAX_BATCH_SIZE:,}).")
    parser.add_argument("--max-in-flight", type=int, default=DEFAULT_MAX_IN_FLIGHT,
                        help=f"Maximum concurrent requests (default: {DEFAULT_MAX_IN_FLIGHT}).")
    parser.add_argument("--target-payload-mb", type=float, default=DEFAULT_TARGET_PAYLOAD_MB,
                        help=f"Target MB per insert request for initial guess "
                             f"(default: {DEFAULT_TARGET_PAYLOAD_MB}).")
    parser.add_argument("--adapt-interval", type=float, default=DEFAULT_ADAPT_INTERVAL,
                        help=f"Seconds between tuning decisions (default: {DEFAULT_ADAPT_INTERVAL}).")
    parser.add_argument("--target-latency", type=float, default=DEFAULT_TARGET_LATENCY,
                        help=f"Target p95 request latency in seconds (default: {DEFAULT_TARGET_LATENCY}).")
    parser.add_argument("--request-timeout", type=float, default=DEFAULT_REQUEST_TIMEOUT,
                        help=f"HTTP request timeout in seconds (default: {DEFAULT_REQUEST_TIMEOUT}).")

    parser.add_argument("--state-file", default=None,
                        help="File to load/save best adaptive settings "
                             "(default: .corelamo_uploader_<db>.json).")
    parser.add_argument("--reset-state", action="store_true",
                        help="Ignore any previously saved best config and start fresh.")

    parser.add_argument("--dry-run", action="store_true",
                        help="Parse input and build batches but do not POST them.")

    return parser.parse_args()


# ─────────────────────────────────────────────────────────────────────────────
# HTTP helpers
# ─────────────────────────────────────────────────────────────────────────────

def post(base_url, path, body, timeout):
    data = body.encode("utf-8") if isinstance(body, str) else body
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        method="POST",
        headers={"Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            text = response.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", errors="replace")
    except urllib.error.URLError as e:
        return {"error": str(e.reason)}
    except TimeoutError:
        return {"error": f"request timed out after {timeout}s"}

    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return {"error": text}


def setup_database(base_url, db_name, shard_count, policy):
    print(f"[INFO] Creating database '{db_name}'...")
    print(post(base_url, f"/api/databases/{db_name}/create-database",
               json.dumps({"shard_count": shard_count}), timeout=30).get("title"))

    print(f"[INFO] Starting database '{db_name}'...")
    print(post(base_url, f"/api/databases/{db_name}/start-database", "", timeout=30).get("title"))

    print("[INFO] Setting policy...")
    print(post(base_url, f"/api/databases/{db_name}/set-policy", policy, timeout=30).get("title"))


# ─────────────────────────────────────────────────────────────────────────────
# Persistent state
# ─────────────────────────────────────────────────────────────────────────────

class StateStore:
    def __init__(self, db_name, path=None):
        self.path = path or f".corelamo_uploader_{db_name}.json"

    def load(self):
        if not os.path.exists(self.path):
            return None
        try:
            with open(self.path, "r", encoding="utf-8") as f:
                return json.load(f)
        except (json.JSONDecodeError, OSError):
            return None

    def save(self, best_batch_size, best_in_flight, best_throughput, best_latency):
        record = {
            "best_batch_size": best_batch_size,
            "best_in_flight": best_in_flight,
            "best_throughput": best_throughput,
            "best_latency": best_latency,
            "saved_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
        }
        with open(self.path, "w", encoding="utf-8") as f:
            json.dump(record, f, indent=2)
        return record


# ─────────────────────────────────────────────────────────────────────────────
# Predictive + aggressive adaptive controller
# ─────────────────────────────────────────────────────────────────────────────

class AdaptiveController:
    """
    Search strategy
    ---------------
    1.  INITIAL GUESS based on sampled average document size.
    2.  EXPLORE in_flight exponentially (1, 2, 4, 8, 12, 16, 24, 32 ...)
        and keep the value with the highest throughput.
    3.  EXPLORE batch_size on a coarse log grid across [min, max],
        then narrow around the winner.
    4.  PERTURB the best known pair with random ±10% jumps and
        keep any improvement.
    5.  After exploration, lock the best settings for the rest of the upload.
    """

    def __init__(
        self,
        is_chunk_mode,
        initial_batch_size,
        initial_in_flight,
        min_batch_size,
        max_batch_size,
        max_in_flight,
        target_payload_bytes,
        adapt_interval,
        target_latency,
        saved_state,
    ):
        self.is_chunk_mode = is_chunk_mode
        self.adapt_interval = adapt_interval
        self.target_latency = target_latency

        self.min_batch_size = min_batch_size
        self.max_batch_size = max_batch_size
        self.max_in_flight = max_in_flight
        self.target_payload_bytes = target_payload_bytes

        self.best_throughput = 0.0
        self.best_latency = float("inf")
        self.best_batch_size = initial_batch_size
        self.best_in_flight = initial_in_flight

        self._lock = threading.RLock()

        # Saved state overrides defaults.
        if saved_state:
            loaded_batch = saved_state.get("best_batch_size", initial_batch_size)
            loaded_flight = saved_state.get("best_in_flight", initial_in_flight)
            self.batch_size = self._clamp_batch(loaded_batch)
            self.in_flight = self._clamp_flight(loaded_flight)
            print(
                f"[STATE] Loaded previous best: batch_size={self.batch_size:,}, "
                f"in_flight={self.in_flight}, "
                f"throughput={saved_state.get('best_throughput', 0):,.0f} docs/s"
            )
        else:
            self.batch_size = initial_batch_size
            self.in_flight = initial_in_flight

        self.phase = "warmup"
        self.state = "measuring"

        # Exploration queues
        self.in_flight_candidates = self._make_in_flight_candidates()
        self.batch_size_candidates = self._make_batch_size_candidates()

        self.candidate_param = None
        self.candidate_value = None
        self.measured = {}          # value -> metrics dict
        self._history = deque()
        self._last_adapt = time.time()

    def _clamp_batch(self, v):
        return max(self.min_batch_size, min(self.max_batch_size, int(v)))

    def _clamp_flight(self, v):
        return max(1, min(self.max_in_flight, int(v)))

    @property
    def current_batch_size(self):
        with self._lock:
            return self.batch_size

    @property
    def current_in_flight(self):
        with self._lock:
            return self.in_flight

    @property
    def exploring(self):
        with self._lock:
            return self.phase != "locked"

    def record(self, duration, docs, bytes_sent, success):
        self._history.append({
            "time": time.time(),
            "duration": duration,
            "docs": docs,
            "bytes": bytes_sent,
            "success": success,
        })

    def _metrics(self):
        recent_ok = [h for h in self._history if h["success"]]
        recent_all = list(self._history)
        if len(recent_ok) < MIN_SAMPLES or not recent_all:
            return None
        elapsed = recent_ok[-1]["time"] - recent_ok[0]["time"]
        if elapsed <= 0:
            return None
        throughput = sum(h["docs"] for h in recent_ok) / elapsed
        durations = sorted(h["duration"] for h in recent_ok)
        p95 = durations[int(len(durations) * 0.95)]
        errors = sum(1 for h in recent_all if not h["success"])
        error_rate = errors / len(recent_all)
        return {
            "throughput": throughput,
            "p95_latency": p95,
            "error_rate": error_rate,
            "samples": len(recent_ok),
        }

    def _make_in_flight_candidates(self):
        """Exponential candidate list: 1, 2, 4, 8, 12, 16, 24, 32 ..."""
        cands = [1, 2]
        v = 4
        while v <= self.max_in_flight:
            cands.append(v)
            if v < 12:
                v += 4
            else:
                v = int(v * 1.5)
        return [c for c in cands if c <= self.max_in_flight]

    def _make_batch_size_candidates(self):
        """Log-spaced grid across the whole allowed range."""
        if self.is_chunk_mode:
            return []
        lo = math.log10(self.min_batch_size)
        hi = math.log10(self.max_batch_size)
        steps = 7
        grid = sorted({
            self._clamp_batch(10 ** (lo + (hi - lo) * i / (steps - 1)))
            for i in range(steps)
        })
        return grid

    def _set_candidate(self, param, value):
        with self._lock:
            if param == "in_flight":
                self.in_flight = self._clamp_flight(value)
            else:
                self.batch_size = self._clamp_batch(value)
            self.candidate_param = param
            self.candidate_value = getattr(self, param)
            self._history.clear()
            self.state = "measuring"

    def _update_best(self, metrics):
        with self._lock:
            if metrics["throughput"] > self.best_throughput:
                self.best_throughput = metrics["throughput"]
                self.best_latency = metrics["p95_latency"]
                self.best_batch_size = self.batch_size
                self.best_in_flight = self.in_flight

    def _lock_best(self):
        with self._lock:
            self.batch_size = self.best_batch_size
            self.in_flight = self.best_in_flight
            self.phase = "locked"

    def _start_perturb(self):
        with self._lock:
            self.phase = "perturb"
            self.state = "measuring"
            self._history.clear()

    def _apply_perturb(self, current_metrics):
        with self._lock:
            # Decide whether last perturbation was an improvement.
            if current_metrics["throughput"] > self.best_throughput * (1 + IMPROVEMENT_PCT / 100):
                self._update_best(current_metrics)
                # Keep perturbing in the same direction from here.
                pass
            else:
                # Revert to best and perturb differently next time.
                self.batch_size = self.best_batch_size
                self.in_flight = self.best_in_flight

            # Generate next perturbation.
            rng = random.Random()
            new_batch = self.best_batch_size
            new_flight = self.best_in_flight

            if not self.is_chunk_mode:
                factor = 1 + rng.uniform(-EXPLORATION_PCT, EXPLORATION_PCT) / 100
                new_batch = self._clamp_batch(self.best_batch_size * factor)

            if rng.random() < 0.7:
                factor = 1 + rng.uniform(-EXPLORATION_PCT, EXPLORATION_PCT) / 100
                new_flight = self._clamp_flight(self.best_in_flight * factor)

            if new_batch == self.batch_size and new_flight == self.in_flight:
                # No change; perturb again.
                return

            self.batch_size = new_batch
            self.in_flight = new_flight
            self._history.clear()
            self.state = "measuring"

    def adapt(self):
        with self._lock:
            now = time.time()
            if now - self._last_adapt < self.adapt_interval:
                return None
            self._last_adapt = now

            metrics = self._metrics()
            if metrics is None:
                return {
                    "action": "collecting",
                    "phase": self.phase,
                    "changed": False,
                    "batch_size": self.batch_size,
                    "in_flight": self.in_flight,
                }

            throughput = metrics["throughput"]
            latency = metrics["p95_latency"]
            error_rate = metrics["error_rate"]

            decision = {
                "throughput": throughput,
                "latency": latency,
                "error_rate": error_rate,
                "batch_size": self.batch_size,
                "in_flight": self.in_flight,
                "phase": self.phase,
                "action": "measure",
                "changed": False,
            }

            # Safety: errors or huge latency -> revert to best and slow down.
            if error_rate > 0.05 or latency > self.target_latency * 2.5:
                self.batch_size = self.best_batch_size
                self.in_flight = max(1, self.best_in_flight - 1)
                self._history.clear()
                decision["action"] = "safety_revert"
                decision["changed"] = True
                return decision

            if self.phase == "warmup":
                self._update_best(metrics)
                self.phase = "in_flight"
                self.state = "ready"
                return decision

            # ── PHASE: explore in_flight ─────────────────────────────────────
            if self.phase == "in_flight":
                if self.state == "ready":
                    if not self.in_flight_candidates:
                        # Move on to batch-size search.
                        self.in_flight = self.best_in_flight
                        self.batch_size = self.best_batch_size
                        if self.is_chunk_mode:
                            self._lock_best()
                            decision["action"] = "locked (chunk mode)"
                            return decision
                        self.phase = "batch_size"
                        self.state = "ready"
                        return decision

                    value = self.in_flight_candidates.pop(0)
                    self._set_candidate("in_flight", value)
                    decision["action"] = f"try_in_flight_{value}"
                    decision["changed"] = True
                    decision["in_flight"] = self.in_flight
                    return decision

                # state == measuring
                self.measured[self.candidate_value] = metrics
                self._update_best(metrics)
                self.state = "ready"
                decision["action"] = "in_flight_measured"
                return decision

            # ── PHASE: explore batch_size on log grid ────────────────────────
            if self.phase == "batch_size":
                if self.state == "ready":
                    if not self.batch_size_candidates:
                        # Grid exhausted; pick best and start perturbing.
                        best = max(self.measured.items(), key=lambda x: x[1]["throughput"])
                        self.batch_size = best[0]
                        self.in_flight = self.best_in_flight
                        self._update_best(best[1])
                        self._start_perturb()
                        decision["action"] = "start_perturb"
                        decision["changed"] = True
                        return decision

                    value = self.batch_size_candidates.pop(0)
                    self._set_candidate("batch_size", value)
                    decision["action"] = f"try_batch_size_{value:,}"
                    decision["changed"] = True
                    decision["batch_size"] = self.batch_size
                    return decision

                self.measured[self.candidate_value] = metrics
                self._update_best(metrics)
                self.state = "ready"
                decision["action"] = "batch_size_measured"
                return decision

            # ── PHASE: random perturbation around best ───────────────────────
            if self.phase == "perturb":
                self._apply_perturb(metrics)
                decision["action"] = "perturb"
                decision["changed"] = True
                decision["batch_size"] = self.batch_size
                decision["in_flight"] = self.in_flight

                # After enough perturbations with no improvement, lock in.
                if self.best_throughput > 0 and throughput < self.best_throughput * 1.01:
                    self.perturb_stagnant = getattr(self, "perturb_stagnant", 0) + 1
                else:
                    self.perturb_stagnant = 0

                if getattr(self, "perturb_stagnant", 0) >= 4:
                    self._lock_best()
                    decision["action"] = "locked"

                return decision

            return decision

    def get_best(self):
        with self._lock:
            return {
                "batch_size": self.best_batch_size,
                "in_flight": self.best_in_flight,
                "throughput": self.best_throughput,
                "latency": self.best_latency,
            }


# ─────────────────────────────────────────────────────────────────────────────
# Input readers
# ─────────────────────────────────────────────────────────────────────────────

def sample_jsonl(path, n=1000):
    """Return (avg_doc_bytes, total_bytes)."""
    sizes = []
    total = 0
    with open(path, "rb") as f:
        for i, line in enumerate(f):
            if i >= n:
                break
            line = line.strip()
            if not line:
                continue
            sizes.append(len(line))
            total += len(line)
    if not sizes:
        return 512, 0
    return total / len(sizes), os.path.getsize(path)


def jsonl_batches(path, controller, add_random_fields):
    batch = []
    with open(path, "rb") as f:
        for line_no, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            if not (line.startswith(b"{") and line.endswith(b"}")):
                print(f"[WARN] skipping line {line_no}: not a JSON object")
                continue

            if add_random_fields:
                extra = b'"random_year":%d,"random_float":%.4f}' % (
                    random.randint(1900, 2024),
                    random.uniform(0.0, 100.0),
                )
                doc = (b"{" + extra) if line == b"{}" else (line[:-1] + b"," + extra)
            else:
                doc = line

            batch.append(doc)

            if len(batch) >= controller.current_batch_size:
                payload = b"[" + b",".join(batch) + b"]"
                yield payload, len(batch), len(payload)
                batch = []

    if batch:
        payload = b"[" + b",".join(batch) + b"]"
        yield payload, len(batch), len(payload)


def chunk_file_source(files):
    for f in files:
        with open(f, "rb") as fh:
            yield fh.read(), 0, os.path.basename(f)


# ─────────────────────────────────────────────────────────────────────────────
# Reporting
# ─────────────────────────────────────────────────────────────────────────────

def human_bytes(n):
    for unit in ["B", "KB", "MB", "GB"]:
        if n < 1024:
            return f"{n:.2f} {unit}"
        n /= 1024
    return f"{n:.2f} TB"


def format_progress(inserted, sent, sent_bytes, elapsed, in_flight, batch_size, is_chunk):
    docs_per_sec = inserted / elapsed if elapsed > 0 else 0
    mb_per_sec = (sent_bytes / (1024 * 1024)) / elapsed if elapsed > 0 else 0
    batch_part = "files" if is_chunk else f"batch={batch_size:,}"
    return (
        f"inserted {inserted:>12,} / sent {sent:>12,} "
        f"| {docs_per_sec:>10,.0f} docs/s | {mb_per_sec:>7.2f} MB/s "
        f"| flight={in_flight:>2} {batch_part} | {elapsed:>6.1f}s"
    )


# ─────────────────────────────────────────────────────────────────────────────
# Main upload loop
# ─────────────────────────────────────────────────────────────────────────────

def main():
    args = parse_args()

    if not args.skip_db_setup and not (bool(args.policy) ^ bool(args.policy_file)):
        print("[ERROR] Provide exactly one of --policy or --policy-file.")
        sys.exit(1)

    policy = args.policy
    if args.policy_file:
        with open(args.policy_file, "r", encoding="utf-8") as f:
            policy = f.read()

    if not args.skip_db_setup:
        setup_database(args.base_url, args.db, args.shard_count, policy)

    input_path = args.input
    is_chunk_mode = os.path.isdir(input_path)
    adaptive = args.adaptive

    # ── Predict initial parameters from file size ──────────────────────────
    if is_chunk_mode:
        initial_batch_size = args.batch_size or args.min_batch_size
        initial_in_flight = args.in_flight or 4
        print(f"[INFO] Chunk mode, initial in_flight={initial_in_flight}")
    else:
        avg_doc_bytes, total_bytes = sample_jsonl(input_path)
        target_bytes = args.target_payload_mb * 1024 * 1024
        predicted_batch = int(target_bytes / max(avg_doc_bytes, 1))
        predicted_batch = max(args.min_batch_size,
                              min(args.max_batch_size, predicted_batch))

        # Heavier files can tolerate more concurrency.
        if total_bytes < 10 * 1024 * 1024:
            predicted_flight = 2
        elif total_bytes < 100 * 1024 * 1024:
            predicted_flight = 4
        elif total_bytes < 1024 * 1024 * 1024:
            predicted_flight = 8
        else:
            predicted_flight = 12

        initial_batch_size = args.batch_size or predicted_batch
        initial_in_flight = args.in_flight or predicted_flight

        print(
            f"[INFO] Sampled avg doc = {avg_doc_bytes:.0f} B, "
            f"file = {human_bytes(total_bytes)} → "
            f"predicted batch_size={predicted_batch:,}, in_flight={predicted_flight}"
        )

    state_store = StateStore(args.db, args.state_file)
    saved_state = None if args.reset_state else state_store.load()

    controller = AdaptiveController(
        is_chunk_mode=is_chunk_mode,
        initial_batch_size=initial_batch_size,
        initial_in_flight=initial_in_flight,
        min_batch_size=args.min_batch_size,
        max_batch_size=args.max_batch_size,
        max_in_flight=args.max_in_flight,
        target_payload_bytes=args.target_payload_mb * 1024 * 1024,
        adapt_interval=args.adapt_interval,
        target_latency=args.target_latency,
        saved_state=saved_state,
    )

    if not adaptive:
        controller.batch_size = initial_batch_size
        controller.in_flight = initial_in_flight
        controller.phase = "locked"

    start = time.time()
    sent = 0
    sent_bytes = 0
    inserted = 0
    insert_path = f"/api/databases/{args.db}/insert"

    print(f"[INFO] Uploading from {input_path} ({'adaptive' if adaptive else 'fixed'} mode)...")

    with ThreadPoolExecutor(max_workers=args.max_in_flight) as pool:
        pending = deque()
        source_exhausted = False

        if is_chunk_mode:
            files = sorted(glob.glob(os.path.join(input_path, "*.json")))
            if not files:
                print(f"[ERROR] No chunk files found in {input_path}.")
                sys.exit(1)
            if args.max_chunks > 0:
                files = files[:args.max_chunks]
            print(f"[INFO] {len(files)} chunk file(s)")
            source_iter = chunk_file_source(files)
        else:
            print(
                f"[INFO] JSONL mode, random fields={args.add_random_fields}, "
                f"initial batch={controller.current_batch_size:,}"
            )
            source_iter = jsonl_batches(input_path, controller, args.add_random_fields)

        source_iter = iter(source_iter)
        batch_no = 0
        last_adapt = time.time()

        while True:
            # Submit new work up to current concurrency limit.
            while not source_exhausted and len(pending) < controller.current_in_flight:
                try:
                    payload, doc_count, _ = next(source_iter)
                except StopIteration:
                    source_exhausted = True
                    break

                batch_no += 1
                sent += doc_count
                sent_bytes += len(payload)

                if args.dry_run:
                    print(f"[DRY RUN] batch {batch_no}: {doc_count} docs, {human_bytes(len(payload))}")
                    continue

                t0 = time.time()
                future = pool.submit(post, args.base_url, insert_path, payload, args.request_timeout)
                pending.append((batch_no, doc_count, len(payload), t0, future))

            # Drain completed requests.
            still_pending = deque()
            for item in pending:
                batch_id, doc_count, payload_len, t0, future = item
                if future.done():
                    duration = time.time() - t0
                    reply = future.result()
                    data = reply.get("data") or {}
                    if "error" in reply or "inserted" not in data:
                        print(f"[ERROR] batch {batch_id}: {reply}")
                        controller.record(duration, 0, payload_len, False)
                    else:
                        got = data["inserted"]
                        inserted += got
                        controller.record(duration, got, payload_len, True)
                        failed = doc_count - got if doc_count else 0
                        if batch_id % 10 == 0 or failed:
                            elapsed = time.time() - start
                            print(
                                f"[batch {batch_id:>5}] "
                                + format_progress(
                                    inserted, sent, sent_bytes, elapsed,
                                    controller.current_in_flight,
                                    controller.current_batch_size,
                                    is_chunk_mode,
                                )
                                + (f" | {failed} failed" if failed else ""),
                                flush=True,
                            )
                else:
                    still_pending.append(item)
            pending = still_pending

            # Adaptive tuning.
            if adaptive and time.time() - last_adapt >= controller.adapt_interval:
                decision = controller.adapt()
                if decision:
                    if decision.get("phase") == "locked":
                        print(
                            f"[ADAPT] locked best: "
                            f"batch_size={decision['batch_size']:,}, "
                            f"in_flight={decision['in_flight']}, "
                            f"throughput={decision['throughput']:,.0f} docs/s"
                        )
                    elif decision["changed"]:
                        print(
                            f"[ADAPT] {decision['action']}: "
                            f"throughput={decision['throughput']:,.0f} docs/s, "
                            f"latency={decision['latency']:.2f}s, "
                            f"errors={decision['error_rate']:.1%} → "
                            f"batch={decision['batch_size']:,}, "
                            f"flight={decision['in_flight']}"
                        )
                last_adapt = time.time()

            if source_exhausted and not pending:
                break

            if len(pending) >= controller.current_in_flight:
                time.sleep(0.01)

    duration = time.time() - start

    best = controller.get_best()
    print(
        f"\n[INFO] Best config this run: "
        f"batch_size={best['batch_size']:,}, in_flight={best['in_flight']}, "
        f"throughput={best['throughput']:,.0f} docs/s"
    )

    if adaptive:
        state_store.save(
            best["batch_size"],
            best["in_flight"],
            best["throughput"],
            best["latency"],
        )
        print(f"[INFO] Saved best config to {state_store.path}")

    print(
        f"[INFO] Done: {inserted:,} documents inserted in {duration:.2f}s "
        f"({inserted / duration:,.0f} docs/s, "
        f"{sent_bytes / (1024 * 1024 * duration):.2f} MB/s)"
    )


if __name__ == "__main__":
    main()