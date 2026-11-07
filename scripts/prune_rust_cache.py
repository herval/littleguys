"""Bound this macOS project's debug cache without interrupting another build."""

import fcntl
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys


CACHE_SIGNATURE = b"Signature: 8a477f597d28d172789f06886806bc55"


def prune_rust_cache(root=None, max_gib=None):
    if max_gib is None:
        max_gib = float(os.environ.get("RUST_DEBUG_CACHE_MAX_GB", "10"))
    if not math.isfinite(max_gib) or max_gib < 0:
        raise ValueError("RUST_DEBUG_CACHE_MAX_GB must be nonnegative (0 disables cleanup)")
    if max_gib == 0:
        return False

    root = Path(root or Path(__file__).resolve().parent.parent).resolve()
    target = root / "src-tauri" / "target"
    debug = target / "debug"
    for directory in (target, debug):
        if directory.is_symlink() or directory.resolve() != directory:
            raise ValueError(f"Refusing to clean a redirected build directory: {directory}")
        if not directory.exists():
            return False
    tag = target / "CACHEDIR.TAG"
    if tag.is_symlink() or not tag.is_file() or not tag.read_bytes().startswith(CACHE_SIGNATURE):
        raise ValueError(f"Refusing to clean a directory without Cargo's cache tag: {target}")

    # Cargo uses flock on debug/.cargo-lock on macOS. A profile-wide cargo clean
    # does NOT take that lock, so delete the generated contents under it ourselves.
    # Keep the directory and lock inode intact so new builds use the same lock.
    lock_path = debug / ".cargo-lock"
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "a+b") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("Rust debug cache is in use; skipping cleanup.")
            return False

        size_kib = int(subprocess.check_output(["du", "-sk", str(debug)], text=True).split()[0])
        if size_kib <= max_gib * 1024 * 1024:
            return False

        print(f"Rust debug cache is {size_kib / 1024 / 1024:.1f} GiB "
              f"(limit {max_gib:g} GiB); cleaning before the build.", flush=True)
        for child in debug.iterdir():
            if child.name == ".cargo-lock":
                continue
            if child.is_dir() and not child.is_symlink():
                shutil.rmtree(child)
            else:
                child.unlink()
    return True


if __name__ == "__main__":
    try:
        prune_rust_cache()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Rust cache cleanup failed: {error}", file=sys.stderr)
        sys.exit(1)
