//! Bounded anonymous STREAM transport only. No source/lease authority from ACKs.
use super::*;
use super::super::super::host_ingress::root_upload::UploadScope;
use rudder_engine::protocol::ObjectRef;
use std::{io::{Read,Write},net::Shutdown,os::unix::net::UnixStream};

#[derive(Clone,Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingObjectsAck {#[serde(rename="type")]kind:String,pub format_version:u32,pub transfer_id:String,pub missing:Vec<ObjectRef>,pub preseeded:Vec<ObjectRef>}
#[path="image_batch_transfer.rs"]
mod image_batch_transfer;
enum Phase {Ready,Idle,Batch,Missing(Vec<ObjectRef>),Object {reference:ObjectRef,queued:u64},Finishing,Finished,Failed}
/// Actual stream received with the exact original Root BEGIN ACK. Non-Serde,
/// non-Clone; stage this owner before validation. Caller retains through error,
/// real worker/TLS join and Root's separate actual uploader-child settlement.
pub struct RootUploadTransfer {
    registration:Arc<RegisteredEngineOwner>,scope:UploadScope,begin:EngineOperationAck,original:OperationBudget,cancelled:Arc<AtomicBool>,stream:UnixStream,
    pending:Vec<u8>,sent:usize,input:Vec<u8>,phase:Phase,write_shutdown:bool,eof:bool,finished_frame:Option<serde_json::Value>,batch:Option<image_batch_transfer::BatchState>,batch_ordinal:u32,batch_objects:u32,batch_bytes:u64,batch_last_key:String,batch_ids:std::collections::BTreeSet<String>,
}
impl RootUploadTransfer {
    pub(super) fn from_original_retaining(out:&mut Option<Self>,registration:Arc<RegisteredEngineOwner>,scope:UploadScope,
        original:OperationBudget,cancelled:Arc<AtomicBool>,begin:EngineOperationAck,fd:OwnedFd)->Result<()> {
        ensure!(out.is_none(),"upload transfer output occupied");
        *out=Some(Self {registration,scope,begin,original,cancelled,stream:fd.into(),pending:Vec::new(),sent:0,input:Vec::new(),phase:Phase::Ready,write_shutdown:false,eof:false,finished_frame:None,batch:None,batch_ordinal:0,batch_objects:0,batch_bytes:0,batch_last_key:String::new(),batch_ids:std::collections::BTreeSet::new()});
        let s=out.as_ref().unwrap();s.scope.validate()?;s.stream.set_nonblocking(true)?;
        let mut kind=0i32;let mut kind_len=std::mem::size_of::<i32>() as libc::socklen_t;
        ensure!(unsafe {libc::getsockopt(s.stream.as_raw_fd(),libc::SOL_SOCKET,libc::SO_TYPE,(&mut kind as *mut i32).cast(),&mut kind_len)}==0&&kind==libc::SOCK_STREAM,"upload descriptor is not STREAM");
        let mut peer:libc::sockaddr_un=unsafe{std::mem::zeroed()};let mut len=std::mem::size_of_val(&peer) as libc::socklen_t;
        ensure!(unsafe{libc::getpeername(s.stream.as_raw_fd(),(&mut peer as *mut libc::sockaddr_un).cast(),&mut len)}==0
            &&peer.sun_family as i32==libc::AF_UNIX&&len as usize==std::mem::size_of::<libc::sa_family_t>(),"upload requires original anonymous STREAM pair");
        s.current()?;Ok(())
    }
    fn protect<T>(&mut self,work:impl FnOnce(&mut Self)->Result<T>)->Result<T> {let result=work(self);if result.is_err(){self.phase=Phase::Failed;}result}
    pub fn poll_write_retaining(&mut self)->Result<bool> {self.protect(|s|s.poll_write_retaining_inner())}
    pub fn poll_ready_retaining(&mut self)->Result<bool> {self.protect(|s|s.poll_ready_retaining_inner())}
    pub fn queue_missing_retaining(&mut self,objects:&[ObjectRef])->Result<()> {self.protect(|s|s.queue_missing_retaining_inner(objects))}
    pub fn poll_missing_retaining(&mut self,out:&mut Option<MissingObjectsAck>)->Result<bool> {self.protect(|s|s.poll_missing_retaining_inner(out))}
    pub fn queue_object_retaining(&mut self,reference:ObjectRef)->Result<()> {self.protect(|s|s.queue_object_retaining_inner(reference))}
    pub fn queue_bytes_retaining(&mut self,bytes:Vec<u8>)->Result<()> {self.protect(|s|s.queue_bytes_retaining_inner(bytes))}
    pub fn poll_object_retaining(&mut self)->Result<bool> {self.protect(|s|s.poll_object_retaining_inner())}
    pub fn queue_finish_retaining(&mut self)->Result<()> {self.protect(|s|s.queue_finish_retaining_inner())}
    pub fn poll_finished_retaining(&mut self)->Result<bool> {self.protect(|s|s.poll_finished_retaining_inner())}
    pub(super) fn begin_ack(&self)->&EngineOperationAck{&self.begin}
    pub fn assert_completed(&self)->Result<()> {self.current()?;ensure!(matches!(self.phase,Phase::Finished)&&self.eof,"upload stream not actually completed");Ok(())}
    pub fn scope_data(&self)->&UploadScope {&self.scope}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner> {&self.registration}
    fn current(&self)->Result<()> {ensure!(!matches!(self.phase,Phase::Failed),"upload transport retained after failure");
        if self.cancelled.load(Ordering::Acquire){self.original.cancel();return Err(fault());}self.original.remaining()?;self.registration.assert_current()}
    fn stage(&mut self,value:serde_json::Value)->Result<()> {
        ensure!(self.pending.is_empty(),"upload prior write still retained");let b=serde_json::to_vec(&value)?;ensure!(!b.is_empty()&&b.len()<=32768,"upload control frame bound");
        self.pending.extend_from_slice(&(b.len() as u32).to_be_bytes());self.pending.extend(b);self.sent=0;Ok(())
    }
    /// Partial writes remain in this owner. Call again only to continue those
    /// SAME bytes; a returned write error never licenses replay/replacement.
    fn poll_write_retaining_inner(&mut self)->Result<bool> {
        let result=(|| {self.current()?;if self.pending.is_empty(){return Ok(true);}
            let done=write_stream_step(&mut self.stream,&mut self.pending,&mut self.sent)?;self.current()?;Ok(done)})();
        if result.is_err(){self.phase=Phase::Failed;}result
    }
    fn receive(&mut self)->Result<Option<serde_json::Value>> {read_frame_step(&mut self.stream,&mut self.input,&mut self.eof)}

