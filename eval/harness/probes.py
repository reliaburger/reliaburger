"""External probes run from the hypervisor host, outside the cluster."""

from __future__ import annotations

import http.client
import json
import socket
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from .core import now_iso


@dataclass
class HttpResult:
    ok: bool
    status: int | None
    latency_ms: float
    body: str
    error: str | None = None


def http_request(ip: str, port: int, path: str = "/", host: str | None = None, method: str = "GET",
                 body: bytes | None = None, timeout: float = 2.0, headers: dict | None = None) -> HttpResult:
    t0 = time.monotonic()
    conn = http.client.HTTPConnection(ip, port, timeout=timeout)
    try:
        hdrs = {"Host": host or ip, "Connection": "close", **(headers or {})}
        conn.request(method, path, body=body, headers=hdrs)
        resp = conn.getresponse()
        data = resp.read(65536).decode(errors="replace")
        return HttpResult(200 <= resp.status < 300, resp.status, (time.monotonic() - t0) * 1000, data)
    except (OSError, http.client.HTTPException) as exc:
        return HttpResult(False, None, (time.monotonic() - t0) * 1000, "", f"{type(exc).__name__}: {exc}")
    finally:
        conn.close()


def tcp_open(ip: str, port: int, timeout: float = 3.0) -> bool:
    try:
        with socket.create_connection((ip, port), timeout=timeout):
            return True
    except OSError:
        return False


def tcp_probe(ip: str, port: int, timeout: float = 3.0, source: str | None = None) -> str:
    """'open', 'refused' (RST) or 'filtered' (no answer before the timeout)."""
    try:
        with socket.create_connection((ip, port), timeout=timeout,
                                      source_address=(source, 0) if source else None):
            return "open"
    except ConnectionRefusedError:
        return "refused"
    except (TimeoutError, socket.timeout):
        return "filtered"
    except OSError as exc:
        return f"error: {exc.strerror or exc}"


@dataclass
class Prober:
    """Sends one request per interval to each target and appends JSONL samples."""

    targets: list[dict]          # {"name", "ip", "port", "host", "path", "expect"}
    out_path: Path
    interval_s: float = 1.0
    timeout_s: float = 2.0
    samples: list[dict] = field(default_factory=list)
    _stop: threading.Event = field(default_factory=threading.Event)
    _thread: threading.Thread | None = None

    def start(self) -> "Prober":
        self.out_path.parent.mkdir(parents=True, exist_ok=True)
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def _loop(self) -> None:
        with open(self.out_path, "a") as fh:
            next_t = time.monotonic()
            while not self._stop.is_set():
                for t in self.targets:
                    r = http_request(t["ip"], t.get("port", 80), t.get("path", "/"), t.get("host"),
                                     timeout=self.timeout_s)
                    ok = r.ok and (t.get("expect") is None or t["expect"] in r.body)
                    s = {"ts": now_iso(), "mono": time.monotonic(), "target": t["name"], "ok": ok,
                         "status": r.status, "latency_ms": round(r.latency_ms, 1), "error": r.error,
                         "body_head": r.body[:200]}
                    self.samples.append(s)
                    fh.write(json.dumps(s) + "\n")
                    fh.flush()
                next_t += self.interval_s
                self._stop.wait(max(0.0, next_t - time.monotonic()))

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=10)

    def window(self, start_mono: float, end_mono: float, target: str | None = None) -> list[dict]:
        return [s for s in self.samples if start_mono <= s["mono"] <= end_mono
                and (target is None or s["target"] == target)]

    @staticmethod
    def summarise(samples: list[dict]) -> dict:
        if not samples:
            return {"count": 0, "ok": 0, "ratio": None}
        ok = sum(1 for s in samples if s["ok"])
        lat = sorted(s["latency_ms"] for s in samples if s["ok"])
        longest, run = 0, 0
        for s in samples:
            run = 0 if s["ok"] else run + 1
            longest = max(longest, run)
        p = (lambda q: lat[min(len(lat) - 1, int(q * len(lat)))] if lat else None)
        return {"count": len(samples), "ok": ok, "ratio": round(ok / len(samples), 4),
                "longest_failure_streak": longest, "p50_ms": p(0.5), "p99_ms": p(0.99),
                "errors": sorted({(s["error"] or str(s["status"])) for s in samples if not s["ok"]})[:10]}


@dataclass
class Sampler:
    """Polls a function at an interval and records (timestamp, value) pairs."""

    fn: object
    out_path: Path
    interval_s: float = 1.0
    samples: list[dict] = field(default_factory=list)
    _stop: threading.Event = field(default_factory=threading.Event)
    _thread: threading.Thread | None = None

    def start(self) -> "Sampler":
        self.out_path.parent.mkdir(parents=True, exist_ok=True)
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def _loop(self) -> None:
        with open(self.out_path, "a") as fh:
            while not self._stop.is_set():
                t = time.monotonic()
                try:
                    value = self.fn()  # type: ignore[operator]
                except Exception as exc:  # recorded, never raised from the thread
                    value = {"error": f"{type(exc).__name__}: {exc}"[:300]}
                s = {"ts": now_iso(), "mono": t, "value": value}
                self.samples.append(s)
                fh.write(json.dumps(s, default=str) + "\n")
                fh.flush()
                self._stop.wait(max(0.0, self.interval_s - (time.monotonic() - t)))

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=30)

    def first(self, predicate, after_mono: float = 0.0) -> dict | None:
        for s in self.samples:
            if s["mono"] >= after_mono and predicate(s["value"]):
                return s
        return None
