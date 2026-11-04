//! Explicit native materializers. No silent clone-to-byte-copy fallback.
use super::*;

#[cfg(target_os = "macos")]
pub fn apfs_clone_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_dir(),
        "APFS clone source must be a real directory"
    );
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            apfs_clone_tree(&entry.path(), &target)?;
        } else if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else if metadata.is_file() {
            let from = std::ffi::CString::new(entry.path().as_os_str().as_bytes())?;
            let to = std::ffi::CString::new(target.as_os_str().as_bytes())?;
            ensure!(
                unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 1) } == 0,
                "forced APFS clonefile failed: {}",
                std::io::Error::last_os_error()
            );
            // clonefile clears setuid/setgid; restore the canonical file mode.
            fs::set_permissions(&target, metadata.permissions())?;
            File::open(&target)?.sync_all()?;
        } else {
            bail!("unsupported APFS clone entry type");
        }
    }
    fs::set_permissions(destination, metadata.permissions())?;
    sync_dir(destination)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn apfs_clone_tree(_: &Path, _: &Path) -> Result<()> {
    bail!("APFS clonefile materialization requires native macOS on a compatible same-filesystem volume")
}
