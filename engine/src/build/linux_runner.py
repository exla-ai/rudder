#!/usr/bin/env python3
"""Trusted managed compiler runner, launched by unshare in private namespaces.

Not a general shell sandbox: only pinned Rust-library and TypeScript compiler
entry points are exposed. Network/time/secret-bearing application steps are
outside the cacheable profile. The probe mode is uncached qualification work.
"""
import argparse
import ctypes
import errno
import json
import os
from pathlib import Path, PurePosixPath
import tempfile

LIBC = ctypes.CDLL(None, use_errno=True)
LIBC.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
LIBC.mount.restype = ctypes.c_int
MS_RDONLY, MS_NOSUID, MS_NODEV, MS_REMOUNT = 1, 2, 4, 32
MS_BIND, MS_REC, MS_PRIVATE = 4096, 16384, 1 << 18


def mount(source, target, filesystem=None, flags=0, data=None):
    encode = lambda value: os.fsencode(value) if value is not None else None
    if LIBC.mount(encode(source), encode(target), encode(filesystem), flags, encode(data)):
        raise OSError(ctypes.get_errno(), "managed action mount failed", str(target))


def relative(value):
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise ValueError("action paths must remain relative")
    return path


def bind(source, target, readonly):
    source = Path(source)
    if source.is_dir():
        target.mkdir(parents=True, exist_ok=True)
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.touch()
    mount(source, target, flags=MS_BIND | (MS_REC if source.is_dir() else 0))
    if readonly:
        mount(None, target, flags=MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV)


def drop_privileges():
    # Root is mapped to the invoking unprivileged UID. Remove the namespace
    # capabilities as well, so a compromised compiler cannot remount inputs.
    if LIBC.prctl(38, 1, 0, 0, 0):  # PR_SET_NO_NEW_PRIVS
        raise OSError(ctypes.get_errno(), "no_new_privs failed")
    if LIBC.prctl(28, 1 | 2 | 4 | 8, 0, 0, 0):  # PR_SET_SECUREBITS
        raise OSError(ctypes.get_errno(), "securebits failed")
    for capability in range(64):
        if LIBC.prctl(24, capability, 0, 0, 0) and ctypes.get_errno() != errno.EINVAL:
            raise OSError(ctypes.get_errno(), "capability bounding-set drop failed")
    class Header(ctypes.Structure):
        _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]
    class Data(ctypes.Structure):
        _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]
    header, data = Header(0x20080522, 0), (Data * 2)()
    if LIBC.capset(ctypes.byref(header), ctypes.byref(data)):
        raise OSError(ctypes.get_errno(), "capability clearing failed")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--prepared", required=True)
    parser.add_argument("--input", action="append", default=[])
    parser.add_argument("--entrypoint")
    parser.add_argument("--output", required=True)
    parser.add_argument("--kind", choices=["rust", "typescript", "probe"], required=True)
    parser.add_argument("--environment", default="{}")
    parser.add_argument("--probe-port", type=int, default=1)
    args = parser.parse_args()
    cwd = Path.cwd().resolve()
    prepared = Path(args.prepared).resolve(strict=True)
    output = relative(args.output)
    if output.parts[0] != "buck-out":
        raise ValueError("action output must be private Buck2 output")
    host_output = cwd.joinpath(*output.parts)
    host_output.parent.mkdir(parents=True, exist_ok=True)
    if not host_output.parent.resolve().is_relative_to(cwd / "buck-out"):
        raise ValueError("output escapes the managed build directory")
    scratch = cwd / "buck-out" / "rudder-scratch"
    scratch.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="action-", dir=scratch))
    mount(None, "/", flags=MS_REC | MS_PRIVATE)
    mount(prepared, root, flags=MS_BIND | MS_REC)
    mount(None, root, flags=MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV)
    for directory in ("workspace", "tmp", "dev"):
        mount("tmpfs", root / directory, "tmpfs", MS_NOSUID | MS_NODEV, "size=64m")
    for value in args.input:
        path = relative(value)
        if path.parts[0] != "inputs":
            raise ValueError("compiler inputs must belong to the declared source closure")
        source = cwd.joinpath(*path.parts)
        if source.is_symlink() or not source.is_file() or not source.resolve().is_relative_to(cwd / "inputs"):
            raise ValueError("compiler input is not an ordinary managed source file")
        bind(source, root / "workspace" / path, readonly=True)
    bind(host_output.parent, root / "workspace" / output.parent, readonly=False)
    bind("/dev/null", root / "dev" / "null", readonly=False)
    environment = json.loads(args.environment)
    if any(not (key in ("LANG", "LC_ALL", "TZ", "SOURCE_DATE_EPOCH") or key.startswith("RUDDER_BUILD_")) for key in environment):
        raise ValueError("undeclared compiler environment")
    environment.update({"PATH": "/tools", "HOME": "/tmp", "TMPDIR": "/tmp", "LANG": "C", "LC_ALL": "C", "TZ": "UTC"})
    output_arg = str(output)
    if args.kind == "rust":
        entrypoint = str(relative(args.entrypoint))
        executable = "/tools/rust/bin/rustc"
        environment["LD_LIBRARY_PATH"] = "/tools/rust/lib"
        argv = [executable, "--sysroot", "/tools/rust", "--crate-type=rlib", "--edition=2021", "--remap-path-prefix=/workspace=.", "-C", "target-cpu=generic", entrypoint, "-o", output_arg]
    elif args.kind == "typescript":
        entrypoint = str(relative(args.entrypoint))
        executable = "/tools/node"
        argv = [executable, "/tools/typescript/lib/tsc.js", "--strict", "--skipLibCheck", "--target", "ES2022", "--module", "commonjs", "--outDir", output_arg, entrypoint]
    else:
        executable = "/tools/node"
        script = """const fs=require('fs'),net=require('net');let escaped=false;try{fs.readFileSync('/etc/passwd');escaped=true}catch{}const s=net.connect({host:'127.0.0.1',port:Number(process.argv[1])});let done=false;function finish(network){if(done)return;done=true;fs.writeFileSync(process.argv[2],JSON.stringify({host_files_visible:escaped,host_network_reachable:network}));s.destroy();process.exit(escaped||network?1:0)}s.on('connect',()=>finish(true));s.on('error',()=>finish(false));setTimeout(()=>finish(false),500);"""
        argv = [executable, "-e", script, str(args.probe_port), output_arg]
    os.chroot(root)
    os.chdir("/workspace")
    drop_privileges()
    os.execve(executable, argv, environment)


if __name__ == "__main__":
    main()
