//! Child of host_ingress; only its ACTUAL RootHostControl may create this owner.
//! New-volume directory initialization only, no project/source/task authority.
//! Stage the carrier before mkdirat/chown/fsync; every uncertain effect remains
//! retained. No Drop deletion, adoption, recursive chown or retry budget exists.
use super::*;
use std::{ffi::CString,io::Read,os::fd::FromRawFd};
const WORKSPACE:&str="/workspace";
const CONTROL:&str="managed-control";
const FRESH:&str="fresh-v4";
const UID:u32=1500;
const GID:u32=1500;
const MAX_DIRS:usize=4;
fn directory_at(parent:&File,name:&str)->Result<File> {
    let name=CString::new(name)?;let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK)};
    ensure!(fd>=0,"managed volume original directory unavailable");Ok(unsafe{File::from_raw_fd(fd)})
}
fn inode(file:&File)->Result<(u64,u64)> {let m=file.metadata()?;Ok((m.dev(),m.ino()))}
fn mount_id(file:&File)->Result<u64> {
    let path=format!("/proc/self/fdinfo/{}",file.as_raw_fd());let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let mut kind:libc::statfs=unsafe{std::mem::zeroed()};ensure!(unsafe{libc::fstatfs(file.as_raw_fd(),&mut kind)}==0&&kind.f_type==0x9fa0,"managed volume fdinfo is not actual procfs");
    let mut bytes=Vec::new();file.take(4097).read_to_end(&mut bytes)?;ensure!(bytes.len()<=4096,"managed volume fdinfo exceeds bound");parse_mount_id(&bytes)
}
fn parse_mount_id(bytes:&[u8])->Result<u64> {
    let text=std::str::from_utf8(bytes)?;let mut found=None;
    for line in text.lines(){if let Some(value)=line.strip_prefix("mnt_id:"){ensure!(found.is_none(),"duplicate kernel mount identity");let value=value.trim();ensure!(!value.is_empty()&&value.bytes().all(|c|c.is_ascii_digit()),"invalid kernel mount identity");let id=value.parse::<u64>()?;ensure!(id>0,"zero kernel mount identity");found=Some(id);}}
    found.context("actual mount identity absent")
}
fn workspace_is_mount(expected:u64)->Result<()> {
    let bytes=storage::proc_bytes("/proc/thread-self/mountinfo",1024*1024)?;let text=std::str::from_utf8(&bytes)?;let mut found=None;
    for line in text.lines(){let (left,_)=line.split_once(" - ").context("incomplete actual mountinfo")?;let fields:Vec<_>=left.split_whitespace().collect();ensure!(fields.len()>=6,"incomplete actual mount fields");
        if fields[4]==WORKSPACE{ensure!(found.is_none(),"ambiguous workspace mount");found=Some(fields[0].parse::<u64>()?);}}
    ensure!(found==Some(expected),"workspace is not the actual separately mounted volume");Ok(())
}
fn clock(original:&FinalizationBudget)->Result<()> {original.remaining()?;ensure!(unsafe{libc::getuid()}==0&&unsafe{libc::geteuid()}==0&&unsafe{libc::getgid()}==0&&unsafe{libc::getegid()}==0,"volume initialization requires original Root UID/GID0");Ok(())}
struct Anchor {path:PathBuf,file:File,id:(u64,u64),mount:u64}
impl Anchor {
    fn open_root(path:&Path)->Result<Self> {
        ensure!(path.canonicalize()?==path,"managed volume ancestor redirects");let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?;
        let value=Self{path:path.into(),id:inode(&file)?,mount:mount_id(&file)?,file};value.root_current()?;Ok(value)
    }
    fn root_current(&self)->Result<()> {
        for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"managed volume root ancestor changed/writable");}
        ensure!(mount_id(&self.file)?==self.mount,"held managed volume mount changed");let named=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&self.path)?;
        ensure!(inode(&named)?==self.id&&mount_id(&named)?==self.mount,"named managed volume mount changed");Ok(())
    }
}
/// Every stage records intent BEFORE syscall. A returned mkdir failure does not
/// authorize reopening/adopting an existing name; original uncertainty remains.
struct NewDirectory {name:String,parent:File,parent_id:(u64,u64),mount:u64,attempted:bool,created_ack:bool,
    held:Option<File>,id:Option<(u64,u64)>,chown_attempted:bool,uid:u32,gid:u32,chown_ack:bool,sync_ack:bool}
