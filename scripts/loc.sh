#!/usr/bin/env bash
#
# Count lines in tracked .rs, .md and .toml files (`make loc`).
#
# Only files git tracks count, so build output (target/) and installed
# dependencies (docs/talks/**/node_modules) never inflate the totals.
# Rust test code is the tail of a src file after a `#[cfg(test)]` block,
# every file declared as `#[cfg(test)] mod name;`, and everything in tests/.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

tracked() {
    git ls-files -z -- "$@"
}

lines() {
    xargs -0 cat 2>/dev/null | wc -l | tr -d ' '
}

# Files declared as `#[cfg(test)] mod name;`, resolved the way rustc does:
# a child of foo.rs lives in foo/, a child of mod.rs/lib.rs/main.rs beside it.
test_module_files() {
    tracked 'src/*.rs' | xargs -0 awk '
        pending && /^[[:space:]]*(pub(\([a-z]+\))? )?mod [a-z_0-9]+;/ {
            name = $0
            sub(/^[[:space:]]*(pub(\([a-z]+\))? )?mod /, "", name)
            sub(/;.*/, "", name)
            dir = FILENAME
            if (dir ~ /(^|\/)(mod|lib|main)\.rs$/) sub(/[^\/]*$/, "", dir)
            else { sub(/\.rs$/, "", dir); dir = dir "/" }
            print dir name ".rs"
            print dir name "/mod.rs"
        }
        { pending = ($0 ~ /^[[:space:]]*#\[cfg\(test\)\]$/) }
    '
}

test_modules=$(test_module_files)

is_test_module() {
    grep -qxF "$1" <<<"$test_modules"
}

src_files=()
module_files=()
while IFS= read -r -d '' file; do
    if is_test_module "$file"; then
        module_files+=("$file")
    else
        src_files+=("$file")
    fi
done < <(tracked 'src/*.rs')

# A `#[cfg(test)]` line starts the test tail unless it only declares a
# separate test module file, which is counted whole below.
split_src() {
    awk -v want="$1" '
        FNR == 1 { t = 0; pending = 0 }
        pending {
            pending = 0
            if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))? )?mod [a-z_0-9]+;/) { s += 2; next }
            t = 1; u++
        }
        !t && /^#\[cfg\(test\)\]/ { pending = 1; next }
        { if (t) u++; else s++ }
        END { print (want == "test") ? u + 0 : s + 0 }
    ' "${src_files[@]}"
}

src=$(split_src src)
test_tail=$(split_src test)
test_modules_lines=0
if ((${#module_files[@]})); then
    test_modules_lines=$(printf '%s\0' "${module_files[@]}" | lines)
fi
integration=$(tracked 'tests/*.rs' | lines)

echo "  .rs (src):  $src"
echo "  .rs (test): $((test_tail + test_modules_lines + integration))"
echo "  .md:   $(tracked '*.md' | lines)"
echo "  .toml: $(tracked '*.toml' | lines)"
echo "  total: $(tracked '*.rs' '*.md' '*.toml' | lines)"
