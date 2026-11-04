//! Storage-only normal retirement on the original bootstrap gate. This does
//! not retire Root registration, the engine process, provider or machine.
use super::*;

/// Original storage service cleanup and Root's original Child wait only.
/// This does not authorize checkpoint release or whole-machine replacement.
#[must_use="retain storage-only receipt with the original bootstrap"]
pub struct StorageServiceRetired {
    _request:String,_retirement:uuid::Uuid,original:FinalizationConstraint,
}
impl StorageServiceRetired {
    pub fn original_constraint(&self)->&FinalizationConstraint {&self.original}
}

pub(super) struct RootStorageStop {
    request:String,original:FinalizationBudget,ready:Option<Vec<u8>>,ready_sent:bool,
    retirement:Option<String>,joined_reply:Option<Vec<u8>>,joined_sent:bool,send_unknown:bool,incoming:Option<(Message,Option<OwnedFd>)>,
}
pub(super) struct NativeStorageStop {
    request:String,original:FinalizationBudget,request_bytes:Option<Vec<u8>>,requested:bool,ready:bool,
    acknowledged:Option<Vec<u8>>,ack_sent:bool,child_joined:bool,send_unknown:bool,incoming:Option<(Message,Option<OwnedFd>)>,
    completed:Option<StorageServiceRetired>,
}
impl NativeStorageStop {
    fn finish_retaining(&mut self,retirement:uuid::Uuid)->Result<()> {
        ensure!(self.requested&&self.ready&&self.ack_sent&&self.child_joined,"original storage retirement phases incomplete");
        self.original.remaining()?;
        if self.completed.is_none(){self.completed=Some(StorageServiceRetired{
            _request:self.request.clone(),_retirement:retirement,original:self.original.constraint()});}
        Ok(())
    }
}
fn canonical_id(value:&str)->Result<()> {
    let id=uuid::Uuid::parse_str(value)?;
    ensure!(id.get_version_num()==4&&id.to_string()==value,"original storage retirement ID differs");Ok(())
}
impl MachineRootBootstrap {
    pub(super) fn capture_storage_stop_retaining(&mut self,request:String,constraint:FinalizationConstraint)->Result<()> {
        ensure!(self.storage_stop.is_none(),"Root original storage stop already captured");canonical_id(&request)?;
        let original=FinalizationBudget::from_constraint(&constraint)?;original.remaining()?;self.root_current()?;
        self.storage_stop=Some(RootStorageStop{request,original,ready:None,ready_sent:false,retirement:None,joined_reply:None,joined_sent:false,send_unknown:false,incoming:None});Ok(())
    }
    pub(super) fn poll_storage_stop_retaining(&mut self)->Result<()> {
        self.root_current()?;self.engine_current()?;
        let stop=self.storage_stop.as_mut().context("Root original storage stop absent")?;
        stop.original.remaining()?;let gate=self.gate.as_mut().context("original bootstrap gate absent")?;
        if !stop.ready_sent {
            if stop.ready.is_none(){stop.ready=Some(gate.frame(Message::StorageStopReady{request:stop.request.clone()})?);}
            ensure!(!stop.send_unknown,"original Root storage-stop send uncertain; explicit reconciliation required");stop.send_unknown=true;
            let sent=gate.endpoint.send(stop.ready.as_ref().unwrap(),None)?;stop.send_unknown=false;if !sent{return Ok(())}stop.ready_sent=true;
        }
        if stop.retirement.is_none() {
            if stop.incoming.is_none(){stop.incoming=gate.poll()?;}
            let Some((message,fd))=stop.incoming.as_ref()else{return Ok(())};
            ensure!(fd.is_none(),"storage retirement ACK has descriptor");
            let Message::StorageStopAcknowledged{request,retirement}=message else{anyhow::bail!("original storage retirement ACK absent")};
            ensure!(*request==stop.request,"storage retirement request changed");canonical_id(retirement)?;
            // This comes only from the fixed Native bootstrap's original
            // authenticated RootServiceStopped owner. It is necessary along
            // with the actual original Child wait, never replaced by its PID.
            stop.retirement=Some(retirement.clone());stop.incoming=None;
        }
        let child=self.service.as_mut().context("original storage Child absent")?;
        if child.status.is_none(){child.status=child.child.try_wait()?;}
        let Some(status)=child.status.as_ref()else{return Ok(())};
        ensure!(status.success(),"original storage Child failed after stop ACK; retain all owners");
        stop.original.remaining()?;
        if !stop.joined_sent {
            if stop.joined_reply.is_none(){stop.joined_reply=Some(gate.frame(Message::StorageChildJoined{request:stop.request.clone()})?);}
            ensure!(!stop.send_unknown,"original Root storage-join send uncertain; explicit reconciliation required");stop.send_unknown=true;
            let sent=gate.endpoint.send(stop.joined_reply.as_ref().unwrap(),None)?;stop.send_unknown=false;if !sent{return Ok(())}stop.joined_sent=true;
        }
        stop.original.remaining()?;Ok(())
    }
}
impl ManagedMachineBootstrap {
    /// OFF App. The caller retains its actual machine/engine locks throughout.
    /// Closes only the storage subset after genuine runtime actor retirement,
    /// authenticated Root cleanup ACK and Root's actual original Child wait.
    /// Every phase retains its original frame, owner and one caller budget.
    pub fn retire_storage_service_retaining(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<bool> {
        if cancelled.load(Ordering::Acquire){original.cancel();}
        original.remaining()?;
        if self.storage_stop.is_none() {
            self.check_current_retaining()?;
            self.storage_stop=Some(NativeStorageStop{request:uuid::Uuid::new_v4().to_string(),original:original.clone(),request_bytes:None,
                requested:false,ready:false,acknowledged:None,ack_sent:false,child_joined:false,send_unknown:false,incoming:None,completed:None});
        }
        let stop=self.storage_stop.as_mut().unwrap();
        ensure!(stop.original.constraint()==original.constraint(),"original bootstrap storage stop budget changed");stop.original.remaining()?;
        let own=self.own.as_ref().context("original native bootstrap process absent")?;
        ensure!(own.pid==std::process::id(),"original storage-stop host process changed");own.live()?;storage::floor(own.pid,1500)?;
        ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"original storage-stop descriptor protection lost");
        self.engine_image.as_ref().unwrap().matches_process(own)?;
        ensure!(storage::id(self.mnt.as_ref().context("original storage-stop MNT absent")?)?==storage::id(&storage::namespace(None,"mnt",libc::CLONE_NEWNS)?)?
            &&storage::id(self.user.as_ref().context("original storage-stop USER absent")?)?==storage::id(&storage::namespace(None,"user",libc::CLONE_NEWUSER)?)?
            &&self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"original storage-stop host namespace/boot changed");
        let gate=self.gate.as_mut().context("original bootstrap gate absent")?;
        if !stop.requested {
            if stop.request_bytes.is_none(){stop.request_bytes=Some(gate.frame(Message::StorageStopRequested{request:stop.request.clone(),constraint:stop.original.constraint()})?);}
            ensure!(!stop.send_unknown,"original Native storage-stop send uncertain; explicit reconciliation required");stop.send_unknown=true;
            let sent=gate.endpoint.send(stop.request_bytes.as_ref().unwrap(),None)?;stop.send_unknown=false;if !sent{return Ok(false)}stop.requested=true;
        }
        if !stop.ready {
            if stop.incoming.is_none(){stop.incoming=gate.poll()?;}
            let Some((message,fd))=stop.incoming.as_ref()else{return Ok(false)};
            ensure!(fd.is_none()&&matches!(message,Message::StorageStopReady{request}if *request==stop.request),"original Root storage-stop readiness differs");stop.ready=true;stop.incoming=None;
        }
        if let Some(runtime)=&self.runtime_client {
            if !runtime.poll_unshared_retirement_retaining(&stop.original,cancelled)?{return Ok(false)}
        }
        let client=self.client.as_mut().context("original storage client absent")?;
        if !client.poll_service_stop_retaining(&stop.original)?{return Ok(false)}
        let ack=client.service_stop_acknowledgment().context("original Root service stop ACK absent")?;
        if !stop.ack_sent {
            if stop.acknowledged.is_none(){stop.acknowledged=Some(gate.frame(Message::StorageStopAcknowledged{request:stop.request.clone(),retirement:ack.retirement().to_string()})?);}
            ensure!(!stop.send_unknown,"original Native storage-ACK send uncertain; explicit reconciliation required");stop.send_unknown=true;
            let sent=gate.endpoint.send(stop.acknowledged.as_ref().unwrap(),None)?;stop.send_unknown=false;if !sent{return Ok(false)}stop.ack_sent=true;
        }
        if !stop.child_joined {
            if stop.incoming.is_none(){stop.incoming=gate.poll()?;}
            let Some((message,fd))=stop.incoming.as_ref()else{return Ok(false)};
            ensure!(fd.is_none()&&matches!(message,Message::StorageChildJoined{request}if *request==stop.request),"original Root storage Child join ACK differs");stop.child_joined=true;stop.incoming=None;
        }
        stop.finish_retaining(ack.retirement())?;Ok(true)
    }
    /// Historical original storage-only acknowledgment. It never grants
    /// replacement, checkpoint release, provider stop or whole-machine drain.
    pub fn storage_service_retired(&self)->bool {self.storage_service_stop_acknowledgment().is_some()}
    pub fn storage_service_stop_acknowledgment(&self)->Option<&StorageServiceRetired> {
        self.storage_stop.as_ref()?.completed.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_stop_late_child_ack_never_publishes_completion() {
        let original=FinalizationBudget::begin(Duration::from_secs(1)).unwrap();
        original.cancel();
        let mut stop=NativeStorageStop{request:uuid::Uuid::new_v4().to_string(),original,request_bytes:None,
            requested:true,ready:true,acknowledged:None,ack_sent:true,child_joined:true,send_unknown:false,incoming:None,completed:None};
        // Synthetic phase flags exercise rejection only. No original receipt
        // can be issued after this budget has ended.
        assert!(stop.finish_retaining(uuid::Uuid::new_v4()).is_err());
        assert!(stop.completed.is_none());assert!(stop.child_joined,"retain observed original join after deadline failure");
    }
    #[test]
    fn actual_stop_gate_keeps_startup_binding_and_independent_original_cleanup_clock() {
        // Actual credentials/pidfd/seqpacket transport only. No bootstrap or
        // Root stop receipt is manufactured by this local wire fixture.
        let startup=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();
        let cleanup=FinalizationBudget::begin(Duration::from_secs(1)).unwrap();
        let data=BootstrapData{version:1,nonce:uuid::Uuid::new_v4().to_string(),constraint:startup.constraint()};
        let mut raw=[-1;2];assert_eq!(unsafe{libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())},0);
        let first=unsafe{OwnedFd::from_raw_fd(raw[0])};let second=unsafe{OwnedFd::from_raw_fd(raw[1])};
        for fd in [&first,&second]{let yes=1i32;assert_eq!(unsafe{libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of::<i32>()as _)},0);}
        let own=Process::current().unwrap();let uid=unsafe{libc::getuid()};
        let mut native=Gate::capture(first,own.fd().try_clone().unwrap(),uid,data.clone(),startup.clone()).unwrap();
        let mut root=Gate::capture(second,own.fd().try_clone().unwrap(),uid,data,startup.clone()).unwrap();
        startup.cancel();cleanup.remaining().unwrap();
        let request=uuid::Uuid::new_v4().to_string();
        let bytes=native.frame(Message::StorageStopRequested{request:request.clone(),constraint:cleanup.constraint()}).unwrap();
        assert!(native.endpoint.send(&bytes,None).unwrap());
        let received=root.poll();
        if unsafe{libc::getgid()}!=uid{assert!(received.is_err());return;}
        let (message,fd)=received.unwrap().unwrap();assert!(fd.is_none());
        let Message::StorageStopRequested{request:actual,constraint}=message else{panic!("explicit storage stop request absent")};
        assert_eq!(actual,request);assert_eq!(constraint,cleanup.constraint());
        assert_eq!(root.data.constraint,startup.constraint(),"channel binding remains original startup data");
        assert!(root.host_ack.is_none(),"stop request never substitutes for original host worker ACK");
    }
}
