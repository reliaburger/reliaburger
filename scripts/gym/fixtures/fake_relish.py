"""A stand-in `relish` that answers from a recorded fixture (stdlib only).

The portable tests point the shim at this instead of a real binary, so the
scripted solver, the shim and the scorer run end to end with no cluster. A
fixture is a JSON object from the argument list, joined by spaces, to the
recorded `stdout` and `exit` of that call on a real cluster
(`fixtures/<scenario>.json`, made from a real run's call log with `gym.py
fixture`). A call with no recording fails with exit 1, as a request the API
refused would; exit 2 would mean a usage error that never left the machine.
"""
import json
import os
import sys


def main(argv):
    with open(os.environ["GYM_FAKE_FIXTURE"], encoding="utf-8") as fixture:
        recorded = json.load(fixture)
    key = " ".join(argv)
    if key not in recorded:
        sys.stderr.write(f"fake relish: no recording for `relish {key}`\n")
        return 1
    sys.stdout.write(recorded[key]["stdout"])
    return recorded[key]["exit"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
