"""A stand-in `holmes` for the portable tests of holmes.py (stdlib only).

It checks the command line the wrapper built (`ask`, `--no-interactive`,
`--bash-always-deny`, a model, never `--bash-always-allow`), runs `kubectl
get pods` through PATH the way a HolmesGPT toolset would, and writes a
`--json-output-file` whose `result` carries `GYM_FAKE_ANSWER`. No model is
called.
"""
import json
import os
from pathlib import Path
import subprocess
import sys


def value(argv, flag):
    return argv[argv.index(flag) + 1] if flag in argv else None


def main(argv):
    if argv == ["version"]:
        print("HolmesGPT 9.9.9")
        return 0
    wrong = []
    if argv[:1] != ["ask"]:
        wrong.append("not ask")
    for flag in ("--no-interactive", "--bash-always-deny"):
        if flag not in argv:
            wrong.append(f"no {flag}")
    if "--bash-always-allow" in argv:
        wrong.append("--bash-always-allow")
    if not value(argv, "--model"):
        wrong.append("no model")
    prompt = value(argv, "--prompt-file")
    if prompt is None or "GYM_ANSWER" not in Path(prompt).read_text():
        wrong.append("no answer contract in the prompt file")
    if not os.environ.get("KUBECONFIG"):
        wrong.append("no KUBECONFIG")
    if wrong:
        sys.stderr.write(f"fake holmes: {', '.join(wrong)}\n")
        return 3
    subprocess.run(["kubectl", "get", "pods", "-A", "-o", "json"], capture_output=True, text=True)
    answer = json.loads(os.environ["GYM_FAKE_ANSWER"])
    result = f"The web pods crash.\n\n```json\n{json.dumps(answer)}\n```"
    Path(value(argv, "--json-output-file")).write_text(json.dumps({
        "result": result,
        "tool_calls": [{"tool_name": "kubectl_get_by_kind_in_namespace"}],
        "metadata": {"costs": {"total_cost": 0.02}},
    }))
    print(result)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
