//! V2 image DATA framing. Only an original registered Root ACK/FD constructs
//! RootUploadTransfer. These headers/ACKs never create source/lease authority.
use super::*;
use rudder_engine::protocol::canonical_json;
#[derive(Clone,Debug,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub(super) struct Header {format_version:u32,transfer_id:String,request_nonce:String,batch_id:String,ordinal:u32,objects:Vec<ObjectRef>,total_bytes:u64}
pub(super) struct BatchState {header:Header,wire:Vec<u8>,sha:String,header_queued:usize,index:usize,queued:u64,observed:Option<serde_json::Value>,ack:Option<serde_json::Value>,unknown:Option<serde_json::Value>}
pub(super) fn key(r:&ObjectRef)->String {let value=serde_json::to_value(r).expect("reference DATA serializes");format!("{}/{}/{}/{}",value["kind"].as_str().unwrap(),value["algorithm"].as_str().unwrap(),r.digest,r.size_bytes)}
fn header(scope:&UploadScope,objects:Vec<ObjectRef>,ordinal:u32,batch_id:String)->Result<BatchState>{
    ensure!(scope.version==2&&scope.mode=="image_batch_v1","batch requires explicit original V2 scope");uuid(&batch_id)?;
    ensure!(!objects.is_empty()&&objects.len()<=1024&&ordinal<100000,"image batch count/ordinal invalid");
    let mut total=0u64;let mut last=String::new();
    for r in &objects {valid_ref(r)?;let next=key(r);ensure!(last.as_bytes()<next.as_bytes(),"batch references not raw UTF8 ordered/unique");last=next;
        ensure!(r.size_bytes<=4194304||scope.manifest.as_ref()==Some(r)&&r.size_bytes<=33554432&&objects.len()==1,"batch ordinary/manifest bound differs");
        total=total.checked_add(r.size_bytes).context("batch byte overflow")?;ensure!(total<=33554432,"batch byte quota");}
    let h=Header{format_version:1,transfer_id:scope.transfer_id.clone(),request_nonce:scope.request_nonce.clone(),batch_id,ordinal,objects,total_bytes:total};
    let wire=canonical_json(&h)?;ensure!(!wire.is_empty()&&wire.len()<=262144,"batch header exceeds bound");let sha=hex(&Sha256::digest(&wire));
    Ok(BatchState{header:h,wire,sha,header_queued:0,index:0,queued:0,observed:None,ack:None,unknown:None})
}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct Ack{format_version:u32,transfer_id:String,request_nonce:String,batch_id:String,ordinal:u32,header_sha256:String,object_count:usize,total_bytes:u64,stored_bitmap:String}
fn ack(value:serde_json::Value,b:&BatchState)->Result<()> {
    let a:Ack=serde_json::from_value(value).map_err(|_|fault())?;let h=&b.header;
    ensure!(a.format_version==1&&a.transfer_id==h.transfer_id&&a.request_nonce==h.request_nonce&&a.batch_id==h.batch_id&&a.ordinal==h.ordinal&&a.header_sha256==b.sha&&a.object_count==h.objects.len()&&a.total_bytes==h.total_bytes,"batch ACK original identity/hash/count differs");
    ensure!(a.stored_bitmap.len()==a.object_count.div_ceil(8)*2&&a.stored_bitmap.bytes().all(|v|v.is_ascii_digit()||(b'a'..=b'f').contains(&v)),"batch bitmap encoding invalid");
    if a.object_count%8!=0 {let raw=a.stored_bitmap.as_bytes();let last=u8::from_str_radix(std::str::from_utf8(&raw[raw.len()-2..])?,16)?;ensure!(last>>(a.object_count%8)==0,"batch bitmap high padding bits nonzero");}Ok(())
}
fn reconciliation(value:&serde_json::Value,b:&BatchState)->Result<()> {
    if value.is_null(){return Ok(());}let scope=value.get("scope").and_then(|v|v.as_str()).context("reconciliation scope absent")?;
    if scope=="stored-original-image-batch-ack-data-only" {
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Stored{format_version:u32,scope:String,ack:serde_json::Value}
        let d:Stored=serde_json::from_value(value.clone()).map_err(|_|fault())?;ensure!(d.format_version==1&&d.scope==scope,"reconciliation format differs");ack(d.ack,b)
    }else{
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Unknown{format_version:u32,scope:String,transfer_id:String,batch_id:String,header_sha256:String,actual_known_objects:Option<u32>,actual_unsettled_objects:Option<u32>}
        ensure!(value.as_object().is_some_and(|v|v.len()==7&&v.contains_key("actual_known_objects")&&v.contains_key("actual_unsettled_objects")),"reconciliation explicit unknown counts absent");
        let d:Unknown=serde_json::from_value(value.clone()).map_err(|_|fault())?;ensure!(d.format_version==1&&d.scope=="image-batch-unknown-data-only"&&d.transfer_id==b.header.transfer_id&&d.batch_id==b.header.batch_id&&d.header_sha256==b.sha,"unknown reconciliation identity differs");
        ensure!(d.actual_known_objects.is_none_or(|n|n as usize<=b.header.objects.len())&&d.actual_unsettled_objects.is_none_or(|n|n as usize<=b.header.objects.len()),"unknown reconciliation counts exceed original batch");Ok(())
    }
}
fn unknown(value:&serde_json::Value,b:&BatchState)->Result<()> {
    #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Unknown{#[serde(rename="type")]kind:String,format_version:u32,transfer_id:String,request_nonce:String,batch_id:String,header_sha256:String,reconciliation:serde_json::Value}
    let d:Unknown=serde_json::from_value(value.clone()).map_err(|_|fault())?;
    ensure!(d.kind=="batch_unknown"&&d.format_version==1&&d.transfer_id==b.header.transfer_id&&d.request_nonce==b.header.request_nonce&&d.batch_id==b.header.batch_id&&d.header_sha256==b.sha,"batch unknown original identity differs");reconciliation(&d.reconciliation,b)
}
impl RootUploadTransfer {
    /// Pure DATA ordering key; callers still read bytes from their actual export.
    pub fn image_batch_reference_key(reference:&ObjectRef)->Result<String>{valid_ref(reference)?;Ok(key(reference))}
    pub fn queue_image_batch_retaining(&mut self,objects:Vec<ObjectRef>,ordinal:u32,batch_id:String)->Result<()> {self.protect(|s|{
        s.current()?;ensure!(matches!(s.phase,Phase::Idle)&&s.batch.as_ref().is_none_or(|b|b.ack.is_some()),"prior batch remains owned");
        ensure!(ordinal==s.batch_ordinal&&!s.batch_ids.contains(&batch_id),"batch ordinal/ID reused");
        let b=header(&s.scope,objects,ordinal,batch_id)?;
        ensure!(s.batch_last_key.as_bytes()<key(&b.header.objects[0]).as_bytes()&&s.batch_objects+u32::try_from(b.header.objects.len())?<=s.scope.maximum_objects&&s.batch_bytes.checked_add(b.header.total_bytes).is_some_and(|n|n<=s.scope.maximum_bytes),"batch whole-transfer ordering/quota differs");
        // Stage original ID/hash/header BEFORE any control/data queue mutation.
        s.batch=Some(b);let b=s.batch.as_ref().unwrap();let control=serde_json::json!({"type":"batch","header_length":b.wire.len(),"header_sha256":b.sha});
        s.stage(control)?;s.phase=Phase::Batch;Ok(())
    })}
    pub fn poll_image_batch_header_retaining(&mut self)->Result<bool>{self.protect(|s|{
        s.current()?;ensure!(matches!(s.phase,Phase::Batch),"batch header phase differs");if !s.poll_write_retaining()?{return Ok(false);}
        let b=s.batch.as_mut().context("original batch absent")?;if b.header_queued==b.wire.len(){return Ok(true);}
        let end=(b.header_queued+16384).min(b.wire.len());s.pending=b.wire[b.header_queued..end].to_vec();s.sent=0;b.header_queued=end;s.current()?;Ok(false)
    })}
    pub fn queue_image_batch_bytes_retaining(&mut self,bytes:Vec<u8>)->Result<()> {self.protect(|s|{
        s.current()?;ensure!(matches!(s.phase,Phase::Batch)&&s.pending.is_empty()&&!bytes.is_empty()&&bytes.len()<=16384,"batch bytes bound/phase/pending differs");
        let b=s.batch.as_mut().context("original batch absent")?;ensure!(b.header_queued==b.wire.len(),"batch header incomplete");
        while b.index<b.header.objects.len()&&b.header.objects[b.index].size_bytes==0{b.index+=1;}
        let r=b.header.objects.get(b.index).context("batch object absent")?;ensure!(b.queued+bytes.len() as u64<=r.size_bytes,"batch object overflow");
        b.queued+=bytes.len() as u64;if b.queued==r.size_bytes{b.index+=1;b.queued=0;}s.pending=bytes;s.sent=0;s.current()?;Ok(())
    })}
    pub fn poll_image_batch_ack_retaining(&mut self)->Result<bool>{self.protect(|s|{
        s.current()?;ensure!(matches!(s.phase,Phase::Batch),"batch ACK phase differs");if !s.poll_write_retaining()?{return Ok(false);}
        {let b=s.batch.as_mut().context("original batch absent")?;while b.index<b.header.objects.len()&&b.header.objects[b.index].size_bytes==0{b.index+=1;}
        ensure!(b.header_queued==b.wire.len()&&b.index==b.header.objects.len()&&b.queued==0,"batch bytes incomplete");}
        let Some(v)=s.receive()?else{ensure!(!s.eof,"batch EOF before ACK");return Ok(false);};
        // Every observed reply is retained outside Phase before validation.
        s.batch.as_mut().unwrap().observed=Some(v.clone());
        if v.get("type").and_then(|v|v.as_str())==Some("batch_unknown"){let b=s.batch.as_ref().unwrap();unknown(&v,b)?;s.batch.as_mut().unwrap().unknown=Some(v);return Err(fault());}
        let mut body=v.clone();let kind=body.as_object_mut().context("batch ACK not object")?.remove("type");ensure!(kind==Some(serde_json::json!("batch_ack")),"batch reply kind differs");ack(body,s.batch.as_ref().unwrap())?;
        // A late ACK remains DATA. No cursor, counters or ID retirement before
        // the unchanged original deadline and actual registration recheck.
        s.current()?;let b=s.batch.as_mut().unwrap();b.ack=Some(v);s.batch_last_key=key(b.header.objects.last().unwrap());s.batch_objects+=b.header.objects.len() as u32;s.batch_bytes+=b.header.total_bytes;s.batch_ids.insert(b.header.batch_id.clone());s.batch_ordinal+=1;s.phase=Phase::Idle;Ok(true)
    })}
    pub(super) fn retain_image_batch_late_data(&mut self,value:serde_json::Value)->Result<()> {let b=self.batch.as_mut().context("original batch absent")?;ensure!(b.observed.is_none(),"duplicate late batch DATA");b.observed=Some(value);Ok(())}
    pub fn image_batch_progress(&self)->(usize,usize,u64,usize){self.batch.as_ref().map(|b|(b.header_queued,b.index,b.queued,self.sent)).unwrap_or((0,0,0,self.sent))}
    pub fn image_batch_unknown_data(&self)->Option<&serde_json::Value>{self.batch.as_ref().filter(|b|b.ack.is_none()).and_then(|b|b.unknown.as_ref().or(b.observed.as_ref()))}
}
#[cfg(test)]#[path="image_batch_transfer_tests.rs"]mod tests;
