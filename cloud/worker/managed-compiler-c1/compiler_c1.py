#!/usr/bin/env python3
"""Separate C1 compiler DATA producer; never a Root/SDK execution grant.
Actual pinned A2 inputs plus complete declared C/C++ driver/sysroot closure.
Every extension effect uses the SAME A2300s Clock/2GiB/65536 Builder bounds.
"""
import argparse,hashlib,os,re,stat,struct,importlib.util,ssl
from pathlib import Path
_spec=importlib.util.spec_from_file_location('compiler_c1_base',Path(__file__).with_name('compiler_artifact.py'))
base=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(base)

PROFILE='rudder-linux-rust-cxx-typescript-c1'
ROOT=Path('/rudder-compiler-c1')
DRIVERS=('gcc','g++','cc','c++','cpp','as','ld','ld.bfd','ar','ranlib','nm','objcopy','objdump','readelf','strip','addr2line','make','pkg-config')
EXECUTABLES=tuple(p for p in base.REQUIRED_FILES if p!='prepared/tools/typescript/lib/tsc.js')+('prepared/tools/rust/bin/cargo','prepared/bin/sh')+tuple('prepared/usr/bin/'+name for name in DRIVERS)+(
 'prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1','prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1plus','prepared/usr/lib/gcc/x86_64-linux-gnu/12/collect2')
REQUIRED=base.REQUIRED_FILES+(
 'prepared/tools/rust/bin/cargo','prepared/usr/bin/gcc','prepared/usr/bin/g++',
 'prepared/usr/bin/as','prepared/usr/bin/ld','prepared/usr/bin/ar','prepared/usr/bin/readelf',
 'prepared/usr/bin/make','prepared/usr/bin/pkg-config','prepared/bin/sh',
 'prepared/usr/include/stdio.h','prepared/usr/include/stdlib.h',
 'prepared/etc/ssl/certs/ca-certificates.crt',
 'prepared/usr/lib/x86_64-linux-gnu/crt1.o','prepared/usr/lib/x86_64-linux-gnu/crti.o',
 'prepared/usr/lib/x86_64-linux-gnu/crtn.o','prepared/usr/lib/x86_64-linux-gnu/libc.so',
 'prepared/usr/include/c++/12/vector','prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1',
 'prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1plus','prepared/usr/lib/gcc/x86_64-linux-gnu/12/collect2',
 'prepared/usr/lib/gcc/x86_64-linux-gnu/12/crtbeginS.o','prepared/usr/lib/gcc/x86_64-linux-gnu/12/libgcc.a',
)

def validate_profile(manifest):
    base.validate_shape(manifest,PROFILE,REQUIRED)
    entries={entry['path']:entry for entry in manifest['entries']}
    base.need(all(entries.get(path,{}).get('kind')=='file' and entries[path]['mode']==0o555 for path in EXECUTABLES),'C1 executable closure mode/member mismatch')

def verify(root,clock,pin=None,uid=0,gid=0):
    result=base.verify(root,clock,pin,uid,gid,PROFILE,REQUIRED)
    manifest,_=base.read_json(root/'manifest.json');validate_profile(manifest);clock.remaining();return result

def public(path):
    path=Path(path)
    base.need(path.is_absolute() and '..' not in path.parts and len(path.parts)<=32,'C1 source path unsupported')
    # Public pinned source-image trees only. /etc/secrets/HOME/cache are absent.
    base.need(path.resolve(strict=True).is_relative_to(Path('/usr')),'C1 source escapes pinned public /usr')
    return path

