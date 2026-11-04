#!/usr/bin/env python3
"""Real pinned jj extension qualification, only in a new disposable directory.

This tests the delegated LocalWorkingCopy compatibility prototype, not Rudder's
VFS, live snapshots, CoW launch, authority or publication protocol.
"""

import argparse
import datetime
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import time
import unittest
import re


ROOT = Path(__file__).resolve().parent.parent
PARSER = argparse.ArgumentParser()
PARSER.add_argument("--binary", type=Path, default=ROOT.parent / "target/debug/rudder-jj")
PARSER.add_argument("--stock-jj", type=Path, default=Path(shutil.which("jj") or "jj"))
PARSER.add_argument("--parent-mount-namespace")
PARSER.add_argument("--scratch-root", type=Path, required=True)
ARGS, UNITTEST_ARGS = PARSER.parse_known_args()
ARGS.binary = ARGS.binary.resolve()
FIXTURE = Path(tempfile.mkdtemp(prefix="qualification-", dir=ARGS.scratch_root))
REPORT = {
    "format_version": 1,
    "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "fixture": str(FIXTURE),
    "binary": str(ARGS.binary),
    "binary_sha256": hashlib.sha256(ARGS.binary.read_bytes()).hexdigest(),
    "jj_crate_versions": {"jj-cli": "0.40.0", "jj-lib": "0.40.0"},
    "upstream_commit": "941d0290f5511db1d1cf8e4fe7365ba7d1828ea0",
    "scope": "explicit local-development CLI compatibility only; not the production WorkspaceView provider",
    "commands": [],
    "checks": [],
}
(FIXTURE / "home").mkdir(mode=0o700)
CONFIG = FIXTURE / "home/config.toml"
CONFIG.write_text('[user]\nname = "Rudder Prototype"\nemail = "prototype@invalid.example"\n')
# Deliberately do not inherit auth, user provider config, Git hooks or editors.
ENV = {k: v for k, v in os.environ.items() if k in ["PATH", "LANG", "LC_ALL", "TERM"]}
ENV.update(
    HOME=str(FIXTURE / "home"),
    JJ_CONFIG=str(CONFIG),
    XDG_CONFIG_HOME=str(FIXTURE / "home"),
    GIT_CONFIG_NOSYSTEM="1",
    GIT_CONFIG_GLOBAL=os.devnull,
    GIT_AUTHOR_NAME="Rudder Prototype",
    GIT_AUTHOR_EMAIL="prototype@invalid.example",
    GIT_COMMITTER_NAME="Rudder Prototype",
    GIT_COMMITTER_EMAIL="prototype@invalid.example",
    PAGER="cat",
)


def run(cwd, command, *, ok=True):
    started = time.monotonic_ns()
    cp = subprocess.run(command, cwd=cwd, env=ENV, text=True, capture_output=True, timeout=30)
    REPORT["commands"].append({
        "cwd": str(cwd.relative_to(FIXTURE)),
        "argv": [str(x) for x in command],
        "status": cp.returncode,
        "elapsed_ms": (time.monotonic_ns() - started) / 1_000_000,
        "stdout": cp.stdout[:16384],
        "stderr": cp.stderr[:16384],
    })
    if ok and cp.returncode:
        raise AssertionError(f"{command!r} failed ({cp.returncode}):\n{cp.stdout}\n{cp.stderr}")
    return cp


def jj(cwd, *args, ok=True):
    return run(cwd, [str(ARGS.binary), "--no-pager", "--color=never", *args], ok=ok)


def git(cwd, *args, ok=True):
    return run(cwd, ["git", *args], ok=ok)


def identity(cwd, revision="@"):
    value = jj(cwd, "log", "-r", revision, "--no-graph", "-T", 'commit_id ++ " " ++ change_id').stdout
    return tuple(value.strip().split())


def fingerprint(root):
    """Hash names, types, modes and contents, including every metadata byte."""
    h = hashlib.sha256()
    size = count = 0
    for path in sorted(root.rglob("*")):
        st = path.lstat()
        h.update(str(path.relative_to(root)).encode() + b"\0")
        h.update(str(stat.S_IMODE(st.st_mode)).encode() + b"\0")
        if path.is_symlink():
            value = os.readlink(path).encode()
        elif path.is_file():
            value = path.read_bytes()
            size += len(value)
            count += 1
        else:
            value = b"directory"
        h.update(value + b"\0")
    return {"sha256": h.hexdigest(), "files": count, "bytes": size}


