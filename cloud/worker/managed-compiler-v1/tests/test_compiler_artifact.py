"""Artifact DATA/parser regressions. No fake compiler execution qualification.

Root runs these separately; the tiny files below prove closure/refusal behavior,
not that a real Rust/TS/Buck2 toolchain executes or can issue a prepared owner.
"""
import importlib.util
import io
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / "artifact/compiler_artifact.py"
SPEC = importlib.util.spec_from_file_location("compiler_artifact", SOURCE)
assets = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assets)


class ArtifactData(unittest.TestCase):
    def setUp(self):
        base = Path("/workspace") if Path("/workspace").is_dir() else Path(tempfile.gettempdir())
        self.scratch = Path(tempfile.mkdtemp(prefix="compiler-artifact-data-", dir=base)).resolve()
        self.root = self.scratch / "artifact"
        self.root.mkdir()

    def tearDown(self):
        for directory, _, _ in os.walk(self.scratch):
            Path(directory).chmod(0o700)
        shutil.rmtree(self.scratch)

    def fixture(self):
        for name in assets.REQUIRED_FILES + ("prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-fixture.rlib",):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture DATA, not compiler or runtime authority\n")
            path.chmod(0o444)
        for name in assets.REQUIRED_DIRS:
            (self.root / name).mkdir(parents=True, exist_ok=True)
        for directory, _, _ in os.walk(self.root, topdown=False):
            Path(directory).chmod(0o555)
        entries, size = assets.inventory(self.root, assets.Clock(), os.getuid(), os.getgid())
        manifest = {"format_version": 1, "profile_id": assets.PROFILE, "entries": entries, "logical_bytes": size}
        self.root.chmod(0o755)
        (self.root / "manifest.json").write_bytes(assets.canonical(manifest))
        (self.root / "manifest.json").chmod(0o444)
        self.root.chmod(0o555)
        return manifest

    def verify(self):
        return assets.verify(self.root, assets.Clock(), expected_uid=os.getuid(), expected_gid=os.getgid())

    def test_full_inventory_then_actual_same_size_corruption_refused(self):
        self.fixture()
        observed = self.verify()
        self.assertEqual(observed["kind"], "compiler-artifact-data")
        path = self.root / "prepared/tools/node"
        path.chmod(0o644)
        original = path.read_bytes()
        path.write_bytes(b"X" + original[1:])
        path.chmod(0o444)
        with self.assertRaisesRegex(ValueError, "closure differs"):
            self.verify()

    def test_extra_file_and_writable_file_refused(self):
        self.fixture()
        self.root.chmod(0o755)
        extra = self.root / "unlisted"
        extra.write_bytes(b"undeclared input")
        extra.chmod(0o444)
        self.root.chmod(0o555)
        with self.assertRaisesRegex(ValueError, "closure differs"):
            self.verify()
        self.root.chmod(0o755)
        extra.unlink()
        self.root.chmod(0o555)
        (self.root / "buck2").chmod(0o644)
        with self.assertRaisesRegex(ValueError, "writable"):
            self.verify()

    def test_symlink_does_not_reopen_outside_bytes(self):
        self.fixture()
        path = self.root / "prepared/tools/node"
        path.parent.chmod(0o755)
        path.unlink()
        outside = self.scratch / "outside"
        outside.write_bytes(b"outside")
        path.symlink_to(outside)
        path.parent.chmod(0o555)
        with self.assertRaisesRegex(ValueError, "link"):
            self.verify()

    def test_required_layout_and_unsafe_manifest_paths_refused(self):
        manifest = self.fixture()
        manifest["entries"] = [entry for entry in manifest["entries"] if entry["path"] != "buck2"]
        with self.assertRaises(ValueError):
            assets.validate_shape(manifest)
        for name in ("../escape", "/absolute", "a//b", "a/../b", "a/./b", "a\0b"):
            with self.assertRaises(ValueError):
                assets.relative(name)

    def test_manifest_bound_and_wrong_explicit_pin_are_refused(self):
        self.fixture()
        with self.assertRaisesRegex(ValueError, "pin differs"):
            assets.verify(self.root, assets.Clock(), "0" * 64, os.getuid(), os.getgid())
        with patch.object(assets, "MAX_MANIFEST", 1):
            # read_json's default is fixed at module definition, so exercise its
            # explicit bound: production read_json uses MAX_MANIFEST unchanged.
            with self.assertRaisesRegex(ValueError, "JSON file type/bound"):
                assets.read_json(self.root / "manifest.json", assets.MAX_MANIFEST)

    def test_expired_original_clock_never_hashes_into_success(self):
        path = self.scratch / "bytes"
        path.write_bytes(b"unchanged")
        clock = assets.Clock()
        clock.end = 0
        with self.assertRaisesRegex(ValueError, "original.*deadline"):
            assets.hash_file(path, clock)

    def test_typescript_link_traversal_duplicate_and_expansion_bounds(self):
        for kind in ("symlink", "traversal", "duplicate", "oversize"):
            with self.subTest(kind=kind):
                archive = self.scratch / (kind + ".tgz")
                with tarfile.open(archive, "w:gz") as stream:
                    member = tarfile.TarInfo("package/../escape" if kind == "traversal" else "package/entry")
                    if kind == "symlink":
                        member.type, member.linkname = tarfile.SYMTYPE, "../../outside"
                    elif kind == "oversize":
                        member.size = 32 * 1024**2 + 1
                    else:
                        member.size = 1
                    if kind == "oversize":
                        # A header alone must be refused before extraction or
                        # allocation of the advertised content.
                        stream.fileobj.write(member.tobuf())
                    else:
                        stream.addfile(member, None if kind == "symlink" else io.BytesIO(b"x"))
                        if kind == "duplicate":
                            stream.addfile(member, io.BytesIO(b"x"))
                output = self.scratch / kind
                with self.assertRaises((ValueError, tarfile.TarError)):
                    assets.extract_typescript(archive, output, assets.Clock())
                self.assertFalse((self.scratch / "escape").exists())

    def test_public_release_redirect_cannot_escape_to_another_origin(self):
        redirect = assets.PinnedRedirect("github.com", assets.Clock())
        request = assets.urllib.request.Request("https://github.com/meta/buck2/releases/download/pinned/asset")
        accepted = redirect.redirect_request(request, None, 302, "", {}, "https://release-assets.githubusercontent.com/owned?public=signed")
        self.assertEqual(accepted.host, "release-assets.githubusercontent.com")
        redirect.redirect_request(request, None, 302, "", {}, "https://objects.githubusercontent.com/owned")
        with self.assertRaises(ValueError):
            redirect.redirect_request(request, None, 302, "", {}, "https://objects.githubusercontent.com/third")
        for target in ("http://release-assets.githubusercontent.com/asset", "https://evil.invalid/asset",
                       "https://token@release-assets.githubusercontent.com/asset"):
            with self.assertRaises(ValueError):
                assets.PinnedRedirect("github.com", assets.Clock()).redirect_request(request, None, 302, "", {}, target)
        with self.assertRaises(ValueError):
            assets.PinnedRedirect("registry.npmjs.org", assets.Clock()).redirect_request(request, None, 302, "", {}, "https://objects.githubusercontent.com/asset")

    def test_source_mode_refusal_reports_original_metadata_and_keeps_bytes(self):
        source = self.scratch / "source"
        source.write_bytes(b"original pinned-input fixture DATA")
        source.chmod(0o666)
        builder = assets.Builder(self.scratch / "fresh-output", assets.Clock())
        with self.assertRaisesRegex(ValueError, "source artifact is not protected") as caught:
            builder.copy_file(source, builder.output / "copy", self.scratch)
        message = str(caught.exception)
        for expected in (str(source), "mode=0o0666", f"uid={os.getuid()}", f"gid={os.getgid()}", "type="):
            self.assertIn(expected, message)
        self.assertEqual(source.read_bytes(), b"original pinned-input fixture DATA")
        self.assertFalse((builder.output / "copy").exists())

    def test_normalization_cannot_mutate_arbitrary_source_tree(self):
        source = self.scratch / "unrelated"
        source.mkdir()
        rustc = source / "bin/rustc"
        rustc.parent.mkdir()
        rustc.write_bytes(b"not the isolated fixed Rust image input")
        rustc.chmod(0o666)
        with self.assertRaisesRegex(ValueError, "isolated fixed pinned Rust"):
            assets.normalize_pinned_rust(source, rustc, [], "saved-image-DATA", self.scratch, assets.Clock())
        self.assertEqual(rustc.stat().st_mode & 0o777, 0o666)
        self.assertEqual(rustc.read_bytes(), b"not the isolated fixed Rust image input")


if __name__ == "__main__":
    unittest.main()
