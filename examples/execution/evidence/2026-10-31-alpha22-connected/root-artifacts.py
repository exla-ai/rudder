#!/usr/bin/env python3
"""Root-only opt-in artifact preparation. Import/help performs no work."""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import time
from datetime import datetime, timezone

PARENT = 'sha256:53c763d9e5886cadc262fa03631e5d08338c44486e2f3ffbb8e0afba4c21d89f'
ROOTS = ('native', 'engine', 'vcs-bridge', 'assets')
FILES = ('Cargo.toml', 'Cargo.lock', 'package.json')
OPTIONAL = ('.cargo/config.toml', '.cargo/config', 'rust-toolchain', 'rust-toolchain.toml')
SKIP = {'.git', '.jj', '.rudder', '.rudder-workspaces', 'target', 'dist', 'node_modules', '__pycache__'}
SECRET_NAMES = {'.env', '.credentials.json', 'auth-profiles.json', 'RUDDER_SHARED.md', 'credentials', 'credentials.toml'}
COMMAND = ['cargo', 'build', '--release', '--workspace', '--locked', '--features',
           'rudder-vcs-bridge/cli', '--bin', 'rudder-native', '--bin', 'rudder-jj',
           '--bin', 'rudder-connected-network-qualifier']
MAX_SOURCE = 256 * 1024 * 1024
REUSE_TARGET = Path('/workspace/code/exla-ai-rudder/target')
REUSE_DISK_FLOOR = 3_489_660_928
INITIAL_RESERVE = 512 * 1024 * 1024
ARTIFACT_NAMES = ('rudder-native', 'rudder-jj', 'rudder-connected-network-qualifier')

def require(value, message):
    if not value:
        raise ValueError(message)

def utc():
    return datetime.now(timezone.utc).isoformat()

def digest(data):
    return hashlib.sha256(data).hexdigest()

def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':')) + '\n').encode()

def private_write(path, data, mode=0o600):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    os.fchmod(fd, mode)
    try:
        with os.fdopen(fd, 'wb') as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
    except BaseException:
        raise

def read_file(path, maximum=32 * 1024 * 1024):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and 0 <= before.st_size <= maximum, 'unsafe/oversize input file')
        data = stream.read(maximum + 1)
        after, named = os.fstat(stream.fileno()), os.lstat(path)
        require(len(data) == before.st_size, 'changed input size')
        for item in (after, named):
            require(stat.S_ISREG(item.st_mode) and (item.st_dev, item.st_ino, item.st_size, item.st_mtime_ns)
                    == (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns), 'changed input identity')
        return data

def read_json(path):
    return json.loads(read_file(path))

def source_files(repo):
    names = list(FILES) + [name for name in OPTIONAL if (repo / name).exists()]
    for root in ROOTS:
        require((repo / root).is_dir() and not (repo / root).is_symlink(), 'missing/unsafe source root')
        for directory, subdirs, leaves in os.walk(repo / root, followlinks=False):
            for name in subdirs:
                require(not (Path(directory) / name).is_symlink(), 'source directory symlink')
            subdirs[:] = sorted(name for name in subdirs if name not in SKIP)
            for name in sorted(leaves):
                require(name not in SECRET_NAMES and not name.startswith('.env.'), 'private-looking compiler input refused')
                names.append((Path(directory) / name).relative_to(repo).as_posix())
    names = sorted(set(names))
    require(len(names) <= 10000, 'source file count bound')
    return names

def verify_source(envelope, root):
    require(envelope['kind'] == 'frozen-working-tree-compiler-inputs-v1', 'wrong source envelope')
    files = envelope['files']
    require(source_files(root) == sorted(files), 'frozen compiler input set changed')
    require(digest(encoded(files)) == envelope['sourceTreeSha256'], 'source tree digest mismatch')
    total = 0
    for name, record in files.items():
        data = read_file(root / name)
        total += len(data)
        require(total <= MAX_SOURCE and digest(data) == record['sha256'] and len(data) == record['bytes'], 'frozen source changed')
        require(stat.S_IMODE(os.stat(root / name).st_mode) == record['mode'], 'frozen source mode changed')
    return True

