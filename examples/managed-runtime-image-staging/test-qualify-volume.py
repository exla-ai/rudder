"""Pure receipt/preflight checks. Never invokes Fly, Cargo or a child."""
import ast
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
import contextlib
from types import SimpleNamespace
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

v = load('volume_qualification', HERE / 'qualify-volume.py')
b = load('candidate_builder', HERE / 'build-candidate.py')

class ReceiptTests(unittest.TestCase):
    def setUp(self):
        self.parent = Path('/workspace/test-candidate')
        self.path = self.parent / 'build-receipt.json'
        self.raw_builder = (HERE / 'build-candidate.py').read_bytes()
        self.revision = 'a' * 40
        self.image = 'registry.fly.io/rudder-workers-exla:test'
        self.digest = 'sha256:' + 'b' * 64
        self.fly = b'synthetic-fly-not-executable'
        self.fly_sha = v.digest(self.fly)
        policy = {'node_base_image': 'node:22-slim@sha256:' + 'c' * 64}
        self.original_docker = '# syntax=docker/dockerfile:1\nFROM example AS managed\n'
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w', format=tarfile.PAX_FORMAT,
                          pax_headers={'comment': self.revision}) as archive:
            for name, raw in {
                'examples/managed-runtime-image-staging/build-candidate.py': self.raw_builder,
                'examples/managed-runtime-image-staging/probe.mjs': b'// synthetic captured probe\n',
                'cloud/worker/Dockerfile': self.original_docker.encode(),
                'cloud/worker/managed-runtime/policy.json': v.canonical(policy),
            }.items():
                row = tarfile.TarInfo(name); row.size = len(raw)
                archive.addfile(row, io.BytesIO(raw))
        self.archive = stream.getvalue()
        intent = v.canonical({'sourceRevision': self.revision, 'output': str(self.parent),
                              'image': self.image, 'buildOnly': True, 'compilerC1Image': b.C1_IMAGE})
        self.build_id = v.digest(intent)
        product = b.synthetic_product()
        self.log = ('#12 0.3 ' + b.MARKER + json.dumps(product) + '\n#13 pushing manifest for '
                    + self.image + '@' + self.digest + ' done\n').encode()
        source = self.parent / 'source'
        argv = ['/workspace/test-fly', 'deploy', '.', '--config', str(source / 'candidate.fly.toml'),
                '--app', 'rudder-workers-exla', '--dockerfile', str(source / 'candidate.Dockerfile'),
                '--build-target', 'managed_c1', '--build-arg', 'WORKER_NODE_IMAGE=' + policy['node_base_image'],
                '--build-arg', 'RUDDER_CANDIDATE_BUILD_ID=' + self.build_id,
                '--build-arg', 'RUDDER_IMAGE_PRESEED_PROFILE=baked-cas-v1', '--build-arg',
                'RUDDER_IMAGE_QUALIFICATION_NONCE=' + self.revision,
                '--build-only', '--push', '--remote-only', '--image-label', 'test']
        stages = ['archive', 'npm-root', 'typescript-cli', 'board', 'chmod-cli', 'npm-cloud',
                  'typescript-cloud', 'remote-build']
        commands = [{'stage': name, 'exitCode': 0, 'actualWait': True, 'pidfdHeld': True,
                     'pipeEofClaimed': False, 'pid': 123 + i} for i, name in enumerate(stages)]
        commands[-1].update(argv=argv, fileBackedLog={'path': str(self.parent / 'remote-build.log'),
                                                    'sha256': v.digest(self.log), 'bytes': len(self.log)})
        js = {'cloud/dist/test.js': {'sha256': 'e' * 64, 'bytes': 1}}
        self.receipt = {'formatVersion': 1, 'sourceRevision': self.revision,
                        'sourceArchive': str(self.parent / 'source.tar'), 'sourceDirectory': str(source),
                        'sourceArchiveSha256': v.digest(self.archive), 'builderSourceSha256': v.digest(self.raw_builder),
                        'dockerfileSha256': v.digest(b.render_dockerfile(self.original_docker, self.revision).encode()),
                        'workerNodeImage': policy['node_base_image'], 'compilerC1Image': b.C1_IMAGE,
                        'compilerC1Pins': b.C1_PINS, 'generatedJs': js,
                        'generatedJsSha256': v.digest(v.canonical(js) + b'\n'),
                        'flyExecutable': {'path': '/workspace/test-fly', 'sha256': self.fly_sha},
                        'commands': commands, 'buildId': self.build_id,
                        'image': self.image, 'digest': self.digest, 'productData': product,
                        'deployed': False, 'sdkQualified': False, 'runtimeQualified': False, 'sourceQualified': False}
        self.files = {str(self.parent / 'source.tar'): self.archive,
                      str(self.parent.with_name(self.parent.name + '.intent.json')): intent,
                      '/workspace/test-fly': self.fly,
                      str(source / 'examples/managed-runtime-image-staging/build-candidate.py'): self.raw_builder,
                      str(source / 'examples/managed-runtime-image-staging/probe.mjs'): b'// synthetic captured probe\n',
                      str(self.parent / 'remote-build.log'): self.log}
        self.sync_rows()

    def sync_rows(self):
        for row in self.receipt['commands']:
            self.files[str(self.parent / (row['stage'] + '.json'))] = v.canonical(row)

    def validate(self):
        def read(path, maximum):
            raw = self.files[str(path)]
            self.assertLessEqual(len(raw), maximum)
            return raw
        with patch('subprocess.Popen', side_effect=AssertionError('child forbidden')):
            return v.validate_build(self.receipt, v.canonical(self.receipt), self.path,
                                    read, vars(b), self.raw_builder, self.fly_sha)

    def test_complete_actual_data_join(self):
        result = self.validate()
        self.assertEqual(result['workerImage'], self.image.split(':')[0] + '@' + self.digest)
        self.assertFalse(result['sdkQualified'])
        self.receipt['sourceRevision'] = 'f' * 40
        self.assertEqual(result['sourceRevision'], self.revision)

    def test_explicit_fly_builder_is_bound_to_original_intent_and_command(self):
        intent_path = str(self.parent.with_name(self.parent.name + '.intent.json'))
        intent = json.loads(self.files[intent_path])
        intent['buildService'] = 'fly'
        self.files[intent_path] = v.canonical(intent)
        previous_build_id = self.build_id
        self.build_id = v.digest(self.files[intent_path])
        self.receipt.update(buildService='fly', buildId=self.build_id)
        argv = self.receipt['commands'][-1]['argv']
        argv[argv.index('RUDDER_CANDIDATE_BUILD_ID=' + previous_build_id)] = 'RUDDER_CANDIDATE_BUILD_ID=' + self.build_id
        argv.insert(argv.index('--build-only'), '--depot=false')
        self.sync_rows()
        self.assertEqual(self.validate()['workerImage'], self.image.split(':')[0] + '@' + self.digest)
        argv.remove('--depot=false')
        self.sync_rows()
        with self.assertRaisesRegex(RuntimeError, 'exact-original-build-only-command'):
            self.validate()

    def test_refuses_changed_or_unknown_build_service(self):
        for service in ('fly', 'unknown', True):
            with self.subTest(service=service):
                self.receipt['buildService'] = service
                with self.assertRaisesRegex(RuntimeError, 'same-original-build-service'):
                    self.validate()

    def test_refuses_prepared_or_foreign_receipt(self):
        for key, value in [('image', None), ('digest', None), ('sourceRevision', 'HEAD'),
                           ('formatVersion', True), ('sourceArchiveSha256', '0' * 64),
                           ('builderSourceSha256', '0' * 64), ('dockerfileSha256', '0' * 64),
                           ('compilerC1Image', 'mutable:latest'), ('sourceDirectory', '/tmp/foreign'),
                           ('buildId', '0' * 64), ('generatedJsSha256', '0' * 64),
                           ('productData', {}), ('workerNodeImage', 'node:latest')]:
            with self.subTest(key=key):
                original = self.receipt[key]; self.receipt[key] = value
                with self.assertRaises((RuntimeError, ValueError)): self.validate()
                self.receipt[key] = original

    def test_authority_never_inferred(self):
        for key in ('deployed', 'sdkQualified', 'runtimeQualified', 'sourceQualified'):
            self.receipt[key] = True
            with self.assertRaises(RuntimeError): self.validate()
            self.receipt[key] = False

    def test_wait_must_be_exact_and_same_record(self):
        for key, value in [('exitCode', True), ('exitCode', 1), ('actualWait', False),
                           ('pidfdHeld', False), ('pipeEofClaimed', True), ('pid', True)]:
            with self.subTest(key=key, value=value):
                original = self.receipt['commands'][-1][key]
                self.receipt['commands'][-1][key] = value; self.sync_rows()
                with self.assertRaises(RuntimeError): self.validate()
                self.receipt['commands'][-1][key] = original; self.sync_rows()
        self.files[str(self.parent / 'remote-build.json')] = b'{}'
        with self.assertRaises(RuntimeError): self.validate()

    def test_no_deploy_or_changed_executable(self):
        self.receipt['commands'][-1]['argv'].remove('--build-only'); self.sync_rows()
        with self.assertRaises(RuntimeError): self.validate()
        self.setUp(); self.files['/workspace/test-fly'] = b'foreign'
        with self.assertRaises(RuntimeError): self.validate()

    def test_log_must_match_actual_row_and_completed_push(self):
        for log in (self.log.replace(b' done', b''), self.log + self.log,
                    self.log.replace(self.image.encode(), b'registry.fly.io/foreign:test'),
                    self.log.replace(b.MARKER.encode(), b'RUN echo ' + b.MARKER.encode())):
            with self.subTest(log_sha=v.digest(log)):
                self.files[str(self.parent / 'remote-build.log')] = log
                row = self.receipt['commands'][-1]['fileBackedLog']
                row.update(sha256=v.digest(log), bytes=len(log)); self.sync_rows()
                with self.assertRaises((RuntimeError, ValueError)): self.validate()

    def test_generated_row_strict_types(self):
        row = self.receipt['generatedJs']['cloud/dist/test.js']; row['bytes'] = True
        self.receipt['generatedJsSha256'] = v.digest(v.canonical(self.receipt['generatedJs']) + b'\n')
        with self.assertRaises(RuntimeError): self.validate()

    def test_duplicate_keys_refused(self):
        with self.assertRaises(RuntimeError): v.strict(b'{"x":1,"x":2}')

    def test_preflight_uses_authenticated_captured_builder_and_probe(self):
        self.files[str(HERE / 'qualify-volume.py')] = (HERE / 'qualify-volume.py').read_bytes()
        self.files[str(self.path)] = v.canonical(self.receipt)
        def read(path, maximum):
            raw = self.files[str(path)]
            self.assertLessEqual(len(raw), maximum)
            return raw
        owner = v.ComponentOwner()
        with patch.object(owner, 'read', side_effect=read), patch.object(v, 'BUILD_RECEIPT', self.path), \
             patch.object(v, 'FLY_SHA256', self.fly_sha), \
             patch('subprocess.Popen', side_effect=AssertionError('child forbidden')):
            self.assertEqual(owner.preflight(), b'// synthetic captured probe\n')
        self.assertEqual(v.BINDING['operatorSource'], {'path': str(HERE / 'qualify-volume.py'),
                         'sha256': v.digest((HERE / 'qualify-volume.py').read_bytes())})
        self.assertEqual(v.BINDING['sourceRevision'], self.revision)

    def test_captured_builder_and_probe_cannot_differ_from_archive(self):
        for name in ('build-candidate.py', 'probe.mjs'):
            key = str(self.parent / 'source/examples/managed-runtime-image-staging' / name)
            original = self.files[key]; self.files[key] = b'changed'
            with self.assertRaisesRegex(RuntimeError, 'same-captured-source-bytes'):
                v.captured_sources(self.receipt, self.path, lambda path, maximum: self.files[str(path)])
            self.files[key] = original

    def test_protected_identity_is_explicit_and_stays_fixed_midrun(self):
        expected = v.protected_expectation('01M6FNM2N7HR0SYWYCSYK9MQJM', 'c' * 64)
        home = {'id': v.PROTECTED_ID, 'instance_id': expected['instanceId'],
                'image_ref': {'digest': 'sha256:' + expected['digest']}, 'state': 'started',
                'config': {'mounts': [{'volume': 'protected-volume'}]}}
        before = v.protected_identity([home], expected)
        self.assertEqual(before['volumeIds'], ['protected-volume'])
        self.assertEqual(v.protected_identity([home], expected), before)
        for key, value in [('instance_id', v.PROTECTED_INSTANCE), ('image_ref', {'digest': 'sha256:' + 'd' * 64}),
                           ('state', 'stopped'), ('id', 'foreign')]:
            changed = dict(home, **{key: value})
            with self.assertRaises(RuntimeError): v.protected_identity([changed], expected)
        self.assertEqual(expected['instanceId'], '01M6FNM2N7HR0SYWYCSYK9MQJM')

    def test_protected_configuration_refuses_partial_or_invalid_values(self):
        self.assertEqual(v.protected_expectation()['id'], v.PROTECTED_ID)
        for instance, image in [(None, 'c' * 64), ('A' * 26, None), (True, 'c' * 64),
                                ('A' * 26, 'sha256:' + 'c' * 64), ('A' * 26, 'C' * 64),
                                ('../not-an-instance', 'c' * 64)]:
            with self.assertRaises(RuntimeError): v.protected_expectation(instance, image)

    def test_failed_no_create_can_exit_only_with_settled_original_carriers(self):
        state = (False, False, False, False, None, None, None)
        child = dict(exitCode=0, constructorCompleted=True, actualChildWait=True,
                     stdoutEOF=True, stderrEOF=True, pidfdHeld=True, observerError=None,
                     timedOut=False, truncated=False)
        self.assertTrue(v.settled_no_create(state, [child], None))
        for index in range(7):
            changed = list(state); changed[index] = True if index < 4 else {'id': 'original'}
            self.assertFalse(v.settled_no_create(tuple(changed), [child], None))
        self.assertFalse(v.settled_no_create(state, [child], 'unknown'))
        for key, value in [('actualChildWait', False), ('stdoutEOF', False), ('stderrEOF', False),
                           ('observerError', 'unknown'), ('timedOut', True), ('truncated', True)]:
            self.assertFalse(v.settled_no_create(state, [dict(child, **{key: value})], None))

    def test_no_create_negative_exit_preserves_failed_report_and_create_holds(self):
        owner = v.ComponentOwner(); owner.output = Path('/workspace/test-only-no-effect')
        failed = dict(componentPassed=False, cleanupVerified=False)
        state = (False, False, False, False, None, None, None)
        with patch.object(owner, 'current_inputs'), patch.object(owner, 'current_output'), \
             patch.object(v, 'durable_new') as persist, \
             patch.object(owner, 'hold', side_effect=AssertionError('original hold')):
            self.assertEqual(owner.finish(failed, create_state=state), 1)
            self.assertEqual(failed, dict(componentPassed=False, cleanupVerified=False))
            persist.assert_called_once_with(owner.output / 'report.json', failed)
            with self.assertRaisesRegex(AssertionError, 'original hold'):
                owner.finish(failed, create_state=(True, False, False, False, None, None, None))

    def test_failed_create_cleanup_requires_known_id_and_settled_original_child(self):
        child = dict(exitCode=1, constructorCompleted=True, actualChildWait=True, stdoutEOF=True,
                     stderrEOF=True, pidfdHeld=True, observerError=None, timedOut=False, truncated=False)
        result = dict(exitCode=1, timedOut=False, child=child)
        machine = {'id': '84e691f2147978'}
        self.assertTrue(v.failed_create_cleanup_candidate(machine, result))
        for changed in (None, {}, {'id': v.PROTECTED_ID}, {'id': 'foreign'}, dict(machine, adopted=True)):
            self.assertFalse(v.failed_create_cleanup_candidate(changed, result))
        for key, value in [('actualChildWait', False), ('stdoutEOF', False), ('stderrEOF', False),
                           ('pidfdHeld', False), ('observerError', 'lost'), ('timedOut', True), ('truncated', True)]:
            self.assertFalse(v.failed_create_cleanup_candidate(machine, dict(result, child=dict(child, **{key: value}))))
        self.assertFalse(v.failed_create_cleanup_candidate(machine, dict(result, exitCode=0)))

    def test_provider_failure_retains_full_bounded_raw_stderr_reference(self):
        message = b'Error: machine failed to reach desired start state; restart policy no\n'
        result = dict(stderr=message, exitCode=1, timedOut=False)
        failure = v.provider_failure(result, '008-machine-create-once', self.parent)
        self.assertEqual(failure['message'], message.decode())
        self.assertFalse(failure['messageTruncated'])
        self.assertEqual(failure['stderrBytes'], len(message))
        self.assertEqual(failure['stderrSha256'], v.digest(message))
        failure = v.provider_failure(dict(result, stderr=b'x' * 17000), '008-machine-create-once', self.parent)
        self.assertEqual(len(failure['message']), 16384)
        self.assertTrue(failure['messageTruncated'])
        self.assertEqual(failure['stderrBytes'], 17000)

    def test_settled_readonly_log_timeout_is_finite_and_exactly_scoped(self):
        owner = v.ComponentOwner(); owner.output = self.parent
        row = dict(exitCode=-15, constructorCompleted=True, actualChildWait=True, stdoutEOF=True,
                   stderrEOF=True, pidfdHeld=True, observerError=None, timedOut=True, truncated=False,
                   stdout=b'partial exact machine logs', stderr=b'diagnostic deadline')
        args = ['logs', '--app', v.APP, '--machine', '84e691f2147978', '--no-tail', '--json']
        with patch.object(v, 'now', return_value=100), patch.object(v, 'FLY', '/test-fly'), \
             patch.object(v, 'capture', return_value=row) as capture, \
             patch.object(owner, 'current_inputs'), patch.object(owner, 'persist'), \
             patch.object(owner, 'hold', side_effect=AssertionError('hold forbidden')):
            observed, out, err = owner.call(args, 'failed-logs', 220, 15, diagnostic=True)
            capture.assert_called_once_with(['/test-fly', *args], 105)
            self.assertTrue(observed['timedOut']); self.assertEqual(out, b'partial exact machine logs')
            self.assertIsNone(owner.remoteUnknown)
            with self.assertRaisesRegex(RuntimeError, 'exact-read-only-machine-diagnostic'):
                owner.call(['machine', 'destroy', '84e691f2147978'], 'invalid', 220, 15, diagnostic=True)

    def exercise_failed_create(self, *, missing_id=False, changed_nonce=False, expire_original=False):
        nonce = 'b' * 32; expected = v.protected_expectation(); calls = []; persisted = {}; timer=[100.0]
        original_unknown, original_finish = v.ComponentOwner.unknown_create, v.ComponentOwner.finish
        class FakeOwner:
            def __init__(self):
                self.calls=[]; self.remoteUnknown=None; self.effectAttempted=False
                self.machine_live=False; self.volume_live=False
            def preflight(self): return b'// no probe accepted after failed create'
            def start_output(self, output): self.output=output; self.effectAttempted=True
            def persist(self, path, raw): persisted[path.name]=json.loads(raw)
            def current_inputs(self): pass
            def current_output(self): pass
            def hold(self): raise AssertionError('original hold retained')
            def unknown_create(self, *args, **kwargs): return original_unknown(self, *args, **kwargs)
            def finish(self, *args, **kwargs): return original_finish(self, *args, **kwargs)
            def call(self, args, label, end, timeout, **kwargs):
                calls.append((args,label,end,timeout,kwargs)); code=0; err=b''
                volume=dict(id='vol_test',name='rudder_qv_'+nonce[:12],region=v.REGION,
                            size_gb=1,encrypted=True,attached_machine_id=None)
                home=dict(id=v.PROTECTED_ID,instance_id=expected['instanceId'],state='started',
                          image_ref={'digest':'sha256:'+expected['digest']},config={'mounts':[{'volume':'vol_protected'}]})
                machine=None
                if self.machine_live:
                    config=json.loads(json.dumps(persisted['machine.json']))
                    if changed_nonce: config['metadata']['qualification']='foreign'
                    machine=dict(id='84e691f2147978',name='rudder-volume-'+nonce[:12],region=v.REGION,
                                 config=config,instance_id='A'*26,state=('starting'if expire_original else('started'if timer[0]>=177 else'starting')),image_ref={'digest':'sha256:'+'b'*64})
                if args[:2]==['machine','list']: data=[home]if args[3]==v.PROTECTED_APP else([machine]if machine else[])
                elif args[:2]==['volumes','list']: data=[volume]if self.volume_live else[]
                elif args[:2]==['secrets','list']: data=[]
                elif args[:2]==['volumes','create']: self.volume_live=True; data=volume
                elif args[:2]==['volumes','show']:
                    if expire_original and not self.machine_live: timer[0]=210
                    if self.machine_live: volume['attached_machine_id']='84e691f2147978'
                    data=volume
                elif args[:2]==['machine','run']:
                    self.machine_live=True; timer[0]=269 if expire_original else 160; code=1; data=b'no known ID'if missing_id else b'Machine ID: 84e691f2147978\n'
                    err=b'Error: machine failed to reach desired start state; restart policy no\n'
                elif args[:2]==['machine','exec']:
                    expected_bytes=b'rudder-fresh-volume-probe-v1\nnonce='+nonce.encode()+b'\n'+bytes(i%251 for i in range(4096))
                    data=dict(nonce=nonce,volumeId='vol_test',machineId='84e691f2147978',instanceId='A'*26,
                              probeSha256=v.digest(self.preflight()),mountPath=v.MOUNT,
                              scope='fresh-Fly-volume-filesystem-component-only',writeBytes=len(expected_bytes),
                              sha256=v.digest(expected_bytes),preRenameSha256=v.digest(expected_bytes))
                    for key in ['passed','newEmptyExceptLostFound','rootOwned','exclusiveCreate','noFollow','fileFsync',
                                'directoryFsync','atomicRename','exactReadback','originalMountRetained','ownedFileHandlesClosed']: data[key]=True
                    for key in ['coldRestartPerformed','restartDurabilityQualified','encryptionVerifiedByGuest','physicalRootQualified',
                                'sourceAuthority','sdkAuthority','haQualified','fleetDurability','normalRuntimeRetirement','productionEnablement','models']: data[key]=False
                elif args[:2]==['machine','destroy']: self.machine_live=False; data={}
                elif args[:2]==['volumes','destroy']: self.volume_live=False; data={}
                elif args[0]=='logs': data={'message':'machine started after image pull'}
                else: raise AssertionError('unexpected provider command '+repr(args))
                raw=data if type(data)is bytes else v.canonical(data)
                row=dict(exitCode=code,constructorCompleted=True,actualChildWait=True,stdoutEOF=True,
                         stderrEOF=True,pidfdHeld=True,observerError=None,timedOut=False,truncated=False)
                self.calls.append(row); return row,raw,err
        owner=FakeOwner()
        with patch.object(v,'ComponentOwner',return_value=owner), patch.object(v,'now',side_effect=lambda:timer[0]), \
             patch.object(v,'BINDING',{'workerImage':'registry.fly.io/rudder-workers-exla@sha256:'+'b'*64}), \
             patch.object(v.uuid,'uuid4',return_value=SimpleNamespace(hex=nonce)), patch.object(v.time,'sleep',side_effect=lambda seconds:timer.__setitem__(0,timer[0]+seconds)), \
             patch.object(sys,'argv',['qualifier','--build-receipt','/workspace/test/build-receipt.json',
                                      '--flyctl-sha256','a'*64,'--output','/workspace/rudder-fresh-volume-run-pure-failed-create']), \
             patch('subprocess.Popen',side_effect=AssertionError('child forbidden')), \
             contextlib.redirect_stdout(io.StringIO()):
            if missing_id or changed_nonce:
                with self.assertRaisesRegex(AssertionError,'original hold retained'): v.main()
            else: self.assertEqual(v.main(),1 if expire_original else 0)
        return owner,calls,persisted

    def test_cold_start_failed_cli_reconciles_same_original_then_requires_probe_and_cleanup(self):
        owner,calls,files=self.exercise_failed_create()
        report=files['report.json']
        self.assertTrue(report['componentPassed']); self.assertTrue(report['cleanupVerified'])
        self.assertFalse(report['originalMachineCreateAcknowledged'])
        self.assertTrue(report['originalMachineCreateReconciled'])
        self.assertFalse(report['machineCreateUncertain'])
        self.assertFalse(owner.machine_live); self.assertFalse(owner.volume_live)
        self.assertEqual(sum(args[:2]==['machine','run']for args,*_ in calls),1)
        self.assertEqual(sum(args[:2]==['machine','exec']for args,*_ in calls),1)
        self.assertGreater(sum('original-machine-observe'in label for _,label,*_ in calls),8)
        reconciliation=files['original-machine-create-reconciled.json']
        self.assertEqual(reconciliation['originalWorkEndBoottime'],280)
        self.assertEqual(reconciliation['observedAtBoottime'],178)
        self.assertFalse(reconciliation['qualificationGranted'])
        self.assertEqual(report['providerFailures'][0]['message'],'Error: machine failed to reach desired start state; restart policy no\n')

    def test_expired_original_work_never_probes_or_passes_but_cleans_known_create(self):
        owner,calls,files=self.exercise_failed_create(expire_original=True)
        report=files['report.json']
        self.assertFalse(report['componentPassed']); self.assertTrue(report['cleanupVerified'])
        self.assertFalse(report['originalMachineCreateAcknowledged'])
        self.assertFalse(report['originalMachineCreateReconciled'])
        self.assertFalse(any(args[:2]==['machine','exec']for args,*_ in calls))
        self.assertFalse(owner.machine_live); self.assertFalse(owner.volume_live)
        self.assertEqual(report['error']['reason'],'original-boot-window-expired')
        logs=[call for call in calls if call[0][0]=='logs']; self.assertEqual(len(logs),1)
        self.assertEqual(logs[0][0],['logs','--app',v.APP,'--machine','84e691f2147978','--no-tail','--json'])
        self.assertEqual(logs[0][3],15); self.assertTrue(logs[0][4]['diagnostic'])

    def test_failed_create_without_original_id_or_matching_nonce_keeps_hold(self):
        for options in (dict(missing_id=True),dict(changed_nonce=True)):
            owner,calls,files=self.exercise_failed_create(**options)
            self.assertTrue(owner.machine_live); self.assertTrue(owner.volume_live)
            self.assertFalse(any(args[:2]in(['machine','destroy'],['volumes','destroy'])for args,*_ in calls))
            self.assertFalse(any(args[0]=='logs'for args,*_ in calls))

    def test_existing_deadlines_and_carrier_unchanged(self):
        self.assertEqual((v.WORK_SECONDS, v.CLEANUP_SECONDS, v.MAX_CALLS, v.CLEANUP_RESERVE), (180,120,64,16))
        expected = {'owned_child.py': '22ee38a91c9f4764f831d3cf19949b4be1c2a562730951788ade8b29d45f7ae9', 'probe.mjs': 'b3ae62ba84ce86ce7dfe89afaa3f9baabb2d35f495c10f8ee38d7d785b695577'}
        for name, sha in expected.items():
            self.assertEqual(v.digest((HERE / name).read_bytes()), sha)

    def test_provider_ownership_and_cleanup_source_preserved(self):
        # Original reviewed V3 bodies, independent of the historical filesystem.
        expected = {'__init__': '5d394e77b56858de31e3f9c896b2824282c0f46d4a40a85654258dbadb2cb284', 'hold': '7367754119f6a48c3d991e48b94054782b216640bcfbc9f08d72ba35bdba914e', 'read': '38ca1a59a37a75854121f4e34ee4eba78ffd415d3b37d5ee1c6331c8be636dfb', 'current_inputs': '22a900690c4776ecdbd82e75f13c370b7e38ed72526d63d76c193338afd7df68', 'start_output': '22193aa74503986842ca6082f91f2bc520f2f4c91d46fd4483a99c6c11061e47', 'current_output': '534b3961b049a10749f43e5f90914d16e15c5c8cc5b14d8e3f4cf4280d0f3da8', 'persist': '7ba51903b586434f2ec433a36c790239826dbc7f0c391ef70b9fd859d983743e', 'call': '7079c3cfb713d69b9ac30e2bc1b160f2e466e934f042cc7e97de0f3f1a5cba00', 'unknown_create': 'ab6a3895d8d3545faba6e0f0f2df92e22d80e941bac56d95e8ebcf539a94f94d', 'finish': 'ea4832e7c3f266fec9f0fb6586017fc496ee06c749c4aa30b9b4a4448270930c'}
        source = (HERE / 'qualify-volume.py').read_text()
        owner = next(n for n in ast.parse(source).body if isinstance(n, ast.ClassDef))
        actual = {n.name: v.digest(ast.dump(n, include_attributes=False).encode())
                  for n in owner.body if isinstance(n, ast.FunctionDef) and n.name != 'preflight'}
        self.assertEqual(actual, expected)
        body = source[source.index('    output = Path(args.output)'):]
        self.assertEqual(v.digest(body.encode()), '3f97de117243f23db758fe0137bb8838b176799c69d5cca66189639837a4b955')

    def test_real_retained_input_refuses_replacement(self):
        with tempfile.TemporaryDirectory(dir='/workspace') as directory:
            path = Path(directory) / 'input'; path.write_bytes(b'synthetic'); path.chmod(0o600)
            owner = v.ComponentOwner()
            try:
                self.assertEqual(owner.read(path, 100), b'synthetic'); owner.current_inputs()
                path.unlink(); path.write_bytes(b'synthetic'); path.chmod(0o600)
                with self.assertRaises(RuntimeError): owner.current_inputs()
            finally:
                for fd in owner.handles: v.os.close(fd)

if __name__ == '__main__':
    unittest.main()
