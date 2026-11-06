"""Build a `relish` with a deliberate bug in it, away from this checkout.

The planted scenario needs a binary whose own source holds the bug, so that
`relish agent source` shows the solver the code it is really running. The
plant is a patch in scripts/gym/plants/; nothing here edits the checkout.
`PlantBuild` extracts the shipped `relish`'s source with `relish agent source
extract`, applies the patch to that copy and builds `relish` from it with the
`embed-source` feature, into a target directory of its own.

It can also check a solver's fix: apply the answer's patch to a fresh planted
copy and run the tests the plant broke.
"""
import json
from pathlib import Path
import shutil
import subprocess

from scenario import GYM_DIR

REPOSITORY = GYM_DIR.parent.parent
BUILD_TIMEOUT = 3600


class PlantError(RuntimeError):
    """The plant didn't apply or the planted build failed."""


def check_applies(patch, root=REPOSITORY):
    """Whether `patch` still applies to the source at `root`.

    The plant is a diff against today's code, so a change to the planted
    function breaks it; `make test-ci-scripts` runs this against the
    checkout to say so the day it happens.
    """
    completed = subprocess.run(
        ["git", "apply", "--check", str(patch)],
        cwd=root, capture_output=True, text=True, check=False,
    )
    return completed.returncode == 0, completed.stderr.strip()


def _run(argv, cwd, env=None, timeout=BUILD_TIMEOUT):
    return subprocess.run(
        argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout, check=False
    )


class PlantBuild:
    """One planted copy of the source and the `relish` built from it."""

    def __init__(self, relish, patch, work_dir, target_dir, env):
        self.relish = Path(relish)
        self.patch = Path(patch)
        self.work_dir = Path(work_dir)
        self.target_dir = Path(target_dir)
        self.env = dict(env)
        self.env["CARGO_TARGET_DIR"] = str(self.target_dir)

    def planted_tree(self, name):
        """A fresh copy of the source with the plant applied."""
        tree = self.work_dir / name
        if tree.exists():
            shutil.rmtree(tree)
        extracted = _run(
            [str(self.relish), "agent", "source", "extract", str(tree)], cwd=self.work_dir,
            env=self.env,
        )
        envelope = json.loads(extracted.stdout or "{}")
        if not envelope.get("ok"):
            raise PlantError(f"relish agent source extract failed: {extracted.stdout.strip()}")
        applied = _run(["git", "apply", str(self.patch)], cwd=tree)
        if applied.returncode != 0:
            raise PlantError(f"the plant didn't apply: {applied.stderr.strip()}")
        return tree

    def build(self):
        """Build the planted `relish` and return its path."""
        tree = self.planted_tree("planted-source")
        built = _run(
            ["cargo", "build", "--locked", "--features", "embed-source", "--bin", "relish"],
            cwd=tree, env=self.env,
        )
        if built.returncode != 0:
            raise PlantError(f"the planted build failed:\n{built.stderr[-4000:]}")
        binary = self.target_dir / "debug" / "relish"
        kept = self.work_dir / "relish-planted"
        shutil.copy2(binary, kept)
        return kept

    def check_patch(self, answer_patch, tests):
        """Apply the solver's patch to a fresh planted copy and run `tests`.

        Returns "pass", or "fail" with the reason.
        """
        tree = self.planted_tree("patched-source")
        patch_file = self.work_dir / "answer.patch"
        patch_file.write_text(answer_patch if answer_patch.endswith("\n") else answer_patch + "\n")
        applied = _run(["git", "apply", str(patch_file)], cwd=tree)
        if applied.returncode != 0:
            return "fail", f"the answer's patch didn't apply: {applied.stderr.strip()}"
        tested = _run(
            ["cargo", "test", "--locked", "--lib", "--", "--exact", *tests], cwd=tree, env=self.env
        )
        if tested.returncode != 0:
            return "fail", f"the reverted tests failed:\n{tested.stdout[-2000:]}"
        # A filter that matches nothing passes too: count what ran.
        if f"test result: ok. {len(tests)} passed" not in tested.stdout:
            return "fail", f"expected {len(tests)} tests to run:\n{tested.stdout[-2000:]}"
        return "pass", "the reverted tests pass"
