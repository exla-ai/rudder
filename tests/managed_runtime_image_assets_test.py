"""Bounded offline BUILD DATA regressions. Written, UNRUN by draft author.
Synthetic archives cannot pass the production Codex policy/manifest pins and
create no native runtime, process, source, receipt or credential authority.
"""
from pathlib import Path
from types import SimpleNamespace
import base64, hashlib, importlib.util, io, json, os, stat, sys, tarfile, tempfile, unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('managed_image_assets',ROOT/'cloud/worker/managed-runtime/assets.py')
assets=importlib.util.module_from_spec(spec);spec.loader.exec_module(assets)
POLICY=ROOT/'cloud/worker/managed-runtime/policy.json'
MEMBER='package/vendor/x86_64-unknown-linux-musl/bin/codex'
ELF=b'\x7fELFsynthetic-component-only'

def archive(entries):
    target=io.BytesIO()
    with tarfile.open(fileobj=target,mode='w:gz') as tf:
        for name,body,kind in entries:
            entry=tarfile.TarInfo(name);entry.type=kind;entry.size=len(body) if kind==tarfile.REGTYPE else 0
            if kind==tarfile.SYMTYPE:entry.linkname='../../outside'
            tf.addfile(entry,io.BytesIO(body) if entry.isfile() else None)
    return target.getvalue()

def synthetic_spec(raw,body=ELF):
    return {'archive_member':MEMBER,'bytes':len(body),'sha256':hashlib.sha256(body).hexdigest(),
        'integrity':'sha512-'+base64.b64encode(hashlib.sha512(raw).digest()).decode(),
        'max_compressed_bytes':1048576,'max_expanded_bytes':1048576}

class ArchiveTests(unittest.TestCase):
    def test_selected_file_and_full_archive_integrity_both_required(self):
        raw=archive([(MEMBER,ELF,tarfile.REGTYPE),('package/unselected',b'ignored',tarfile.REGTYPE)])
        with tempfile.TemporaryDirectory() as name:
            output=Path(name)/'codex'
            result=assets.extract(io.BytesIO(raw),synthetic_spec(raw),output,'codex',assets.original_end())
            self.assertEqual(output.read_bytes(),ELF);self.assertEqual(stat.S_IMODE(output.stat().st_mode),0o555)
            self.assertEqual(output.stat().st_nlink,1);self.assertTrue(result['archiveIntegrityVerified']);self.assertFalse(result['registrySignaturesVerified'])
            self.assertFalse((Path(name)/'unselected').exists())
            with self.assertRaises(assets.Rejected):assets.manifest_value({'codex':(hashlib.sha256(ELF).hexdigest(),len(ELF))},'runtime')

    def test_wrong_full_archive_integrity_never_publishes_selected_elf(self):
        raw=archive([(MEMBER,ELF,tarfile.REGTYPE)]);p=synthetic_spec(raw);p['integrity']='sha512-invalid'
        with tempfile.TemporaryDirectory() as name:
            output=Path(name)/'codex'
            with self.assertRaises(assets.Rejected):assets.extract(io.BytesIO(raw),p,output,'codex',assets.original_end())
            self.assertFalse(output.exists());self.assertTrue(any(Path(name).glob('.codex.pending-*')))

    def test_traversal_symlink_and_duplicate_even_unselected_are_rejected(self):
        bads=[[(MEMBER,ELF,tarfile.REGTYPE),('package/../escape',b'x',tarfile.REGTYPE)],
              [(MEMBER,ELF,tarfile.REGTYPE),('package/link',b'',tarfile.SYMTYPE)],
              [(MEMBER,ELF,tarfile.REGTYPE),(MEMBER,ELF,tarfile.REGTYPE)]]
        for entries in bads:
            with self.subTest(entries=entries),tempfile.TemporaryDirectory() as name:
                raw=archive(entries);output=Path(name)/'codex'
                with self.assertRaises(assets.Rejected):assets.extract(io.BytesIO(raw),synthetic_spec(raw),output,'codex',assets.original_end())
                self.assertFalse(output.exists())

    def test_compressed_and_expanded_byte_bounds(self):
        raw=archive([(MEMBER,ELF,tarfile.REGTYPE),('package/extra',b'x'*8192,tarfile.REGTYPE)])
        for field in ['max_compressed_bytes','max_expanded_bytes']:
            with self.subTest(field=field),tempfile.TemporaryDirectory() as name:
                p=synthetic_spec(raw);p[field]=16;output=Path(name)/'codex'
                with self.assertRaises(assets.Rejected):assets.extract(io.BytesIO(raw),p,output,'codex',assets.original_end())
                self.assertFalse(output.exists())

    def test_expired_original_deadline_has_no_new_staging_effect(self):
        raw=archive([(MEMBER,ELF,tarfile.REGTYPE)])
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaises(assets.Rejected):assets.extract(io.BytesIO(raw),synthetic_spec(raw),Path(name)/'codex','codex',-1)
            self.assertEqual(list(Path(name).iterdir()),[])

    def test_disk_floor_failure_prevents_publication(self):
        raw=archive([(MEMBER,ELF,tarfile.REGTYPE)])
        with tempfile.TemporaryDirectory() as name:
            def blocked():raise assets.Rejected('synthetic disk floor')
            with self.assertRaises(assets.Rejected):assets.extract(io.BytesIO(raw),synthetic_spec(raw),Path(name)/'codex','codex',assets.original_end(),blocked)
            self.assertEqual(list(Path(name).iterdir()),[])

    @unittest.skipUnless(sys.platform=='linux','Linux no-overwrite publication')
    def test_publication_does_not_replace_existing_name(self):
        with tempfile.TemporaryDirectory() as name:
            a,b=Path(name)/'source',Path(name)/'target';a.write_bytes(b'new');b.write_bytes(b'original')
            with self.assertRaises(assets.Rejected):assets.rename_new(a,b)
            self.assertEqual(a.read_bytes(),b'new');self.assertEqual(b.read_bytes(),b'original')

