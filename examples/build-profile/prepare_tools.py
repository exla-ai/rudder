#!/usr/bin/env python3
"""Fetch fixed frontend/cache binaries and snapshot the qualified host compilers.

All writes are under the supplied owned directory. No global install or mutable
compiler/build cache is shared. Existing compiler versions must match tools.lock.
Their complete copied bytes become the prepared-environment CAS input identity.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time
import urllib.request


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def fetch(url, destination, expected):
    if not destination.exists():
        temporary = destination.with_suffix(destination.suffix + ".partial")
        with urllib.request.urlopen(url, timeout=60) as response, temporary.open("wb") as out:
            shutil.copyfileobj(response, out)
        temporary.rename(destination)
    if sha256(destination) != expected:
        raise ValueError("download checksum mismatch: " + destination.name)


def decompress_zstd(source, destination):
    data = source.read_bytes()
    library = ctypes.CDLL("libzstd.so.1")
    library.ZSTD_getFrameContentSize.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    library.ZSTD_getFrameContentSize.restype = ctypes.c_ulonglong
    size = library.ZSTD_getFrameContentSize(data, len(data))
    if not 0 < size < 512 * 1024 * 1024:
        raise ValueError("unsupported frontend archive size")
    output = ctypes.create_string_buffer(size)
    library.ZSTD_decompress.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t]
    library.ZSTD_decompress.restype = ctypes.c_size_t
    actual = library.ZSTD_decompress(output, size, data, len(data))
    if actual != size:
        raise ValueError("frontend decompression failed")
    destination.write_bytes(output.raw)
    destination.chmod(0o755)


def copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination, follow_symlinks=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--typescript", type=Path, default=Path(__file__).resolve().parents[2] / "node_modules/typescript")
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("this measured fixture qualifies Linux x86_64 only")
    root = args.directory.resolve()
    if str(root) == "/tmp" or str(root).startswith("/tmp/"):
        raise ValueError("large tool snapshots must be outside the worker's small /tmp")
    root.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(root).free < 3 * 1024 * 1024 * 1024:
        raise ValueError("tool snapshot needs at least 3 GiB free on its owned volume")
    lock = json.loads(Path(__file__).with_name("tools.lock.json").read_text())
    tools = root / "tools"
    tools.mkdir(exist_ok=True)
    archive = tools / "buck2.zst"
    fetch(lock["buck2"]["url"], archive, lock["buck2"]["archive_sha256"])
    buck2 = tools / "buck2"
    if not buck2.exists():
        decompress_zstd(archive, buck2)
    if sha256(buck2) != lock["buck2"]["executable_sha256"]:
        raise ValueError("Buck2 executable checksum mismatch")
    cache = tools / "bazel-remote"
    fetch(lock["bazel_remote"]["url"], cache, lock["bazel_remote"]["executable_sha256"])
    cache.chmod(0o755)
    rustc = Path(subprocess.check_output(["rustup", "which", "rustc"], text=True).strip()).resolve()
    sysroot = Path(subprocess.check_output([str(rustc), "--print", "sysroot"], text=True).strip()).resolve()
    rust_version = subprocess.check_output([str(rustc), "--version"], text=True).strip()
    node = Path(shutil.which("node")).resolve()
    node_version = subprocess.check_output([str(node), "--version"], text=True).strip()
    ts_root = args.typescript.resolve()
    ts_version = json.loads((ts_root / "package.json").read_text())["version"]
    expected = lock["compiler_versions"]
    if not rust_version.startswith("rustc " + expected["rustc"] + " ") or node_version != expected["node"] or ts_version != expected["typescript"]:
        raise ValueError("host compiler versions differ from the pinned fixture; use a matching prepared toolchain")
    prepared = root / "prepared"
    started = time.perf_counter()
    marker_path = prepared / ".prepared-complete.json"
    previous = json.loads(marker_path.read_text()) if marker_path.exists() else {}
    platform_identity = {"os": "linux", "architecture": platform.machine(), "abi": "-".join(platform.libc_ver())}
    snapshot_identity = {"preparation_revision": 2, "rustc": rust_version, "node": node_version, "typescript": ts_version, "platform": platform_identity}
    if any(previous.get(key) != value for key, value in snapshot_identity.items()):
        if prepared.exists():
            shutil.rmtree(prepared)
        for directory in ("workspace", "tmp", "dev", "proc"):
            (prepared / directory).mkdir(parents=True, exist_ok=True)
        copy(rustc, prepared / "tools/rust/bin/rustc")
        rust_libraries = list((sysroot / "lib").glob("*.so*"))
        for library in rust_libraries:
            copy(library, prepared / "tools/rust/lib" / library.name)
        host = re.search(r"host: (.+)", subprocess.check_output([str(rustc), "-vV"], text=True)).group(1)
        target_lib = sysroot / "lib/rustlib" / host / "lib"
        shutil.copytree(target_lib, prepared / "tools/rust/lib/rustlib" / host / "lib", symlinks=False)
        copy(node, prepared / "tools/node")
        shutil.copytree(ts_root, prepared / "tools/typescript", symlinks=False)
        python = Path("/usr/bin/python3").resolve()
        python_stdlib = Path(subprocess.check_output([str(python), "-c", "import sysconfig;print(sysconfig.get_path('stdlib'))"], text=True).strip())
        copy(python, prepared / "tools/python/bin/python3")
        shutil.copytree(python_stdlib, prepared / "tools/python/lib" / python_stdlib.name, symlinks=False)
        unshare = Path(shutil.which("unshare")).resolve()
        copy(unshare, prepared / "tools/unshare")
        # Resolve dynamic loader/shared libraries into their expected paths.
        # No /etc, user home, package cache, socket or credential tree is copied.
        dependencies = set()
        for executable in [node, rustc, python, unshare, *rust_libraries, *python_stdlib.rglob("*.so")]:
            with executable.open("rb") as stream:
                if stream.read(4) != b"\x7fELF":
                    continue  # LLVM also ships a small text linker script.
            output = subprocess.check_output(["ldd", str(executable)], text=True, stderr=subprocess.DEVNULL)
            for line in output.splitlines():
                candidate = line.split("=>", 1)[-1].strip().split(" ", 1)[0]
                if candidate.startswith("/") and not Path(candidate).resolve().is_relative_to(sysroot):
                    dependencies.add(candidate)
        for dependency in dependencies:
            copy(dependency, prepared / dependency.lstrip("/"))
        marker = {"format_version": 1, **snapshot_identity,
                  "compiler_paths": {"rust": "tools/rust/bin/rustc", "node": "tools/node", "typescript": "tools/typescript/lib/tsc.js", "python": "tools/python/bin/python3", "unshare": "tools/unshare"}}
        (prepared / ".prepared-complete.json").write_text(json.dumps(marker, sort_keys=True))
    result = {"format_version": 1, "directory": str(root), "prepared": str(prepared), "buck2": str(buck2), "bazel_remote": str(cache),
              "frontend_sha256": sha256(buck2), "host_driver": {"python_path": "/usr/bin/python3", "python_sha256": sha256(Path("/usr/bin/python3").resolve()), "preparation_python_sha256": sha256(Path(shutil.which("python3")).resolve()), "unshare_sha256": sha256(Path(shutil.which("unshare")).resolve()), "kernel": platform.release()},
              "prepare_seconds": time.perf_counter() - started, "compilers": json.loads((prepared / ".prepared-complete.json").read_text())}
    (root / "toolchain.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
