import fcntl
from pathlib import Path
import tempfile
import unittest

from prune_rust_cache import CACHE_SIGNATURE, prune_rust_cache


class PruneRustCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="rust cache test ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.project = self.root / "src-tauri"
        self.target = self.project / "target"
        for profile in ("debug", "release", "aarch64-apple-darwin/release"):
            directory = self.target / profile
            directory.mkdir(parents=True)
            (directory / "artifact").write_bytes(b"x" * 8192)
        (self.target / "CACHEDIR.TAG").write_bytes(CACHE_SIGNATURE + b"\n")
        (self.project / "source.rs").write_text("fn main() {}\n")
        self.debug = self.target / "debug"

    def test_keeps_small_cache_and_supports_disabling_cleanup(self):
        self.assertFalse(prune_rust_cache(self.root, 10))
        self.assertFalse(prune_rust_cache(self.root, 0))
        self.assertTrue((self.debug / "artifact").exists())

    def test_cleans_oversized_cache_preserving_releases_source_and_lock(self):
        lock = self.debug / ".cargo-lock"
        lock.touch()
        inode = lock.stat().st_ino
        self.assertTrue(prune_rust_cache(self.root, 1 / 1024 / 1024))
        self.assertEqual(list(self.debug.iterdir()), [lock])
        self.assertEqual(lock.stat().st_ino, inode)
        for relative in ("release/artifact", "aarch64-apple-darwin/release/artifact"):
            self.assertTrue((self.target / relative).exists())
        self.assertTrue((self.project / "source.rs").exists())

    def test_skips_cache_while_cargo_build_lock_is_held(self):
        with (self.debug / ".cargo-lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse(prune_rust_cache(self.root, 1 / 1024 / 1024))
            self.assertTrue((self.debug / "artifact").exists())

    def test_rejects_invalid_limits_and_missing_cache_tag(self):
        for limit in (-1, float("nan"), float("inf")):
            with self.assertRaises(ValueError):
                prune_rust_cache(self.root, limit)
        (self.target / "CACHEDIR.TAG").unlink()
        with self.assertRaisesRegex(ValueError, "cache tag"):
            prune_rust_cache(self.root, 1 / 1024 / 1024)
        self.assertTrue((self.debug / "artifact").exists())

    def test_refuses_redirected_cache_and_does_not_follow_child_symlinks(self):
        redirected = self.target / "saved-debug"
        self.debug.rename(redirected)
        self.debug.symlink_to(redirected, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "redirected"):
            prune_rust_cache(self.root, 1 / 1024 / 1024)
        self.debug.unlink()
        redirected.rename(self.debug)
        (self.debug / "linked-release").symlink_to(self.target / "release", target_is_directory=True)
        self.assertTrue(prune_rust_cache(self.root, 1 / 1024 / 1024))
        self.assertTrue((self.target / "release/artifact").exists())

    def test_does_nothing_before_first_build(self):
        self.assertFalse(prune_rust_cache(self.root / "new-checkout", 10))


if __name__ == "__main__":
    unittest.main()
