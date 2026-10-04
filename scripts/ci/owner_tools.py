"""Resolve actual Rust toolchain executables rather than hashing a rustup proxy."""
from pathlib import Path
import shutil
import subprocess

import contracts as c


def rust_tools(environment, run=subprocess.run):
    """Use the selected compiler's sysroot, preserving current toolchain policy."""
    proxy = shutil.which('rustc', path=environment.get('PATH'))
    c.require(proxy is not None, 'selected Rust compiler missing')
    observed = run([proxy, '--print', 'sysroot'], env=environment, capture_output=True, text=True)
    c.require(observed.returncode == 0, 'actual Rust sysroot query failed')
    root = Path(observed.stdout.strip())
    c.require(root.is_absolute(), 'actual Rust sysroot is not absolute')
    result = {}
    for name in ('rustc', 'cargo'):
        executable = root / 'bin' / name
        c.require(executable.is_file(), 'actual selected toolchain executable missing: ' + name)
        result[name] = str(executable.resolve())
    return result