def _driver_source(raw,clock,allowed,owner):
    # DATA resolver only. The production caller below fixes /usr/root0; tests
    # use an actual private filesystem fixture, never a runtime/path grant.
    clock.remaining()
    base.need(isinstance(raw,str) and 0<len(raw.encode())<=4096 and raw==raw.strip()
      and not raw.startswith('//') and not any(ord(c)<32 or ord(c)==127 for c in raw),'C1 driver path encoding/bound')
    source=Path(raw)
    base.need(source.is_absolute() and len(source.parts)<=32,'C1 driver path must be bounded absolute DATA')
    base.need(allowed.is_absolute() and allowed.resolve(strict=True)==allowed and source.is_relative_to(allowed),'C1 driver path not rooted in declared public source')
    canonical=source.resolve(strict=True)
    base.need(canonical.is_relative_to(allowed),'C1 canonical driver target escapes public source')
    before=canonical.lstat();base.need(stat.S_ISREG(before.st_mode) and (before.st_uid,before.st_gid)==owner and not before.st_mode&0o022,'C1 canonical driver file is not protected regular data')
    # Retain/check actual original directory versions throughout resolution.
    # Source fd/hash validation remains the existing Builder responsibility.
    parents=[]
    for parent in canonical.parents:
        clock.remaining();meta=parent.lstat()
        base.need(stat.S_ISDIR(meta.st_mode) and not meta.st_mode&0o022 and (meta.st_uid,meta.st_gid) in (owner,(0,0)),'C1 canonical driver ancestor is not protected')
        parents.append((parent,base.version(meta)))
        if parent==allowed:break
    base.need(parents and parents[-1][0]==allowed,'C1 canonical driver root absent')
    for parent,version in parents:clock.remaining();base.need(base.version(parent.lstat())==version,'C1 driver ancestor changed during resolution')
    base.need(base.version(canonical.lstat())==base.version(before) and source.resolve(strict=True)==canonical,'C1 driver target changed during resolution')
    clock.remaining();return canonical

def driver_source(raw,clock):return _driver_source(raw,clock,Path('/usr'),(0,0))

def record_driver_paths(args,values,clock):
    # Public compiler path DATA only, written before resolving the returned
    # name. Failed output stays in the fresh original build scratch, no cleanup.
    clock.remaining()
    for value in values.values():base.need(isinstance(value.get('raw'),str) and len(value['raw'].encode())<=4096 and not any(ord(c)<32 or ord(c)==127 for c in value['raw']),'C1 recorded driver path bound/encoding')
    raw=base.canonical({'format_version':1,'scope':'C1-fixed-compiler-driver-path-DATA-only','paths':values})
    base.need(len(raw)<=256*1024,'C1 driver path receipt bound')
    target=args.scratch/'c1-driver-paths.json';pending=args.scratch/'c1-driver-paths.pending'
    with pending.open('xb') as out:out.write(raw);out.flush();os.fsync(out.fileno())
    pending.chmod(0o444);os.replace(pending,target)
    # Same bounded public path DATA also reaches Root's retained PRIVATE build
    # log if a failed BuildKit layer cannot be exported. Never a success marker.
    name=next(reversed(values));print(base.json.dumps({'kind':'C1-driver-path-DATA','name':name,'value':values[name]},sort_keys=True),flush=True)
    clock.remaining()

def dynamic_elf(path,clock):
    clock.remaining();before=path.lstat()
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try:
        base.need(base.version(os.fstat(fd))==base.version(before),'C1 ELF changed at open')
        head=os.pread(fd,64,0)
        base.need(len(head)==64 and head[:6]==b'\x7fELF\x02\x01' and struct.unpack_from('<H',head,18)[0]==62,'C1 ELF must be actual little-endian x86_64')
        kind=struct.unpack_from('<H',head,16)[0]
        if kind==1:return False # Actual CRT/relocatable .o, never executable ldd input.
        base.need(kind in (2,3),'unsupported C1 ELF kind')
        offset=struct.unpack_from('<Q',head,32)[0];size,count=struct.unpack_from('<HH',head,54)
        base.need(size==56 and count<=128 and offset+size*count<=before.st_size,'C1 ELF program header bound')
        dynamic=False
        for i in range(count):
            clock.remaining();entry=os.pread(fd,size,offset+size*i);base.need(len(entry)==size,'C1 program header ended early')
            dynamic|=struct.unpack_from('<I',entry)[0]==2
        base.need(base.version(os.fstat(fd))==base.version(before)==base.version(path.lstat()),'C1 ELF changed during type capture')
        return dynamic
    finally:os.close(fd)

