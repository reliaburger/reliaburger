"""A stand-in `kubectl-ai` for the portable tests of kubectl_ai.py (stdlib only).

It checks the command line the wrapper built (`--quiet`, a model, the
read-only kubeconfig, never `--skip-permissions`), runs `kubectl get pods`
through PATH the way kubectl-ai's kubectl tool would, and answers in prose
with `GYM_FAKE_ANSWER` as a fenced JSON object at the end. No model is called.
"""
import json
import os
import subprocess
import sys


def value(argv, flag):
    return argv[argv.index(flag) + 1] if flag in argv else None


def main(argv):
    if argv == ["version"]:
        print("version: 9.9.9")
        return 0
    wrong = []
    if "--quiet" not in argv:
        wrong.append("not --quiet")
    if "--skip-permissions" in argv:
        wrong.append("--skip-permissions")
    if not value(argv, "--model"):
        wrong.append("no model")
    if value(argv, "--kubeconfig") != os.environ.get("KUBECONFIG"):
        wrong.append("not the solver's kubeconfig")
    if "GYM_ANSWER" not in argv[-1]:
        wrong.append("no answer contract in the prompt")
    if wrong:
        sys.stderr.write(f"fake kubectl-ai: {', '.join(wrong)}\n")
        return 3
    if os.environ.get("GYM_FAKE_PERMISSION_STOP"):
        # What --quiet does when the model asks for a write.
        print("Error: RunOnce mode cannot handle permission requests. The following commands "
              "require approval:\n* kubectl delete deployment web")
        return 1
    shown = subprocess.run(["kubectl", "get", "pods", "-A", "-o", "json"], capture_output=True, text=True)
    print(f"I listed the pods ({len(shown.stdout)} bytes).")
    print("Here is my answer:\n```json")
    print(json.dumps(json.loads(os.environ["GYM_FAKE_ANSWER"]), indent=2))
    print("```")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
