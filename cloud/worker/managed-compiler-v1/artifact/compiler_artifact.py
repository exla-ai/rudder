#!/usr/bin/env python3
"""Build/verify separate compiler artifact DATA. Never issues runtime authority.

Only the build subcommand executes tools, all from the two explicitly pinned
source images. No rustup, HOME discovery, npm scripts, model or credential IO.
"""
import argparse
import base64
import ctypes
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import stat
import subprocess
import tarfile
import time
import urllib.request
import urllib.parse

PROFILE = "rudder-linux-rust-typescript-v1"
CHUNK = 65536
MAX_FILES = 65536
MAX_BYTES = 2 * 1024**3  # This separate artifact, NOT a change to worker quota.
MAX_MANIFEST = 32 * 1024**2
REQUIRED_FILES = (
    "buck2", "prepared/tools/rust/bin/rustc", "prepared/tools/node",
    "prepared/tools/typescript/lib/tsc.js", "prepared/tools/python/bin/python3",
    "prepared/tools/unshare", "prepared/lib64/ld-linux-x86-64.so.2",
)
REQUIRED_DIRS = tuple("prepared/" + x for x in ("workspace", "tmp", "dev", "proc"))


def need(value, message):
    if not value:
        raise ValueError(message)


class Clock:
    def __init__(self):
        if platform.system() == "Linux":
            need(hasattr(time, "CLOCK_BOOTTIME"), "Linux suspend-inclusive build clock unavailable")
            self.read = lambda: time.clock_gettime(time.CLOCK_BOOTTIME)
        else:
            # DATA verifier fixtures are portable; Linux is required to build.
            self.read = time.monotonic
        self.last = self.read()
        self.end = self.last + 300

    def remaining(self):
        now = self.read()
        need(now >= self.last, "artifact original clock regressed")
        self.last = now
        value = self.end - now
        need(value > 0, "original compiler-artifact build/verify deadline expired")
        return value


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def pairs(items):
    out = {}
    for key, value in items:
        need(key not in out, "duplicate JSON key")
        out[key] = value
    return out


def read_json(path, limit=MAX_MANIFEST):
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_size <= limit, "JSON file type/bound")
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        need(version(os.fstat(stream.fileno())) == version(before), "JSON changed at open")
        raw = stream.read(limit + 1)
        need(version(os.fstat(stream.fileno())) == version(before), "held JSON changed")
    need(len(raw) == before.st_size and len(raw) <= limit and version(path.lstat()) == version(before), "JSON bound/identity changed")
    return json.loads(raw, object_pairs_hook=pairs), raw


def relative(value):
    need(isinstance(value, str) and 0 < len(value.encode()) <= 1024 and "\0" not in value, "artifact path bound")
    path = PurePosixPath(value)
    need(not path.is_absolute() and str(path) == value and len(path.parts) <= 32
         and all(part not in ("", ".", "..") for part in path.parts), "unsafe artifact path")
    return path


def version(meta):
    return (meta.st_dev, meta.st_ino, meta.st_size, meta.st_mode,
            meta.st_mtime_ns, meta.st_ctime_ns, meta.st_uid, meta.st_gid)


def hash_file(path, clock, maximum=MAX_BYTES):
    clock.remaining()
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_size <= maximum, "file type/size unsupported")
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    digest, count = hashlib.sha256(), 0
    with os.fdopen(fd, "rb") as stream:
        need(version(os.fstat(stream.fileno())) == version(before), "original file changed at open")
        while data := stream.read(CHUNK):
            clock.remaining()
            count += len(data)
            need(count <= before.st_size, "file grew during hash")
            digest.update(data)
        need(version(os.fstat(stream.fileno())) == version(before), "held file changed during hash")
    need(count == before.st_size and version(path.lstat()) == version(before), "file changed during hash")
    clock.remaining()
    return digest.hexdigest(), count


