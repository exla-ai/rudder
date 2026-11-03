"""Pure captured-source checks and complete packaging into disposable local fixtures."""
import argparse
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import os
import subprocess
import re
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('controller_packaging', ROOT/'examples/managed-runtime-image-staging/prepare-context.py')
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class Packaging(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory()
  self.addCleanup(self.tmp.cleanup)
  self.root=Path(self.tmp.name).resolve()
  self.source=self.root/'source'
  self.setup=self.source/'examples/managed-runtime-image-staging'
  fixed=[m.ENTRY_SOURCE_PATH,'examples/managed-runtime-image-staging/prepare-context.py','cloud/package.json','cloud/package-lock.json']
  fixed+=['cloud/src/managed-home-provisioning/'+n for n in ('genuine-ingress-log-worker.py','genuine-ingress-log-project.py')]
  fixed+=['examples/managed-runtime-image-staging/'+n for n in ('supervise.py','install.mjs','health.mjs','receive-private.py','genuine-ingress-target-check.mjs','genuine-ingress-target-probe.py','Dockerfile','fly.toml')]
  for name in fixed:self.write(name,(ROOT/name).read_bytes() if name.endswith('prepare-context.py') else b'{}\n' if name.endswith('.json') else b'captured source\n')
  required=set(m.MODULES)|{'runtime-qualification-driver.js','runtime-qualification-completion-schema.js','runtime-qualification.js','compiler-qualification-driver.js','compiler-qualification-completion-schema.js','compiler-qualification.js','compiler-sdk-semantic-data.js','staging-role-activation-data.js','staging-role-activation.js','staging-listener-admission.js','staging-positive-ingress-driver.js'}
  for name in required:self.write('cloud/dist/managed-home-provisioning/'+name,b'export {};\n')
  for name in ('cloud/dist/home-control-sessions.js','cloud/dist/project-authority/authority.js','cloud/dist/fabric-cas.js','dist/main.js'):self.write(name,b'export {};\n')
  for name in ('pg','ws','@aws-sdk/client-s3','@aws-sdk/credential-provider-web-identity'):
   self.write('cloud/node_modules/'+name+'/package.json',b'{"dependencies":{}}\n')
  self.archive=self.root/'source.tar'
  with tarfile.open(self.archive,'w') as t:
   for name in fixed:t.add(self.source/name,arcname=name,recursive=False)
  self.receipt={'sourceRevision':'a'*40,'sourceArchiveSha256':m.raw_row(self.archive.read_bytes())['sha256'],'generatedJs':{str(p.relative_to(self.source)):m.raw_row(p.read_bytes()) for base in (self.source/'dist',self.source/'cloud/dist') for p in base.rglob('*.js')}}
  self.receipt_path=self.root/'build-receipt.json'
  self.receipt_path.write_text(json.dumps(self.receipt))
 def write(self,name,raw):
  p=self.source/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(raw);p.chmod(0o644)
  return p
 def validate(self):return m.validate_capture(self.source,self.setup,self.receipt,self.archive)
 def test_complete_recipe_copies_actual_modules_and_keeps_permissions(self):
  node=b'\x7fELFsynthetic qualified Node';node_archive=self.root/'node.tar.gz'
  with tarfile.open(node_archive,'w:gz') as t:
   member=tarfile.TarInfo('node');member.size=len(node);t.addfile(member,io.BytesIO(node))
  old=(m.NODE_SHA,m.NODE_ARCHIVE_SHA)
  self.addCleanup(lambda:setattr(m,'NODE_SHA',old[0]))
  self.addCleanup(lambda:setattr(m,'NODE_ARCHIVE_SHA',old[1]))
  m.NODE_SHA=m.raw_row(node)['sha256'];m.NODE_ARCHIVE_SHA=m.raw_row(node_archive.read_bytes())['sha256']
  fly=self.root/'flyctl';fly.write_bytes(b'\x7fELFsynthetic Fly');fly.chmod(0o755)
  args=argparse.Namespace(source=self.source,setup=self.setup,archive=node_archive,output=self.root/'context',flyctl=fly,build_receipt=self.receipt_path,source_archive=self.archive,flyctl_sha256=m.raw_row(fly.read_bytes())['sha256'])
  result=m.prepare(args)
  manifest=json.loads((args.output/'payload/context-manifest.json').read_bytes())
  self.assertTrue(result['no_build_or_provider_action'])
  self.assertEqual(manifest['generated_js'],self.receipt['generatedJs'])
  self.assertEqual(manifest['captured_source']['sourceRevision'],'a'*40)
  self.assertIn('staging-positive-ingress-driver.js',manifest['genuine_ingress_packaging']['compiledModules'])
  self.assertFalse(manifest['genuine_ingress_packaging']['targetQualified'])
  for rel,expected in manifest['files'].items():
   p=args.output/'payload'/rel
   self.assertEqual(m.raw_row(p.read_bytes()),{k:expected[k] for k in ('sha256','bytes')})
   self.assertEqual(oct(p.stat().st_mode&0o777),expected['mode'])
  self.assertEqual((args.output/'payload/context-manifest.json').stat().st_mode&0o777,0o444)
  with self.assertRaises(AssertionError):m.prepare(args)
 def test_optional_genuine_build_check_is_disabled_by_default(self):
  docker=(ROOT/'examples/managed-runtime-image-staging/Dockerfile').read_text()
  self.assertIn('ARG RUN_GENUINE_INGRESS_TARGET_CHECK=false',docker)
  command=re.search(r'RUN if .*?; fi',docker,re.S).group(0).removeprefix('RUN ').replace('\\\n','')
  command=re.sub(r'/usr/local/bin/rudder-runtime-qualification-node [^;]+','exit 7',command)
  for selected,expected in [('false',0),('true',7),('invalid',1)]:
   result=subprocess.run(['/bin/sh','-c',command],env=dict(os.environ,RUN_GENUINE_INGRESS_TARGET_CHECK=selected),capture_output=True)
   self.assertEqual(result.returncode,expected)
 def test_capture_binds_all_modules_and_fixed_sources(self):
  binding,rows=self.validate()
  self.assertIn('staging-positive-ingress-driver.js',binding['compiledModules'])
  self.assertIn('examples/managed-runtime-image-staging/Dockerfile',rows)
  self.assertEqual(binding['qualifyRuntimeRoleSource'],rows[m.ENTRY_SOURCE_PATH])
 def test_missing_or_extra_generated_row_refuses(self):
  saved=dict(self.receipt['generatedJs'])
  del self.receipt['generatedJs']['dist/main.js']
  with self.assertRaises(AssertionError):self.validate()
  self.receipt['generatedJs']=saved
  self.receipt['generatedJs']['cloud/dist/not-built.js']=m.raw_row(b'x')
  with self.assertRaises(AssertionError):self.validate()
 def test_changed_generated_bytes_refuse(self):
  (self.source/'cloud/dist/managed-home-provisioning/staging-sdk-runtime-flow.js').write_bytes(b'changed')
  with self.assertRaises(AssertionError):self.validate()
 def test_changed_entry_or_setup_refuses(self):
  for name in (m.ENTRY_SOURCE_PATH,'examples/managed-runtime-image-staging/supervise.py'):
   p=self.source/name;raw=p.read_bytes();p.write_bytes(b'changed')
   with self.assertRaises(AssertionError):self.validate()
   p.write_bytes(raw)
 def test_foreign_setup_or_archive_digest_refuses(self):
  with self.assertRaises(AssertionError):m.validate_capture(self.source,self.root/'other-setup',self.receipt,self.archive)
  self.receipt['sourceArchiveSha256']='b'*64
  with self.assertRaises(AssertionError):self.validate()
 def test_unsafe_generated_file_or_row_refuses(self):
  p=self.source/'dist/main.js';raw=p.read_bytes();p.unlink();p.symlink_to(self.source/'cloud/dist/fabric-cas.js')
  with self.assertRaises(AssertionError):self.validate()
  p.unlink();p.write_bytes(raw);p.chmod(0o666)
  with self.assertRaises(AssertionError):self.validate()
  p.chmod(0o644);self.receipt['generatedJs']['dist/main.js']['bytes']=True
  with self.assertRaises(AssertionError):self.validate()

if __name__=='__main__':unittest.main()