def public_certificates(builder,prepared,args,clock):
    # Only public Mozilla roots owned by the signed ca-certificates package.
    # Never read machine-local trust, private keys, /etc configuration or HOME.
    names=base.run(['/usr/bin/dpkg-query','-L','ca-certificates'],clock).splitlines()
    names=sorted(name for name in names if name.startswith('/usr/share/ca-certificates/mozilla/') and name.endswith('.crt'))
    base.need(0<len(names)<=512,'C1 public certificate member bound')
    bundle=bytearray()
    for name in names:
        clock.remaining();source=public(name);before=source.lstat()
        base.need(source.resolve(strict=True)==source and stat.S_ISREG(before.st_mode) and before.st_uid==0 and before.st_gid==0 and not before.st_mode&0o022 and before.st_size<=65536,'C1 public certificate not protected')
        fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
        with os.fdopen(fd,'rb') as stream:
            raw=stream.read(65537)
            base.need(base.version(os.fstat(stream.fileno()))==base.version(before)==base.version(source.lstat()),'C1 public certificate changed')
        base.need(len(raw)==before.st_size and raw.startswith(b'-----BEGIN CERTIFICATE-----\n') and raw.rstrip().endswith(b'-----END CERTIFICATE-----') and raw.count(b'-----BEGIN CERTIFICATE-----')==1,'C1 public certificate encoding')
        ssl.PEM_cert_to_DER_cert(raw.decode('ascii'))
        bundle.extend(raw.rstrip()+b'\n');base.need(len(bundle)<=2*1024*1024,'C1 public certificate bundle bound')
    path=args.scratch/'public-ca-certificates.crt'
    with path.open('xb') as stream:stream.write(bundle);stream.flush();os.fsync(stream.fileno())
    path.chmod(0o444);builder.copy_file(path,prepared/'etc/ssl/certs/ca-certificates.crt',args.scratch)
    return {'source_package':'ca-certificates','members':len(names),'sha256':hashlib.sha256(bundle).hexdigest()}