def share_seed_readonly(seed):
    if ARGS.parent_mount_namespace:
        current = os.readlink("/proc/self/ns/mnt")
        if current == ARGS.parent_mount_namespace:
            raise RuntimeError("refusing to mount in caller's mount namespace")
        run(FIXTURE, ["mount", "--make-rprivate", "/"])
        run(FIXTURE, ["mount", "--bind", str(seed), str(seed)])
        run(FIXTURE, ["mount", "-o", "remount,bind,ro", str(seed), str(seed)])
        try:
            (seed / "must-not-be-writable").write_text("bad")
        except OSError as error:
            if error.errno != errno.EROFS:
                raise
            REPORT["seed_readonly"] = {"kernel_enforced": True, "write_errno": error.errno}
        else:
            raise AssertionError("read-only seed mount accepted a write")
    else:
        # Portable non-adversarial fallback: integrity is checked before/after.
        for item in seed.rglob("*"):
            item.chmod(0o555 if item.is_dir() else 0o444)
        seed.chmod(0o555)
        REPORT["seed_readonly"] = {"kernel_enforced": False, "mode_only": True}


class WorkingCopyExtension(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.template = FIXTURE / "prepared-template"
        cls.template.mkdir()
        REPORT["version"] = jj(cls.template, "--version").stdout.strip()
        REPORT["git_version"] = git(cls.template, "--version").stdout.strip()
        jj(cls.template, "rudder-init-local")
        (cls.template / "base.txt").write_text("base\n")
        (cls.template / "tool.sh").write_text("#!/bin/sh\nprintf 'fixture\\n'\n")
        (cls.template / "tool.sh").chmod(0o755)
        (cls.template / "link").symlink_to("base.txt")
        (cls.template / ".gitignore").write_text("/deps/\n/scratch/\n")
        jj(cls.template, "--config", "git.write-change-id-header=false", "describe", "-m", "legacy baseline")
        cls.legacy = identity(cls.template)
        legacy_raw = git(cls.template, "cat-file", "commit", cls.legacy[0]).stdout
        if "\nchange-id " in legacy_raw:
            raise AssertionError("legacy fixture unexpectedly contains a change-id header")
        jj(cls.template, "bookmark", "create", "legacy", "-r", "@")
        jj(cls.template, "new", "-m", "modern baseline")
        (cls.template / "modern.txt").write_text("modern\n")
        jj(cls.template, "status")
        cls.modern = identity(cls.template)
        modern_raw = git(cls.template, "cat-file", "commit", cls.modern[0]).stdout
        if "\nchange-id " not in modern_raw:
            raise AssertionError("modern fixture lacks its jj change-id header")
        jj(cls.template, "bookmark", "create", "baseline", "-r", "@")
        jj(cls.template, "new", "-m", "prepared attempt template")
        (cls.template / "deps").mkdir()
        (cls.template / "deps/prepared.txt").write_text("ignored prepared dependency\n")
        jj(cls.template, "status")
        git(cls.template, "repack", "-a", "-d")
        cls.seed = FIXTURE / "immutable-seed/objects"
        shutil.copytree(cls.template / ".git/objects", cls.seed)
        share_seed_readonly(cls.seed)
        cls.seed_before = fingerprint(cls.seed)
        cls.template_before = fingerprint(cls.template)
        REPORT["seed"] = cls.seed_before
        REPORT["baseline"] = {"legacy": cls.legacy, "modern": cls.modern}

    @classmethod
    def tearDownClass(cls):
        if fingerprint(cls.seed) != cls.seed_before:
            raise AssertionError("shared object seed was mutated")
        if fingerprint(cls.template) != cls.template_before:
            raise AssertionError("prepared template was mutated")

    def setUp(self):
        self.directory = FIXTURE / self._testMethodName
        self.directory.mkdir()
        self.a = self.fork("a")
        self.b = self.fork("b")

    def fork(self, name, source=None):
        source = source or self.template
        target = self.directory / name
        # Eager metadata/source copies are intentional prototype setup, NOT CoW
        # or launch performance. No hardlinks or shared writable VCS state.
        def ignore(directory, names):
            return ["objects"] if Path(directory) == source / ".git" else []
        shutil.copytree(source, target, ignore=ignore, symlinks=True)
        (target / ".git/objects/info").mkdir(parents=True)
        (target / ".git/objects/info/alternates").write_text(str(self.seed) + "\n")
        return target

    def test_registration_and_normal_commands(self):
        self.assertEqual((self.a / ".jj/working_copy/type").read_text(), "rudder-local-development-v1")
        self.assertEqual((self.a / ".jj/repo/store/git_target").read_text(), "../../../.git")
        cp = run(self.a, [str(ARGS.stock_jj), "status"], ok=False)
        self.assertNotEqual(cp.returncode, 0)
        self.assertIn("rudder-local-development-v1", cp.stderr)
        self.assertIn("working copy has no changes", jj(self.a, "status").stdout)
        self.assertEqual(identity(self.a, "legacy"), self.legacy)
        self.assertEqual(identity(self.a, "baseline"), self.modern)
        before = identity(self.a)
        (self.a / "base.txt").write_text("private edit\n")
        self.assertIn("base.txt", jj(self.a, "status").stdout)
        self.assertIn("private edit", jj(self.a, "diff", "--git").stdout)
        jj(self.a, "describe", "-m", "real extension edit")
        after = identity(self.a)
        self.assertNotEqual(before[0], after[0])
        self.assertEqual(before[1], after[1], "snapshot/description must preserve jj change identity")
        jj(self.a, "new", "-m", "real new change")
        self.assertNotEqual(identity(self.a)[1], after[1])
        self.assertTrue((self.a / "tool.sh").stat().st_mode & stat.S_IXUSR)
        self.assertEqual(os.readlink(self.a / "link"), "base.txt")
        self.assertEqual(jj(self.a, "file", "list").stdout.splitlines(), [".gitignore", "base.txt", "link", "modern.txt", "tool.sh"])

    def test_private_metadata_and_object_isolation(self):
        before_b = fingerprint(self.b)
        (self.a / "only-a.txt").write_text("private new object a\n")
        jj(self.a, "describe", "-m", "private a")
        a_commit, _ = identity(self.a)
        jj(self.a, "bookmark", "create", "only-a", "-r", "@")
        jj(self.a, "new", "-m", "a child")
        self.assertEqual(fingerprint(self.b), before_b, "mutating a must not touch any b metadata or bytes")
        self.assertFalse((self.b / "only-a.txt").exists())
        self.assertNotEqual(git(self.b, "cat-file", "-e", a_commit, ok=False).returncode, 0)
        self.assertNotEqual(git(self.b, "show-ref", "--verify", "refs/heads/only-a", ok=False).returncode, 0)
        self.assertEqual((self.b / "base.txt").read_text(), "base\n")
        for part in [".jj/working_copy/checkout", ".jj/repo/workspace_store/workspaces", ".git/config"]:
            if (self.a / part).exists():
                self.assertNotEqual((self.a / part).stat().st_ino, (self.b / part).stat().st_ino)
        self.assertTrue(any((self.a / ".git/objects").glob("??/*")), "new objects must be written privately")
        self.assertEqual(fingerprint(self.seed), self.seed_before)
        self.assertEqual(fingerprint(self.template), self.template_before)

    def test_real_conflict_and_git_header_round_trip(self):
        jj(self.a, "new", "baseline", "-m", "left")
        (self.a / "base.txt").write_text("left\n")
        jj(self.a, "bookmark", "create", "left", "-r", "@")
        jj(self.a, "new", "baseline", "-m", "right")
        (self.a / "base.txt").write_text("right\n")
        jj(self.a, "bookmark", "create", "right", "-r", "@")
        jj(self.a, "new", "left", "right", "-m", "unresolved merge")
        conflict_commit, conflict_change = identity(self.a)
        self.assertEqual(jj(self.a, "log", "-r", "@", "--no-graph", "-T", "conflict").stdout, "true")
        self.assertIn("base.txt", jj(self.a, "status").stdout)
        marker = (self.a / "base.txt").read_text()
        self.assertIn("<<<<<<<", marker)
        raw = git(self.a, "cat-file", "commit", conflict_commit).stdout
        self.assertIn("\njj:trees ", raw)
        self.assertIn("\njj:conflict-labels ", raw)
        self.assertIn("\nchange-id ", raw)
        self.assertEqual(sum(line.startswith("parent ") for line in raw.splitlines()), 2)
        jj(self.a, "bookmark", "create", "unresolved", "-r", "@")
        jj(self.a, "git", "export")
        self.assertEqual(git(self.a, "rev-parse", "refs/heads/unresolved").stdout.strip(), conflict_commit)
        # An independent ordinary stock-jj repository imports actual Git objects,
        # without copying any jj op/view/extra metadata from the custom workspace.
        export = self.directory / "git-roundtrip"
        export.mkdir()
        git(export, "init", "-q")
        (export / ".git/objects/info/alternates").write_text(str(self.a / ".git/objects") + "\n")
        git(export, "update-ref", "refs/heads/unresolved", conflict_commit)
        run(export, [str(ARGS.stock_jj), "git", "init", "--colocate"])
        cp = run(export, [str(ARGS.stock_jj), "log", "-r", "unresolved", "--no-graph", "-T", 'commit_id ++ " " ++ change_id ++ " " ++ conflict'])
        self.assertEqual(cp.stdout.strip(), f"{conflict_commit} {conflict_change} true")
        run(export, [str(ARGS.stock_jj), "edit", "unresolved"])
        self.assertEqual((export / "base.txt").read_text(), marker, "fresh stock jj must retain conflict sides and labels")
        # Resolving the materialized conflict rewrites the same jj change, leaving
        # the exported old conflict commit and b's history intact.
        (self.a / "base.txt").write_text("resolved\n")
        jj(self.a, "status")
        resolved_commit, resolved_change = identity(self.a)
        self.assertNotEqual(resolved_commit, conflict_commit)
        self.assertEqual(resolved_change, conflict_change)
        self.assertEqual(jj(self.a, "log", "-r", "@", "--no-graph", "-T", "conflict").stdout, "false")
        self.assertEqual(identity(self.b, "baseline"), self.modern)
        REPORT["conflict_roundtrip"] = {"commit": conflict_commit, "change": conflict_change, "resolved_commit": resolved_commit}

    def test_legacy_extra_metadata_is_required_and_copied_privately(self):
        self.assertEqual(identity(self.a, "legacy"), self.legacy)
        self.assertEqual(identity(self.b, "legacy"), self.legacy)
        control = self.directory / "git-only-control"
        control.mkdir()
        git(control, "init", "-q")
        (control / ".git/objects/info/alternates").write_text(str(self.seed) + "\n")
        git(control, "update-ref", "refs/heads/legacy", self.legacy[0])
        run(control, [str(ARGS.stock_jj), "git", "init", "--colocate"])
        imported = run(control, [str(ARGS.stock_jj), "log", "-r", "legacy", "--no-graph", "-T", "change_id"]).stdout.strip()
        self.assertNotEqual(imported, self.legacy[1], "Git-only import is expected to lose legacy jj change metadata")
        REPORT["legacy_extra"] = {"preserved_change": self.legacy[1], "git_only_derived_change": imported}

    def test_checkout_restore_and_real_git_commit_import(self):
        jj(self.a, "new", "baseline", "-m", "scratch changes")
        (self.a / "base.txt").write_text("must restore\n")
        jj(self.a, "status")
        jj(self.a, "restore", "--from", "baseline", "base.txt")
        self.assertEqual((self.a / "base.txt").read_text(), "base\n")
        jj(self.a, "edit", "legacy")
        self.assertFalse((self.a / "modern.txt").exists())
        jj(self.a, "new", "baseline", "-m", "git interop")
        (self.a / "git-authored.txt").write_text("ordinary Git bytes\n")
        git(self.a, "add", "git-authored.txt")
        git(self.a, "commit", "-q", "-m", "Git-authored commit")
        git_commit = git(self.a, "rev-parse", "HEAD").stdout.strip()
        jj(self.a, "status")
        self.assertEqual(identity(self.a, git_commit)[0], git_commit)
        self.assertIn("ordinary Git bytes", jj(self.a, "file", "show", "-r", git_commit, "git-authored.txt").stdout)
        self.assertEqual(identity(self.a, "legacy"), self.legacy)

    def test_git_version_is_not_global_gate_but_fetch_needs_porcelain(self):
        remote = self.directory / "remote.git"
        git(self.directory, "init", "--bare", "-q", str(remote))
        jj(self.a, "git", "remote", "add", "fixture", str(remote))
        jj(self.a, "bookmark", "create", "to-export", "-r", "baseline")
        jj(self.a, "git", "export")
        pushed = jj(self.a, "git", "push", "--remote", "fixture", "--bookmark", "to-export", ok=False)
        self.assertEqual(pushed.returncode, 0, pushed.stderr)
        self.assertEqual(git(self.directory, "--git-dir", str(remote), "rev-parse", "refs/heads/to-export").stdout.strip(), self.modern[0])
        fetched = jj(self.a, "git", "fetch", "--remote", "fixture", ok=False)
        version = tuple(int(n) for n in re.search(r"(\d+)\.(\d+)\.(\d+)", REPORT["git_version"]).groups())
        if version < (2, 41, 0):
            self.assertNotEqual(fetched.returncode, 0)
            self.assertIn("porcelain", fetched.stderr)
            self.assertIn("git >= 2.41.0", fetched.stderr)
            fetch_result = "rejected unsupported --porcelain with minimum-version guidance"
        else:
            self.assertEqual(fetched.returncode, 0, fetched.stderr)
            fetch_result = "passed"
        REPORT["git_version_qualification"] = {
            "version": REPORT["git_version"],
            "local_commands": "passed",
            "local_bare_push": "passed",
            "local_bare_fetch": fetch_result,
        }


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(WorkingCopyExtension)
    REPORT["checks"] = [test.id() for test in suite]
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    REPORT["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    REPORT["tests"] = {"run": outcome.testsRun, "failures": len(outcome.failures), "errors": len(outcome.errors), "successful": outcome.wasSuccessful()}
    report_path = FIXTURE / "report.json"
    report_path.write_text(json.dumps(REPORT, indent=2) + "\n")
    print(f"Report: {report_path}")
    raise SystemExit(0 if outcome.wasSuccessful() else 1)
