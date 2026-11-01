//! Explicit provider-owner DATA. Deserialization alone creates no ownership.
use super::*;
#[derive(Clone,Debug,PartialEq,Eq,Deserialize)]#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct ProviderBinding {pub engine_instance_id:String,pub machine_boot_id:String,pub app_name:String,pub network_name:String,pub machine_id:String,pub instance_id:String,pub incarnation:String,pub store_id:String,pub config_sha256:String,pub machine_config_sha256:String}
#[derive(Clone,Debug,PartialEq,Eq,Deserialize)]#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct ProviderGrant {pub format_version:u32,pub ownership_mode:String,pub authority_id:String,pub account_id:String,pub project_id:String,pub owner_epoch:String,pub ownership_id:String,pub engine_instance_id:String,pub machine_boot_id:String,pub heartbeat_serial:String,pub request_id:String,pub provider:ProviderBinding}
impl ProviderGrant {
 pub(crate) fn validate_data(&self,r:&RegistrationAck)->Result<()>{for value in [&self.authority_id,&self.project_id,&self.ownership_id,&self.engine_instance_id]{uuid(value)?;}
  ensure!(self.format_version==2&&self.ownership_mode=="provider-fenced-v1"&&self.owner_epoch=="2"&&self.account_id==r.binding.account_id&&self.engine_instance_id==r.engine_instance_id&&self.machine_boot_id==r.boot_id,"provider owner original scope differs");
  rudder_engine::fleet_lease::Counter::parse(&self.heartbeat_serial)?;bounded_key(&self.request_id)?;let b=&r.binding;let p=&self.provider;
  ensure!(p.engine_instance_id==r.engine_instance_id&&p.machine_boot_id==r.boot_id&&p.app_name==b.app_name&&p.network_name==b.network_name&&p.machine_id==b.machine_id&&p.instance_id==b.instance_id
   &&p.incarnation==b.incarnation&&p.store_id==b.store_id&&p.config_sha256==b.config_sha256&&p.machine_config_sha256==b.machine_config_sha256,"provider original machine/Root binding differs");Ok(())
 }
 pub(crate) fn same_owner(&self,next:&Self)->bool{self.format_version==next.format_version&&self.ownership_mode==next.ownership_mode&&self.authority_id==next.authority_id&&self.account_id==next.account_id&&self.project_id==next.project_id&&self.owner_epoch==next.owner_epoch&&self.ownership_id==next.ownership_id&&self.engine_instance_id==next.engine_instance_id&&self.machine_boot_id==next.machine_boot_id&&self.provider==next.provider}
}
#[derive(Deserialize)]#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct ProviderAccepted {pub format_version:u32,pub authority_id:String,pub project_id:String,pub operation_id:String,pub sequence:String,pub kind:String,pub data:ProviderGrant}
impl ProviderAccepted {pub(super) fn validate_data(&self,r:&RegistrationAck)->Result<()>{self.data.validate_data(r)?;uuid(&self.operation_id)?;rudder_engine::fleet_lease::Counter::parse(&self.sequence)?;
 ensure!(self.format_version==1&&self.kind=="provider-owner-first-acquire-v1"&&self.authority_id==self.data.authority_id&&self.project_id==self.data.project_id,"provider original accepted receipt differs");Ok(())}}
#[cfg(test)]#[path="provider_owner_protocol_tests.rs"]mod tests;
