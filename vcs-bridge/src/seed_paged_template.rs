//! V2 verifier. Flattening bounded descriptor pages once is compatibility DATA;
//! it is not a per-agent scan or the flat V1 wire format.
use super::*;
impl VerifiedMetadataTemplate {
    fn open_paged_retained(store:&rudder_engine::store::LocalStore,seed:&ObjectRef,root:&Path,
        object_mount:&Path,require_readonly:bool,mut check:impl FnMut()->Result<()>)->Result<Self>{
        check()?;ensure!(seed.kind==ObjectKind::Directory&&store.retention()?.pins.values().any(|r|r==seed),"paged seed lacks actual retained directory authority");check()?;
        let(descriptor,identity,manifest)=read_paged_seed(store,seed,&mut check)?;
        ensure_directory(root)?;let root=root.canonicalize()?;let object_mount=object_mount.canonicalize()?;
        ensure!(object_mount==root.join("objects"),"paged objects escape original seed export");
        if require_readonly{require_readonly_mount(&root)?;require_readonly_mount(&object_mount)?;}
        let mut file=atomic::open_read(&root.join("descriptor.json"))?;
        ensure!(file.metadata()?.len()==identity.size_bytes,"paged exported descriptor length differs");
        let mut bytes=Vec::new();file.take((paged::V2_MAX_DESCRIPTOR_BYTES+1)as u64).read_to_end(&mut bytes)?;
        identity.verify(&bytes)?;
        // Hash the original shared tree ONCE before sharing the actual template.
        // The kernel RO mount and retained actual Root read owner protect it
        // afterward. No CoW child repeats this history read.
        for (base,expected) in [(root.join("metadata"),&manifest.metadata),(object_mount.clone(),&manifest.objects)]{
            let mut observed=BTreeMap::new();let mut pending=vec![(base,PathBuf::new())];let mut bytes=0u64;
            while let Some((base,rel))=pending.pop(){check()?;ensure!(rel.components().count()<=paged::V2_PATH_DEPTH,"paged export depth exceeds profile");
                let m=fs::symlink_metadata(&base)?;ensure!(m.is_dir()&&!m.file_type().is_symlink(),"paged exported namespace redirects");
                for e in fs::read_dir(&base)?{check()?;let e=e?;let p=e.path();let rel=rel.join(e.file_name());let m=fs::symlink_metadata(&p)?;
                    ensure!(!m.file_type().is_symlink()&&(m.is_dir()||m.is_file()),"paged export contains unsupported entry");
                    if m.is_dir(){pending.push((p,rel));continue}
                    ensure!(observed.len()<paged::V2_MAX_FILES&&m.len()<=paged::V2_MAX_FILE_BYTES,"paged export file bound exceeded");
                    bytes=bytes.checked_add(m.len()).context("paged exported bytes overflow")?;ensure!(bytes<=paged::V2_MAX_LOGICAL_BYTES,"paged exported bytes exceed profile");
                    let mut f=atomic::open_read(&p)?;let mut hash=Sha256::new();let mut count=0;let mut buffer=vec![0;64<<10];
                    loop{check()?;let n=f.read(&mut buffer)?;check()?;if n==0{break}count+=n as u64;ensure!(count<=m.len(),"paged exported file grew");hash.update(&buffer[..n]);}
                    ensure!(count==m.len(),"paged exported read incomplete");observed.insert(rel.to_str().context("paged path not UTF8")?.to_owned(),SeedFile{sha256:hex::encode(hash.finalize()),size_bytes:count});
                }
            }ensure!(&observed==expected,"paged exported bytes differ from pinned seed");
        }
        check()?;Ok(Self{root,object_mount,manifest:Arc::new(manifest),identity,require_readonly,retained_seed:Some(seed.clone()),paged:Some(descriptor)})
    }
    pub fn open_paged_retained_bounded(store:&rudder_engine::store::LocalStore,seed:&ObjectRef,root:&Path,objects:&Path,
        original:&rudder_engine::execution::finalization_budget::FinalizationBudget,cancelled:&std::sync::atomic::AtomicBool)->Result<Self>{
        Self::open_paged_retained(store,seed,root,objects,true,||{
            if cancelled.load(std::sync::atomic::Ordering::Acquire){original.cancel();anyhow::bail!("original paged verification cancelled");}original.remaining()?;Ok(())})
    }
    pub fn profile(&self)->SeedProfile {if self.paged.is_some(){SeedProfile::PagedCowV2}else{SeedProfile::SmallV1}}
    pub fn paged_descriptor(&self)->Option<&PagedSeedDescriptor>{self.paged.as_ref()}
}
