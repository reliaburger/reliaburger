"""Stop with a clear message when python3 can't run the release scripts (#1483).

The harness needs `tomllib`, which is Python 3.11 and later, and macOS's
`/usr/bin/python3` is 3.9. Without this check a soak gets through its setup
and then dies on an import, so this file must itself run on any python3.
"""
import sys

try:
    import tomllib  # noqa: F401
except ModuleNotFoundError:
    print(
        "the release scripts need Python 3.11 or later, and python3 here is "
        f"{sys.version.split()[0]}: run `uv python install 3.13` or "
        "`brew install python@3.13` and put it first on PATH",
        file=sys.stderr,
    )
    sys.exit(1)