def inventory(root, clock, expected_uid=0, expected_gid=0):
    entries, total = [], 0
    pending = [(root, "")]
    while pending:
        directory, prefix = pending.pop()
        before = directory.lstat()
        need(stat.S_ISDIR(before.st_mode) and before.st_uid == expected_uid and before.st_gid == expected_gid
             and stat.S_IMODE(before.st_mode) == 0o555, "artifact directory ownership/mode/type")
        with os.scandir(directory) as scan:
            children = sorted(scan, key=lambda item: os.fsencode(item.name))
        for item in children:
            clock.remaining()
            name = prefix + item.name
            relative(name)
            if name == "manifest.json":
                continue
            need(len(entries) < MAX_FILES, "artifact entry bound exceeded")
            path, meta = Path(item.path), item.stat(follow_symlinks=False)
            need(meta.st_uid == expected_uid and meta.st_gid == expected_gid, "artifact owner/group changed")
            mode = stat.S_IMODE(meta.st_mode)
            if stat.S_ISDIR(meta.st_mode):
                need(mode == 0o555, "artifact directory mode")
                entries.append({"path": name, "kind": "directory", "mode": mode})
                pending.append((path, name + "/"))
            else:
                need(stat.S_ISREG(meta.st_mode) and mode in (0o444, 0o555), "artifact special/link/writable entry")
                digest, size = hash_file(path, clock)
                total += size
                need(total <= MAX_BYTES, "separate compiler artifact byte bound exceeded")
                entries.append({"path": name, "kind": "file", "mode": mode, "size_bytes": size, "sha256": digest})
        need(version(directory.lstat()) == version(before), "artifact directory changed during inventory")
    entries.sort(key=lambda entry: entry["path"].encode())
    return entries, total


def validate_shape(manifest):
    need(set(manifest) == {"format_version", "profile_id", "entries", "logical_bytes"}
         and type(manifest["format_version"]) is int and manifest["format_version"] == 1
         and manifest["profile_id"] == PROFILE, "artifact manifest schema/profile")
    entries = manifest["entries"]
    need(isinstance(entries, list) and 0 < len(entries) <= MAX_FILES, "artifact entry bound")
    prior, seen, total = None, {}, 0
    for entry in entries:
        path = str(relative(entry["path"]))
        need(path != "manifest.json" and (prior is None or prior < path.encode()), "manifest paths duplicate/unordered")
        prior = path.encode()
        parent = str(PurePosixPath(path).parent)
        need(parent == "." or seen.get(parent) == "directory", "manifest lacks declared parent directory")
        if entry["kind"] == "directory":
            need(set(entry) == {"path", "kind", "mode"} and type(entry["mode"]) is int
                 and entry["mode"] == 0o555, "directory manifest fields")
        else:
            need(entry["kind"] == "file" and set(entry) == {"path", "kind", "mode", "size_bytes", "sha256"}
                 and type(entry["mode"]) is int and entry["mode"] in (0o444, 0o555) and type(entry["size_bytes"]) is int
                 and 0 <= entry["size_bytes"] <= MAX_BYTES
                 and re.fullmatch("[0-9a-f]{64}", entry["sha256"]) is not None, "file manifest fields")
            total += entry["size_bytes"]
            need(total <= MAX_BYTES, "manifest byte bound")
        seen[path] = entry["kind"]
    need(type(manifest["logical_bytes"]) is int and manifest["logical_bytes"] == total, "manifest total differs")
    need(all(seen.get(path) == "file" for path in REQUIRED_FILES)
         and all(seen.get(path) == "directory" for path in REQUIRED_DIRS), "required compiler layout absent")
    need(any(path.startswith("prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/")
             and path.endswith(".rlib") for path in seen), "Rust standard-library closure absent")


def verify(root, clock, expected_sha=None, expected_uid=0, expected_gid=0):
    need(root.is_absolute() and root.resolve(strict=True) == root, "artifact root redirects")
    meta = (root / "manifest.json").lstat()
    need(stat.S_ISREG(meta.st_mode) and meta.st_uid == expected_uid and meta.st_gid == expected_gid
         and stat.S_IMODE(meta.st_mode) == 0o444, "manifest owner/mode/type")
    manifest, raw = read_json(root / "manifest.json")
    digest = hashlib.sha256(raw).hexdigest()
    need(raw == canonical(manifest), "manifest must be canonical compact UTF8 without newline")
    need(expected_sha is None or digest == expected_sha, "artifact manifest pin differs")
    validate_shape(manifest)
    entries, total = inventory(root, clock, expected_uid, expected_gid)
    need(entries == manifest["entries"] and total == manifest["logical_bytes"], "actual compiler closure differs")
    clock.remaining()
    return {"format_version": 1, "kind": "compiler-artifact-data", "profile_id": PROFILE,
            "manifest_sha256": digest, "entries": len(entries), "logical_bytes": total}


