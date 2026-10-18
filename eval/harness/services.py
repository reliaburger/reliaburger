"""Host-side lab services: OTLP collector, kind baseline cluster, HTTP receivers.

All are named with the rbeval- prefix and removed by teardown_all().
"""

from __future__ import annotations

import http.server
import json
import os
import threading
import time
from pathlib import Path

from .core import ROOT, Config, RunLog, now_iso, wait_until
from .probes import tcp_open


def _check_prefix(cfg: Config, name: str, kind: str) -> None:
    prefix = cfg.safety[kind]["allowed_container_prefix" if kind == "docker" else "allowed_cluster_prefix"]
    if not name.startswith(prefix):
        raise RuntimeError(f"safety: refusing to manage {kind} resource {name!r}")


# --------------------------------------------------------------------------- collector


class Collector:
    """OpenTelemetry Collector (contrib) in Docker, bound to the lab bridge address."""

    def __init__(self, cfg: Config, log: RunLog, out_dir: Path):
        self.cfg, self.log = cfg, log
        self.name = cfg.lab["collector"]["container_name"]
        self.image = cfg.lab["collector"]["image"]
        self.bind = cfg.lab["network"]["gateway"]
        self.out = out_dir
        _check_prefix(cfg, self.name, "docker")

    def start(self) -> None:
        self.stop()
        self.out.mkdir(parents=True, exist_ok=True)
        os.chmod(self.out, 0o777)
        conf = ROOT / "fixtures" / "collector" / "otel-collector.yaml"
        self.log.run(["docker", "run", "-d", "--name", self.name,
                      "-p", f"{self.bind}:4317:4317", "-p", f"{self.bind}:4318:4318",
                      "-v", f"{conf}:/etc/otelcol/config.yaml:ro", "-v", f"{self.out}:/out",
                      self.image, "--config", "/etc/otelcol/config.yaml"], check=True, timeout=600)
        wait_until(lambda: tcp_open(self.bind, 4317, 1), 60, 1, "collector OTLP port")

    def stop(self) -> None:
        self.log.run(["docker", "rm", "-f", self.name], check=False, quiet=True)

    def trace_ids(self) -> set[str]:
        ids: set[str] = set()
        path = self.out / "traces.jsonl"
        if not path.exists():
            return ids
        for line in path.read_text().splitlines():
            try:
                doc = json.loads(line)
            except json.JSONDecodeError:
                continue
            for rs in doc.get("resourceSpans", []):
                for ss in rs.get("scopeSpans", []):
                    for sp in ss.get("spans", []):
                        ids.add(sp.get("traceId", ""))
        return ids

    def services(self) -> set[str]:
        names: set[str] = set()
        path = self.out / "traces.jsonl"
        if path.exists():
            for line in path.read_text().splitlines():
                try:
                    doc = json.loads(line)
                except json.JSONDecodeError:
                    continue
                for rs in doc.get("resourceSpans", []):
                    for a in rs.get("resource", {}).get("attributes", []):
                        if a.get("key") == "service.name":
                            names.add(a["value"].get("stringValue", ""))
        return names


# --------------------------------------------------------------------------- kind


class KindBaseline:
    """A kind cluster that validates Kubernetes fixtures before migration."""

    def __init__(self, cfg: Config, log: RunLog):
        self.cfg, self.log = cfg, log
        self.name = cfg.lab["baseline_k8s"]["kind_cluster"]
        self.image = cfg.lab["baseline_k8s"]["node_image"]
        self.kubeconfig = cfg.state_dir / "kind.kubeconfig"
        _check_prefix(cfg, self.name, "kind")

    def exists(self) -> bool:
        r = self.log.run(["kind", "get", "clusters"], check=False, quiet=True)
        return self.name in r.stdout.split()

    def ensure(self) -> None:
        if not self.exists():
            self.log.run(["kind", "create", "cluster", "--name", self.name, "--image", self.image,
                          "--kubeconfig", str(self.kubeconfig), "--wait", "120s"], check=True, timeout=900)
        elif not self.kubeconfig.exists():
            r = self.log.run(["kind", "get", "kubeconfig", "--name", self.name], check=True)
            self.kubeconfig.write_text(r.stdout)

    def kubectl(self, *args: str, check: bool = True, timeout: float = 300, input: str | None = None):
        return self.log.run(["kubectl", "--kubeconfig", str(self.kubeconfig), *args], check=check,
                            timeout=timeout, input=input, host="kind")

    def delete(self) -> None:
        if self.exists():
            self.log.run(["kind", "delete", "cluster", "--name", self.name], check=False, timeout=300)


# --------------------------------------------------------------------------- HTTP receiver


class Receiver:
    """Records every HTTP request it receives (webhook / alert destination)."""

    def __init__(self, bind: str, port: int, out_path: Path, status: int = 200):
        self.bind, self.port, self.out = bind, port, out_path
        self.requests: list[dict] = []
        self.status = status
        recv = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def _handle(self):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n).decode(errors="replace") if n else ""
                entry = {"ts": now_iso(), "mono": time.monotonic(), "method": self.command,
                         "path": self.path, "headers": dict(self.headers), "body": body,
                         "client": self.client_address[0]}
                recv.requests.append(entry)
                with open(recv.out, "a") as fh:
                    fh.write(json.dumps(entry) + "\n")
                self.send_response(recv.status)
                self.end_headers()
                self.wfile.write(b"ok")

            do_POST = do_PUT = do_GET = _handle

            def log_message(self, *a):
                pass

        self.server = http.server.ThreadingHTTPServer((bind, port), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self) -> "Receiver":
        self.out.parent.mkdir(parents=True, exist_ok=True)
        self.thread.start()
        return self

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def teardown_all(context) -> None:
    keep = os.environ.get("RBEVAL_KEEP_LAB") == "1"
    for svc in context.shared.get("services", []):
        try:
            if isinstance(svc, KindBaseline) and keep:
                continue
            svc.stop() if hasattr(svc, "stop") else svc.delete()
        except Exception as exc:  # teardown is best effort and logged
            print(f"[rbeval] service teardown failed: {exc}")