class SourceAndSchemaTests(unittest.TestCase):
    def test_policy_bound_to_actual_official_metadata_and_corrected_sidecar(self):
        p=assets.policy_at(POLICY)
        data=(ROOT/'native/tests/fixtures/codex_protocol/codex-acquisition.json').read_bytes();m=json.loads(data)
        self.assertEqual(hashlib.sha256(data).hexdigest(),p['codex']['metadata_sha256'])
        self.assertEqual(m['registryMetadata']['dist']['tarball'],p['codex']['url'])
        self.assertEqual(m['registryMetadata']['dist']['integrity'],p['codex']['integrity'])
        self.assertEqual(hashlib.sha256((ROOT/'cloud/worker/host-control.mjs').read_bytes()).hexdigest(),p['sidecar_script_sha256'])
        self.assertEqual(hashlib.sha256((ROOT/'cloud/worker/host-control-core.mjs').read_bytes()).hexdigest(),p['sidecar_core_sha256'])
        lock=json.loads((ROOT/'cloud/worker/managed-runtime/sidecar-package-lock.json').read_text())
        self.assertEqual(lock['packages']['node_modules/ws']['integrity'],p['ws']['integrity'])
        self.assertFalse(p['claude_supported'])

    def test_missing_or_malformed_core_pin_is_not_an_old_policy_fallback(self):
        original=json.loads(POLICY.read_text())
        for value in [None,'0'*64,'A'*64,'a'*63,17]:
            changed=dict(original)
            if value is None:changed.pop('sidecar_core_sha256')
            else:changed['sidecar_core_sha256']=value
            with self.subTest(value=value),tempfile.TemporaryDirectory() as name:
                path=Path(name)/'policy.json';path.write_text(json.dumps(changed))
                with self.assertRaises(assets.Rejected):assets.policy_at(path)

    def test_exact_26_file_sidecar_refuses_omitted_substituted_or_wrong_core(self):
        p=assets.policy_at(POLICY);files={path:(hashlib.sha256(path.encode()).hexdigest(),1) for path in assets.SIDECAR_FILES}
        for name,field in [('host-control.mjs','sidecar_script_sha256'),('host-control-core.mjs','sidecar_core_sha256'),
                           ('root-upload.mjs','uploader_script_sha256'),('package.json','sidecar_package_sha256'),('package-lock.json','sidecar_lock_sha256')]:files[name]=(p[field],1)
        for addon in ['bufferutil','utf-8-validate']:files['node_modules/ws/node_modules/'+addon+'/index.js']=(assets.sha(assets.DENY),len(assets.DENY))
        archived=[{'path':path.removeprefix('node_modules/ws/'),'sha256':digest,'size_bytes':size} for path,(digest,size) in files.items()
                  if path.startswith('node_modules/ws/') and not path.startswith('node_modules/ws/node_modules/')]
        # Explicit synthetic receipt DATA for the validator seam, never an
        # installed-image/SRI/Root/provisioning/credential qualification receipt.
        receipt={'artifact':'ws','archiveIntegrityVerified':True,'integrity':p['ws']['integrity'],'sourcePolicySha256':p['_raw_sha256'],'files':archived}
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);package=root/'node_modules/ws/package.json';package.parent.mkdir(parents=True);package.write_text(json.dumps({'name':'ws','version':'8.22.0'}))
            received=root/'receipt.json';received.write_text(json.dumps(receipt));assets.expected_sidecar(root,files,p,received)
            missing=dict(files);missing.pop('host-control-core.mjs')
            substituted={**missing,'extra.mjs':('f'*64,1)}
            wrong={**files,'host-control-core.mjs':('f'*64,1)}
            for changed in [missing,substituted,wrong]:
                with self.assertRaises(assets.Rejected):assets.expected_sidecar(root,changed,p,received)

    def test_sidecar_four_field_pin_binds_exact_raw_manifest_and_both_scripts(self):
        p=assets.policy_at(POLICY);files={path:('a'*64,1) for path in assets.SIDECAR_FILES}
        files['host-control.mjs']=(p['sidecar_script_sha256'],1);files['host-control-core.mjs']=(p['sidecar_core_sha256'],1);files['root-upload.mjs']=(p['uploader_script_sha256'],1)
        manifest=assets.manifest_value(files,'sidecar');raw=assets.canonical(manifest);pin=assets.pin_value(manifest,raw,'sidecar',p)
        self.assertEqual(set(pin),{'sidecar_manifest_sha256','sidecar_script_sha256','sidecar_core_sha256','files'})
        self.assertEqual(len(pin['files']),26);self.assertEqual(pin['sidecar_manifest_sha256'],hashlib.sha256(raw).hexdigest())
        self.assertEqual(pin['sidecar_core_sha256'],p['sidecar_core_sha256'])
        changed=json.loads(json.dumps(manifest));changed['files'][0]['sha256']='f'*64
        with self.assertRaises(assets.Rejected):assets.pin_value(changed,assets.canonical(changed),'sidecar',p)
        changed=json.loads(json.dumps(manifest));changed['files'][0]['authority']=True
        with self.assertRaises(assets.Rejected):assets.pin_value(changed,assets.canonical(changed),'sidecar',p)

    def test_runtime_manifest_exact_paths_and_no_optional_claude_fabrication(self):
        value=assets.manifest_value({'codex':(assets.CODEX_SHA,assets.CODEX_BYTES)},'runtime')
        self.assertEqual(value['entries'][0]['path'],'/opt/rudder-managed-runtime/v1/codex')
        self.assertEqual(value['profile_id'],'fly-managed-image-v1')
        with self.assertRaises(assets.Rejected):assets.manifest_value({'codex':(assets.CODEX_SHA,assets.CODEX_BYTES),'claude':('a'*64,10)},'runtime')
        raw=assets.canonical(value);self.assertEqual(raw,assets.canonical(json.loads(raw)))
        self.assertNotEqual(hashlib.sha256(raw).hexdigest(),hashlib.sha256(raw+b' ').hexdigest())

    def test_sidecar_manifest_contains_every_file_except_manifest_itself(self):
        files={'host-control.mjs':('a'*64,10),'node_modules/ws/wrapper.mjs':('b'*64,20)}
        value=assets.manifest_value(files,'sidecar')
        self.assertEqual([v['path'] for v in value['files']],sorted(files));self.assertNotIn('manifest.json',[v['path'] for v in value['files']])

    def test_docker_managed_is_opt_in_and_default_legacy_block_preserved(self):
        docker=(ROOT/'cloud/worker/Dockerfile').read_text();legacy=docker.split('FROM worker-common AS legacy\n',1)[1]
        self.assertIn('FROM worker-common AS managed\n',docker);self.assertIn('CMD []',docker.split('FROM worker-common AS managed\n',1)[1].split('FROM worker-common AS legacy\n',1)[0])
        self.assertIn('COPY --chown=0:0 --chmod=0555 cloud/worker/host-control-core.mjs /out/sidecar/host-control-core.mjs',docker)
        self.assertIn('npm install -g @openai/codex@latest acpx@latest hunkdiff@latest pnpm@latest',legacy)
        self.assertTrue(legacy.rstrip().endswith('ENTRYPOINT ["/usr/local/bin/rudder-worker-root-bootstrap"]'))
        self.assertNotIn('managed-runtime',legacy)

