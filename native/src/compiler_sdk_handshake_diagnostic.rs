//! Closed diagnostic DATA from ONLY the original failed startup helper's pipes.
//! No process/result/settlement authority is constructed by parsing stderr.
use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {kind:String,component:String,stage:String,class:String,io_kind:Option<String>,errno:Option<i32>,check:Option<String>,#[serde(default)]error_text:Option<String>}
fn helper_check(name:&str)->Option<BootstrapCheck>{Some(match name{
    "SdkHelperEntry"=>BootstrapCheck::SdkHelperEntry,
    "SdkHelperConnect"=>BootstrapCheck::SdkHelperConnect,
    "SdkHelperParent"=>BootstrapCheck::SdkHelperParent,
    "SdkHelperParentImage"=>BootstrapCheck::SdkHelperParentImage,
    "SdkHelperCapsule"=>BootstrapCheck::SdkHelperCapsule,
    "SdkHelperServiceImage"=>BootstrapCheck::SdkHelperServiceImage,
    "SdkHelperLauncher"=>BootstrapCheck::SdkHelperLauncher,
    "SdkHelperLauncherImage"=>BootstrapCheck::SdkHelperLauncherImage,
    "SdkHelperFloor"=>BootstrapCheck::SdkHelperFloor,
    "SdkHelperWorkspace"=>BootstrapCheck::SdkHelperWorkspace,
    "SdkHelperNamespaceWorkload"=>BootstrapCheck::SdkHelperNamespaceWorkload,
    "SdkHelperResultSend"=>BootstrapCheck::SdkHelperResultSend,
    _=>return None,
})}
fn kind_label(kind:io::ErrorKind)->&'static str{match kind{
    io::ErrorKind::PermissionDenied=>"permission_denied",io::ErrorKind::NotFound=>"not_found",io::ErrorKind::TimedOut=>"timed_out",
    io::ErrorKind::WouldBlock=>"would_block",io::ErrorKind::Interrupted=>"interrupted",io::ErrorKind::InvalidData=>"invalid_data",_=>"other",
}}
fn closed_kind(label:&str)->Option<io::ErrorKind>{Some(match label{
    "permission_denied"=>io::ErrorKind::PermissionDenied,"not_found"=>io::ErrorKind::NotFound,"timed_out"=>io::ErrorKind::TimedOut,
    "would_block"=>io::ErrorKind::WouldBlock,"interrupted"=>io::ErrorKind::Interrupted,"invalid_data"=>io::ErrorKind::InvalidData,"other"=>io::ErrorKind::Other,_=>return None,
})}
fn project(raw:&[u8])->Option<anyhow::Error>{
    if raw.is_empty()||raw.len()>65536||!raw.ends_with(b"\n"){return None}
    // FIRST row only: an inner stderr relay has its fixed DATA prefix first,
    // so arbitrary tool bytes cannot be searched/adopted as a helper marker.
    let mut lines=raw.split(|byte|*byte==b'\n');let first=lines.next()?;
    if first.len()>4096||lines.any(|line|line.iter().copied().skip_while(u8::is_ascii_whitespace).next()==Some(b'{')){return None}
    let frame:Frame=serde_json::from_slice(first).ok()?; // rejects duplicate/unknown fields
    let value:serde_json::Value=serde_json::from_slice(first).ok()?;
    let object=value.as_object()?;
    if ![7,8].contains(&object.len())||["kind","component","stage","class","io_kind","errno","check"].iter().any(|key|!object.contains_key(*key))
        ||frame.kind!="rudder-root-retained-failure-v1"||frame.component!="compiler_sdk_helper"||frame.stage!="handshake"{return None}
    let operation=match frame.error_text.as_deref(){
        Some(text)=>Some(SdkHelperOperation::from_text(text,frame.io_kind.as_deref(),frame.errno)?),
        None if object.contains_key("error_text")=>return None,
        None=>None,
    };
    let check=helper_check(frame.check.as_deref()?)?;
    let error=match(frame.class.as_str(),frame.io_kind.as_deref(),frame.errno){
        ("validation_or_other",None,None)=>Some(anyhow::Error::new(check)),
        ("io",Some(label),errno)=>{
            let kind=closed_kind(label)?;
            let os=match errno{Some(number)if(1..=4095).contains(&number)=>io::Error::from_raw_os_error(number),None=>io::Error::from(kind),_=>return None};
            if kind_label(os.kind())!=label{return None}Some(anyhow::Error::new(os).context(check))
        }
        _=>None,
    }?;
    Some(match operation{Some(operation)=>error.context(operation),None=>error})
}
pub(super) fn failed_original_stderr(raw:&[u8],startup:bool)->anyhow::Error{
    // Selected compiler stderr is arbitrary input, even with original custody.
    if startup{if let Some(error)=project(raw){return error}}
    anyhow::Error::new(BootstrapCheck::SdkHelperExitUnknown)
}
#[cfg(test)]
#[path="compiler_sdk_handshake_diagnostic_tests.rs"]
mod tests;