def freeze(args):
    repo, destination = Path(args.repo).resolve(strict=True), Path(args.output).absolute()
    require(not destination.exists() and not destination.is_relative_to(repo), 'freeze must be new and outside checkout')
    require(re.fullmatch('[0-9a-f]{40}', args.base_revision), 'explicit base revision required')
    destination.mkdir(mode=0o700)
    source = destination / 'source'
    source.mkdir(mode=0o700)
    records, total = {}, 0
    names = source_files(repo)
    for name in names:
        data = read_file(repo / name)
        total += len(data)
        require(total <= MAX_SOURCE, 'source byte bound')
        mode = 0o755 if os.stat(repo / name).st_mode & 0o111 else 0o644
        target = source / name
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        private_write(target, data, mode)
        records[name] = {'sha256': digest(data), 'bytes': len(data), 'mode': mode}
    require(source_files(repo) == names, 'working tree input set changed while freezing')
    for name, record in records.items():
        require(digest(read_file(repo / name)) == record['sha256'], 'working tree changed while freezing')
        require((0o755 if os.stat(repo / name).st_mode & 0o111 else 0o644) == record['mode'], 'source executable mode changed while freezing')
    envelope = {'formatVersion': 1, 'kind': 'frozen-working-tree-compiler-inputs-v1',
                'sourceKind': 'frozen-working-tree', 'baseRevision': args.base_revision,
                'baseRevisionIsCallerDeclared': True, 'createdAt': utc(),
                'scope': 'all first-party Cargo workspace/compiler inputs; no VCS archive or full product snapshot claim',
                'sourceTreeSha256': digest(encoded(records)), 'sourceBytes': total,
                'sourceFiles': len(records), 'packageVersion': json.loads(read_file(source / 'package.json'))['version'],
                'files': records}
    verify_source(envelope, source)
    body = encoded(envelope)
    private_write(destination / 'source-envelope.json', body)
    return {'status': 'frozen', 'sourceEnvelopeSha256': digest(body), 'sourceTreeSha256': envelope['sourceTreeSha256'],
            'sourceFiles': len(records), 'sourceBytes': total}

def artifact(path):
    data = read_file(path, 128 * 1024 * 1024)
    require(data[:6] == b'\x7fELF\x02\x01' and data[18:20] == b'\x3e\x00', 'expected x86_64 Linux release ELF')
    return {'path': str(path), 'sha256': digest(data), 'bytes': len(data), 'elfMachine': 'EM_X86_64'}

def target_plan(root, reuse_target=None):
    """Selection only. Reuse is explicit, fixed-path and externally coordinated."""
    reused = reuse_target is not None
    if reused:
        require(reuse_target == str(REUSE_TARGET), 'reuse target must be the exact allowed Main cache path')
        target = REUSE_TARGET
        named = os.lstat(target)
        require(stat.S_ISDIR(named.st_mode) and not stat.S_ISLNK(named.st_mode)
                and target.resolve(strict=True) == target, 'reuse target must be a real directory without symlinks')
    else:
        target = root / 'target'
        require(not os.path.lexists(target), 'fresh private target required by default')
    floor = REUSE_DISK_FLOOR if reused else 1024**3
    initial = floor + INITIAL_RESERVE if reused else 4 * 1024**3
    return target, {'cargoTargetReused': reused, 'cargoTargetPath': str(target),
                    'compilerQueuePolicy': 'Root sole compiler queue; externally coordinated' if reused else 'private fresh target',
                    'limits': {'diskFloorBytes': floor, 'initialFreeRequiredBytes': initial,
                               'initialFreeComparison': '>' if reused else '>=',
                               'initialReserveBytes': INITIAL_RESERVE, 'compilerSeconds': 1800,
                               'compilerLogBytes': 64 * 1024 * 1024}}

def check_target(target, held):
    actual, named = os.fstat(held), os.lstat(target)
    require(stat.S_ISDIR(actual.st_mode) and stat.S_ISDIR(named.st_mode)
            and (actual.st_dev, actual.st_ino) == (named.st_dev, named.st_ino)
            and target.resolve(strict=True) == target, 'held Cargo target directory was replaced')

