"""Claim registry and citation verification.

Every scenario's @claim_ tags must resolve to a claim in claims/claims.yaml
whose citations are verified against the product's source at the pinned
docs ref. A scenario with an unresolved or unverifiable claim is BLOCKED.
"""

from __future__ import annotations

import os
import re
import subprocess
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from .core import ROOT, Config, load_yaml

GITHUB_RAW = "https://raw.githubusercontent.com/reliaburger/reliaburger"


def _norm(text: str) -> str:
    text = re.sub(r"[`*_>#|]", " ", text)
    text = text.replace("’", "'").replace("“", '"').replace("”", '"')
    return re.sub(r"\s+", " ", text).strip().lower()


@dataclass
class CitationCheck:
    claim_id: str
    ref: str
    ok: bool
    detail: str = ""


class Claims:
    def __init__(self, cfg: Config):
        self.cfg = cfg
        data = load_yaml(ROOT / "claims" / "claims.yaml")
        self.by_id = {c["id"]: c for c in data["claims"]}
        self.cache = cfg.state_dir / "source-cache" / cfg.product["docs_ref"]
        self._checks: dict[str, list[CitationCheck]] = {}

    def get(self, claim_id: str) -> dict | None:
        return self.by_id.get(claim_id)

    def source_refs(self, claim_ids: list[str]) -> list[str]:
        refs = []
        for cid in claim_ids:
            c = self.by_id.get(cid)
            if not c:
                continue
            for s in c.get("sources", []):
                ref = s["ref"]
                refs.append(ref if ref.startswith("claims/") else f"{ref}@{self.cfg.product['docs_ref'][:12]}")
        return refs

    def _read(self, path: str) -> str | None:
        if path.startswith("claims/"):
            p = ROOT / path
            return p.read_text() if p.exists() else None
        cached = self.cache / path
        if cached.exists():
            return cached.read_text()
        ref = self.cfg.product["docs_ref"]
        text = None
        repo = os.environ.get("RBEVAL_PRODUCT_REPO")
        if repo and Path(repo, ".git").exists():
            r = subprocess.run(["git", "-C", repo, "show", f"{ref}:{path}"], capture_output=True)
            if r.returncode == 0:
                text = r.stdout.decode()
        if text is None:
            commit = self.cfg.product["commit"] if not re.fullmatch(r"[0-9a-f]{40}", ref) else ref
            try:
                with urllib.request.urlopen(f"{GITHUB_RAW}/{commit}/{path}", timeout=30) as resp:
                    text = resp.read().decode()
            except OSError:
                return None
        cached.parent.mkdir(parents=True, exist_ok=True)
        cached.write_text(text)
        return text

    def verify(self, claim_id: str) -> list[CitationCheck]:
        if claim_id in self._checks:
            return self._checks[claim_id]
        out: list[CitationCheck] = []
        claim = self.by_id.get(claim_id)
        if claim is None:
            out.append(CitationCheck(claim_id, "-", False, "claim id not in claims/claims.yaml"))
        elif not claim.get("sources"):
            out.append(CitationCheck(claim_id, "-", False, "claim has no sources"))
        else:
            for s in claim["sources"]:
                out.append(self._verify_one(claim_id, s))
        self._checks[claim_id] = out
        return out

    def _verify_one(self, claim_id: str, source: dict) -> CitationCheck:
        ref = source["ref"]
        m = re.fullmatch(r"(.+?):(\d+)(?:-(\d+))?", ref)
        if not m:
            return CitationCheck(claim_id, ref, False, "ref is not path:line[-line]")
        path, a, b = m.group(1), int(m.group(2)), int(m.group(3) or m.group(2))
        text = self._read(path)
        if text is None:
            return CitationCheck(claim_id, ref, False, "source file not readable at docs ref")
        lines = text.splitlines()
        window = _norm(" ".join(lines[max(0, a - 3): b + 2]))
        segments = [seg for seg in re.split(r"\s*(?:\.\.\.|…)\s*", _norm(source.get("quote", ""))) if len(seg) > 3]
        missing = [seg for seg in segments if seg not in window]
        if missing:
            return CitationCheck(claim_id, ref, False, f"quote not found near lines {a}-{b}: {missing[0][:80]!r}")
        return CitationCheck(claim_id, ref, True)

    def is_verified(self, claim_id: str) -> tuple[bool, list[CitationCheck]]:
        checks = self.verify(claim_id)
        # A claim is linked when at least one citation verifies and none is unreadable.
        return any(c.ok for c in checks), checks
