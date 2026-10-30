"""Freeze source, build the attributed qualifier and package public fixtures.

Usage: python3 build.py REPOSITORY PRIVATE_CACHE [--resume RUN_DIRECTORY]
       [--reuse-driver QUALIFIED_DRIVER --reuse-driver-sha EXPECTED_SHA256]
The dedicated private Cargo target is a build cache; no source/runtime directory is copied
without applying the explicit name policy first. The optional resume path keeps
the original frozen manifest and verifies every frozen file before rebuilding.
"""
from pathlib import Path
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('repository')
parser.add_argument('private_cache')
parser.add_argument('--resume')
parser.add_argument('--reuse-driver')
parser.add_argument('--reuse-driver-sha')
arguments = parser.parse_args()
assert bool(arguments.reuse_driver) == bool(arguments.reuse_driver_sha)
repo = Path(arguments.repository).resolve()
base = Path(arguments.private_cache).resolve()
base.mkdir(parents=True, exist_ok=True)
EXACT_SUPPORT = {
    'Cargo.toml', 'Cargo.lock', 'engine/Cargo.toml', 'native/Cargo.toml',
    'vcs-bridge/Cargo.toml', 'engine/src/build/frontend_driver.py',
    'engine/src/build/linux_runner.py', 'native/src/permission_hook.mjs',
    'assets/rudder.icns', 'tests/fixtures/task-routing.json',
    'tests/fixtures/readiness-cases.json', 'examples/build-profile/Cargo.lock',
    'examples/build-profile/package-lock.json',
    'examples/build-profile/tools.lock.json',
    'examples/build-profile/prepare_tools.py', 'examples/build-profile/run.py',
    'examples/build-profile/rudder.toml', 'examples/build-profile/rust/lib.rs',
    'examples/build-profile/typescript/main.ts',
}
RUST_ROOTS = ('engine/src/', 'engine/tests/', 'native/src/', 'native/tests/',
              'vcs-bridge/src/', 'vcs-bridge/tests/', 'vcs-bridge/examples/')
HELPER_INPUTS = {'src/native-binary.ts', 'src/fabric-local.ts',
                 'src/fabric-remote.ts', 'src/fabric-fetch.ts'}


def source_name(name):
    parts = Path(name).parts
    return (bool(parts) and not Path(name).is_absolute()
            and not any(p.startswith('.') or p in {
                'target', 'dist', 'node_modules', 'cache', 'session', 'sessions',
                'logs', 'runtime'} for p in parts)
            and (name in EXACT_SUPPORT
                 or name.endswith('.rs') and name.startswith(RUST_ROOTS)))


def regular(root, name):
    assert source_name(name) or name in HELPER_INPUTS, 'not allowlisted'
    cursor = root
    for component in Path(name).parts:
        cursor /= component
        assert not cursor.is_symlink(), 'source symlink refused'
    return cursor.is_file()


def names():
    raw = subprocess.check_output([
        'git', 'ls-files', '-co', '--exclude-standard', '-z', 'engine',
        'native', 'vcs-bridge', 'examples/build-profile', 'Cargo.toml',
        'Cargo.lock', 'assets/rudder.icns', 'tests/fixtures/task-routing.json',
        'tests/fixtures/readiness-cases.json'], cwd=repo)
    return sorted({p.decode() for p in raw.split(b'\0')
                   if p and source_name(p.decode())})


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify_frozen():
    # Enumerate names only. Validate the whole inventory before reading files.
    inventory = []
    for entry in source.rglob('*'):
        assert not entry.is_symlink(), 'frozen source symlink refused'
        if entry.is_file():
            name = str(entry.relative_to(source))
            assert source_name(name) or name in HELPER_INPUTS
            inventory.append(name)
    assert set(inventory) - HELPER_INPUTS == set(files)
    assert all(source_name(name) for name in files)
    for name, expected in files.items():
        assert regular(source, name)
        assert digest((source / name).read_bytes()) == expected, name


if arguments.resume:
    run = Path(arguments.resume).resolve()
    assert run.parent == base, 'resume must remain within the chosen cache'
    source = run / 'source'
    manifest_data = (run / 'source-sha256.json').read_bytes()
    manifest = json.loads(manifest_data)
    files = manifest['files']
    verify_frozen()
    state = json.loads((run / 'state.json').read_text())
    assert state['sourceManifestSha256'] == digest(manifest_data)