    fn poll_ready_retaining_inner(&mut self)->Result<bool> {self.current()?;ensure!(matches!(self.phase,Phase::Ready),"upload ready state differs");
        let Some(v)=self.receive()?else{ensure!(!self.eof,"upload closed before ready");return Ok(false);};
        ensure!(v==serde_json::json!({"type":"ready","format_version":1,"transfer_id":self.scope.transfer_id,"request_nonce":self.scope.request_nonce}),"upload ready binding differs");
        self.current()?;self.phase=Phase::Idle;Ok(true)
    }
    fn queue_missing_retaining_inner(&mut self,objects:&[ObjectRef])->Result<()> {self.current()?;ensure!(self.scope.version==1&&matches!(self.phase,Phase::Idle)&&!objects.is_empty()&&objects.len()<=128,"upload missing batch state/count invalid");
        let mut seen=std::collections::BTreeSet::new();for r in objects {valid_ref(r)?;ensure!(r.size_bytes<=self.scope.maximum_object_bytes||self.scope.mode=="image"&&self.scope.manifest.as_ref()==Some(r)&&r.size_bytes<=self.scope.maximum_manifest_bytes.unwrap_or(0),"missing reference exceeds original scope");ensure!(seen.insert(r),"upload missing batch duplicate");}
        self.stage(serde_json::json!({"type":"missing","objects":objects}))?;self.phase=Phase::Missing(objects.to_vec());Ok(())}
    fn poll_missing_retaining_inner(&mut self,out:&mut Option<MissingObjectsAck>)->Result<bool> {self.current()?;ensure!(out.is_none(),"upload missing result occupied");
        ensure!(matches!(self.phase,Phase::Missing(_)),"upload missing state differs");if !self.poll_write_retaining()?{return Ok(false);}let Some(v)=self.receive()?else{ensure!(!self.eof,"upload closed before missing ACK");return Ok(false);};
        let a:MissingObjectsAck=serde_json::from_value(v).map_err(|_|fault())?;let Phase::Missing(refs)=&self.phase else{unreachable!()};let mut seen=std::collections::BTreeSet::new();
        ensure!(a.kind=="missing_ack"&&a.format_version==1&&a.transfer_id==self.scope.transfer_id,"upload missing ACK scope differs");
        for r in a.missing.iter().chain(&a.preseeded){ensure!(refs.contains(r)&&seen.insert(r),"upload missing/preseed ACK not supplied/disjoint");}
        *out=Some(a);self.current()?;self.phase=Phase::Idle;Ok(true)
    }
    fn queue_object_retaining_inner(&mut self,reference:ObjectRef)->Result<()> {self.current()?;ensure!(self.scope.version==1&&matches!(self.phase,Phase::Idle),"upload object state differs");valid_ref(&reference)?;
        ensure!(reference.size_bytes<=self.scope.maximum_object_bytes||self.scope.mode=="image"&&self.scope.manifest.as_ref()==Some(&reference)&&reference.size_bytes<=self.scope.maximum_manifest_bytes.unwrap_or(0),"upload object exceeds scope");self.stage(serde_json::json!({"type":"object","object":reference}))?;
        self.phase=Phase::Object {reference,queued:0};Ok(())}
    /// Bytes are DATA. The BIN adapter calls the actual retained source reader;
    /// this method itself cannot attest a CAS/source or invent a verified read.
    fn queue_bytes_retaining_inner(&mut self,bytes:Vec<u8>)->Result<()> {self.current()?;ensure!(self.pending.is_empty()&&!bytes.is_empty()&&bytes.len()<=16384,"upload chunk bound/pending invalid");
        let Phase::Object {reference,queued}=&mut self.phase else{return Err(fault());};ensure!(*queued+bytes.len() as u64<=reference.size_bytes,"upload object overflow");
        *queued+=bytes.len() as u64;self.pending=bytes;self.sent=0;Ok(())}
    fn poll_object_retaining_inner(&mut self)->Result<bool> {self.current()?;if !self.poll_write_retaining()?{return Ok(false);}
        let Phase::Object {reference,queued}=&self.phase else{return Err(fault());};ensure!(*queued==reference.size_bytes,"upload object bytes incomplete");let expected=reference.clone();
        let Some(v)=self.receive()?else{ensure!(!self.eof,"upload closed before object ACK");return Ok(false);};
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Ack{#[serde(rename="type")]kind:String,format_version:u32,transfer_id:String,object:ObjectRef,stored:bool}
        let a:Ack=serde_json::from_value(v).map_err(|_|fault())?;let _actual_stored=a.stored;
        ensure!(a.kind=="object_ack"&&a.format_version==1&&a.transfer_id==self.scope.transfer_id&&a.object==expected,"upload object ACK differs");self.current()?;self.phase=Phase::Idle;Ok(true)
    }
    fn queue_finish_retaining_inner(&mut self)->Result<()> {self.current()?;ensure!(matches!(self.phase,Phase::Idle)&&(self.scope.version==1||self.batch_ordinal>0),"upload finish state differs");self.stage(serde_json::json!({"type":"finish"}))?;self.phase=Phase::Finishing;Ok(())}
    fn poll_finished_retaining_inner(&mut self)->Result<bool> {self.current()?;ensure!(matches!(self.phase,Phase::Finishing|Phase::Finished),"upload final state differs");
        if !self.poll_write_retaining()?{return Ok(false);}if !self.write_shutdown {self.write_shutdown=true;self.stream.shutdown(Shutdown::Write)?;}
        if self.finished_frame.is_none(){if let Some(v)=self.receive()?{self.finished_frame=Some(v);}}
        if let Some(v)=&self.finished_frame {ensure!(*v==serde_json::json!({"type":"finished","format_version":1,"transfer_id":self.scope.transfer_id,"request_nonce":self.scope.request_nonce,"http_settled":true,"source_eof":true}),"upload final receipt differs");
            if !self.eof {ensure!(self.receive()?.is_none(),"upload extra final frame");}if self.eof {self.current()?;self.phase=Phase::Finished;return Ok(true);}}
        else {ensure!(!self.eof,"upload EOF before final receipt");}Ok(false)
    }
    /// Read-only settlement facts after a failed clock/cancel. No sends, new
    /// budget, transfer permission or project/lease owner is created here.
    pub fn observe_settlement_retaining(&mut self)->Result<bool> {if let Some(v)=self.receive()?{if self.scope.version==2&&matches!(self.phase,Phase::Failed)&&self.batch.is_some(){self.retain_image_batch_late_data(v)?;}else{ensure!(self.finished_frame.is_none(),"upload duplicate late frame");self.finished_frame=Some(v);}}Ok(self.eof)}
}
fn valid_ref(r:&ObjectRef)->Result<()> {r.validate()?;ensure!(matches!(r.kind,rudder_engine::protocol::ObjectKind::Blob|rudder_engine::protocol::ObjectKind::File|rudder_engine::protocol::ObjectKind::Directory
    |rudder_engine::protocol::ObjectKind::DirectoryShard|rudder_engine::protocol::ObjectKind::SourceRoot|rudder_engine::protocol::ObjectKind::EnvironmentRoot)&&r.size_bytes<=33554432,"upload reference outside storage protocol");Ok(())}

// One actual nonblocking write. The caller owns original clock/registration
// checks and turns any error absorbing; this helper never replaces/replays bytes.
fn write_stream_step(stream:&mut UnixStream,pending:&mut Vec<u8>,sent:&mut usize)->Result<bool>{
    ensure!(*sent<=pending.len(),"upload partial write cursor invalid");if pending.is_empty(){return Ok(true);}
    match stream.write(&pending[*sent..]){Ok(0)=>return Err(fault()),Ok(n)=>*sent+=n,
        Err(e)if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::Interrupted)=>return Ok(false),Err(_)=>return Err(fault())}
    if *sent==pending.len(){pending.clear();*sent=0;}Ok(pending.is_empty())
}

