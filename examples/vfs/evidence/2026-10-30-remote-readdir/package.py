"""Audit and archive a completed owned run: python3 package.py RUN_DIRECTORY."""
from pathlib import Path
import datetime
import hashlib
import json
import shutil
import subprocess
import sys
import tarfile

run = Path(sys.argv[1]).resolve()
destination = Path(__file__).resolve().parent
repo = destination.parents[3]
exact = {
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
roots = ('engine/src/', 'engine/tests/', 'native/src/', 'native/tests/',
         'vcs-bridge/src/', 'vcs-bridge/tests/', 'vcs-bridge/examples/')
helper_names = {'src/native-binary.ts', 'src/fabric-local.ts',
                'src/fabric-remote.ts', 'src/fabric-fetch.ts'}


def allowed(name):
    parts = Path(name).parts
    return (bool(parts) and not Path(name).is_absolute()
            and not any(part.startswith('.') or part in {
                'target', 'dist', 'node_modules', 'cache', 'sessions',
                'logs', 'runtime'} for part in parts)
            and (name in exact or name in helper_names
                 or name.endswith('.rs') and name.startswith(roots)))


def digest(data):
    return hashlib.sha256(data).hexdigest()


manifest_bytes = (run / 'source-sha256.json').read_bytes()
manifest = json.loads(manifest_bytes)
helper_bytes = (run / 'helper-input-sha256.json').read_bytes()
helper = json.loads(helper_bytes)
assert set(helper) == helper_names
expected = {**manifest['files'], **helper}
assert all(allowed(name) for name in expected)
ready = json.loads((run / 'ready-driver.json').read_text())
report = json.loads((run / 'report.json').read_text())
assert report['ok'] and report['exitCode'] == 0 and report['cleanupVerified']
assert ready['sourceManifestSha256'] == digest(manifest_bytes)
assert ready['buildInfo']['build_source_sha256'] == digest(manifest_bytes)
assert report['artifacts'] == ready
with tarfile.open(run / 'source.tar.gz', 'r:gz') as archive:
    members = archive.getmembers()
    # Validate every member name/type before exposing any member content.
    assert all(member.isfile() and member.name.startswith('source/')
               and allowed(member.name[len('source/'):]) for member in members)
    assert len(members) == len(expected)
    assert {m.name for m in members} == {'source/' + name for name in expected}
    for member in members:
        name = member.name[len('source/'):]
        assert digest(archive.extractfile(member).read()) == expected[name], name
assert digest((run / 'source.tar.gz').read_bytes()) == ready['sourceArchiveSha256']

for name, public_name in [
    ('report.json', 'fly-mounted.json'), ('mounted-output.log', 'mounted-output.log'),
    ('resumed-build.log', 'build.log'), ('machine.json', 'machine-config.json'),
    ('source-sha256.json', 'source-sha256.json'),
    ('helper-input-sha256.json', 'helper-input-sha256.json'),
    ('source.tar.gz', 'source.tar.gz'),
]:
    shutil.copyfile(run / name, destination / public_name)

qualified = {'format_version': 1, 'purpose': 'exact qualified Rust and final helper source',
             'rust_support_manifest_sha256': digest(manifest_bytes),
             'helper_manifest_sha256': digest(helper_bytes), 'files': expected}
qualified_bytes = (json.dumps(qualified, sort_keys=True, separators=(',', ':')) + '\n').encode()
(destination / 'qualified-source-sha256.json').write_bytes(qualified_bytes)
changed = []
for name, sha in expected.items():
    cursor = repo
    for component in Path(name).parts:
        cursor /= component
        assert not cursor.is_symlink()
    if not cursor.is_file() or digest(cursor.read_bytes()) != sha:
        changed.append(name)
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
match = {'format_version': 1, 'verified_at': now,
         'source_manifest_sha256': digest(manifest_bytes),
         'qualified_source_manifest_sha256': digest(qualified_bytes),
         'source_files_compared': len(expected),
         'strict_allowlist_before_source_reads': True,
         'checkout_matches_frozen_qualification': not changed,
         'changed_files': changed,
         'source_archive_sha256': ready['sourceArchiveSha256']}
(destination / 'current-source-match.json').write_text(json.dumps(match, indent=2) + '\n')
audit = {'format_version': 1, 'allowlist_applied_before_file_content_reads': True,
         'member_names_validated_before_archive_content_reads': True,
         'regular_files_only': True, 'archive_matches_manifests': True,
         'source_archive_sha256': ready['sourceArchiveSha256'],
         'source_archive_file_count': len(members),
         'source_manifest_sha256': digest(manifest_bytes),
         'qualified_source_manifest_sha256': digest(qualified_bytes),
         'helper_manifest_sha256': digest(helper_bytes),
         'helper_file_count': len(helper), 'hidden_runtime_session_cache_files': 0,
         'binary_and_payload_archive_published': False}
(destination / 'archive-audit.json').write_text(json.dumps(audit, indent=2) + '\n')

fly = __import__('os').environ.get('FLYCTL', 'flyctl')
rows = json.loads(subprocess.check_output(
    [fly, 'machine', 'list', '-a', report['app'], '--json'], text=True, timeout=60))
cleanup = {'format_version': 1, 'verified_at': now, 'app': report['app'],
           'machine': report['machine'], 'name': report['name'],
           'qualification': report['qualification'],
           'machine_id_absent': not any(m['id'] == report['machine'] for m in rows),
           'owned_name_and_nonce_absent': not any(m.get('name') == report['name']
               and m.get('config', {}).get('metadata', {}).get('qualification')
               == report['qualification'] for m in rows),
           'production_machines_mutated': False}
assert cleanup['machine_id_absent'] and cleanup['owned_name_and_nonce_absent']
(destination / 'cleanup.json').write_text(json.dumps(cleanup, indent=2) + '\n')
print(json.dumps({'package': str(destination), 'archive': audit,
                  'source_comparison': match, 'cleanup': cleanup}))