def seal_artifacts(root, target, reused, disk_floor=0):
    """Copy bytes first. Compatibility aliases never link to the shared cache."""
    directory = root / 'artifacts'
    directory.mkdir(mode=0o700)
    compatibility = root / 'target/release'
    if reused:
        compatibility.mkdir(mode=0o700, parents=True)
    records = {}
    for name in ARTIFACT_NAMES:
        source = target / 'release' / name
        before = artifact(source)
        data = read_file(source, 128 * 1024 * 1024)
        require(digest(data) == before['sha256'], 'release cache artifact changed before private copy')
        require(min(shutil.disk_usage(root).free, shutil.disk_usage(target).free)
                > disk_floor + len(data) + 1024 * 1024, 'insufficient space to seal artifact above selected disk floor')
        immutable = directory / name
        private_write(immutable, data, 0o500)
        sealed = artifact(immutable)
        require(sealed['sha256'] == before['sha256'] and sealed['bytes'] == before['bytes'], 'private artifact copy mismatch')
        require(artifact(source) == before, 'release cache artifact changed while sealing')
        alias = compatibility / name
        if not reused:
            # This is only the newly built private target, never Main's cache.
            alias.unlink()
        os.link(immutable, alias, follow_symlinks=False)
        records[name] = {**artifact(alias), 'immutablePath': str(immutable)}
    for path in (directory, compatibility):
        directory_fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    return records

