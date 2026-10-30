use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FabricPaths {
    pub root: PathBuf,
    pub journal: PathBuf,
    pub cas: PathBuf,
}

impl FabricPaths {
    pub fn new(home: &Path) -> Self {
        let root = home.join("fabric");
        Self { journal: root.join("journal.sqlite"), cas: root.join("cas"), root }
    }
}

/// Local checkout identity only. Cloud project UUIDs are assigned separately
/// and must survive moves and replica materialization on other machines.
pub fn local_project_id(repo: &Path) -> Result<String> {
    let canonical = repo.canonicalize().context("canonicalize project checkout")?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        canonical.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = canonical.to_str().context("project path must be UTF-8 on this platform")?.as_bytes();
    Ok(format!("local:{}", hex::encode(Sha256::digest(bytes))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_aliases_use_one_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("repo")).unwrap();
        let first = local_project_id(&root.path().join("repo")).unwrap();
        assert_eq!(first, local_project_id(&root.path().join("repo/../repo")).unwrap());
        #[cfg(unix)] {
            std::os::unix::fs::symlink(root.path().join("repo"), root.path().join("alias")).unwrap();
            assert_eq!(first, local_project_id(&root.path().join("alias")).unwrap());
        }
    }
}