@unittest.skipUnless(os.geteuid()==0 and sys.platform=='linux','actual root-owned image filesystem checks; no mock owner')
class RootFilesystemTests(unittest.TestCase):
    def test_actual_root_fd_hash_and_hardlink_symlink_refusal(self):
        with tempfile.TemporaryDirectory(prefix='rudder-image-test-',dir='/run') as name:
            root=Path(name);file=root/'file';file.write_bytes(b'bounded root image data');os.chmod(file,0o555)
            digest,size=assets.file_hash(file,1024,assets.original_end());self.assertEqual(size,file.stat().st_size)
            self.assertEqual(digest,hashlib.sha256(file.read_bytes()).hexdigest())
            os.link(file,root/'other')
            with self.assertRaises(assets.Rejected):assets.file_hash(file,1024,assets.original_end())
            (root/'other').unlink();os.symlink('file',root/'link')
            with self.assertRaises(assets.Rejected):assets.enumerate_files(root,'sidecar',assets.original_end())

    def test_extra_regular_file_changes_actual_closure(self):
        with tempfile.TemporaryDirectory(prefix='rudder-image-test-',dir='/run') as name:
            root=Path(name);(root/'codex').write_bytes(b'not a qualified runtime');os.chmod(root/'codex',0o555)
            files,_=assets.enumerate_files(root,'runtime',assets.original_end())
            with self.assertRaises(assets.Rejected):assets.manifest_value(files,'runtime')
            (root/'undeclared').write_bytes(b'no hidden files');os.chmod(root/'undeclared',0o555)
            more,_=assets.enumerate_files(root,'runtime',assets.original_end());self.assertEqual(set(more),{'codex','undeclared'})

if __name__=='__main__':unittest.main()
