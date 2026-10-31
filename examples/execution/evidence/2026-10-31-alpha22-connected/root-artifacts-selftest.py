#!/usr/bin/env python3
"""Pure synthetic private files. Does not enter the compiler/build function."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys
import tempfile

sys.dont_write_bytecode = True
base = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('root_artifacts', base / 'root-artifacts.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

with tempfile.TemporaryDirectory(prefix='mock-root-artifacts-', dir=base) as temporary:
    root = Path(temporary)
    repo = root / 'repo'
    repo.mkdir()
    for name in module.ROOTS:
        (repo / name).mkdir()
        (repo / name / 'example.rs').write_text('// synthetic first-party input, never compiled\n')
    (repo / 'engine/empty.rs').write_bytes(b'')
    for name in ('Cargo.toml', 'Cargo.lock'):
        (repo / name).write_text('# synthetic fixture only\n')
    (repo / 'package.json').write_text(json.dumps({'version': '4.0.0-alpha.21'}))
    destination = root / 'frozen'
    result = module.freeze(argparse.Namespace(repo=str(repo), output=str(destination), base_revision='1' * 40))
    assert result['status'] == 'frozen'
    envelope = module.read_json(destination / 'source-envelope.json')
    assert module.verify_source(envelope, destination / 'source')
    changed = destination / 'source/native/example.rs'
    original = changed.read_bytes()
    changed.write_bytes(b'changed mock bytes')
    try:
        module.verify_source(envelope, destination / 'source')
        raise AssertionError('changed compiler input was accepted')
    except ValueError:
        pass
    changed.write_bytes(original)
    artifacts = {}
    for name in ('rudder-native', 'rudder-jj', 'rudder-connected-network-qualifier'):
        location = destination / 'target/release' / name
        location.parent.mkdir(parents=True, exist_ok=True)
        data = bytearray(128)
        data[:6] = b'\x7fELF\x02\x01'
        data[18] = 0x3e
        location.write_bytes(data)
        artifacts[name] = module.artifact(location)
    log = destination / 'compiler.private.log'
    log.write_text('mock only; compiler was not invoked\n')
    receipt = {'formatVersion': 1, 'kind': 'actual-cargo-release-compiler-receipt-v1',
        'sourceKind': 'frozen-working-tree',
        'ok': True, 'exitCode': 0, 'sourceUnchanged': True, 'argv': module.COMMAND,
        'compilerProcessReaped': True, 'guardReason': None,
        'sourceEnvelopeSha256': module.digest(module.read_file(destination / 'source-envelope.json')),
        'sourceTreeSha256': envelope['sourceTreeSha256'], 'logPath': str(log),
        'logSha256': module.digest(log.read_bytes()), 'artifacts': artifacts}
    module.private_write(destination / 'compiler-receipt.json', module.encoded(receipt))
    assert module.stage(argparse.Namespace(directory=str(destination)))['status'] == 'image-staged'
    metadata = destination / 'mock-image-build-metadata.json'
    metadata.write_text(json.dumps({'containerimage.digest': 'sha256:' + '2' * 64}))
    answer = module.binding(argparse.Namespace(directory=str(destination), metadata=str(metadata),
        image='registry.fly.io/rudder-workers-exla:alpha22-connect-canary-artifact-selftest'))
    assert answer['status'] == 'bound-not-run'
    binding = module.read_json(destination / 'run-binding.json')
    assert binding['provenance']['nativeBinaryExecuted'] is False
    assert binding['provenance']['baseRevisionIsCallerDeclared'] is True
    assert 'shippedRevision' not in binding['provenance']

print(json.dumps({'kind': 'alpha22-root-artifacts-selftest', 'testOnlyMock': True,
    'freezeStageBindingSyntheticFilesPassed': True, 'changedSourceRejected': True,
    'compilerCommands': 0, 'providerCalls': 0, 'dnsCalls': 0, 'actualArtifactsQualified': False}))
