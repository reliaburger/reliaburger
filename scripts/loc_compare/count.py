"""Count code lines of each project's main language with scc, one project at a time.

Rules, the same for every project: scc's code lines (no blanks, no comments),
generated files dropped (--no-gen: a "do not edit" marker in the file's head),
and these excluded: test files (*_test.go), zz_generated*, *.pb.go, *.pb.gw.go,
and directories named vendor, third_party, test, tests, testdata, testing,
testutil, e2e, e2e_node, docs, doc, examples, example, hack.
"""
import json
import os
import re
import subprocess
import sys

ROOT = sys.argv[1]
EXCLUDE_DIRS = "vendor,third_party,test,tests,testdata,testing,testutil,e2e,e2e_node,docs,doc,examples,example,hack,.git"
NOT_MATCH = r"(_test\.go$|zz_generated|\.pb\.go$|\.pb\.gw\.go$)"


def scc(path, extra=()):
    args = ["scc", "--format", "json", "--no-gen", "--exclude-dir", EXCLUDE_DIRS, "-M", NOT_MATCH, *extra, path]
    return {row["Name"]: row for row in json.loads(subprocess.run(args, capture_output=True, text=True, check=True).stdout)}


def raw_go(path):
    args = ["scc", "--format", "json", "--include-ext", "go", path]
    rows = json.loads(subprocess.run(args, capture_output=True, text=True, check=True).stdout)
    return sum(r["Code"] for r in rows)


def go_mod(path):
    direct = total = 0
    for dirpath, dirs, names in os.walk(path):
        dirs[:] = [d for d in dirs if d not in ("vendor", ".git", "test", "hack", "examples")]
        if "go.mod" not in names or dirpath != path:
            continue
        inblock = False
        for line in open(os.path.join(dirpath, "go.mod")):
            s = line.strip()
            if s.startswith("require ("):
                inblock = True
                continue
            if inblock and s == ")":
                inblock = False
                continue
            dep = s[len("require "):] if s.startswith("require ") and "(" not in s else (s if inblock else None)
            if dep and not dep.startswith("//"):
                total += 1
                if "// indirect" not in dep:
                    direct += 1
    return direct, total


results = {}
for name in sys.argv[2:]:
    path = os.path.join(ROOT, name)
    rows = scc(path)
    lang = "Rust" if name == "reliaburger-src" else "Go"
    entry = {"language": lang, "code": rows.get(lang, {}).get("Code", 0)}
    if lang == "Go":
        entry["raw_go"] = raw_go(path)
        entry["direct_deps"], entry["total_deps"] = go_mod(path)
    for other in ("TypeScript", "TypeScript Typings", "JavaScript", "C", "C Header"):
        if other in rows:
            entry[other] = rows[other]["Code"]
    results[name] = entry
    print(name, json.dumps(entry), flush=True)
json.dump(results, open(os.path.join(ROOT, "counts.json"), "w"), indent=1)
