"""Copy Reliaburger's src/ without its test code, for a like-for-like line count.

Test code, as scripts/loc.sh defines it: the tail of a file from a column-0
`#[cfg(test)]` that isn't just declaring a module file, and every file
declared as `#[cfg(test)] mod name;`. src/fuzz/ is test-only too (it compiles
under `cfg(any(test, feature = "fuzzing"))`), so it goes as well.
"""
import os
import re
import shutil
import subprocess
import sys

REPO, OUT = sys.argv[1], sys.argv[2]
MOD = re.compile(r"^\s*(pub(\([a-z]+\))? )?mod ([a-z_0-9]+);")
files = subprocess.run(["git", "-C", REPO, "ls-files", "src/*.rs"], capture_output=True, text=True, check=True).stdout.split()

test_modules = set()
for path in files:
    lines = open(os.path.join(REPO, path)).read().split("\n")
    for prev, line in zip(lines, lines[1:]):
        m = MOD.match(line)
        if prev.strip() == "#[cfg(test)]" and m:
            name = m.group(3)
            base = os.path.dirname(path) if re.search(r"(^|/)(mod|lib|main)\.rs$", path) else path[:-3]
            test_modules.add(os.path.join(base, name + ".rs"))
            test_modules.add(os.path.join(base, name, "mod.rs"))

if os.path.exists(OUT):
    shutil.rmtree(OUT)
kept = 0
for path in files:
    if path in test_modules or path.startswith("src/fuzz/"):
        continue
    lines = open(os.path.join(REPO, path)).read().split("\n")
    out = []
    for i, line in enumerate(lines):
        if line.startswith("#[cfg(test)]"):
            following = lines[i + 1] if i + 1 < len(lines) else ""
            if not MOD.match(following):
                break
        out.append(line)
    dest = os.path.join(OUT, path)
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    open(dest, "w").write("\n".join(out))
    kept += 1
print(f"kept {kept} of {len(files)} files; {len(test_modules) // 2} test module declarations")
