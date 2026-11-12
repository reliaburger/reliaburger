"""A stand-in `kubectl` that answers from a fixture (stdlib only).

The Kubernetes counterpart of fake_relish.py: the portable tests put the
recording shim in front of this, so the solvers, the shim and the scorer run
end to end with no cluster. A fixture maps the arguments, joined by spaces
and with any `--kubeconfig PATH` taken out, to the `stdout`, `stderr` and
`exit` of that call (fixtures/k8s-<scenario>.json). The k8s fixtures are
written by hand from kubectl's output format until the first lab run
records real ones with `gym.py fixture`.

A call with no recording fails the way the API server refuses a read-only
account's write: exit 1 and a `Forbidden` message. An unknown command fails
the way kubectl's own parser does, before any request.
"""
import json
import os
import sys

COMMANDS = {
    "annotate", "api-resources", "api-versions", "apply", "attach", "auth", "autoscale",
    "certificate", "cluster-info", "completion", "config", "cordon", "cp", "create", "debug",
    "delete", "describe", "diff", "drain", "edit", "events", "exec", "explain", "expose", "get",
    "help", "kustomize", "label", "logs", "options", "patch", "plugin", "port-forward", "proxy",
    "replace", "rollout", "run", "scale", "set", "taint", "top", "uncordon", "version", "wait",
}


def key_of(argv):
    kept = []
    skip = False
    for argument in argv:
        if skip:
            skip = False
            continue
        if argument == "--kubeconfig":
            skip = True
            continue
        if argument.startswith("--kubeconfig="):
            continue
        kept.append(argument)
    return " ".join(kept)


def main(argv):
    words = [a for a in argv if not a.startswith("-")]
    if words and words[0] not in COMMANDS:
        sys.stderr.write(f'error: unknown command "{words[0]}" for "kubectl"\n')
        return 1
    with open(os.environ["GYM_FAKE_FIXTURE"], encoding="utf-8") as fixture:
        recorded = json.load(fixture)
    key = key_of(argv)
    if key not in recorded:
        sys.stderr.write(
            f"Error from server (Forbidden): fake kubectl has no recording for `kubectl {key}`\n"
        )
        return 1
    sys.stdout.write(recorded[key].get("stdout", ""))
    sys.stderr.write(recorded[key].get("stderr", ""))
    return recorded[key].get("exit", 0)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
