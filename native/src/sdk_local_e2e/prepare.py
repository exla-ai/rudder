#!/usr/bin/python3
"""Offline local fixture only. Copies explicit runtime inputs, never profiles.

This is not the production SDK producer, catalogue or admission handshake.
Principal executable bytes have checked-in pins. All copied closure members
are recorded in a separate manifest and rehashed before every helper run.
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
import sys

HERE = Path(__file__).resolve().parent
PINS = json.loads((HERE / 'pins.json').read_text())


def require(ok, message):
    if not ok:
        raise RuntimeError('LOCAL_SDK_PRECONDITION: ' + message)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def inputs():
    require(platform.system() + '-' + platform.machine() == PINS['platform'],
            'use the pinned Linux x86_64 development image')
    require((os.getuid(), os.geteuid(), os.getgid(), os.getegid()) == (1500,) * 4,
            'run as real/effective UID/GID1500; do not spoof helper credentials')
    require(os.access('/workspace', os.W_OK), 'provide writable /workspace fixture storage')
    require(platform.python_version() == PINS['python_version'],
            'install pinned Python ' + PINS['python_version'])
    node = Path(shutil.which('node') or '/usr/local/bin/node').resolve()
    require(node.is_file() and sha(node) == PINS['node_sha256'],
            'install pinned Node ' + PINS['node_version'] + ' (exact binary SHA256 in pins.json)')
    python = Path(sys.executable).resolve()
    require(sha(python) == PINS['python_sha256'], 'use pinned /usr/bin/python3.11 binary')
    candidates = [Path(os.environ['SDK_HELPER_LOCAL_TYPESCRIPT'])] if os.environ.get('SDK_HELPER_LOCAL_TYPESCRIPT') else [p / 'node_modules/typescript' for p in HERE.parents]
    typescript = next((p for p in candidates if (p / 'lib/tsc.js').is_file()), None)
    require(typescript is not None,
            'install TypeScript 5.9.3 offline or set SDK_HELPER_LOCAL_TYPESCRIPT to its package directory')
    require(json.loads((typescript / 'package.json').read_text())['version'] == PINS['typescript_version']
            and sha(typescript / 'lib/tsc.js') == PINS['tsc_sha256']
            and sha(typescript / 'lib/_tsc.js') == PINS['tsc_impl_sha256'],
            'TypeScript compiler bytes differ from the checked-in 5.9.3 pins')
    require(shutil.which('ldd') is not None, 'install ldd for the explicit ELF runtime closure')
    return python, node, typescript


def namespace_probe():
    # Disposable subprocess: never mutate the caller's namespace or identities.
    code = '''import ctypes,os
libc=ctypes.CDLL(None,use_errno=True)
if libc.unshare(0x10000000)!=0:raise OSError(ctypes.get_errno(),'CLONE_NEWUSER')
open('/proc/self/setgroups','w').write('deny')
open('/proc/self/uid_map','w').write('0 1500 1')
open('/proc/self/gid_map','w').write('0 1500 1')
if libc.unshare(0x20000|0x40000000|0x20000000)!=0:raise OSError(ctypes.get_errno(),'mount/net/PID namespaces')
'''
    probe = subprocess.run([sys.executable, '-I', '-c', code], capture_output=True, text=True, timeout=10)
    require(probe.returncode == 0, 'enable unprivileged user, mount, net and PID namespaces; probe: ' + probe.stderr.strip())


def prepare(destination):
    python, node, typescript = inputs()
    namespace_probe()
    root = Path(destination)
    require(root.is_absolute() and str(root).startswith('/workspace/rudder-sdk-real-'),
            'fixture destination must be a new /workspace/rudder-sdk-real-* directory')
    require(not root.exists(), 'fixture destination already exists')
    root.mkdir(mode=0o700)
    prepared = root / 'floor/prepared'
    prepared.mkdir(parents=True)

    def copy(source, target):
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.exists():
            require(sha(source) == sha(target), 'conflicting runtime closure member: ' + str(target))
            return
        shutil.copyfile(source, target, follow_symlinks=True)
        target.chmod(0o555 if os.access(source, os.X_OK) else 0o444)

    def elf(source, target):
        copy(source, target)
        result = subprocess.run(['ldd', str(source)], capture_output=True, text=True, check=True)
        for line in result.stdout.splitlines():
            # Copy only absolute library filenames reported for these pinned ELF
            # executables and stdlib extensions, never a host filesystem tree.
            match = re.search(r'(?:=>\s*)?(/[^\s]+)', line)
            if match:
                library = Path(match[1])
                copy(library, prepared / str(library).lstrip('/'))

    elf(python, prepared / 'tools/python/bin/python3')
    elf(node, prepared / 'tools/node')
    stdlib = Path('/usr/lib/python3.11')
    require(stdlib.is_dir(), 'install the pinned Python stdlib and lib-dynload')
    for source in sorted(stdlib.rglob('*')):
        relative = source.relative_to(stdlib)
        if any(part in ('__pycache__', 'test', 'tests', 'ensurepip', 'dist-packages', 'site-packages') for part in relative.parts):
            continue
        if source.is_file() and source.suffix in ('.py', '.so'):
            target = prepared / 'tools/python/lib/python3.11' / relative
            if source.suffix == '.so':
                elf(source, target)
            else:
                copy(source, target)
    for source in sorted((typescript / 'lib').glob('*')):
        if source.is_file() and source.suffix in ('.js', '.ts', '.json'):
            copy(source, prepared / 'tools/typescript/lib' / source.name)
    for name in ('workspace', 'tmp', 'proc'):
        (prepared / name).mkdir()
    # Selected execution only requires a retained readonly file here. Buck2 is
    # never executed or qualified by this TypeScript fixture.
    (root / 'floor/buck2').write_text('local selected fixture: Buck2 not executed\n')
    members = []
    for path in sorted((root / 'floor').rglob('*')):
        if path.is_file():
            members.append({'path': str(path.relative_to(root / 'floor')), 'size': path.stat().st_size, 'sha256': sha(path)})
    manifest = {'kind': 'local-sdk-closure-data-v1', 'sdk_admission': False,
                'principal_pins': PINS, 'members': members}
    (root / 'closure.json').write_text(json.dumps(manifest, sort_keys=True, separators=(',', ':')) + '\n')
    require(sha(root / 'closure.json') == (HERE / 'closure.sha256').read_text().strip(),
            'Python/TypeScript/ELF closure differs from closure.sha256; use the pinned development image or review the local fixture closure pin')
    print(json.dumps({'kind': 'local-sdk-fixture-prepared-v1', 'sdk_admission': False,
                      'closure_sha256': sha(root / 'closure.json'), 'members': len(members)}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--preflight', action='store_true')
    parser.add_argument('--prepare')
    args = parser.parse_args()
    try:
        if args.prepare:
            prepare(args.prepare)
        else:
            inputs()
            namespace_probe()
            print('LOCAL_SDK_PREFLIGHT=passed (real pinned tools; execution not yet tested)')
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(2)