fn read_frame_step(stream:&mut UnixStream,input:&mut Vec<u8>,eof:&mut bool)->Result<Option<serde_json::Value>> {
        if input.len()>=4 {let n=u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;ensure!(n>0&&n<=32768,"upload frame size invalid");
            if input.len()==4+n {let b=input.split_off(4);input.clear();return Ok(Some(serde_json::from_slice(&b).map_err(|_|fault())?));}}
        if *eof{return Ok(None);}let need=if input.len()<4 {4-input.len()}else{4+u32::from_be_bytes(input[..4].try_into().unwrap()) as usize-input.len()};
        let mut b=[0;4096];let limit=need.min(b.len());
        match stream.read(&mut b[..limit]) {Ok(0)=>{*eof=true;ensure!(input.is_empty(),"upload truncated frame at EOF");},Ok(n)=>input.extend_from_slice(&b[..n]),
            Err(e)if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::Interrupted)=>{},Err(_)=>return Err(fault())}Ok(None)
    }

#[cfg(test)]
mod frame_tests {
    use super::*;
    #[test]
    fn actual_stream_split_header_body_and_eof_are_distinct() {
        let (mut a,mut b)=UnixStream::pair().unwrap();a.set_nonblocking(true).unwrap();
        let value=serde_json::json!({"type":"x"});let raw=serde_json::to_vec(&value).unwrap();let h=(raw.len() as u32).to_be_bytes();
        let mut input=Vec::new();let mut eof=false;
        b.write_all(&h[..2]).unwrap();assert!(read_frame_step(&mut a,&mut input,&mut eof).unwrap().is_none());
        b.write_all(&h[2..]).unwrap();assert!(read_frame_step(&mut a,&mut input,&mut eof).unwrap().is_none());
        b.write_all(&raw).unwrap();assert!(read_frame_step(&mut a,&mut input,&mut eof).unwrap().is_none());
        assert_eq!(read_frame_step(&mut a,&mut input,&mut eof).unwrap(),Some(value));assert!(!eof);
        b.shutdown(Shutdown::Write).unwrap();assert!(read_frame_step(&mut a,&mut input,&mut eof).unwrap().is_none());assert!(eof);
    }
    #[test]
    fn truncated_and_oversized_stream_frames_fail_closed() {
        for bytes in [vec![0,0],(32769u32).to_be_bytes().to_vec()] {
            let (mut a,mut b)=UnixStream::pair().unwrap();a.set_nonblocking(true).unwrap();b.write_all(&bytes).unwrap();b.shutdown(Shutdown::Write).unwrap();
            let mut input=Vec::new();let mut eof=false;assert!(read_frame_step(&mut a,&mut input,&mut eof).unwrap().is_none());
            assert!(read_frame_step(&mut a,&mut input,&mut eof).is_err());
        }
    }
}
