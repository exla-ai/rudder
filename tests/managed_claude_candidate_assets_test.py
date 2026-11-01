"""Small offline DATA regressions, UNRUN by author. No backend execution.
The synthetic stream seam cannot pass the fixed production policy/GPG checks.
"""
from pathlib import Path
import hashlib, importlib.util, io, json, os, stat, tempfile, unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
SPEC=importlib.util.spec_from_file_location('claude_candidate_assets',ROOT/'cloud/worker/managed-runtime/claude_assets.py')
candidate=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(candidate)
assets=candidate.assets
META=ROOT/'cloud/worker/managed-runtime/claude'

class ClaudeCandidateTests(unittest.TestCase):
    def test_public_signed_manifest_selects_exact_platform_hash_and_size(self):
        policy=assets.policy_at(ROOT/'cloud/worker/managed-runtime/policy.json')
        manifest=json.loads((META/'manifest.json').read_bytes())
        self.assertEqual(candidate.selected(manifest,policy['claude'])['size'],328262000)
        for field,value in [('version','2.1.315'),('size',1),('checksum','f'*64)]:
            changed=json.loads(json.dumps(manifest))
            if field=='version':changed[field]=value
            else:changed['platforms']['linux-x64'][field]=value
            with self.assertRaises(assets.Rejected):candidate.selected(changed,policy['claude'])

    def test_candidate_policy_keeps_execution_support_false_and_uploader_pin(self):
        policy=assets.policy_at(ROOT/'cloud/worker/managed-runtime/policy.json')
        self.assertTrue(policy['claude_packaged']);self.assertFalse(policy['claude_supported'])
        self.assertEqual(policy['uploader_script_sha256'],hashlib.sha256((ROOT/'cloud/worker/root-upload.mjs').read_bytes()).hexdigest())
        changed=dict(policy);changed['claude_supported']=True
        with tempfile.TemporaryDirectory() as name:
            p=Path(name)/'policy.json';p.write_text(json.dumps(changed))
            with self.assertRaises(assets.Rejected):assets.policy_at(p)

    def test_two_file_manifest_is_exact_and_codex_only_helper_seam_stays_compatible(self):
        codex={'codex':(assets.CODEX_SHA,assets.CODEX_BYTES)};both={**codex,'claude':(assets.CLAUDE_SHA,assets.CLAUDE_BYTES)}
        self.assertEqual(len(assets.manifest_value(codex,'runtime')['entries']),1)
        data=assets.manifest_value(both,'runtime',{'claude_packaged':True})
        self.assertEqual([e['path'].split('/')[-1] for e in data['entries']],['claude','codex'])
        for files in [codex,{**both,'extra':('a'*64,1)},{**both,'claude':('a'*64,assets.CLAUDE_BYTES)}]:
            with self.assertRaises(assets.Rejected):assets.manifest_value(files,'runtime',{'claude_packaged':True})

    def test_verifier_status_requires_full_primary_and_signing_fingerprint(self):
        valid=('[GNUPG:] VALIDSIG '+candidate.FP+' 2026-10-30 1793404192 0 4 0 1 10 00 '+candidate.FP+'\n').encode()
        candidate.status_valid(valid)
        for raw in [b'[GNUPG:] GOODSIG BAA929FF1A7ECACE Any UID\n',valid+valid,valid.replace(candidate.FP.encode(),b'0'*40,1),valid.replace(b' 1 10 00 ',b' 1 2 00 '),valid+b'[GNUPG:] BADSIG x\n']:
            with self.assertRaises(assets.Rejected):candidate.status_valid(raw)

    def test_pinned_metadata_rejects_mutation_without_invoking_verifier(self):
        with tempfile.TemporaryDirectory() as name:
            p=Path(name)/'metadata';p.write_bytes(b'original');digest=hashlib.sha256(b'original').hexdigest()
            self.assertEqual(candidate.pinned(p,digest,100,assets.original_end()),b'original')
            p.write_bytes(b'changed')
            with self.assertRaises(assets.Rejected):candidate.pinned(p,digest,100,assets.original_end())

    def test_atomic_raw_stream_refuses_wrong_digest_truncation_and_existing_output(self):
        body=b'\x7fELFsmall synthetic data';digest=hashlib.sha256(body).hexdigest()
        with tempfile.TemporaryDirectory() as name,patch.object(candidate,'SIZE',len(body)),patch.object(candidate,'SHA',digest):
            parent=Path(name)
            for index,bad in enumerate([body[:-1],body+b'x',b'\x7fELF'+b'x'*(len(body)-4)]):
                out=parent/('bad'+str(index))
                with self.assertRaises(assets.Rejected):candidate.install_raw(io.BytesIO(bad),out,assets.original_end())
                self.assertFalse(out.exists())
            output=parent/'claude';candidate.install_raw(io.BytesIO(body),output,assets.original_end())
            self.assertEqual(output.read_bytes(),body);self.assertEqual(stat.S_IMODE(output.stat().st_mode),0o555);self.assertEqual(output.stat().st_nlink,1)
            with self.assertRaises(assets.Rejected):candidate.install_raw(io.BytesIO(body),output,assets.original_end())

    def test_fragmented_first_elf_bytes_and_disk_cancel_do_not_publish_early(self):
        body=b'\x7fELFfragmented';digest=hashlib.sha256(body).hexdigest()
        class Fragmented(io.BytesIO):
            def read(self,n=-1):return super().read(min(n,1))
        with tempfile.TemporaryDirectory() as name,patch.object(candidate,'SIZE',len(body)),patch.object(candidate,'SHA',digest):
            output=Path(name)/'claude';candidate.install_raw(Fragmented(body),output,assets.original_end());self.assertEqual(output.read_bytes(),body)
            cancelled=Path(name)/'cancelled'
            def bounds():raise assets.Rejected('SYNTHETIC_DISK_FLOOR')
            with self.assertRaises(assets.Rejected):candidate.install_raw(io.BytesIO(body),cancelled,assets.original_end(),bounds)
            self.assertFalse(cancelled.exists())

if __name__=='__main__':unittest.main()