def collect(builder,prepared,sysroot,args,clock):
    policy,policy_raw=base.read_json(args.policy/'c1-policy.json',65536)
    base.need(set(policy)=={'format_version','profile_id','target','packages','public_package_prefixes','drivers','print_files'} and policy['format_version']==1 and policy['profile_id']==PROFILE and policy['target']=='x86_64-linux-gnu','C1 policy schema/profile')
    base.need(policy['drivers']==list(DRIVERS),'C1 fixed driver set differs')
    base.need(len(policy['packages'])<=48 and len(set(policy['packages']))==len(policy['packages']),'C1 package bound/duplicate')
    versions={};selected=[]
    certificates=public_certificates(builder,prepared,args,clock)
    # Complete public headers from this fixed source image; no host SDK include
    # discovery or private package manager cache enters the artifact.
    builder.copy_tree(Path('/usr/include'),prepared/'usr/include',Path('/usr'))
    for package in policy['packages']:
        clock.remaining();base.need(re.fullmatch('[a-z0-9][a-z0-9+.-]{0,63}',package),'C1 package name')
        version=base.run(['/usr/bin/dpkg-query','-W','-f=${Status}\t${Version}',package],clock)
        status,actual_version=version.split('\t',1);base.need(status=='install ok installed','required C1 package is not installed')
        versions[package]=actual_version
        names=base.run(['/usr/bin/dpkg-query','-L',package],clock).splitlines();base.need(len(names)<=16384,'C1 package path count')
        for name in names:
            clock.remaining()
            if not any(name.startswith(prefix) for prefix in policy['public_package_prefixes']):continue
            source=public(name)
            if source.is_dir():continue
            resolved=source.resolve(strict=True);base.need(resolved.is_file(),'C1 package special payload')
            # Already copied the complete original header tree once.
            if name.startswith('/usr/include/'):continue
            builder.copy_file(source,prepared/name.lstrip('/'),Path('/usr'))
            if resolved!=source and any(str(resolved).startswith(prefix) for prefix in policy['public_package_prefixes']):
                builder.copy_file(resolved,prepared/str(resolved).lstrip('/'),Path('/usr'))
        selected.append(package)
    for name in policy['drivers']:
        base.need(re.fullmatch('[a-z0-9+.-]{1,64}',name),'C1 driver component')
        source=public('/usr/bin/'+name);builder.copy_file(source,prepared/'usr/bin'/name,Path('/usr'))
    builder.copy_file(public('/bin/sh'),prepared/'bin/sh',Path('/usr'))
    gcc=Path('/usr/bin/gcc')
    base.need(base.run([str(gcc),'-dumpmachine'],clock)=='x86_64-linux-gnu','actual C1 GCC target differs')
    base.need(base.run([str(gcc),'-print-sysroot'],clock) in ('','/'),'unsupported alternate GCC sysroot')
    print_files={}
    raw=base.run([str(gcc),'-print-libgcc-file-name'],clock);print_files['libgcc-internal-root']={'raw':raw};record_driver_paths(args,print_files,clock)
    libgcc=driver_source(raw,clock);gcc_root=libgcc.parent;print_files['libgcc-internal-root']['canonical']=str(libgcc);record_driver_paths(args,print_files,clock)
    base.need(gcc_root.is_relative_to(Path('/usr/lib/gcc/x86_64-linux-gnu')),'C1 GCC internal root differs')
    builder.copy_tree(gcc_root,prepared/str(gcc_root).lstrip('/'),Path('/usr'))
    for name in policy['print_files']:
        base.need(re.fullmatch('[A-Za-z0-9_.+-]{1,64}',name),'C1 printed file component')
        raw=base.run([str(gcc),'-print-file-name='+name],clock);print_files[name]={'raw':raw};record_driver_paths(args,print_files,clock)
        source=driver_source(raw,clock);print_files[name]['canonical']=str(source);record_driver_paths(args,print_files,clock)
        # Canonical guest path has no traversal. Existing package collection
        # separately materializes real linker aliases at their declared names.
        builder.copy_file(source,prepared/str(source).lstrip('/'),Path('/usr'))
    cargo=sysroot/'bin/cargo';builder.copy_file(cargo,prepared/'tools/rust/bin/cargo',sysroot)
    cargo_version=base.run([str(cargo),'--version'],clock);base.need(cargo_version.startswith('cargo '),'actual C1 Cargo version missing')
    builder.copy_file(args.policy/'c1-policy.json',args.output/'inputs/c1-policy.json',args.policy)
    builder.copy_file(args.scratch/'c1-driver-paths.json',args.output/'inputs/c1-driver-paths.json',args.scratch)
    builder.copy_file(Path(__file__),args.output/'c1-construction.py',Path(__file__).parent);(args.output/'c1-construction.py').chmod(0o444)
    for path in sorted((Path(__file__).parent/'workloads').iterdir()):
        base.need(path.is_file() and not path.is_symlink(),'C1 workload fixture must be fixed file')
        builder.copy_file(path,args.output/'workloads'/path.name,path.parent)
    # A2's ldd closure remains authoritative for actual dynamic ELF seeds.
    # CRT ET_REL objects and static-musl executables must never be run by ldd.
    builder.elf={p for p in builder.elf if dynamic_elf(p,clock)}
    dynamic_seeds=sorted(str(p) for p in builder.elf)
    return {'format_version':1,'profile_id':PROFILE,'c1_policy_sha256':hashlib.sha256(policy_raw).hexdigest(),'recipe_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'public_certificates':certificates,
      'actual_packages':versions,'actual_gcc_version':base.run([str(gcc),'--version'],clock).splitlines()[0],
      'actual_cargo_version':cargo_version,'gcc_internal_root':str(gcc_root),'driver_resolved_files':print_files,'dynamic_elf_seeds':dynamic_seeds,
      'cargo_dependency_cache_included':False,'repository_or_credentials_included':False,'execution_qualified':False}

def main():
    parser=argparse.ArgumentParser();commands=parser.add_subparsers(dest='command',required=True)
    build=commands.add_parser('build')
    for name in ('output','scratch','policy','node','prerequisites'):build.add_argument('--'+name,type=Path,required=True)
    build.add_argument('--rust-source-image',required=True);build.add_argument('--node-source-image',required=True);build.add_argument('--normalize-pinned-rust',action='store_true')
    verify_parser=commands.add_parser('verify');verify_parser.add_argument('--root',type=Path,required=True);verify_parser.add_argument('--expected-manifest-sha256')
    args=parser.parse_args();clock=base.Clock()
    if args.command=='build':
        base.need(args.output==Path('/out/rudder-compiler-c1'),'C1 fixed fresh output required')
        result=base.build(args,clock,extension=collect,profile=PROFILE,required_files=REQUIRED)
        manifest,_=base.read_json(args.output/'manifest.json');validate_profile(manifest);clock.remaining()
    else:
        base.need(args.root in (ROOT,Path('/out/rudder-compiler-c1')),'C1 fixed artifact verification root required')
        result=verify(args.root,clock,args.expected_manifest_sha256)
    print(base.json.dumps({**result,'scope':'compiler-C1-DATA-only','execution_qualified':False,'sdk_grant':False},sort_keys=True))
if __name__=='__main__':main()
