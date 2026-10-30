use anyhow::{Context, Result, ensure};
#[cfg(feature = "jj")]
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) const MAX_STATE_BYTES: u64 = 1024 * 1024;

pub(crate) fn open_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "metadata must be a regular file"
    );
    Ok(file)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    read_json_bounded(path, MAX_STATE_BYTES)
}

#[cfg_attr(not(feature = "jj"), allow(dead_code))]
pub(crate) fn read_json_bounded<T: DeserializeOwned>(path: &Path, maximum: u64) -> Result<T> {
    ensure!(
        maximum <= 128 * 1024 * 1024,
        "metadata bound exceeds supported maximum"
    );
    let file = open_read(path)?;
    ensure!(
        file.metadata()?.len() <= maximum,
        "metadata exceeds size bound"
    );
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "metadata exceeds size bound");
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_json_bounded(path, value, MAX_STATE_BYTES)
}

#[cfg_attr(not(feature = "jj"), allow(dead_code))]
pub(crate) fn write_json_bounded<T: Serialize>(path: &Path, value: &T, maximum: u64) -> Result<()> {
    let bytes = rudder_engine::protocol::canonical_json(value)?;
    write_bytes_bounded(path, &bytes, maximum)
}

pub(crate) fn write_bytes_bounded(path: &Path, bytes: &[u8], maximum: u64) -> Result<()> {
    ensure!(
        maximum <= 128 * 1024 * 1024,
        "metadata bound exceeds supported maximum"
    );
    ensure!(bytes.len() as u64 <= maximum, "metadata exceeds size bound");
    let parent = path.parent().context("metadata parent missing")?;
    let temporary = parent.join(format!(".rudder-write-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(feature = "jj")]
pub(crate) fn lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "lock must be a regular file");
    file.lock_exclusive()?;
    Ok(file)
}
