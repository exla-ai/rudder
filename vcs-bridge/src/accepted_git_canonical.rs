//! One actual fixed Git producer; immutable output DATA stays private until the
//! parent independently verifies every object. Original children/pipes survive
//! cancellation/errors. No thread, process detachment, Drop IO or retry.
use super::*;
use std::{os::{fd::AsRawFd,unix::process::CommandExt},process::{Child,ChildStdin,ChildStdout,ChildStderr,Command,Stdio,ExitStatus},time::Duration};
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize)]
pub struct CanonicalProcessData{pub original_child_pid_data:Option<u32>,pub exit_code_data:Option<i32>,pub stdout_eof:bool,pub stderr_eof:bool,pub stdin_bytes_sent:usize,pub stdout_bytes:usize,pub stderr_bytes:usize,pub spawn_error_returned:bool,pub stop_requested:bool}
pub(super) struct CanonicalInput{
 original:FinalizationBudget,cancelled:Arc<AtomicBool>,source:Option<HeldDir>,source_objects:Option<HeldDir>,parent:Option<HeldDir>,directory:Option<HeldDir>,directory_intent:Option<PathBuf>,image:Option<HeldFile>,image_parent:Option<HeldDir>,
 child:Option<Child>,process_image:Option<File>,input:Option<ChildStdin>,stdout:Option<ChildStdout>,stderr:Option<ChildStderr>,request:Vec<u8>,written:usize,
 output:Vec<u8>,error:Vec<u8>,stdout_eof:bool,stderr_eof:bool,status:Option<ExitStatus>,spawn_attempted:bool,spawn_error_returned:bool,kill_attempted:bool,ready:bool,
}
impl CanonicalInput{
 pub(super) fn stage(original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self{Self{original,cancelled,source:None,source_objects:None,parent:None,directory:None,directory_intent:None,image:None,image_parent:None,child:None,process_image:None,input:None,stdout:None,stderr:None,request:Vec::new(),written:0,output:Vec::new(),error:Vec::new(),stdout_eof:false,stderr_eof:false,status:None,spawn_attempted:false,spawn_error_returned:false,kill_attempted:false,ready:false}}
 pub(super) fn produce(&mut self,source:&gix::Repository,target:&GitBackend,expected:ObjectId,check:&mut impl FnMut()->Result<()>)->Result<(PathBuf,PathBuf)>{
  ensure!(cfg!(target_os="linux"),"PackedHistoryV2 canonical FD handoff requires Linux");ensure!(!self.spawn_attempted,"canonical source producer already attempted; no replay");time(&self.original,&self.cancelled)?;check()?;
  let config=source.config_snapshot();ensure!(config.string("extensions.partialClone").is_none(),"packed source refuses partial-clone configuration");
  if let Some(remotes)=config.sections_by_name("remote"){for remote in remotes{ensure!(remote.body().value_implicit("promisor").is_none(),"packed source refuses promisor remote configuration");}}
  let source_packs=source.common_dir().join("objects/pack");match fs::read_dir(&source_packs){Ok(entries)=>{let mut n=0;for e in entries{time(&self.original,&self.cancelled)?;n+=1;ensure!(n<=200_128,"source pack namespace exceeds profile");ensure!(!e?.path().extension().is_some_and(|x|x=="promisor"),"packed source refuses promised-object stores");}},Err(e)if e.kind()==io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
  self.source=Some(HeldDir::open(source.common_dir().canonicalize()?)?);self.source_objects=Some(HeldDir::open(source.common_dir().join("objects").canonicalize()?)?);self.parent=Some(HeldDir::open(target.git_repo_path().join("objects"))?);ensure!(self.source_objects.as_ref().unwrap().id!=self.parent.as_ref().unwrap().id,"source and target object stores alias the same actual directory");
  let directory=self.parent.as_ref().unwrap().path.join(format!(".rudder-canonical-{}",uuid::Uuid::new_v4()));
  // Directory intent is retained before creation; the original target owner
  // also retains its parent. No error removes an unknown partially created root.
  self.directory_intent=Some(directory.clone());self.request=format!("{expected}\n").into_bytes();fs::DirBuilder::new().mode(0o700).create(&directory)?;self.directory=Some(HeldDir::open(directory)?);
  self.image_parent=Some(HeldDir::open(PathBuf::from("/usr/bin"))?);self.image=Some(HeldFile::open(PathBuf::from("/usr/bin/git"))?);
  let image=self.image.as_ref().unwrap();ensure!(image.version.uid==0&&image.version.gid==0&&image.version.mode&0o6111==0o111,"canonical producer must be the fixed non-setid Root-owned Git image");
  let mut command=Command::new(&image.path);command.env_clear().env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_NO_LAZY_FETCH","1").env("GIT_TERMINAL_PROMPT","0").env("GIT_NO_REPLACE_OBJECTS","1").env("GIT_ATTR_NOSYSTEM","1")
   .args(["--no-replace-objects","-c","core.hooksPath=/dev/null","-c","core.fsmonitor=false","-c","core.sharedRepository=false","-c","pack.writeReverseIndex=false","-c","pack.writeBitmapIndex=false","-c","gc.auto=0","-c","pack.threads=1","-c","pack.packSizeLimit=0","--git-dir=/proc/self/fd/3","pack-objects","--revs","--reuse-delta","--reuse-object","--index-version=2","--window=0","--depth=64","--threads=1"])
   .arg(self.directory.as_ref().unwrap().path.join("pack")).current_dir(&self.directory.as_ref().unwrap().path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
  let source_fd=self.source.as_ref().unwrap().file.as_raw_fd();unsafe{command.pre_exec(move||{
   if libc::dup2(source_fd,3)<0{return Err(io::Error::last_os_error())}let flags=libc::fcntl(3,libc::F_GETFD);if flags<0||libc::fcntl(3,libc::F_SETFD,flags&!libc::FD_CLOEXEC)<0{return Err(io::Error::last_os_error())}
   let file=libc::rlimit{rlim_cur:MAX_ENCODED_BYTES as libc::rlim_t,rlim_max:MAX_ENCODED_BYTES as libc::rlim_t};let memory=libc::rlimit{rlim_cur:(512u64<<20)as libc::rlim_t,rlim_max:(512u64<<20)as libc::rlim_t};
   if libc::setrlimit(libc::RLIMIT_FSIZE,&file)<0||libc::setrlimit(libc::RLIMIT_AS,&memory)<0{return Err(io::Error::last_os_error())}libc::umask(0o022);Ok(())
  });}
  self.current()?;check()?;self.spawn_attempted=true;match command.spawn(){Ok(child)=>self.child=Some(child),Err(error)=>{self.spawn_error_returned=true;return Err(error.into())}}
  let result=(||->Result<()> {
  // Actual Child is retained BEFORE extracting/configuring any inherited pipe.
  let child=self.child.as_mut().unwrap();self.input=child.stdin.take();self.stdout=child.stdout.take();self.stderr=child.stderr.take();
  for fd in[self.input.as_ref().context("canonical child stdin absent")?.as_raw_fd(),self.stdout.as_ref().context("canonical child stdout absent")?.as_raw_fd(),self.stderr.as_ref().context("canonical child stderr absent")?.as_raw_fd()]{nonblocking(fd)?;}
  // Stdin holds the child behind the accepted revision admission. The proc-exe
  // witness is compared to the actual captured immutable Git image first.
  #[cfg(target_os="linux")]{self.process_image=Some(File::open(format!("/proc/{}/exe",self.child.as_ref().unwrap().id()))?);let m=self.process_image.as_ref().unwrap().metadata()?;let v=&self.image.as_ref().unwrap().version;ensure!((m.dev(),m.ino(),m.len())==(v.dev,v.ino,v.len),"canonical child original executable differs");}
  loop{time(&self.original,&self.cancelled)?;self.current()?;check()?;
   if let Some(input)=self.input.as_mut(){match input.write(&self.request[self.written..]){Ok(n)=>{ensure!(n>0,"canonical child stdin stopped");self.written+=n;if self.written==self.request.len(){self.input=None;}},Err(e)if e.kind()==io::ErrorKind::WouldBlock=>{},Err(e)=>return Err(e.into())}}
   self.poll_settlement()?;if self.status.is_some()&&self.stdout_eof&&self.stderr_eof{break}
   std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);
  }ensure!(self.status.as_ref().unwrap().success(),"canonical source Git child failed; bounded diagnostics retained");self.current()?;check()?;Ok(())})();
  if result.is_err(){self.request_kill_retaining();return result.map(|_|unreachable!())}
  let output=std::str::from_utf8(&self.output)?;ensure!(output.len()==41&&output.ends_with('\n'),"canonical child pack result differs");let hash=ObjectId::from_hex(output[..40].as_bytes())?;
  let pack=self.directory.as_ref().unwrap().path.join(format!("pack-{hash}.pack"));let index=pack.with_extension("idx");
  ensure!(fs::read_dir(&self.directory.as_ref().unwrap().path)?.map(|e|e.map(|e|e.file_name())).collect::<io::Result<BTreeSet<_>>>()?==BTreeSet::from([pack.file_name().unwrap().to_os_string(),index.file_name().unwrap().to_os_string()]),"canonical producer left unexpected output/effects");
  self.ready=true;self.current()?;time(&self.original,&self.cancelled)?;Ok((pack,index))
 }
 #[cfg(test)]pub(super) fn original_child_retained_for_test(&self)->bool{self.child.is_some()&&self.process_image.is_some()}
 pub(super) fn process_data(&self)->CanonicalProcessData{CanonicalProcessData{original_child_pid_data:self.child.as_ref().map(Child::id),exit_code_data:self.status.as_ref().and_then(ExitStatus::code),stdout_eof:self.stdout_eof,stderr_eof:self.stderr_eof,stdin_bytes_sent:self.written,stdout_bytes:self.output.len(),stderr_bytes:self.error.len(),spawn_error_returned:self.spawn_error_returned,stop_requested:self.kill_attempted}}
 pub(super) fn settled_data(&self)->bool{self.status.as_ref().is_some_and(|s|s.success())&&self.stdout_eof&&self.stderr_eof}
 pub(super) fn current(&self)->Result<()>{time(&self.original,&self.cancelled)?;for dir in[&self.source,&self.source_objects,&self.parent,&self.directory,&self.image_parent].into_iter().flatten(){dir.current()?;}if let Some(image)=&self.image{image.current()?;}Ok(())}
 pub(super) fn directory(&self)->Result<&Path>{ensure!(self.ready,"canonical original output ACK absent");Ok(&self.directory.as_ref().unwrap().path)}
 /// Cleanup-only real observations, no clock replacement or import readiness.
 pub(super) fn poll_settlement(&mut self)->Result<bool>{if self.child.is_none(){ensure!(!self.spawn_attempted||self.spawn_error_returned,"canonical spawn effect unknown; owner retained");return Ok(true)}if let Some(child)=&mut self.child{if self.status.is_none(){self.status=child.try_wait()?;}}
  drain(&mut self.stdout,&mut self.output,&mut self.stdout_eof,64)?;drain(&mut self.stderr,&mut self.error,&mut self.stderr_eof,4096)?;Ok(self.status.is_some()&&self.stdout_eof&&self.stderr_eof)
 }
 pub(super) fn request_kill_retaining(&mut self){if !self.kill_attempted&&self.status.is_none(){self.kill_attempted=true;if let Some(child)=&mut self.child{let _=child.kill();}}}
}
fn nonblocking(fd:i32)->Result<()>{let old=unsafe{libc::fcntl(fd,libc::F_GETFL)};ensure!(old>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,old|libc::O_NONBLOCK)}>=0,"canonical pipe nonblocking setup failed");Ok(())}
fn drain<T:Read>(pipe:&mut Option<T>,bytes:&mut Vec<u8>,eof:&mut bool,maximum:usize)->Result<()>{if let Some(pipe)=pipe{let mut buffer=[0;1024];for _ in 0..8{match pipe.read(&mut buffer){Ok(0)=>{*eof=true;break},Ok(n)=>{ensure!(bytes.len()+n<=maximum,"canonical child bounded control output exceeded");bytes.extend_from_slice(&buffer[..n]);},Err(e)if e.kind()==io::ErrorKind::WouldBlock=>break,Err(e)if e.kind()==io::ErrorKind::Interrupted=>continue,Err(e)=>return Err(e.into())}}}Ok(())}
