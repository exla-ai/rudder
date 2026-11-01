"""Source/policy DATA only, UNRUN. No executable or image/provider authority."""
from pathlib import Path
import hashlib, importlib.util, json, tempfile, unittest

ROOT=Path(__file__).resolve().parents[1]
SCRIPT=ROOT/'cloud/worker/managed-runtime/assets.py'
SPEC=importlib.util.spec_from_file_location('qualified_origin_assets',SCRIPT)
assets=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(assets)
POLICY=ROOT/'cloud/worker/managed-runtime/policy.json'

class QualifiedControlOriginAssetsTests(unittest.TestCase):
    def test_exact_scripts_are_pinned_without_changing_sidecar_26_closure(self):
        p=assets.policy_at(POLICY)
        for name,field in [('host-control.mjs','sidecar_script_sha256'),('host-control-core.mjs','sidecar_core_sha256'),('root-upload.mjs','uploader_script_sha256')]:
            self.assertEqual(hashlib.sha256((ROOT/'cloud/worker'/name).read_bytes()).hexdigest(),p[field])
        self.assertEqual(len(assets.SIDECAR_FILES),26)
        self.assertEqual(set(assets.SIDECAR_FILES)&{'host-control.mjs','root-upload.mjs'},{'host-control.mjs','root-upload.mjs'})
        self.assertFalse(p['claude_supported'])

    def test_missing_zero_or_malformed_uploader_pin_never_uses_old_policy(self):
        original=json.loads(POLICY.read_bytes())
        for value in [None,'0'*64,'A'*64,'a'*63,17]:
            p=dict(original)
            if value is None:p.pop('uploader_script_sha256')
            else:p['uploader_script_sha256']=value
            with tempfile.TemporaryDirectory() as directory:
                path=Path(directory)/'policy.json';path.write_text(json.dumps(p))
                with self.assertRaises(assets.Rejected):assets.policy_at(path)

    def test_build_pin_shape_stays_four_fields_and_complete_file_hashes(self):
        p=assets.policy_at(POLICY)
        files={name:(hashlib.sha256(name.encode()).hexdigest(),1) for name in assets.SIDECAR_FILES}
        for name,field in [('host-control.mjs','sidecar_script_sha256'),('host-control-core.mjs','sidecar_core_sha256'),('root-upload.mjs','uploader_script_sha256')]:files[name]=(p[field],1)
        manifest=assets.manifest_value(files,'sidecar',p);raw=assets.canonical(manifest);pin=assets.pin_value(manifest,raw,'sidecar',p)
        self.assertEqual(set(pin),{'sidecar_manifest_sha256','sidecar_script_sha256','sidecar_core_sha256','files'})
        self.assertEqual(pin['files'],manifest['files']);self.assertEqual(len(pin['files']),26)
        self.assertEqual(next(f['sha256'] for f in pin['files'] if f['path']=='root-upload.mjs'),p['uploader_script_sha256'])
        wrong=json.loads(json.dumps(manifest));next(f for f in wrong['files'] if f['path']=='root-upload.mjs')['sha256']='f'*64
        with self.assertRaises(assets.Rejected):assets.pin_value(wrong,assets.canonical(wrong),'sidecar',p)

if __name__=='__main__':unittest.main()
