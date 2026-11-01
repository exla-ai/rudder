//! Fixed eight-frame V2 local seed transfer. This is independent from the
//! Cloud/image uploader lane. No new clocks, uncertain replay or hidden queue.
use super::*;
use std::collections::VecDeque;
const WINDOW:usize=8;
const INVENTORY_BATCH:usize=16; // encoded typed header stays below 4092 bytes.
enum Expected{Inventory(Vec<ObjectRef>),Chunk}
struct Frame{serial:u64,bytes:Vec<u8>,expected:Expected,sent:bool,packet:Option<service::Packet>}
struct CurrentObject{reference:ObjectRef,bytes:Vec<u8>,offset:usize,empty_sent:bool}
/// Stored in the actual ClientSeedOwner BEFORE the first window message.
/// Unknown frames keep exact bytes/serial/received packets and the source pin.
pub(super) struct SeedPagedTransfer{
    slot:uuid::Uuid,references:Vec<ObjectRef>,inventory_at:usize,missing:VecDeque<ObjectRef>,
    frames:VecDeque<Frame>,current:Option<CurrentObject>,completed:u64,failed:bool,finished:bool,
}
impl SeedPagedTransfer{
    fn capture(slot:uuid::Uuid,references:Vec<ObjectRef>)->Result<Self>{ensure!(references.len()<=rudder_vcs_bridge::seed::paged::V2_MAX_REFS,"V2 transfer ref bound exceeded");
        Ok(Self{slot,references,inventory_at:0,missing:VecDeque::new(),frames:VecDeque::with_capacity(WINDOW),current:None,completed:0,failed:false,finished:false})}
    fn enqueue(&mut self,client:&mut RootStorageClient,request:Request,payload:&[u8],expected:Expected,original:&FinalizationBudget)->Result<()> {
        ensure!(self.frames.len()<WINDOW,"V2 frame window full");let serial=client.serial.checked_add(1).context("V2 window serial exhausted")?;
        let bytes=service::encode(&Envelope{version:1,serial,constraint:original.constraint(),request},payload)?;
        self.frames.push_back(Frame{serial,bytes,expected,sent:false,packet:None});client.serial=serial;Ok(())
    }
    fn time(owner:&ClientSeedOwner,original:&FinalizationBudget)->Result<()>{if owner.original_cancel.as_ref().is_some_and(|c|c.load(Ordering::Acquire)){
        service::cancel_event(&owner.cancel);original.cancel();anyhow::bail!("V2 original source transfer cancelled");}original.remaining()?;Ok(())}
}
impl RootStorageClient {
    pub(super) fn transfer_paged_seed_retaining(&mut self,owner:&mut ClientSeedOwner,references:Vec<ObjectRef>,original:&FinalizationBudget)->Result<()> {
        ensure!(owner.paged_transfer.is_none()&&self.seed_window_inflight.is_none(),"original V2 transfer already started/unresolved");
        let slot=owner.slot.context("original V2 Root slot absent")?;SeedPagedTransfer::time(owner,original)?;
        owner.paged_transfer=Some(SeedPagedTransfer::capture(slot,references)?);self.seed_window_inflight=Some(slot);
        let result=(||{
            loop{
                SeedPagedTransfer::time(owner,original)?;self.current()?;owner.source.check()?;
                // All inventory messages are bounded and pipelined. Data is
                // sent only after actual Root missing-object acknowledgements.
                {let transfer=owner.paged_transfer.as_mut().unwrap();
                    while transfer.inventory_at<transfer.references.len()&&transfer.frames.len()<WINDOW{
                        let end=(transfer.inventory_at+INVENTORY_BATCH).min(transfer.references.len());
                        let refs=transfer.references[transfer.inventory_at..end].to_vec();
                        transfer.enqueue(self,Request::PagedInventory{slot,references:refs.clone()},&[],Expected::Inventory(refs),original)?;
                        transfer.inventory_at=end;
                    }
                }
                let inventory_done={let t=owner.paged_transfer.as_ref().unwrap();t.inventory_at==t.references.len()&&!t.frames.iter().any(|f|matches!(f.expected,Expected::Inventory(_)))};
                if inventory_done {
                    while owner.paged_transfer.as_ref().unwrap().frames.len()<WINDOW{
                        let next={let t=owner.paged_transfer.as_mut().unwrap();if t.current.is_none(){t.missing.pop_front()}else{None}};
                        if let Some(reference)=next{SeedPagedTransfer::time(owner,original)?;owner.source.check()?;
                            let bytes=owner.source.store.read_object(&reference)?;reference.verify(&bytes)?;SeedPagedTransfer::time(owner,original)?;
                            owner.paged_transfer.as_mut().unwrap().current=Some(CurrentObject{reference,bytes,offset:0,empty_sent:false});}
                        let t=owner.paged_transfer.as_mut().unwrap();let Some(object)=t.current.as_mut()else{break};
                        let offset=object.offset;let end=(offset+CHUNK).min(object.bytes.len());let payload=object.bytes[offset..end].to_vec();
                        let reference=object.reference.clone();object.offset=end;let done=end==object.bytes.len();object.empty_sent=true;
                        t.enqueue(self,Request::PagedChunk{slot,reference,offset},&payload,Expected::Chunk,original)?;
                        if done{t.current=None;}
                    }
                }
                let mut progressed=false;
                {let t=owner.paged_transfer.as_mut().unwrap();for frame in &mut t.frames{
                    if frame.sent{continue}self.current()?;original.remaining()?;
                    // Error after attempted send fences this SAME carrier. No
                    // call retries an unknown send or allocates a replacement.
                    if self.endpoint.send(&frame.bytes,None)?{frame.sent=true;progressed=true}else{break}
                }}
                for _ in 0..WINDOW {
                    let t=owner.paged_transfer.as_mut().unwrap();let Some(frame)=t.frames.front_mut()else{break};if !frame.sent{break}
                    if frame.packet.is_none(){frame.packet=self.endpoint.receive()?;}
                    let Some(packet)=&frame.packet else{break};
                    let retained=self.private_v2_recovery.take();
                    ensure!(packet.fds.is_empty()&&packet.credentials.pid==self.server.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,"V2 frame reply is not original Root");
                    let(answer,payload):(Answer,_)=service::decode(&packet.bytes)?;
                    ensure!(answer.version==1&&answer.serial==frame.serial&&payload.is_empty(),"V2 original frame reply order differs");
                    match (&frame.expected,answer.reply){
                        (Expected::Inventory(refs),Reply::PagedMissing{slot:actual,missing})=>{
                            ensure!(actual==slot&&missing.len()<=refs.len(),"V2 missing acknowledgement exceeds original inventory");
                            let mut last=None;for index in missing{ensure!(index<refs.len()&&last.is_none_or(|old|old<index),"V2 missing indices duplicate/unordered");
                                ensure!(t.missing.len()<t.references.len(),"V2 missing queue exceeds original closure");t.missing.push_back(refs[index].clone());last=Some(index);}
                        }
                        (Expected::Chunk,Reply::Accepted)=>{},_=>anyhow::bail!("V2 original frame not acknowledged; carrier retained"),
                    }
                    // Only actually acknowledged frames leave the bounded
                    // window; source/whole-transfer custody remains charged.
                    t.frames.pop_front();t.completed+=1;self.private_v2_recovery=retained;progressed=true;
                }
                let done={let t=owner.paged_transfer.as_ref().unwrap();t.inventory_at==t.references.len()&&t.frames.is_empty()&&t.missing.is_empty()&&t.current.is_none()};
                if done{owner.paged_transfer.as_mut().unwrap().finished=true;self.seed_window_inflight=None;SeedPagedTransfer::time(owner,original)?;return Ok(())}
                if !progressed{std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
            }
        })();if result.is_err(){owner.paged_transfer.as_mut().unwrap().failed=true;self.fenced=true;}result
    }
    /// Bounded read-only late frame observation; it does not resume transfers,
    /// read more source data, resend messages, issue Ready or renew the clock.
    pub fn observe_paged_seed_window_retaining(&mut self,owner:&mut ClientSeedOwner)->Result<usize>{
        self.current_physical()?;let t=owner.paged_transfer.as_mut().context("original V2 frame window absent")?;
        ensure!(self.seed_window_inflight==Some(t.slot),"another original window owns channel");let mut known=0;
        for frame in &mut t.frames{if !frame.sent{break}if frame.packet.is_none(){frame.packet=self.endpoint.receive()?;}
            if frame.packet.is_none(){break}known+=1;}
        Ok(known)
    }
}
