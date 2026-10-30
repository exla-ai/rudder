//! Linux FUSE wire adapter. Does not provision permissions, credentials or a
//! privileged broker. Mount only inside a freshly owned mount namespace.
use super::*;
use fuser::*;
use std::ffi::CString;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;

pub struct MountedVfs {
    view: VfsView,
    session: Option<BackgroundSession>,
    path: PathBuf,
    // Acquired while the view is authorized. Cleanup never needs a fresh
    // lookup through a view whose attempt has since been fenced.
    flush_handle: Option<File>,
}
/// Proof minted only by an explicit fresh-helper namespace transition.
pub struct PrivateMountNamespace {
    identity: std::ffi::OsString,
    user_namespace: std::ffi::OsString,
    uid: u32,
}
impl PrivateMountNamespace {
    pub fn enter() -> Result<Self> {
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "direct FUSE bootstrap requires root"
        );
        ensure!(
            fs::read_dir("/proc/self/task")?.count() == 1,
            "mount helper must enter before creating threads"
        );
        Self::finish()
    }
    /// Worker bootstrap must grant device access separately. Only the caller's
    /// current UID/GID map to themselves; no initial-user-namespace authority.
    pub fn enter_for_current_user() -> Result<Self> {
        ensure!(
            fs::read_dir("/proc/self/task")?.count() == 1,
            "rootless namespace requires a fresh single-thread helper"
        );
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        ensure!(
            uid != 0 && uid == unsafe { libc::geteuid() } && gid == unsafe { libc::getegid() },
            "rootless helper requires equal non-root real/effective credentials"
        );
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
            return Err(std::io::Error::last_os_error()).context("create rootless user namespace");
        }
        fs::write("/proc/self/setgroups", b"deny")
            .context("deny supplementary groups in new user namespace")?;
        fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).context(
            "write self UID map; helper must be a fresh executable with owned proc map files",
        )?;
        fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))
            .context("write self GID map")?;
        Self::finish()
    }
    fn finish() -> Result<Self> {
        let before = fs::read_link("/proc/self/ns/mnt")?;
        if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let slash = c"/";
        if unsafe {
            libc::mount(
                std::ptr::null(),
                slash.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let after = fs::read_link("/proc/self/ns/mnt")?;
        ensure!(after != before, "mount namespace did not change");
        Ok(Self {
            identity: after.into_os_string(),
            user_namespace: fs::read_link("/proc/self/ns/user")?.into_os_string(),
            uid: unsafe { libc::geteuid() },
        })
    }
}
impl MountedVfs {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Unmount must succeed and all kernel requests must finish before the
    /// view can checkpoint. EBUSY leaves it mounted and fails closed.
    pub fn unmount(&mut self) -> Result<VfsView> {
        ensure!(self.session.is_some(), "FUSE session absent");
        let f = self
            .flush_handle
            .as_ref()
            .context("mount flush handle unavailable; cannot certify checkpoint")?;
        if unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(f)) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.finish_unmount(true)?;
        Ok(self.view.clone())
    }
    /// Best-effort owned cleanup after failure/fencing. Success means the
    /// strict mount/server closed, never that a checkpoint may be acknowledged.
    /// Already acknowledged private writes retain their ordinary durability.
    pub fn discard(&mut self) -> Result<()> {
        ensure!(self.session.is_some(), "FUSE session absent");
        self.finish_unmount(false)
    }
    fn finish_unmount(&mut self, checkpoint_barrier: bool) -> Result<()> {
        // Our own descriptor must not keep the mount busy.
        drop(self.flush_handle.take());
        let path = CString::new(self.path.as_os_str().as_bytes())?;
        // fuser's own unmount is lazy. A normal unmount is the barrier: no
        // busy handles, detached process references or unfinished kernel users.
        if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if checkpoint_barrier {
                // A busy authorized view can retry. A fenced view fails this
                // lookup and must use discard, which mints no checkpoint proof.
                self.flush_handle = cleanup_handle(&self.path).ok();
            }
            return Err(error.into());
        }
        let session = self.session.take().context("FUSE session absent")?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let view = self.view.clone();
        std::thread::spawn(move || {
            let result = session.join();
            if result.is_ok() && checkpoint_barrier {
                view.0.mounted.store(false, Ordering::Release);
                if let Ok(mut handles) = view.0.handles.lock() {
                    handles.clear();
                }
            }
            let _ = send.send(result);
        });
        receive
            .recv_timeout(Duration::from_secs(10))
            .context("FUSE drain deadline; view stays fenced until server confirms shutdown")??;
        Ok(())
    }
}
impl Drop for MountedVfs {
    fn drop(&mut self) {
        // Dropping a busy/error session may abort the kernel connection through
        // fuser's destructor. It must NEVER mint the checkpoint barrier.
        if self.session.is_some() {
            let _ = self.discard();
        }
    }
}