impl NewDirectory {
    fn stage(parent:&File,name:&str,mount:u64)->Result<Self> {
        ensure!(!name.is_empty()&&name.as_bytes().iter().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(b))&&!matches!(name,"."|".."),"invalid fixed new directory component");
        ensure!(mount_id(parent)?==mount,"new directory parent mount differs");Ok(Self{name:name.into(),parent:parent.try_clone()?,parent_id:inode(parent)?,mount,attempted:false,created_ack:false,held:None,id:None,chown_attempted:false,uid:0,gid:0,chown_ack:false,sync_ack:false})
    }
    fn create_retaining(&mut self,original:&FinalizationBudget)->Result<()> {
        clock(original)?;ensure!(!self.attempted,"new directory creation already attempted; original capsule retained");self.parent_current(0)?;self.attempted=true;
        let name=CString::new(self.name.as_str())?;let result=unsafe{libc::mkdirat(self.parent.as_raw_fd(),name.as_ptr(),0o700)};
        ensure!(result==0,"new managed directory exists or creation is uncertain");self.created_ack=true;
        self.held=Some(directory_at(&self.parent,&self.name)?);self.id=Some(inode(self.held.as_ref().unwrap())?);self.current()?;
        ensure!(fs::read_dir(format!("/proc/self/fd/{}",self.held.as_ref().unwrap().as_raw_fd()))?.next().is_none(),"new managed directory was not empty");
        self.held.as_ref().unwrap().sync_all()?;self.parent.sync_all()?;self.sync_ack=true;clock(original)
    }
    fn parent_current(&self,uid:u32)->Result<()> {
        let m=self.parent.metadata()?;ensure!(m.is_dir()&&(m.dev(),m.ino())==self.parent_id&&m.uid()==uid&&m.gid()==uid&&m.mode()&0o022==0&&mount_id(&self.parent)?==self.mount,"original new directory parent changed");Ok(())
    }
    fn current(&self)->Result<()> {
        ensure!(self.created_ack,"original mkdir acknowledgment absent");let file=self.held.as_ref().context("original created directory handle absent")?;
        let named=directory_at(&self.parent,&self.name)?;
        for held in [file,&named]{let m=held.metadata()?;ensure!(m.is_dir()&&Some((m.dev(),m.ino()))==self.id&&m.uid()==self.uid&&m.gid()==self.gid&&m.mode()&0o7777==0o700&&mount_id(held)?==self.mount,"original created directory custody changed");}Ok(())
    }
    fn handoff_empty_retaining(&mut self,original:&FinalizationBudget)->Result<()> {
        clock(original)?;self.current()?;ensure!(!self.chown_attempted&&self.sync_ack,"original directory handoff already attempted/unready");
        ensure!(fs::read_dir(format!("/proc/self/fd/{}",self.held.as_ref().unwrap().as_raw_fd()))?.next().is_none(),"directory handoff contains preexisting data");
        self.chown_attempted=true;let result=unsafe{libc::fchown(self.held.as_ref().unwrap().as_raw_fd(),UID,GID)};ensure!(result==0,"new directory handoff uncertain");
        self.uid=UID;self.gid=GID;self.chown_ack=true;self.current()?;self.held.as_ref().unwrap().sync_all()?;self.parent.sync_all()?;clock(original)
    }
    fn handoff_parent_retaining(&mut self,child:&NewDirectory,original:&FinalizationBudget)->Result<()> {
        clock(original)?;self.current()?;self.parent_current(0)?;child.current()?;
        ensure!(child.chown_ack&&Some(child.parent_id)==self.id&&!self.chown_attempted&&self.sync_ack,"new parent/child original handoff differs");
        let mut entries=fs::read_dir(format!("/proc/self/fd/{}",self.held.as_ref().unwrap().as_raw_fd()))?;
        let only=entries.next().transpose()?.context("original managed child absent")?;ensure!(only.file_name()==std::ffi::OsStr::new(&child.name)&&entries.next().is_none(),"managed parent contains unknown data");
        self.chown_attempted=true;ensure!(unsafe{libc::fchown(self.held.as_ref().unwrap().as_raw_fd(),UID,GID)}==0,"new managed parent handoff uncertain");
        self.uid=UID;self.gid=GID;self.chown_ack=true;self.current()?;self.held.as_ref().unwrap().sync_all()?;self.parent.sync_all()?;clock(original)
    }

}
/// Actual Root config/process/mount and original new-directory owners. This is
/// not a provider-volume durability receipt or a fresh project/source permission.
pub(super) struct HostVolumeInit {root:Process,root_dir:Anchor,workspace:Anchor,mnt:File,user:File,boot:Vec<u8>,config:File,config_stamp:Stamp,
    original:FinalizationBudget,store:String,nonce:String,directories:Vec<NewDirectory>,attempted:bool,ready:bool}
