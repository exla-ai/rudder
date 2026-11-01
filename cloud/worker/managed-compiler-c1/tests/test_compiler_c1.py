"""UNRUN DATA tests. No actual compiler/provider/Root grant is executed."""
import hashlib,importlib.util,json,os,struct,tempfile,unittest
from pathlib import Path

ROOT=Path(__file__).parents[1]
spec=importlib.util.spec_from_file_location('c1',ROOT/'compiler_c1.py');c1=importlib.util.module_from_spec(spec);spec.loader.exec_module(c1)
base=c1.base

class CompilerC1(unittest.TestCase):
    def setUp(self):self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
    def tearDown(self):
        for directory,_,_ in os.walk(self.root):Path(directory).chmod(0o700)
        self.temp.cleanup()
    def fixture(self):
        files=set(c1.REQUIRED)|set(c1.EXECUTABLES)|{'prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libtest.rlib'}
        for name in sorted(files):
            p=self.root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(('DATA '+name).encode());p.chmod(0o555 if name in c1.EXECUTABLES else 0o444)
        for name in base.REQUIRED_DIRS:(self.root/name).mkdir(parents=True,exist_ok=True)
        for directory,_,_ in os.walk(self.root,topdown=False):Path(directory).chmod(0o555)
        entries,total=base.inventory(self.root,base.Clock(),os.getuid(),os.getgid())
        manifest={'format_version':1,'profile_id':c1.PROFILE,'entries':entries,'logical_bytes':total}
        raw=base.canonical(manifest);self.root.chmod(0o755);(self.root/'manifest.json').write_bytes(raw);(self.root/'manifest.json').chmod(0o444);self.root.chmod(0o555);return manifest,hashlib.sha256(raw).hexdigest()
    def test_C1_has_separate_manifest_profile_and_exact_required_tools(self):
        manifest,pin=self.fixture();c1.validate_profile(manifest)
        result=c1.verify(self.root,base.Clock(),pin,os.getuid(),os.getgid())
        self.assertEqual(result['profile_id'],c1.PROFILE)
        with self.assertRaises(ValueError):base.validate_shape(manifest) # C1 cannot masquerade as A2.
        legacy={**manifest,'profile_id':base.PROFILE};base.validate_shape(legacy)
        with self.assertRaises(ValueError):base.validate_shape(legacy,c1.PROFILE,c1.REQUIRED)
    def test_missing_CRT_or_CXX_internal_is_not_complete_C1(self):
        manifest,_=self.fixture()
        for required in ('prepared/usr/lib/x86_64-linux-gnu/crt1.o','prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1plus'):
            wrong=json.loads(json.dumps(manifest));entry=next(x for x in wrong['entries'] if x['path']==required);wrong['entries'].remove(entry);wrong['logical_bytes']-=entry['size_bytes']
            with self.assertRaises(ValueError):base.validate_shape(wrong,c1.PROFILE,c1.REQUIRED)
    def test_nonexecutable_driver_cannot_be_complete_C1(self):
        manifest,_=self.fixture();next(e for e in manifest['entries'] if e['path']=='prepared/usr/bin/gcc')['mode']=0o444
        with self.assertRaises(ValueError):c1.validate_profile(manifest)
    def test_actual_byte_corruption_and_symlink_replacement_refuse(self):
        _,pin=self.fixture();file=self.root/'prepared/usr/bin/gcc';before=file.read_bytes();file.chmod(0o644);file.write_bytes(b'Y'*len(before));file.chmod(0o444)
        with self.assertRaises(ValueError):base.verify(self.root,base.Clock(),pin,os.getuid(),os.getgid(),c1.PROFILE,c1.REQUIRED)
        file.parent.chmod(0o755);file.unlink();file.symlink_to('/etc/passwd');file.parent.chmod(0o555)
        with self.assertRaises(ValueError):base.verify(self.root,base.Clock(),pin,os.getuid(),os.getgid(),c1.PROFILE,c1.REQUIRED)
    def elf(self,kind,dynamic=True,bad=False):
        raw=bytearray(120);raw[:6]=b'\x7fELF\x02\x01';struct.pack_into('<H',raw,16,kind);struct.pack_into('<H',raw,18,62);struct.pack_into('<Q',raw,32,64);struct.pack_into('<HH',raw,54,56,200 if bad else 1);struct.pack_into('<I',raw,64,2 if dynamic else 1)
        path=self.root/('elf-'+str(kind)+str(dynamic)+str(bad));path.write_bytes(raw);return path
    def test_CRT_relocatables_and_static_ELFs_do_not_enter_ldd(self):
        self.assertFalse(c1.dynamic_elf(self.elf(1),base.Clock()));self.assertFalse(c1.dynamic_elf(self.elf(2,False),base.Clock()));self.assertTrue(c1.dynamic_elf(self.elf(3),base.Clock()))
        with self.assertRaises(ValueError):c1.dynamic_elf(self.elf(3,bad=True),base.Clock())
    def test_unchanged_quota_and_original_clock(self):
        self.assertEqual(base.MAX_BYTES,2*1024**3);self.assertEqual(base.MAX_FILES,65536);self.assertEqual(base.MAX_MANIFEST,32*1024**2)
        clock=base.Clock();clock.end=0
        with self.assertRaises(ValueError):c1.dynamic_elf(self.elf(3),clock)
    def test_C1_policy_is_fixed_offline_surface_not_repository_cache(self):
        policy=json.loads((ROOT/'inputs/c1-policy.json').read_bytes());self.assertEqual(policy['profile_id'],c1.PROFILE);self.assertEqual(policy['target'],'x86_64-linux-gnu')
        self.assertIn('libc6-dev',policy['packages']);self.assertIn('libstdc++-12-dev',policy['packages']);self.assertIn('libsqlite3-dev',policy['packages'])
        with self.assertRaises(ValueError):c1.public('/etc/passwd')
    def driver_fixture(self):
        public=self.root/'usr';public.mkdir(mode=0o755);(public/'gcc/a/b').mkdir(parents=True);(public/'crt').mkdir();file=public/'crt/crt1.o';file.write_bytes(b'actual CRT fixture');file.chmod(0o444)
        for directory,_,_ in os.walk(public):Path(directory).chmod(0o755)
        return public.resolve(),file.resolve()
    def test_actual_driver_relative_components_resolve_to_canonical_public_file(self):
        public,file=self.driver_fixture();raw=str(public/'gcc/a/b/../../../crt/crt1.o')
        actual=c1._driver_source(raw,base.Clock(),public,(os.getuid(),os.getgid()));self.assertEqual(actual,file);self.assertNotIn('..',actual.parts)
        # Manifest/package paths remain strict. Only trusted driver-output DATA
        # gets canonical resolution; there is no generic path-policy relaxation.
        with self.assertRaises(ValueError):c1.public(raw)
        with self.assertRaises(ValueError):base.relative('usr/gcc/../../../crt1.o')
    def test_driver_traversal_symlink_writable_and_bound_rejections(self):
        public,file=self.driver_fixture();private=self.root/'private';private.mkdir();outside=private/'secret';outside.write_bytes(b'not an SDK input')
        (public/'escape').symlink_to(outside);(public/'crt/inside').symlink_to(file)
        self.assertEqual(c1._driver_source(str(public/'crt/inside'),base.Clock(),public,(os.getuid(),os.getgid())),file)
        for raw in [str(public/'../private/secret'),str(public/'escape'),str(file)+'\n','//'+str(file).lstrip('/'),'relative/crt1.o',str(public)+'/'+'x'*4096]:
            with self.assertRaises((ValueError,OSError)):c1._driver_source(raw,base.Clock(),public,(os.getuid(),os.getgid()))
        (public/'crt').chmod(0o777)
        with self.assertRaises(ValueError):c1._driver_source(str(file),base.Clock(),public,(os.getuid(),os.getgid()))
        (public/'crt').chmod(0o755);file.chmod(0o666)
        with self.assertRaises(ValueError):c1._driver_source(str(file),base.Clock(),public,(os.getuid(),os.getgid()))

if __name__=='__main__':unittest.main()
