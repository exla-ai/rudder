"""Pure operator adaptation checks. No provider, compiler or child operations."""
import ast
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('derive_lifecycle', HERE / 'derive-lifecycle.py')
d = importlib.util.module_from_spec(spec); spec.loader.exec_module(d)


class ProtectedAdapterTests(unittest.TestCase):
    def setUp(self):
        self.expected = dict(d.PROTECTED_HOME, instanceId='01M6FNM2N7HR0SYWYCSYK9MQJM', digest='c' * 64)
        self.rows = {
            'controller/run.py': ("PROTECTED_APP='rudder-workers-exla';PROTECTED='8edd225a7d0378';"
                "PROTECTED_INSTANCE='" + d.PROTECTED_HOME['instanceId'] + "';PROTECTED_IMAGE='sha256:"
                + d.PROTECTED_HOME['digest'] + "'\n"
                "def protected(self, cleanup=False):\n"
                " status,m=self.api('GET', '/v1/apps/'+PROTECTED_APP+'/machines/'+PROTECTED, cleanup=cleanup)\n"
                " need(status==200 and identity(m)==(PROTECTED,PROTECTED_INSTANCE,PROTECTED_IMAGE),'PROTECTED_HOME_CHANGED')\n").encode(),
            'cleanup/cleanup.py': ("PROTECTED_APP = 'rudder-workers-exla'\nPROTECTED = "
                + repr((d.PROTECTED_HOME['id'], d.PROTECTED_HOME['instanceId'], 'sha256:' + d.PROTECTED_HOME['digest']))
                + "\ndef protected(self, final=False):\n"
                " status,m=self.api('GET','/v1/apps/'+PROTECTED_APP+'/machines/'+PROTECTED[0],final_guard=final)\n"
                " need(status==200 and identity(m)==PROTECTED,'PROTECTED_HOME_CHANGED')\n"
                "def delete(self,path):\n"
                " need(PROTECTED[0] not in path and not path.startswith('/v1/apps/'+PROTECTED_APP),'PROTECTED_DELETE_FORBIDDEN')\n").encode(),
            'supervision/run.py': b'# unchanged supervision\n',
        }

    def test_default_identity_keeps_original_source_bytes(self):
        self.assertEqual(d.protected_home({}), d.PROTECTED_HOME)
        self.assertEqual(d.rewrite_protected(self.rows, d.protected_home({})), self.rows)

    def test_rewrites_only_known_identity_literals_and_preserves_guards(self):
        changed = d.rewrite_protected(self.rows, d.protected_home({'protectedHome': self.expected}))
        self.assertEqual(changed['supervision/run.py'], self.rows['supervision/run.py'])
        for name in ('controller/run.py', 'cleanup/cleanup.py'):
            self.assertIn(self.expected['instanceId'].encode(), changed[name])
            self.assertIn(('sha256:' + self.expected['digest']).encode(), changed[name])
            original_functions = [ast.dump(n, include_attributes=False) for n in ast.parse(self.rows[name]).body if isinstance(n, ast.FunctionDef)]
            changed_functions = [ast.dump(n, include_attributes=False) for n in ast.parse(changed[name]).body if isinstance(n, ast.FunctionDef)]
            self.assertEqual(changed_functions, original_functions)
            def need(value, code):
                if not value: raise RuntimeError(code)
            namespace = {'need': need, 'identity': lambda m: (m.get('id'), m.get('instance_id'), (m.get('image_ref') or {}).get('digest'))}
            exec(compile(changed[name], name, 'exec'), namespace)
            expected_identity = (self.expected['id'], self.expected['instanceId'], 'sha256:' + self.expected['digest'])
            if name.startswith('controller'):
                self.assertEqual(tuple(namespace[k] for k in ('PROTECTED', 'PROTECTED_INSTANCE', 'PROTECTED_IMAGE')), expected_identity)
            else:
                self.assertEqual(namespace['PROTECTED'], expected_identity)
            home = {'id': expected_identity[0], 'instance_id': expected_identity[1], 'image_ref': {'digest': expected_identity[2]}}
            owner = SimpleNamespace(api=lambda *args, **kwargs: (200, home))
            namespace['protected'](owner)
            home['instance_id'] = d.PROTECTED_HOME['instanceId']
            with self.assertRaisesRegex(RuntimeError, 'PROTECTED_HOME_CHANGED'):
                namespace['protected'](owner, True)
            if name.startswith('cleanup'):
                with self.assertRaisesRegex(RuntimeError, 'PROTECTED_DELETE_FORBIDDEN'):
                    namespace['delete'](owner, '/v1/apps/' + self.expected['app'] + '/machines/' + self.expected['id'])
        self.assertEqual(self.rows['supervision/run.py'], changed['supervision/run.py'])

    def test_rejects_foreign_or_partial_config(self):
        for key, value in [('app', 'other'), ('id', 'other'), ('instanceId', True),
                           ('instanceId', '../invalid'), ('digest', 'sha256:' + 'c' * 64), ('digest', 'C' * 64)]:
            with self.subTest(key=key, value=value), self.assertRaises(d.h.Refusal):
                d.protected_home({'protectedHome': dict(self.expected, **{key: value})})
        with self.assertRaises(d.h.Refusal): d.protected_home({'protectedHome': {'instanceId': 'A' * 26}})
        with self.assertRaises(d.h.Refusal): d.protected_home({'protectedHome': dict(self.expected, extra=True)})

    def test_rejects_missing_or_duplicated_known_literals(self):
        for raw in (b'# missing', self.rows['controller/run.py'] * 2):
            with self.assertRaises(d.h.Refusal):
                d.rewrite_protected(dict(self.rows, **{'controller/run.py': raw}), self.expected)

    def test_intent_binds_operator_identity_separately_from_image_capture(self):
        with tempfile.TemporaryDirectory(dir='/workspace') as directory:
            parent = Path(directory); config = parent / 'config.json'; output = parent / 'derived'
            config.write_text(json.dumps({'protectedHome': self.expected})); config.chmod(0o600)
            binding = {'capture': {'sourceRevision': 'a' * 40}}
            with patch.object(d, 'render', return_value=(self.rows, binding, self.rows)), \
                 patch('subprocess.Popen', side_effect=AssertionError('child forbidden')), \
                 contextlib.redirect_stdout(io.StringIO()):
                d.derive(SimpleNamespace(output=output, config=config, baseline=parent / 'baseline'))
            intent = json.loads(output.with_name('derived.intent.json').read_bytes())
            result = json.loads((output / 'derived.json').read_bytes())
            for value in (intent, result):
                self.assertEqual(value['protectedHome'], self.expected)
                self.assertEqual(value['imageCaptureRevision'], 'a' * 40)
                self.assertEqual(value['operatorSource'], d.operator_source())
            self.assertEqual(intent['files']['controller/run.py'], result['derived']['controller/run.py'])


if __name__ == '__main__':
    unittest.main()
