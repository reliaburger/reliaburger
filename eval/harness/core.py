"""Configuration, outcome exceptions, secret redaction and the command log.

Every external command the harness runs goes through `RunLog.run`, which
records argv, timestamps, exit code and output (redacted) to
results/<run-id>/commands.jsonl and to the current scenario's directory.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import re
import shlex
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

ROOT = Path(__file__).resolve().parent.parent


# --------------------------------------------------------------------------
# Outcome exceptions. environment.py maps them onto the report statuses.
# --------------------------------------------------------------------------


class Blocked(Exception):
    """The scenario's preconditions could not be established (not a product failure)."""


class Unsupported(Exception):
    """The product does not offer the capability the scenario needs."""


class ProductFailure(AssertionError):
    """The product did something observable that contradicts the expectation."""


class CommandFailed(Exception):
    def __init__(self, result: "CmdResult", message: str = ""):
        self.result = result
        super().__init__(
            message
            or f"command failed rc={result.rc}: {result.display}\n"
            f"stderr: {result.stderr[-1500:]}\nstdout: {result.stdout[-800:]}"
        )


# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------


def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")


def load_yaml(path: Path) -> Any:
    with open(path) as fh:
        return yaml.safe_load(fh)


@dataclass
class Config:
    product: dict
    product_key: str
    lab: dict
    thresholds: dict
    safety: dict

    @classmethod
    def load(cls, product_key: str | None = None) -> "Config":
        cfg_dir = ROOT / "config"
        products = load_yaml(cfg_dir / "product.yaml")
        key = product_key or os.environ.get("RBEVAL_PRODUCT") or products["default"]
        if key not in products["targets"]:
            raise SystemExit(f"unknown product target {key!r}; known: {list(products['targets'])}")
        return cls(
            product=products["targets"][key],
            product_key=key,
            lab=load_yaml(cfg_dir / "lab.yaml"),
            thresholds=load_yaml(cfg_dir / "thresholds.yaml"),
            safety=load_yaml(cfg_dir / "safety.yaml"),
        )

    @property
    def state_dir(self) -> Path:
        # One lab at a time. state.json records which product target built
        # it; clusters are not compatible across releases before 1.0
        # (docs/releasing.md), so a different target rebuilds the lab.
        return ROOT / self.lab["state_dir"]

    @property
    def cache_dir(self) -> Path:
        return Path(os.path.expanduser(self.lab["cache_dir"]))

    def threshold(self, dotted: str) -> Any:
        node: Any = self.thresholds
        for part in dotted.split("."):
            node = node[part]
        if isinstance(node, dict) and "value" in node:
            return node["value"]
        return node


# --------------------------------------------------------------------------
# Redaction
# --------------------------------------------------------------------------


class Redactor:
    def __init__(self, patterns: list[str]):
        self._patterns = [re.compile(p) for p in patterns]
        self._literals: set[str] = set()
        self._lock = threading.Lock()

    def add(self, secret: str | None) -> None:
        if secret and len(secret) >= 6:
            with self._lock:
                self._literals.add(secret.strip())

    def __call__(self, text: str | None) -> str:
        if not text:
            return text or ""
        with self._lock:
            literals = sorted(self._literals, key=len, reverse=True)
        for lit in literals:
            text = text.replace(lit, "«redacted»")
        for pat in self._patterns:
            text = pat.sub("«redacted»", text)
        return text


# --------------------------------------------------------------------------
# Command log
# --------------------------------------------------------------------------


@dataclass
class CmdResult:
    argv: list[str]
    rc: int
    stdout: str
    stderr: str
    start: str
    duration_ms: int
    host: str = "local"
    display: str = ""
    seq: int = 0

    @property
    def ok(self) -> bool:
        return self.rc == 0

    def json(self) -> Any:
        return json.loads(self.stdout)