class PinnedRedirect(urllib.request.HTTPRedirectHandler):
    def __init__(self, original, clock):
        self.original, self.clock, self.count = original, clock, 0

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # GitHub release URLs use a signed public asset URL. No bearer/auth
        # headers are ever supplied. npm redirects and arbitrary origins fail.
        target = urllib.parse.urlsplit(newurl)
        self.clock.remaining()
        self.count += 1
        need(self.original == "github.com" and self.count <= 2 and target.scheme == "https"
             and target.hostname in ("release-assets.githubusercontent.com", "objects.githubusercontent.com")
             and target.port in (None, 443) and target.username is None and target.password is None,
             "pinned artifact redirect origin/count refused")
        return urllib.request.Request(newurl, headers={"Accept-Encoding": "identity", "User-Agent": "rudder-compiler-artifact-v1"})


def fetch(url, destination, algorithm, expected, maximum, clock):
    need(not destination.exists() and url.startswith("https://"), "fresh HTTPS artifact required")
    digest, count = hashlib.new(algorithm), 0
    origin = urllib.parse.urlsplit(url)
    need(origin.hostname in ("github.com", "registry.npmjs.org") and origin.port in (None, 443)
         and origin.username is None and origin.password is None, "pinned artifact origin")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), PinnedRedirect(origin.hostname, clock))
    request = urllib.request.Request(url, headers={"Accept-Encoding": "identity", "User-Agent": "rudder-compiler-artifact-v1"})
    with opener.open(request, timeout=min(30, clock.remaining())) as response, destination.open("xb") as output:
        while data := response.read(CHUNK):
            clock.remaining()
            count += len(data)
            need(count <= maximum, "compressed artifact bound exceeded")
            digest.update(data)
            output.write(data)
    need(digest.digest() == expected, "pinned artifact integrity differs")
    clock.remaining()


def extract_typescript(archive, output, clock):
    count, total, seen = 0, 0, set()
    with tarfile.open(archive, mode="r|gz") as stream:
        for member in stream:
            clock.remaining()
            count += 1
            need(count <= 8192 and member.name.startswith("package/"), "TypeScript archive entry bound/prefix")
            name = member.name.removeprefix("package/").rstrip("/")
            if not name:
                need(member.isdir(), "TypeScript archive root type")
                continue
            relative(name)
            need(name not in seen and (member.isfile() or member.isdir()), "TypeScript archive duplicate/link/special entry")
            seen.add(name)
            path = output / name
            if member.isdir():
                path.mkdir(parents=True, exist_ok=True)
                continue
            need(0 <= member.size <= 32 * 1024**2, "TypeScript member bound")
            total += member.size
            need(total <= 128 * 1024**2, "TypeScript expanded bound")
            path.parent.mkdir(parents=True, exist_ok=True)
            remaining = member.size
            with stream.extractfile(member) as source, path.open("xb") as dest:
                while remaining:
                    clock.remaining()
                    data = source.read(min(CHUNK, remaining))
                    need(data, "truncated TypeScript member")
                    dest.write(data)
                    remaining -= len(data)
            path.chmod(0o555 if member.mode & 0o111 else 0o444)


