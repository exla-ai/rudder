//! Executable SDK subset DATA. No Root/mount/pin authority constructor. The
//! actual issuer separately derives these members from its verified full graph.
use super::*;
use crate::directory::{DirectoryStore,DirectoryEntry,EntryNode,PagedDirectory};
use std::cell::RefCell;
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerSdkSemanticData {pub format_version:u32,pub prepared:ObjectRef,pub buck2:ObjectRef,
    pub buck2_logical_size:u64,pub buck2_mode:u32,pub directory:ObjectRef}
#[derive(Default)]struct Pages(RefCell<BTreeMap<ObjectRef,Vec<u8>>>);
impl DirectoryStore for Pages {
    fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{self.0.borrow().get(r).cloned().ok_or_else(||anyhow::anyhow!("SDK derived metadata absent"))}
    fn put(&self,kind:ObjectKind,bytes:&[u8])->Result<ObjectRef>{ensure!(matches!(kind,ObjectKind::Directory|ObjectKind::DirectoryShard)&&bytes.len()<=4096,"SDK derived metadata bound");let r=ObjectRef::from_bytes(kind,bytes);let mut map=self.0.borrow_mut();ensure!(map.len()<4,"SDK derived page count");map.insert(r.clone(),bytes.to_vec());Ok(r)}
}
impl CompilerSdkSemanticData {
    /// Pure canonical DATA construction, never evidence that refs belong to a
    /// baked catalogue. Only the actual retained metadata owner proves that.
    pub fn from_members_data(prepared:ObjectRef,buck2:ObjectRef,buck2_logical_size:u64,buck2_mode:u32)->Result<(Self,BTreeMap<ObjectRef,Vec<u8>>)>{
        prepared.validate()?;buck2.validate()?;ensure!(prepared.kind==ObjectKind::Directory&&prepared.size_bytes<=4096&&prepared.size_bytes>0,"SDK prepared member must be a directory");
        ensure!(matches!(buck2.kind,ObjectKind::Blob|ObjectKind::File)&&buck2_logical_size>0&&buck2_logical_size<=2*1024*1024*1024&&buck2_mode&!0o777==0&&buck2_mode&0o111!=0,"SDK static frontend member differs");
        let pages=Pages::default();let directory=PagedDirectory::new(&pages).build(0o555,vec![
            DirectoryEntry::new(b"buck2",EntryNode::File{object:buck2.clone(),logical_size:buck2_logical_size,mode:buck2_mode})?,
            DirectoryEntry::new(b"prepared",EntryNode::Directory{object:prepared.clone()})?])?;
        let data=Self{format_version:1,prepared,buck2,buck2_logical_size,buck2_mode,directory};Ok((data,pages.0.into_inner()))
    }
    pub fn validate_data(&self)->Result<()>{ensure!(self.format_version==1,"SDK semantic subset version differs");let(expected,_)=Self::from_members_data(self.prepared.clone(),self.buck2.clone(),self.buck2_logical_size,self.buck2_mode)?;ensure!(&expected==self,"SDK semantic directory differs from its exact members");Ok(())}
}

#[cfg(test)]mod tests{use super::*;
    #[test]fn semantic_directory_matches_independent_lossless_canonical_metadata_golden(){
        let prepared=ObjectRef{kind:ObjectKind::Directory,algorithm:crate::protocol::HashAlgorithm::Sha256,digest:"a".repeat(64),size_bytes:230};
        let buck2=ObjectRef{kind:ObjectKind::Blob,algorithm:crate::protocol::HashAlgorithm::Sha256,digest:"b".repeat(64),size_bytes:1024};
        let(data,pages)=CompilerSdkSemanticData::from_members_data(prepared,buck2,1024,0o555).unwrap();data.validate_data().unwrap();
        assert_eq!(data.directory.digest,"de3b8f0c240d6afa3d6c7f5b899f9f901d5c4d03e063ae5851f07985750e96ed");assert_eq!(data.directory.size_bytes,229);assert_eq!(pages.len(),2);
    }
}