def build(args):
    root = Path(args.directory).resolve(strict=True)
    source = root / 'source'
    envelope = read_json(root / 'source-envelope.json')
    verify_source(envelope, source)
    require(sys.platform == 'linux' and platform.machine() == 'x86_64', 'Linux x86_64 build required')
    require(not any(os.path.lexists(root / name) for name in ('target', 'artifacts', 'compiler-receipt.json')),
            'new private artifact/receipt namespace required')
    target, target_record = target_plan(root, getattr(args, 'reuse_target', None))
    root_free = shutil.disk_usage(root).free
    target_free = shutil.disk_usage(target if target_record['cargoTargetReused'] else root).free
    available = min(root_free, target_free)
    initial = target_record['limits']['initialFreeRequiredBytes']
    require(available > initial if target_record['cargoTargetReused'] else available >= initial,
            'insufficient free space for selected target mode and reserve')
    target_record.update(initialFrozenRootFreeBytes=root_free, initialTargetFreeBytes=target_free,
                         initialMinimumFreeBytes=available)
    if not target_record['cargoTargetReused']:
        target.mkdir(mode=0o700)
    held_target = os.open(target, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        return build_held(args, root, source, target, envelope, target_record, held_target)
    finally:
        os.close(held_target)

def build_held(args, root, source, target, envelope, target_record, held_target):
    check_target(target, held_target)
    env = {name: os.environ[name] for name in ('PATH', 'HOME', 'CARGO_HOME', 'RUSTUP_HOME') if name in os.environ}
    env.update(CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL='0', CARGO_BUILD_JOBS='2', TMPDIR=str(root / 'tmp'))
    (root / 'tmp').mkdir(mode=0o700)
    started = utc()
    versions = {}
    for name, command in [('rustc', ['rustc', '-Vv']), ('cargo', ['cargo', '-V'])]:
        answer = subprocess.run(command, cwd=source, env=env, capture_output=True, timeout=15, check=True)
        require(len(answer.stdout) <= 8192 and len(answer.stderr) <= 8192, 'toolchain metadata bound')
        versions[name] = {'argv': command, 'stdout': answer.stdout.decode(), 'stdoutSha256': digest(answer.stdout)}
    log = root / 'compiler.private.log'
    guard_reason = None
    with os.fdopen(os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), 'wb') as stream:
        process = subprocess.Popen(COMMAND, cwd=source, env=env, stdin=subprocess.DEVNULL,
                                   stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + 1800
        while True:
            try:
                exit_code = process.wait(timeout=1)
                break
            except subprocess.TimeoutExpired:
                if time.monotonic() >= deadline:
                    guard_reason = 'compiler-timeout'
                elif min(shutil.disk_usage(root).free, shutil.disk_usage(target).free) < target_record['limits']['diskFloorBytes']:
                    guard_reason = 'compiler-disk-floor'
                elif log.stat().st_size > 64 * 1024 * 1024:
                    guard_reason = 'compiler-log-bound'
                if guard_reason:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait()
                    exit_code = 124
                    break
    try:
        unchanged = verify_source(envelope, source)
    except ValueError:
        unchanged = False
    receipt = {'formatVersion': 1, 'kind': 'actual-cargo-release-compiler-receipt-v1', 'startedAt': started,
               'finishedAt': utc(), 'argv': COMMAND, 'exitCode': exit_code, 'ok': exit_code == 0 and unchanged,
               'guardReason': guard_reason, 'compilerProcessReaped': True,
               'sourceUnchanged': unchanged, 'sourceKind': 'frozen-working-tree',
               'sourceEnvelopeSha256': digest(read_file(root / 'source-envelope.json')),
               'sourceTreeSha256': envelope['sourceTreeSha256'], 'toolchain': versions,
               'logPath': str(log), 'logSha256': digest(read_file(log, 64 * 1024 * 1024)),
               **target_record, 'artifactsCopied': False, 'immutableArtifactsDirectory': str(root / 'artifacts'),
               'artifactCompatibilityDirectory': str(root / 'target/release'), 'artifacts': {}}
    if receipt['ok']:
        check_target(target, held_target)
        receipt['artifacts'] = seal_artifacts(root, target, target_record['cargoTargetReused'],
                                               target_record['limits']['diskFloorBytes'])
        check_target(target, held_target)
        receipt['artifactsCopied'] = True
    private_write(root / 'compiler-receipt.json', encoded(receipt))
    require(receipt['ok'], 'actual compiler failed; private receipt/log retained')
    return {'status': 'compiled', 'compilerReceiptSha256': digest(encoded(receipt)),
            'artifacts': {name: row['sha256'] for name, row in receipt['artifacts'].items()}}

def staged_inputs(root):
    envelope_path, receipt_path = root / 'source-envelope.json', root / 'compiler-receipt.json'
    envelope, receipt = read_json(envelope_path), read_json(receipt_path)
    verify_source(envelope, root / 'source')
    require(receipt['kind'] == 'actual-cargo-release-compiler-receipt-v1' and receipt['ok'] is True
            and receipt['exitCode'] == 0 and receipt['sourceUnchanged'] is True and receipt['argv'] == COMMAND
            and receipt['sourceKind'] == 'frozen-working-tree'
            and receipt['compilerProcessReaped'] is True and receipt['guardReason'] is None,
            'successful actual compiler receipt required')
    require(receipt['sourceEnvelopeSha256'] == digest(read_file(envelope_path))
            and receipt['sourceTreeSha256'] == envelope['sourceTreeSha256'], 'compiler source binding mismatch')
    for record in receipt['artifacts'].values():
        current = artifact(Path(record['path']))
        require(current == {key: record[key] for key in current}, 'compiled artifact changed')
        if receipt.get('artifactsCopied'):
            immutable = Path(record['immutablePath'])
            require(immutable == root / 'artifacts' / Path(record['path']).name, 'private artifact copy path changed')
            copied = artifact(immutable)
            require(all(copied[key] == record[key] for key in ('sha256', 'bytes', 'elfMachine')), 'immutable artifact changed')
        else:
            require(not receipt.get('cargoTargetReused'), 'reused cache receipt requires private immutable copies')
    require(digest(read_file(receipt['logPath'], 64 * 1024 * 1024)) == receipt['logSha256'], 'compiler log changed')
    return envelope, receipt

def provenance(root, envelope, receipt):
    return {'formatVersion': 1, 'kind': 'rudder-connected-network-qualifier-image',
            'proposedVersion': '4.0.0-alpha.22', 'workerVersion': '4.0.0-alpha.21',
            'sourceKind': 'frozen-working-tree', 'revision': envelope['baseRevision'],
            'sourceBaseRevision': envelope['baseRevision'], 'baseRevisionIsCallerDeclared': True,
            'sourceEnvelopeSha256': digest(read_file(root / 'source-envelope.json')),
            'sourceTreeSha256': envelope['sourceTreeSha256'],
            'compilerReceiptSha256': digest(read_file(root / 'compiler-receipt.json')),
            'qualifierSha256': receipt['artifacts']['rudder-connected-network-qualifier']['sha256'],
            'nativeSha256': receipt['artifacts']['rudder-native']['sha256'],
            'nativeBinaryExecuted': False, 'parentDigest': PARENT}

def stage(args):
    root = Path(args.directory).resolve(strict=True)
    envelope, receipt = staged_inputs(root)
    image = root / 'image'
    image.mkdir(mode=0o700)
    template = Path(__file__).resolve().parent / 'Dockerfile'
    private_write(image / 'Dockerfile', read_file(template))
    private_write(image / 'provenance.json', encoded(provenance(root, envelope, receipt)), 0o644)
    private_write(image / 'rudder-connected-network-qualifier',
                  read_file(receipt['artifacts']['rudder-connected-network-qualifier'].get('immutablePath',
                            receipt['artifacts']['rudder-connected-network-qualifier']['path'])), 0o755)
    return {'status': 'image-staged', 'parentDigest': PARENT,
            'qualifierSha256': receipt['artifacts']['rudder-connected-network-qualifier']['sha256']}

def binding(args):
    root = Path(args.directory).resolve(strict=True)
    envelope, receipt = staged_inputs(root)
    require(re.fullmatch(r'registry\.fly\.io/rudder-workers-exla:alpha22-connect-canary-[A-Za-z0-9_.-]{1,80}', args.image), 'private canary tag required')
    metadata_path = Path(args.metadata).resolve(strict=True)
    metadata = read_json(metadata_path)
    image_digest = metadata.get('containerimage.digest')
    require(re.fullmatch('sha256:[0-9a-f]{64}', image_digest or '') and image_digest != PARENT, 'actual derived image digest required')
    expected = provenance(root, envelope, receipt)
    require(read_json(root / 'image/provenance.json') == expected, 'image provenance changed')
    require(digest(read_file(root / 'image/rudder-connected-network-qualifier')) == expected['qualifierSha256'], 'image qualifier changed')
    require(read_file(root / 'image/Dockerfile') == read_file(Path(__file__).resolve().parent / 'Dockerfile'), 'pinned Dockerfile changed')
    result = {'formatVersion': 1, 'kind': 'alpha22-connect-canary-artifact-binding-v1',
              'directory': str(root), 'image': args.image, 'digest': image_digest,
              'parentDigest': PARENT, 'sourceKind': 'frozen-working-tree', 'revision': envelope['baseRevision'],
              'sourceEnvelopeSha256': expected['sourceEnvelopeSha256'], 'sourceTreeSha256': expected['sourceTreeSha256'],
              'compilerReceiptSha256': expected['compilerReceiptSha256'],
              'nativeSha256': expected['nativeSha256'], 'qualifierSha256': expected['qualifierSha256'],
              'buildMetadataPath': str(metadata_path), 'buildMetadataSha256': digest(read_file(metadata_path)),
              'dockerfileSha256': digest(read_file(root / 'image/Dockerfile')), 'provenance': expected}
    private_write(root / 'run-binding.json', encoded(result))
    return {'status': 'bound-not-run', 'image': args.image + '@' + image_digest,
            'runBindingSha256': digest(encoded(result)), 'providerCalls': 0}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='mode', required=True)
    freeze_parser = sub.add_parser('freeze')
    freeze_parser.add_argument('--repo', required=True)
    freeze_parser.add_argument('--output', required=True)
    freeze_parser.add_argument('--base-revision', required=True)
    for name in ('build', 'stage', 'binding'):
        item = sub.add_parser(name)
        item.add_argument('--directory', required=True)
        if name == 'build':
            item.add_argument('--reuse-target', help='Root sole compiler queue only; exact /workspace/code/exla-ai-rudder/target')
        if name == 'binding':
            item.add_argument('--image', required=True)
            item.add_argument('--metadata', required=True)
    args = parser.parse_args()
    result = globals()[args.mode](args)
    print(json.dumps(result, sort_keys=True))

if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Private compiler output stays in its explicit log. Do not echo argv,
        # environment, source contents or third-party error payloads here.
        print(json.dumps({'status': 'rejected', 'errorType': type(error).__name__}), file=sys.stderr)
        sys.exit(1)
