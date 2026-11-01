//! Actual retained verified-graph subset, not a JSON/from-ref SDK owner.
use super::*;
use crate::execution::compiler_sdk::CompilerSdkSemanticData;
pub struct CompilerSdkSemanticInputs {source:Arc<VerifiedBakedInputs>,data:Option<CompilerSdkSemanticData>,
    metadata:BTreeMap<ObjectRef,Vec<u8>>,ready:bool}
impl CompilerSdkSemanticInputs {
    pub fn capture_retaining(out:&mut Option<Self>,source:Arc<VerifiedBakedInputs>,catalog:&CompilerSdkCatalogData,original:&dyn ImageImportFence)->Result<()>{
        ensure!(out.is_none(),"original SDK semantic owner already occupied");*out=Some(Self{source,data:None,metadata:BTreeMap::new(),ready:false});let held=out.as_mut().unwrap();
        original.remaining()?;catalog.validate_data()?;held.source.current(original)?;let binding=held.source.binding_data()?;
        ensure!(binding.inventory==catalog.inventory&&binding.manifest==catalog.manifest&&binding.installer_input==catalog.installer_input,"SDK subset differs from original whole-catalogue owner");
        let reader=Metadata{bytes:&held.source.metadata,original};let(data,metadata)=derive_members(&reader,&binding.installer_input,|r|held.source.members.contains(r))?;
        held.data=Some(data);held.metadata=metadata;held.source.current(original)?;original.remaining()?;held.ready=true;Ok(())
    }
    pub fn data(&self)->Result<&CompilerSdkSemanticData>{ensure!(self.ready,"SDK original subset derivation incomplete");self.source.binding_data()?;self.data.as_ref().context("SDK original subset absent")}
    pub fn current(&self,original:&dyn ImageImportFence)->Result<()>{self.data()?.validate_data()?;self.source.current(original)}
    pub fn derived_metadata_data(&self)->Result<&BTreeMap<ObjectRef,Vec<u8>>>{self.data()?;Ok(&self.metadata)}
}
fn derive_members(reader:&impl DirectoryStore,root:&ObjectRef,contains:impl Fn(&ObjectRef)->bool)->Result<(CompilerSdkSemanticData,BTreeMap<ObjectRef,Vec<u8>>)>{
    let tree=PagedDirectory::new(reader);let prepared=tree.lookup(root,b"prepared")?.context("SDK executable prepared namespace absent")?;
    let directory::EntryNode::Directory{object:prepared}=prepared.node else{bail!("SDK prepared member is not an original directory")};
    let buck=tree.lookup(root,b"buck2")?.context("SDK static frontend absent")?;
    let directory::EntryNode::File{object:buck2,logical_size,mode}=buck.node else{bail!("SDK static frontend is not an original file")};
    ensure!(contains(&prepared)&&contains(&buck2),"SDK semantic member outside original verified graph");
    CompilerSdkSemanticData::from_members_data(prepared,buck2,logical_size,mode)
}
#[cfg(test)]
#[path="backed_compiler_semantics_tests.rs"]mod tests;