pub fn mount(
    view: VfsView,
    path: impl AsRef<Path>,
    namespace: &PrivateMountNamespace,
) -> Result<MountedVfs> {
    ensure!(
        fs::read_link("/proc/self/ns/mnt")?.into_os_string() == namespace.identity,
        "mount namespace proof differs"
    );
    ensure!(
        fs::read_link("/proc/self/ns/user")?.into_os_string() == namespace.user_namespace
            && unsafe { libc::geteuid() } == namespace.uid,
        "namespace credential proof differs"
    );
    let path = path.as_ref();
    let expected = view.manager().root().join("mounts").join(view.id());
    ensure!(
        path == expected,
        "mountpoint must be the view's exclusively managed mount directory"
    );
    secure_dir(path)?;
    ensure!(
        fs::read_dir(path)?.next().is_none(),
        "mountpoint is nonempty"
    );
    // Direct device/mount syscalls only: never fall back to a setuid helper.
    let lifecycle = view.db()?;
    mutable(&lifecycle)?;
    ensure!(
        !view.0.mounted.swap(true, Ordering::AcqRel),
        "view already mounted"
    );
    drop(lifecycle);
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("rudder-vfs".into()),
        MountOption::Subtype("rudder".into()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::RW,
    ];
    config.n_threads = Some(1);
    config.acl = SessionACL::Owner;
    let fs = Adapter {
        view: view.clone(),
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    let start = (|| -> Result<BackgroundSession> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open("/dev/fuse")?;
        let device_meta = device.metadata()?;
        ensure!(
            device_meta.file_type().is_char_device(),
            "/dev/fuse is not a character device"
        );
        let target = CString::new(path.as_os_str().as_bytes())?;
        let options = CString::new(format!(
            "fd={},rootmode=40700,user_id={},group_id={},default_permissions",
            std::os::fd::AsRawFd::as_raw_fd(&device),
            namespace.uid,
            unsafe { libc::getegid() }
        ))?;
        if unsafe {
            libc::mount(
                c"rudder-vfs".as_ptr(),
                target.as_ptr(),
                c"fuse".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                options.as_ptr().cast(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        match Session::from_fd(fs, device.into(), SessionACL::Owner, config).and_then(|s| s.spawn())
        {
            Ok(s) => Ok(s),
            Err(e) => {
                unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
                Err(e.into())
            }
        }
    })();
    match start {
        Ok(session) => {
            let mut mounted = MountedVfs {
                view,
                session: Some(session),
                path: path.to_path_buf(),
                flush_handle: None,
            };
            mounted.flush_handle = Some(cleanup_handle(path)?);
            Ok(mounted)
        }
        Err(e) => {
            view.0.mounted.store(false, Ordering::Release);
            Err(e.into())
        }
    }
}

fn cleanup_handle(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}

struct Adapter {
    view: VfsView,
    uid: u32,
    gid: u32,
}
const TTL: Duration = Duration::ZERO;
fn failure(e: anyhow::Error) -> Errno {
    Errno::from_i32(errno(&e))
}
fn kind(k: NodeKind) -> FileType {
    match k {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
    }
}
fn time(ns: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns.max(0) as u64)
}
impl Adapter {
    fn attr(&self, a: VfsAttr) -> FileAttr {
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: time(a.atime_ns),
            mtime: time(a.mtime_ns),
            ctime: time(a.ctime_ns),
            crtime: UNIX_EPOCH,
            kind: kind(a.kind),
            perm: a.mode as u16,
            nlink: a.nlink.min(u32::MAX as u64) as u32,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
    fn empty(&self, r: Result<()>, reply: ReplyEmpty) {
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(failure(e)),
        }
    }
    fn entry(&self, r: Result<VfsAttr>, reply: ReplyEntry) {
        match r {
            Ok(a) => reply.entry(&TTL, &self.attr(a), Generation(1)),
            Err(e) => reply.error(failure(e)),
        }
    }
}
impl Filesystem for Adapter {
    fn init(&mut self, _: &Request, c: &mut KernelConfig) -> std::io::Result<()> {
        // fuser's default requests no WRITEBACK_CACHE. Cached write-through
        // retains normal shared mmap while fsync reaches our private backing.
        c.set_max_write(1024 * 1024)
            .map_err(|_| std::io::Error::other("FUSE max-write unsupported"))?;
        Ok(())
    }
    fn lookup(&self, _: &Request, p: INodeNo, n: &OsStr, r: ReplyEntry) {
        self.entry(self.view.lookup(p.0, n.as_bytes()), r)
    }
    fn getattr(&self, _: &Request, i: INodeNo, _: Option<FileHandle>, r: ReplyAttr) {
        match self.view.attr(i.0) {
            Ok(a) => r.attr(&TTL, &self.attr(a)),
            Err(e) => r.error(failure(e)),
        }
    }
    fn setattr(
        &self,
        _: &Request,
        i: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        _: Option<FileHandle>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        r: ReplyAttr,
    ) {
        if uid.is_some_and(|u| u != self.uid)
            || gid.is_some_and(|g| g != self.gid)
            || flags.is_some_and(|f| !f.is_empty())
        {
            r.error(Errno::EPERM);
            return;
        }
        let convert = |v: TimeOrNow| match v {
            TimeOrNow::Now => now(),
            TimeOrNow::SpecificTime(t) => t
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(i64::MAX as u128) as i64,
        };
        match self.view.set_attr(
            i.0,
            mode.map(|m| m & 0o7777),
            size,
            atime.map(convert),
            mtime.map(convert),
        ) {
            Ok(a) => r.attr(&TTL, &self.attr(a)),
            Err(e) => r.error(failure(e)),
        }
    }
    fn readlink(&self, _: &Request, i: INodeNo, r: ReplyData) {
        match self.view.readlink(i.0) {
            Ok(b) => r.data(&b),
            Err(e) => r.error(failure(e)),
        }
    }
    fn mkdir(&self, _: &Request, p: INodeNo, n: &OsStr, mode: u32, umask: u32, r: ReplyEntry) {
        self.entry(
            self.view.create(
                p.0,
                n.as_bytes(),
                NodeKind::Directory,
                mode & !umask & 0o7777,
                b"",
            ),
            r,
        )
    }
    fn symlink(&self, _: &Request, p: INodeNo, n: &OsStr, target: &Path, r: ReplyEntry) {
        self.entry(
            self.view.create(
                p.0,
                n.as_bytes(),
                NodeKind::Symlink,
                0o777,
                target.as_os_str().as_bytes(),
            ),
            r,
        )
    }
    fn unlink(&self, _: &Request, p: INodeNo, n: &OsStr, r: ReplyEmpty) {
        self.empty(self.view.unlink(p.0, n.as_bytes(), false), r)
    }
    fn rmdir(&self, _: &Request, p: INodeNo, n: &OsStr, r: ReplyEmpty) {
        self.empty(self.view.unlink(p.0, n.as_bytes(), true), r)
    }
    fn rename(
        &self,
        _: &Request,
        p: INodeNo,
        n: &OsStr,
        np: INodeNo,
        nn: &OsStr,
        f: RenameFlags,
        r: ReplyEmpty,
    ) {
        self.empty(
            self.view
                .rename(p.0, n.as_bytes(), np.0, nn.as_bytes(), f.bits()),
            r,
        )
    }
    fn link(&self, _: &Request, i: INodeNo, p: INodeNo, n: &OsStr, r: ReplyEntry) {
        self.entry(self.view.link(i.0, p.0, n.as_bytes()), r)
    }
    fn create(
        &self,
        _: &Request,
        p: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        r: ReplyCreate,
    ) {
        let result = (|| {
            let a = self.view.create(
                p.0,
                n.as_bytes(),
                NodeKind::File,
                mode & !umask & 0o7777,
                b"",
            )?;
            let h = self.view.open_handle(a.ino, flags, false)?;
            Ok((a, h))
        })();
        match result {
            Ok((a, h)) => r.created(
                &TTL,
                &self.attr(a),
                Generation(1),
                FileHandle(h),
                FopenFlags::empty(),
            ),
            Err(e) => r.error(failure(e)),
        }
    }
    fn open(&self, _: &Request, i: INodeNo, flags: OpenFlags, r: ReplyOpen) {
        let raw = flags.0;
        match self.view.open_handle(i.0, raw, false) {
            Ok(h) => r.opened(FileHandle(h), FopenFlags::empty()),
            Err(e) => r.error(failure(e)),
        }
    }
    fn read(
        &self,
        _: &Request,
        i: INodeNo,
        h: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        r: ReplyData,
    ) {
        let result = self
            .view
            .check_handle(i.0, h.0, false)
            .and_then(|_| self.view.read(i.0, offset, size as usize));
        match result {
            Ok(b) => r.data(&b),
            Err(e) => r.error(failure(e)),
        }
    }
    fn write(
        &self,
        _: &Request,
        i: INodeNo,
        h: FileHandle,
        offset: u64,
        b: &[u8],
        _: WriteFlags,
        flags: OpenFlags,
        _: Option<LockOwner>,
        r: ReplyWrite,
    ) {
        let result = self.view.check_handle(i.0, h.0, true).and_then(|_| {
            self.view.write(
                i.0,
                offset,
                b,
                flags.0 & (libc::O_SYNC | libc::O_DSYNC) != 0,
            )
        });
        match result {
            Ok(n) => r.written(n as u32),
            Err(e) => r.error(failure(e)),
        }
    }
    fn flush(&self, _: &Request, _: INodeNo, _: FileHandle, _: LockOwner, r: ReplyEmpty) {
        r.ok()
    }
    fn release(
        &self,
        _: &Request,
        i: INodeNo,
        h: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        r: ReplyEmpty,
    ) {
        self.empty(self.view.close_handle(i.0, h.0), r)
    }
    fn fsync(&self, _: &Request, i: INodeNo, _: FileHandle, _: bool, r: ReplyEmpty) {
        self.empty(self.view.sync(i.0), r)
    }
    fn opendir(&self, _: &Request, i: INodeNo, f: OpenFlags, r: ReplyOpen) {
        match self.view.open_handle(i.0, f.0, true) {
            Ok(h) => r.opened(FileHandle(h), FopenFlags::empty()),
            Err(e) => r.error(failure(e)),
        }
    }
    fn readdir(&self, _: &Request, i: INodeNo, h: FileHandle, offset: u64, mut r: ReplyDirectory) {
        let mut cookie = offset;
        loop {
            match self.view.read_directory_page(
                i.0,
                h.0,
                cookie,
                super::readdir::DIRECTORY_PAGE_ENTRIES,
            ) {
                Ok(page) => {
                    for entry in page.entries {
                        if r.add(
                            INodeNo(entry.ino),
                            entry.next_cookie,
                            kind(entry.kind),
                            OsStr::from_bytes(&entry.name),
                        ) {
                            r.ok();
                            return;
                        }
                        cookie = entry.next_cookie;
                    }
                    if page.eof {
                        r.ok();
                        return;
                    }
                }
                Err(e) => {
                    r.error(failure(e));
                    return;
                }
            }
        }
    }
    fn releasedir(&self, _: &Request, i: INodeNo, h: FileHandle, _: OpenFlags, r: ReplyEmpty) {
        self.empty(self.view.close_handle(i.0, h.0), r)
    }
    fn fsyncdir(&self, _: &Request, i: INodeNo, _: FileHandle, _: bool, r: ReplyEmpty) {
        self.empty(self.view.sync(i.0), r)
    }
    fn statfs(&self, _: &Request, _: INodeNo, r: ReplyStatfs) {
        let result = (|| -> Result<libc::statvfs> {
            let path = CString::new(self.view.manager().root().as_os_str().as_bytes())?;
            let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            if unsafe { libc::statvfs(path.as_ptr(), value.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(unsafe { value.assume_init() })
        })();
        match result {
            Ok(v) => r.statfs(
                v.f_blocks,
                v.f_bfree,
                v.f_bavail,
                v.f_files,
                v.f_ffree,
                v.f_bsize as u32,
                255,
                v.f_frsize as u32,
            ),
            Err(e) => r.error(failure(e)),
        }
    }
}