else:
    run = base / str(uuid.uuid4())
    source = run / 'source'
    source.mkdir(parents=True)
    files = {}
    originals = names()
    for name in originals:
        if not regular(repo, name):
            continue
        data = (repo / name).read_bytes()
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        shutil.copystat(repo / name, destination)
        files[name] = digest(data)
    assert originals == names()
    assert all(regular(repo, name)
               and digest((repo / name).read_bytes()) == sha
               for name, sha in files.items()), 'source changed during freeze'
    manifest = {
        'format_version': 1, 'purpose': 'exact-source remote mounted qualification',
        'repository_revision': subprocess.check_output(
            ['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip(),
        'uncommitted_engine_source': True, 'workspace_manifest_changed': False,
        'originals_unchanged_during_copy': True, 'files': files,
    }
    manifest_data = (json.dumps(manifest, sort_keys=True,
                                separators=(',', ':')) + '\n').encode()
    (run / 'source-sha256.json').write_bytes(manifest_data)
    state = {
        'directory': str(run), 'source': str(source),
        'sourceManifestSha256': digest(manifest_data), 'nonce': run.name,
        'name': 'vfs-capture-' + run.name[:12],
        'machineConfig': str(run / 'machine.json'),
    }

sha = digest(manifest_data)
(run / 'state.json').write_text(json.dumps(state, indent=2) + '\n')
(base / 'current.json').write_text(json.dumps(state, indent=2) + '\n')
print(json.dumps({'phase': 'frozen', 'directory': str(run),
                  'sourceManifestSha256': sha}), flush=True)
env = os.environ.copy()
target_directory = Path(os.environ.get('RUDDER_QUALIFICATION_TARGET_DIR', str(base / 'target'))).resolve()
env['CARGO_TARGET_DIR'] = str(target_directory)
env.setdefault('CARGO_BUILD_JOBS', '2')
env['RUDDER_QUALIFICATION_SOURCE_SHA'] = sha
build_start = time.monotonic()
with (run / 'resumed-build.log').open('w') as log:
    if arguments.reuse_driver:
        reused_driver = Path(arguments.reuse_driver).resolve()
        assert digest(reused_driver.read_bytes()) == arguments.reuse_driver_sha
        reused_info = json.loads(subprocess.check_output(
            [str(reused_driver), '--build-info'], text=True))
        assert reused_info['build_source_sha256'] == sha
        log.write('Reused exact previously qualified release driver; '
                  'no redundant Cargo build.\nSource manifest SHA256: ' + sha
                  + '\nDriver SHA256: ' + arguments.reuse_driver_sha + '\n')
    else:
        result = subprocess.run([
            'cargo', 'build', '--manifest-path', str(source / 'Cargo.toml'),
            '-p', 'rudder-engine', '--bin', 'rudder-vfs-remote-qualifier',
            '--release', '--locked'], cwd=source, env=env,
            stdout=log, stderr=subprocess.STDOUT)
        assert result.returncode == 0, 'frozen release build failed'
verify_frozen()
artifact = run / 'payload'
(artifact / 'helper').mkdir(parents=True, exist_ok=True)
driver = artifact / 'driver'
shutil.copy2(reused_driver if arguments.reuse_driver
             else target_directory / 'release/rudder-vfs-remote-qualifier', driver)
if arguments.reuse_driver:
    assert digest(driver.read_bytes()) == arguments.reuse_driver_sha
info = json.loads(subprocess.check_output([str(driver), '--build-info'], text=True))
assert info['build_source_sha256'] == sha, 'unattributed or overwritten binary'
helper = {}
for name in sorted(HELPER_INPUTS):
    assert regular(repo, name)
    data = (repo / name).read_bytes()
    helper[name] = digest(data)
    destination = source / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
# Discover imports only after copying the explicitly allowed helper inputs.
subprocess.run([
    str(repo / 'node_modules/.bin/esbuild'), 'src/fabric-fetch.ts', '--bundle',
    '--platform=node', '--format=esm',
    '--outfile=' + str(artifact / 'helper/fetch.mjs'),
    '--metafile=' + str(run / 'helper-metafile.json')], cwd=source, check=True,
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
actual_helper = set(json.loads((run / 'helper-metafile.json').read_text())['inputs'])
assert actual_helper <= HELPER_INPUTS, 'unexpected helper source import'
assert all(regular(repo, name)
           and digest((repo / name).read_bytes()) == value
           and digest((source / name).read_bytes()) == value
           for name, value in helper.items())
(artifact / 'helper/helper.mjs').write_text(
    "import{runFabricFetchHelper}from'./fetch.mjs';await runFabricFetchHelper();\n")
(run / 'helper-input-sha256.json').write_text(json.dumps(helper, indent=2) + '\n')
with tarfile.open(run / 'payload.tar.gz', 'w:gz') as archive:
    for name in ('driver', 'helper/fetch.mjs', 'helper/helper.mjs'):
        archive.add(artifact / name, arcname=name, recursive=False)
with tarfile.open(run / 'source.tar.gz', 'w:gz') as archive:
    for name in sorted(set(files) | set(helper)):
        assert regular(source, name)
        archive.add(source / name, arcname='source/' + name, recursive=False)
verify_frozen()
ready = {
    'format_version': 1, 'driverPath': str(driver),
    'driverSha256': digest(driver.read_bytes()),
    'payloadPath': str(run / 'payload.tar.gz'),
    'payloadSha256': digest((run / 'payload.tar.gz').read_bytes()),
    'sourceManifestSha256': sha,
    'sourceArchiveSha256': digest((run / 'source.tar.gz').read_bytes()),
    'sourceArchiveFiles': len(set(files) | set(helper)),
    'frozenSourceUnchangedAfterBuild': True, 'workspaceManifestChanged': False,
    'buildInfo': info, 'helperInputs': helper,
    'releaseBuildElapsedMs': round((time.monotonic() - build_start) * 1000),
    'reusedPreviouslyQualifiedReleaseDriver': bool(arguments.reuse_driver),
}
(run / 'ready-driver.json').write_text(json.dumps(ready, indent=2) + '\n')
print(json.dumps({'phase': 'ready', **ready}), flush=True)