impl HostVolumeInit {
    fn capture(host:&RootHostControl)->Result<Self> {
        clock(&host.original)?;host.current()?;let status=String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384)?)?;ensure!(super::super::caps(&status)?&3==3,"managed volume requires actual CHOWN/DAC_OVERRIDE capabilities");ensure!(matches!(host.data.store_policy,HostReceiptStorePolicy::InitializeEmptyV1),"existing managed volume needs explicit retained recovery; no initialization/adoption");
        ensure!(host.data.state_directory==Path::new(WORKSPACE).join(CONTROL).join(&host.data.store_id),"managed state directory differs from original store binding");
        for p in &host.data.projects {let id=&p.prepare.namespace.project_id;ensure!(uuid::Uuid::parse_str(id).is_ok_and(|v|v.get_version_num()==4&&v.to_string()==*id),"fresh namespace is not a generated UUID");
            let base=Path::new(WORKSPACE).join(FRESH).join(id);ensure!(p.checkout==base.join("repository")&&p.home==base.join("home")&&p.cas==host.data.state_directory.join("cas")&&p.endpoints==base.join("endpoints"),"fresh namespace layout differs from original provisioned config");}
        let root_dir=Anchor::open_root(Path::new("/"))?;let workspace=Anchor::open_root(Path::new(WORKSPACE))?;
        ensure!(workspace.file.metadata()?.mode()&0o001!=0,"managed volume must allow UID1500 to traverse its Root-owned parent");
        ensure!(workspace.mount!=root_dir.mount,"image workspace fallback is not a distinct mounted volume");workspace_is_mount(workspace.mount)?;
        let value=Self{root:Process::current()?,root_dir,workspace,mnt:storage::namespace(None,"mnt",libc::CLONE_NEWNS)?,user:storage::namespace(None,"user",libc::CLONE_NEWUSER)?,
            boot:storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,config:host.file.try_clone()?,config_stamp:host.stamp.clone(),original:host.original.clone(),store:host.data.store_id.clone(),nonce:host.nonce.clone(),directories:Vec::with_capacity(MAX_DIRS),attempted:false,ready:false};
        value.physical_current()?;clock(&value.original)?;Ok(value)
    }
    fn physical_current(&self)->Result<()> {
        self.root.live()?;ensure!(self.root.pid==std::process::id(),"managed volume owner changed process");storage::floor(self.root.pid,0)?;self.root_dir.root_current()?;self.workspace.root_current()?;
        self.config_stamp.check(&self.config,0o600)?;ensure!(storage::id(&self.mnt)?==storage::id(&storage::namespace(None,"mnt",libc::CLONE_NEWNS)?)?&&storage::id(&self.user)?==storage::id(&storage::namespace(None,"user",libc::CLONE_NEWUSER)?)?&&self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"managed volume original namespace/boot changed");workspace_is_mount(self.workspace.mount)
    }
    fn creation_current(&self)->Result<()> {self.physical_current()?;for d in &self.directories {if d.created_ack {d.current()?;}}clock(&self.original)}
    fn prepare_retaining(&mut self)->Result<()> {
        clock(&self.original)?;self.physical_current()?;ensure!(!self.attempted,"managed volume original initialization already attempted");self.attempted=true;
        // Stage each actual parent/intent BEFORE mkdirat. All ancestors remain
        // Root-owned/nonwritable until every creation/fsync is acknowledged.
        self.directories.push(NewDirectory::stage(&self.workspace.file,CONTROL,self.workspace.mount)?);self.directories[0].create_retaining(&self.original)?;
        self.creation_current()?;let parent=self.directories[0].held.as_ref().unwrap();self.directories.push(NewDirectory::stage(parent,&self.store,self.workspace.mount)?);self.directories[1].create_retaining(&self.original)?;
        self.creation_current()?;let state=self.directories[1].held.as_ref().unwrap();self.directories.push(NewDirectory::stage(state,"cas",self.workspace.mount)?);self.directories[2].create_retaining(&self.original)?;
        self.creation_current()?;self.directories.push(NewDirectory::stage(&self.workspace.file,FRESH,self.workspace.mount)?);self.directories[3].create_retaining(&self.original)?;
        self.creation_current()?;self.directories[2].handoff_empty_retaining(&self.original)?;self.directories[3].handoff_empty_retaining(&self.original)?;
        // Exactly known child ownership, never recursive traversal/chown.
        {let(state,rest)=self.directories.split_at_mut(2);state[1].handoff_parent_retaining(&rest[0],&self.original)?;}
        {let(control,rest)=self.directories.split_at_mut(1);control[0].handoff_parent_retaining(&rest[0],&self.original)?;}
        self.physical_current()?;clock(&self.original)?;self.ready=true;Ok(())
    }
    pub(super) fn check_current(&self)->Result<()> {self.physical_current()?;ensure!(self.ready&&self.directories.len()==MAX_DIRS,"managed volume initialization unacknowledged/unknown");for d in &self.directories {d.current()?;}Ok(())}
}
impl RootHostControl {
    /// Root bootstrap calls BEFORE first engine/sidecar dispatch, after its
    /// actual Self carrier was returned and retained OUTSIDE catch_unwind.
    pub(crate) fn initialize_volume_retaining(&mut self)->Result<()> {
        self.current()?;if !self.data.projects.iter().any(|p|uuid::Uuid::parse_str(&p.prepare.namespace.project_id).is_ok_and(|u|u.get_version_num()==4&&u.to_string()==p.prepare.namespace.project_id)){return Ok(());}
        if let Some(volume)=&self.volume_init {ensure!(volume.nonce==self.nonce&&volume.store==self.data.store_id,"managed volume original host binding differs");if volume.ready{return volume.check_current();}}
        ensure!(self.volume_init.is_none(),"original managed volume initialization retained for recovery");
        self.volume_init=Some(HostVolumeInit::capture(self)?);self.volume_init.as_mut().unwrap().prepare_retaining()
    }
}
#[cfg(test)]
#[path="host_volume_init_tests.rs"]
mod tests;
