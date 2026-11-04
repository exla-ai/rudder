//! Metadata-only SDK file binding on an already captured genuine baked graph.
//! This issues content DATA, never a Root, SDK-exec or ordinary LocalPin grant.
use super::*;
#[path="backed_compiler_semantics.rs"]mod semantic;
pub use semantic::CompilerSdkSemanticInputs;
use crate::execution::compiler_sdk::CompilerSdkCatalogData;
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct CompilerFileBindingData {pub content:ObjectRef,pub logical_size:u64,pub mode:u32}
fn parts(relative:&str)->Result<Vec<&[u8]>> {
    ensure!(!relative.is_empty()&&relative.len()<=1024&&!relative.contains('\0')&&!relative.starts_with('/')&&!relative.ends_with('/'),"SDK path must be a bounded relative member");
    let parts:Vec<_>=relative.split('/').collect();ensure!(parts.len()<=32&&parts.iter().all(|p|!p.is_empty()&&*p!="."&&*p!=".."&&p.len()<=255),"SDK relative path component invalid");
    Ok(parts.into_iter().map(str::as_bytes).collect())
}
impl VerifiedBakedInputs {
    pub fn compiler_file_content_data(&self,catalog:&CompilerSdkCatalogData,relative:&str,original:&dyn ImageImportFence)->Result<CompilerFileBindingData>{
        original.remaining()?;catalog.validate_data()?;let binding=self.binding_data()?;
        ensure!(binding.inventory==catalog.inventory&&binding.manifest==catalog.manifest&&binding.installer_input==catalog.installer_input,"SDK metadata lookup differs from actual original baked graph");
        let names=parts(relative)?;let reader=Metadata{bytes:&self.metadata,original};let tree=PagedDirectory::new(&reader);let mut directory=catalog.installer_input.clone();
        for(i,name)in names.iter().enumerate(){original.remaining()?;let entry=tree.lookup(&directory,name)?.context("SDK actual installer member absent")?;
            match entry.node {
                directory::EntryNode::Directory{object}if i+1<names.len()=>directory=object,
                directory::EntryNode::File{object,logical_size,mode}if i+1==names.len()=>{
                    ensure!(self.members.contains(&object)&&matches!(object.kind,ObjectKind::Blob|ObjectKind::File),"SDK actual file outside verified graph");original.remaining()?;return Ok(CompilerFileBindingData{content:object,logical_size,mode});
                }
                _=>bail!("SDK installer path has wrong type or symlink"),
            }
        }bail!("SDK actual file path empty")
    }
}
#[cfg(test)]mod tests {
    use super::*;
    #[test]fn sdk_metadata_lookup_refuses_absolute_traversal_empty_and_unbounded_paths(){
        assert_eq!(parts("prepared/tools/node").unwrap(),vec![b"prepared".as_slice(),b"tools".as_slice(),b"node".as_slice()]);
        for p in ["","/prepared/tools/node","prepared//node","prepared/../node","prepared/./node","prepared/node/","prepared/\0node"]{assert!(parts(p).is_err());}
        assert!(parts(&"a/".repeat(33)).is_err());assert!(parts(&"a".repeat(1025)).is_err());
    }
}
