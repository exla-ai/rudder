//! Kernel Child/pipe ownership only. This cannot issue a Root/SDK/source or
//! general process-tree-stop receipt. The genuine Root wrapper is separate.
use super::*;
pub(super) struct ChildSettlement {child:Option<std::process::Child>,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,
    pid:u32,exit:Option<std::process::ExitStatus>,stdout_eof:bool,stderr_eof:bool,stdout_bytes:u64,stderr_bytes:u64,attempted:bool,ready:bool}
impl ChildSettlement {
    pub(super) fn stage_retaining(out:&mut Option<Self>,child:&mut Option<std::process::Child>,stdout:&mut Option<std::process::ChildStdout>,stderr:&mut Option<std::process::ChildStderr>)->Result<()> {
        ensure!(out.is_none()&&child.is_some()&&stdout.is_some()&&stderr.is_some(),"actual original SDK Child/pipes absent or already transferred");let pid=child.as_ref().unwrap().id();
        *out=Some(Self{child:child.take(),stdout:stdout.take(),stderr:stderr.take(),pid,exit:None,stdout_eof:false,stderr_eof:false,stdout_bytes:0,stderr_bytes:0,attempted:false,ready:false});Ok(())
    }
    pub(super) fn run_retaining(&mut self,original:&FinalizationBudget)->Result<()> {self.run_expected_retaining(original,ExpectedExit::Successful)}
    pub(super) fn run_terminal_failure_retaining(&mut self,original:&FinalizationBudget)->Result<()> {self.run_expected_retaining(original,ExpectedExit::TerminalFailure)}
    fn run_expected_retaining(&mut self,original:&FinalizationBudget,expected:ExpectedExit)->Result<()> {
        ensure!(!self.attempted,"original Child settlement cannot replay");self.attempted=true;original.remaining()?;
        for fd in [self.stdout.as_ref().unwrap().as_raw_fd(),self.stderr.as_ref().unwrap().as_raw_fd()]{let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
            ensure!(flags>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"SDK original pipe nonblocking settlement unavailable");}
        loop{original.remaining()?;
            fn drain(fd:i32,eof:&mut bool,total:&mut u64)->Result<()>{if *eof{return Ok(())}let mut bytes=[0u8;4096];for _ in 0..16{let n=unsafe{libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
                if n==0{*eof=true;break}if n<0{let e=io::Error::last_os_error();if e.kind()==io::ErrorKind::WouldBlock{break}if e.kind()==io::ErrorKind::Interrupted{continue}return Err(e.into())}
                *total=total.checked_add(n as u64).context("SDK pipe settlement overflow")?;ensure!(*total<=65536,"SDK pipe settlement bound exceeded");}Ok(())}
            drain(self.stdout.as_ref().unwrap().as_raw_fd(),&mut self.stdout_eof,&mut self.stdout_bytes)?;drain(self.stderr.as_ref().unwrap().as_raw_fd(),&mut self.stderr_eof,&mut self.stderr_bytes)?;
            if self.exit.is_none(){self.exit=self.child.as_mut().unwrap().try_wait()?;}
            if let Some(exit)=self.exit.as_ref(){ensure!(expected.matches(exit),"SDK actual original Child exit differs from typed settlement");if self.stdout_eof&&self.stderr_eof{break}}
            std::thread::sleep(original.remaining()?.min(Duration::from_millis(1)));
        }
        original.remaining()?;self.stdout.take();self.stderr.take();self.child.take();self.ready=true;Ok(())
    }
    pub(super) fn original_failed_pid_data(&self)->Result<u32>{ensure!(self.ready&&self.exit.as_ref().is_some_and(|e|!e.success())&&self.stdout_eof&&self.stderr_eof,"original failed Child wait/pipe settlement ACK absent");Ok(self.pid)}
    pub(super) fn original_pid_data(&self)->Result<u32>{ensure!(self.ready&&self.exit.as_ref().is_some_and(|e|e.success())&&self.stdout_eof&&self.stderr_eof,"original Child wait/pipe settlement ACK absent");Ok(self.pid)}
}

#[derive(Clone,Copy)]enum ExpectedExit{Successful,TerminalFailure}
impl ExpectedExit{fn matches(self,exit:&std::process::ExitStatus)->bool{match self{Self::Successful=>exit.success(),Self::TerminalFailure=>!exit.success()}}}

#[cfg(test)]#[path="compiler_sdk_child_settlement_tests.rs"]mod tests;

#[cfg(test)]#[path="compiler_sdk_terminal_child_tests.rs"]mod terminal_tests;