class Builder:
    def __init__(self, output, clock):
        need(output.is_absolute() and not output.exists(), "compiler output must be fresh")
        output.mkdir(mode=0o700)
        self.output, self.clock, self.entries, self.bytes, self.elf = output, clock, 0, 0, set()

    def copy_file(self, source, destination, allowed):
        self.clock.remaining()
        resolved = source.resolve(strict=True)
        need(resolved.is_relative_to(allowed.resolve(strict=True)), "source symlink escapes declared input")
        before = resolved.stat()
        need(stat.S_ISREG(before.st_mode) and before.st_uid == 0 and before.st_gid == 0
             and before.st_mode & 0o022 == 0,
             "source artifact is not protected regular image data: "
             f"path={source} resolved={resolved} mode={stat.S_IMODE(before.st_mode):#06o} "
             f"uid={before.st_uid} gid={before.st_gid} type={stat.S_IFMT(before.st_mode):#08o}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            need(hash_file(resolved, self.clock) == hash_file(destination, self.clock), "conflicting copied dependency")
            return
        self.entries += 1
        self.bytes += before.st_size
        need(self.entries <= MAX_FILES and self.bytes <= MAX_BYTES, "compiler construction bound")
        with resolved.open("rb") as src, destination.open("xb") as dst:
            need(version(os.fstat(src.fileno())) == version(before), "source open changed")
            prefix, copied = b"", 0
            while data := src.read(CHUNK):
                self.clock.remaining()
                if not prefix:
                    prefix = data[:4]
                copied += len(data)
                need(copied <= before.st_size, "source grew during construction")
                dst.write(data)
            need(copied == before.st_size and version(os.fstat(src.fileno())) == version(before)
                 and version(resolved.stat()) == version(before), "source changed during construction")
        destination.chmod(0o555 if before.st_mode & 0o111 else 0o444)
        if prefix == b"\x7fELF":
            self.elf.add(resolved)

    def copy_tree(self, source, destination, allowed, ancestors=()):
        self.clock.remaining()
        resolved = source.resolve(strict=True)
        need(resolved.is_relative_to(allowed.resolve(strict=True)) and len(ancestors) <= 32, "source tree escape/depth")
        before = resolved.stat()
        identity = (before.st_dev, before.st_ino)
        need(stat.S_ISDIR(before.st_mode) and before.st_uid == 0 and before.st_gid == 0
             and before.st_mode & 0o022 == 0 and identity not in ancestors, "source tree custody/cycle")
        if not destination.exists():
            self.entries += 1
            need(self.entries <= MAX_FILES, "compiler construction entry bound")
        destination.mkdir(parents=True, exist_ok=True)
        for child in sorted(resolved.iterdir(), key=lambda item: os.fsencode(item.name)):
            # Bytecode is generated scratch, not a required Python source input.
            if child.name == "__pycache__" or child.suffix == ".pyc":
                continue
            if child == Path("/usr/lib/python3.11/sitecustomize.py") and child.is_symlink():
                # Debian's ONE public stdlib bridge. No general /etc import.
                need(child.resolve(strict=True) == Path("/etc/python3.11/sitecustomize.py"), "Python public bridge changed")
                self.copy_file(child, destination / child.name, Path("/etc/python3.11"))
                continue
            if child.resolve(strict=True).is_dir():
                self.copy_tree(child, destination / child.name, allowed, ancestors + (identity,))
            else:
                self.copy_file(child, destination / child.name, allowed)
        need(version(resolved.stat()) == version(before), "source directory changed")


def run(command, clock):
    completed = subprocess.run(command, check=True, stdin=subprocess.DEVNULL, capture_output=True,
                               timeout=min(10, clock.remaining()), env={"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C"})
    need(len(completed.stdout) + len(completed.stderr) <= 256 * 1024, "pinned tool output bound")
    clock.remaining()
    return completed.stdout.decode().strip()


def record_prerequisites(args, clock):
    policy, _ = read_json(args.policy / "source-images.json", 65536)
    need(os.geteuid() == 0 and args.snapshot == policy["debian_snapshot"], "prerequisite snapshot differs")
    packages = run(["/usr/bin/dpkg-query", "-W", "-f=${binary:Package}\t${Version}\n"], clock)
    required = ("python3", "util-linux", "libc-bin", "libzstd1")
    rows = dict(line.split("\t", 1) for line in packages.splitlines())
    need(all(name in rows or any(key.split(":")[0] == name for key in rows) for name in required), "declared prerequisites absent")
    metadata = {}
    for path in sorted(Path("/var/lib/apt/lists").glob("*InRelease")):
        need(len(metadata) < 8, "apt release metadata bound")
        digest, size = hash_file(path, clock, 16 * 1024**2)
        metadata[path.name] = {"sha256": digest, "size_bytes": size}
    need(len(metadata) == 2 and platform.libc_ver() == ("glibc", "2.36"), "snapshot metadata/ABI differs")
    keyring_sha, _ = hash_file(Path("/usr/share/keyrings/debian-archive-keyring.gpg"), clock, 16 * 1024**2)
    receipt = {"format_version": 1, "debian_snapshot": args.snapshot, "abi": "glibc-2.36",
               "declared_packages": list(required), "all_installed_package_versions": rows,
               "debian_archive_keyring_sha256": keyring_sha, "signed_release_metadata": metadata}
    with args.output.open("xb") as stream:
        stream.write(canonical(receipt))
    args.output.chmod(0o444)
    return {"format_version": 1, "kind": "compiler-prerequisites-data", "packages": len(rows)}


def normalize_pinned_rust(sysroot, rustc, roots, source_image, scratch, clock):
    """Construction only, in the isolated pinned source-image assembly stage.

    This never relaxes Builder/verify or adopts a runtime/user tree. Record the
    actual old metadata and byte hashes before changing the selected Rust inputs.
    Only root's cloned Docker layer is modified; byte identity must be unchanged.
    """
    need(os.geteuid() == 0 and sysroot.parent == Path("/usr/local/rustup/toolchains")
         and sysroot.resolve(strict=True) == sysroot and rustc == sysroot / "bin/rustc",
         "normalization requires isolated fixed pinned Rust input")
    selected = [rustc, *roots]
    found, pending = {}, list(selected)
    while pending:
        clock.remaining()
        path = pending.pop().resolve(strict=True)
        need(path.is_relative_to(sysroot), "normalization input escapes fixed Rust source")
        relative(str(path.relative_to(sysroot)))
        if path in found:
            continue
        meta = path.lstat()
        need(stat.S_ISDIR(meta.st_mode) or stat.S_ISREG(meta.st_mode), "normalization source type")
        need(stat.S_IMODE(meta.st_mode) & 0o7000 == 0, "normalization privileged source mode")
        need(len(found) < MAX_FILES, "normalization entry bound")
        found[path] = meta
        if stat.S_ISDIR(meta.st_mode):
            pending.extend(path.iterdir())
    records, bytes_seen, settled = [], 0, {}
    for path, before in sorted(found.items(), key=lambda pair: os.fsencode(str(pair[0]))):
        clock.remaining()
        identity = (before.st_dev, before.st_ino)
        mode = stat.S_IMODE(before.st_mode) & ~0o022
        if identity in settled:
            digest, normalized = settled[identity]
            actual = path.lstat()
            need((actual.st_dev, actual.st_ino) == identity and actual.st_uid == 0
                 and actual.st_gid == 0 and stat.S_IMODE(actual.st_mode) == mode,
                 "normalization hardlink alias changed")
        else:
            need(version(path.lstat()) == version(before), "normalization original source changed")
            digest = None
            if stat.S_ISREG(before.st_mode):
                bytes_seen += before.st_size
                need(bytes_seen <= MAX_BYTES, "normalization byte bound")
                digest, _ = hash_file(path, clock)
            os.chown(path, 0, 0, follow_symlinks=False)
            os.chmod(path, mode, follow_symlinks=False)
            normalized = path.lstat()
            need((normalized.st_dev, normalized.st_ino) == identity and normalized.st_uid == 0
                 and normalized.st_gid == 0 and stat.S_IMODE(normalized.st_mode) == mode,
                 "normalization metadata ACK differs")
            if digest is not None:
                after_digest, _ = hash_file(path, clock)
                need(after_digest == digest, "normalization changed original Rust bytes")
            settled[identity] = (digest, normalized)
        records.append({"path": str(path.relative_to(sysroot)),
                        "original_uid": before.st_uid, "original_gid": before.st_gid,
                        "original_mode": stat.S_IMODE(before.st_mode), "normalized_uid": 0,
                        "normalized_gid": 0, "normalized_mode": mode,
                        "original_sha256": digest, "normalized_sha256": digest})
    result = {"format_version": 1, "kind": "compiler-source-normalization-data",
              "source_image": source_image, "policy": "selected-rust-uid-gid-0-remove-group-other-write-v1",
              "entries": records}
    raw = canonical(result)
    need(len(raw) <= MAX_MANIFEST, "normalization receipt bound")
    output = scratch / "rust-normalization.json"
    with output.open("xb") as stream:
        stream.write(raw)
    output.chmod(0o444)
    clock.remaining()
    return output


def build(args, clock):
    need(os.geteuid() == 0 and platform.system() == "Linux" and platform.machine() == "x86_64", "compiler artifact requires Linux x86_64 root build")
    lock, lock_raw = read_json(args.policy / "tools.lock.json", 65536)
    ts, _ = read_json(args.policy / "typescript.json", 65536)
    images, _ = read_json(args.policy / "source-images.json", 65536)
    prerequisites, _ = read_json(args.prerequisites, 1024 * 1024)
    need(args.rust_source_image == images["rust"] and args.node_source_image == images["node"], "unqualified source image override")
    need(prerequisites["debian_snapshot"] == images["debian_snapshot"] and prerequisites["abi"] == "glibc-2.36"
         and platform.libc_ver() == ("glibc", "2.36"), "actual prerequisite snapshot/ABI differs")
    need(args.scratch.is_absolute() and args.scratch.is_relative_to(Path("/workspace"))
         and args.scratch.is_dir() and not any(args.scratch.iterdir()), "fresh /workspace artifact scratch required")
    candidates = sorted(Path("/usr/local/rustup/toolchains").iterdir())
    need(0 < len(candidates) <= 8, "Rust source toolchain inventory bound")
    candidates = [path for path in candidates if (path / "bin/rustc").is_file()]
    need(len(candidates) == 1, "Rust source must contain one explicit installed toolchain")
    sysroot, expected = candidates[0], lock["compiler_versions"]
    rustc = sysroot / "bin/rustc"
    rust_version = run([str(rustc), "--version"], clock)
    need(rust_version.startswith("rustc " + expected["rustc"] + " ")
         and run([str(args.node), "--version"], clock) == expected["node"]
         and ts["version"] == expected["typescript"], "actual source versions differ from pinned profile")
    need(Path(run([str(rustc), "--print", "sysroot"], clock)).resolve() == sysroot.resolve(), "Rust sysroot differs")
    rust_libraries = sorted((sysroot / "lib").glob("*.so*"))
    rust_target = sysroot / "lib/rustlib/x86_64-unknown-linux-gnu/lib"
    normalization = None
    if args.normalize_pinned_rust:
        normalization = normalize_pinned_rust(sysroot, rustc, [*rust_libraries, rust_target],
                                              args.rust_source_image, args.scratch, clock)
    b = Builder(args.output, clock)
    prepared = args.output / "prepared"
    for name in ("tools.lock.json", "typescript.json", "source-images.json"):
        b.copy_file(args.policy / name, args.output / "inputs" / name, args.policy)
    b.copy_file(args.prerequisites, args.output / "inputs/prerequisites.json", args.prerequisites.parent)
    if normalization is not None:
        b.copy_file(normalization, args.output / "inputs/rust-normalization.json", args.scratch)
    b.copy_file(Path(__file__), args.output / "construction-recipe.py", Path(__file__).parent)
    (args.output / "construction-recipe.py").chmod(0o444)
    for path in REQUIRED_DIRS:
        (args.output / path).mkdir(parents=True, exist_ok=True)
    b.copy_file(rustc, prepared / "tools/rust/bin/rustc", sysroot)
    for path in rust_libraries:
        b.copy_file(path, prepared / "tools/rust/lib" / path.name, sysroot)
    host = "x86_64-unknown-linux-gnu"
    b.copy_tree(rust_target, prepared / "tools/rust/lib/rustlib" / host / "lib", sysroot)
    b.copy_file(args.node, prepared / "tools/node", args.node.parent)
    b.copy_file(Path("/usr/bin/python3"), prepared / "tools/python/bin/python3", Path("/usr"))
    stdlib = Path(run(["/usr/bin/python3", "-I", "-c", "import sysconfig; print(sysconfig.get_path('stdlib'))"], clock))
    b.copy_tree(stdlib, prepared / "tools/python/lib" / stdlib.name, Path("/usr"))
    b.copy_file(Path("/usr/bin/unshare"), prepared / "tools/unshare", Path("/usr"))
    archive = args.scratch / "typescript.tgz"
    integrity = ts["integrity"].removeprefix("sha512-")
    need(ts["integrity"].startswith("sha512-") and ts["url"] == "https://registry.npmjs.org/typescript/-/typescript-" + ts["version"] + ".tgz", "TypeScript pinned origin")
    fetch(ts["url"], archive, "sha512", base64.b64decode(integrity, validate=True), 32 * 1024**2, clock)
    extract_typescript(archive, prepared / "tools/typescript", clock)
    package, _ = read_json(prepared / "tools/typescript/package.json", 65536)
    need(package["version"] == expected["typescript"], "actual TypeScript package version differs")
    archive = args.scratch / "buck2.zst"
    buck = lock["buck2"]
    need(buck["url"] == "https://github.com/meta/buck2/releases/download/" + buck["release"] + "/buck2-x86_64-unknown-linux-musl.zst", "Buck2 pinned origin")
    fetch(buck["url"], archive, "sha256", bytes.fromhex(buck["archive_sha256"]), 256 * 1024**2, clock)
    compressed = archive.read_bytes()
    lib = ctypes.CDLL("libzstd.so.1")
    lib.ZSTD_getFrameContentSize.argtypes, lib.ZSTD_getFrameContentSize.restype = [ctypes.c_void_p, ctypes.c_size_t], ctypes.c_ulonglong
    size = lib.ZSTD_getFrameContentSize(compressed, len(compressed))
    need(0 < size <= 512 * 1024**2, "Buck2 expanded bound")
    lib.ZSTD_decompress.argtypes, lib.ZSTD_decompress.restype = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t], ctypes.c_size_t
    decompressed = ctypes.create_string_buffer(size)
    need(lib.ZSTD_decompress(decompressed, size, compressed, len(compressed)) == size, "Buck2 decompression failed")
    need(hashlib.sha256(decompressed.raw).hexdigest() == buck["executable_sha256"], "Buck2 executable pin differs")
    (args.output / "buck2").write_bytes(decompressed.raw)
    (args.output / "buck2").chmod(0o555)
    # ldd resolves the transitive loader/library set in the pinned Debian input.
    # No mutable home or broad /etc tree is copied; action isolation is a gate.
    for path in sorted(b.elf):
        output = run(["/usr/bin/ldd", str(path)], clock)
        need("not found" not in output, "pinned compiler library dependency absent")
        for line in output.splitlines():
            candidate = line.split("=>", 1)[-1].strip().split(" ", 1)[0]
            if candidate.startswith("/"):
                source = Path(candidate)
                if not source.resolve().is_relative_to(sysroot.resolve()):
                    b.copy_file(source, prepared / str(source).lstrip("/"), Path("/usr"))
    receipt = {"format_version": 1, "source_images": images, "actual_rustc_version": rust_version,
               "versions": expected, "tools_lock_sha256": hashlib.sha256(lock_raw).hexdigest(),
               "recipe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    (args.output / "construction.json").write_bytes(canonical(receipt))
    (args.output / "construction.json").chmod(0o444)
    for directory, _, _ in os.walk(args.output, topdown=False):
        Path(directory).chmod(0o555)
    entries, total = inventory(args.output, clock)
    manifest = {"format_version": 1, "profile_id": PROFILE, "entries": entries, "logical_bytes": total}
    validate_shape(manifest)
    raw = canonical(manifest)
    need(len(raw) <= MAX_MANIFEST, "compiler manifest bound")
    # Only this fresh build-owned directory is changed; no existing artifact is
    # adopted. A failed build keeps partial output and issues no success output.
    args.output.chmod(0o755)
    with (args.output / "manifest.json").open("xb") as stream:
        stream.write(raw)
    (args.output / "manifest.json").chmod(0o444)
    args.output.chmod(0o555)
    return verify(args.output, clock)


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    builder = commands.add_parser("build")
    for name in ("output", "scratch", "policy", "node", "prerequisites"):
        builder.add_argument("--" + name, type=Path, required=True)
    builder.add_argument("--rust-source-image", required=True)
    builder.add_argument("--node-source-image", required=True)
    builder.add_argument("--normalize-pinned-rust", action="store_true")
    verifier = commands.add_parser("verify")
    verifier.add_argument("--root", type=Path, required=True)
    verifier.add_argument("--expected-manifest-sha256")
    recorder = commands.add_parser("record-prerequisites")
    recorder.add_argument("--policy", type=Path, required=True)
    recorder.add_argument("--snapshot", required=True)
    recorder.add_argument("--output", type=Path, required=True)
    args, clock = parser.parse_args(), Clock()
    if args.command == "build":
        result = build(args, clock)
    elif args.command == "verify":
        result = verify(args.root, clock, args.expected_manifest_sha256)
    else:
        result = record_prerequisites(args, clock)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