@dataclass
class RunLog:
    run_dir: Path
    redact: Redactor
    scenario_dir: Path | None = None
    scenario_id: str | None = None
    operator_actions: list[dict] = field(default_factory=list)
    _seq: int = 0
    _lock: threading.Lock = field(default_factory=threading.Lock)

    def __post_init__(self) -> None:
        self.run_dir.mkdir(parents=True, exist_ok=True)

    def _write(self, record: dict) -> None:
        line = json.dumps(record, sort_keys=True) + "\n"
        with self._lock:
            with open(self.run_dir / "commands.jsonl", "a") as fh:
                fh.write(line)
            if self.scenario_dir:
                with open(self.scenario_dir / "commands.jsonl", "a") as fh:
                    fh.write(line)

    def _store_output(self, seq: int, name: str, text: str) -> str | None:
        if len(text) <= 4000:
            return None
        target_dir = (self.scenario_dir or self.run_dir) / "output"
        target_dir.mkdir(parents=True, exist_ok=True)
        path = target_dir / f"{seq:05d}.{name}.txt"
        path.write_text(text)
        return str(path.relative_to(self.run_dir))

    def note_operator_action(self, description: str, kind: str = "command") -> None:
        """Count a step a human operator performs when following the docs."""
        entry = {"ts": now_iso(), "kind": kind, "description": self.redact(description),
                 "scenario": self.scenario_id}
        with self._lock:
            self.operator_actions.append(entry)
        if self.scenario_dir:
            with open(self.scenario_dir / "operator_actions.jsonl", "a") as fh:
                fh.write(json.dumps(entry) + "\n")

    def run(
        self,
        argv: list[str],
        *,
        input: str | bytes | None = None,
        timeout: float = 300,
        check: bool = False,
        env: dict | None = None,
        host: str = "local",
        display: str | None = None,
        operator: str | None = None,
        quiet: bool = False,
    ) -> CmdResult:
        with self._lock:
            self._seq += 1
            seq = self._seq
        shown = self.redact(display or shlex.join(argv))
        start = now_iso()
        t0 = time.monotonic()
        full_env = None
        if env:
            full_env = os.environ.copy()
            full_env.update(env)
        try:
            proc = subprocess.run(
                argv,
                input=input.encode() if isinstance(input, str) else input,
                capture_output=True,
                timeout=timeout,
                env=full_env,
            )
            rc = proc.returncode
            out = proc.stdout.decode(errors="replace")
            err = proc.stderr.decode(errors="replace")
        except subprocess.TimeoutExpired as exc:
            rc = 124
            out = (exc.stdout or b"").decode(errors="replace") if isinstance(exc.stdout, bytes) else (exc.stdout or "")
            err = ((exc.stderr or b"").decode(errors="replace") if isinstance(exc.stderr, bytes) else (exc.stderr or "")) + f"\n[harness] timed out after {timeout}s"
        duration_ms = int((time.monotonic() - t0) * 1000)
        r_out, r_err = self.redact(out), self.redact(err)
        record = {
            "seq": seq,
            "scenario": self.scenario_id,
            "host": host,
            "start": start,
            "duration_ms": duration_ms,
            "rc": rc,
            "cmd": shown,
            "operator_step": operator,
            "stdout": r_out[-4000:],
            "stderr": r_err[-4000:],
            "stdout_file": self._store_output(seq, "stdout", r_out),
            "stderr_file": self._store_output(seq, "stderr", r_err),
        }
        if not quiet:
            self._write(record)
        if operator:
            self.note_operator_action(operator)
        result = CmdResult(argv=argv, rc=rc, stdout=out, stderr=err, start=start,
                           duration_ms=duration_ms, host=host, display=shown, seq=seq)
        if check and rc != 0:
            raise CommandFailed(result)
        return result


def wait_until(predicate, timeout: float, interval: float = 1.0, what: str = "condition"):
    """Poll `predicate` until it returns a truthy value; return (value, seconds)."""
    t0 = time.monotonic()
    last_exc: Exception | None = None
    while True:
        try:
            value = predicate()
            if value:
                return value, time.monotonic() - t0
        except (CommandFailed, OSError, ValueError, KeyError) as exc:
            last_exc = exc
        if time.monotonic() - t0 > timeout:
            raise TimeoutError(f"timed out after {timeout}s waiting for {what}"
                               + (f" (last error: {last_exc})" if last_exc else ""))
        time.sleep(interval)
