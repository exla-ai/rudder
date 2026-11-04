use rudder_engine::store::LocalStore;
use std::{fs, io::{Seek, SeekFrom, Write}};

fn fixture() -> (tempfile::TempDir, LocalStore, Vec<u8>, rudder_engine::protocol::ObjectRef) {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let bytes: Vec<_> = (0..6 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
    let reference = store.put_blob(&bytes).unwrap();
    (tmp, store, bytes, reference)
}

#[test]
fn verified_ranges_cross_io_buffers_and_clamp_only_at_eof() {
    let (_tmp, store, bytes, reference) = fixture();
    assert_eq!(store.read_blob_range(&reference, 65_530, 100).unwrap(), bytes[65_530..65_630]);
    assert_eq!(store.read_blob_range(&reference, bytes.len() as u64 - 9, 100).unwrap(), bytes[bytes.len() - 9..]);
    assert!(store.read_blob_range(&reference, bytes.len() as u64 + 1, 16).unwrap().is_empty());
    assert!(store.read_blob_range(&reference, u64::MAX, 2).is_err());
    assert!(store.read_blob_range(&reference, 0, 16 * 1024 * 1024 + 1).is_err());
}

#[test]
fn stored_length_is_checked_before_allocating_a_buffer_for_corrupt_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let reference = store.put_blob(b"x").unwrap();
    let path = store.object_path(&reference).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    // A sparse corrupt object consumes no large fixture allocation or disk copy.
    fs::OpenOptions::new().write(true).open(path).unwrap().set_len(64 * 1024 * 1024).unwrap();
    let failure = store.read_object(&reference).unwrap_err();
    assert!(failure.to_string().contains("object size mismatch before read"));
}

#[test]
fn corruption_outside_selected_range_exposes_no_bytes_to_export() {
    let (_tmp, store, bytes, reference) = fixture();
    let object = store.object_path(&reference).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut file = fs::OpenOptions::new().write(true).open(&object).unwrap();
    file.seek(SeekFrom::Start(bytes.len() as u64 - 1)).unwrap();
    file.write_all(&[255]).unwrap();
    assert!(store.read_blob_range(&reference, 0, 10).is_err());
    let mut output = Vec::new();
    assert!(store.copy_blob_to(&reference, &mut output).is_err());
    assert!(output.is_empty(), "unverified legacy contents must not reach the writer");
    assert_eq!(fs::read_dir(store.root().join("tmp")).unwrap().count(), 0);
}

#[test]
fn verified_legacy_export_keeps_a_stable_snapshot_after_unlocking_the_store() {
    let (_tmp, store, bytes, reference) = fixture();
    struct MutatingWriter { source: std::path::PathBuf, output: Vec<u8>, first: bool }
    impl Write for MutatingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.first {
                self.first = false;
                #[cfg(unix)] {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&self.source, fs::Permissions::from_mode(0o600))?;
                }
                fs::write(&self.source, b"external mutation after verification")?;
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut writer = MutatingWriter { source: store.object_path(&reference).unwrap(), output: Vec::new(), first: true };
    store.copy_blob_to(&reference, &mut writer).unwrap();
    assert_eq!(writer.output, bytes, "export must read the private verified snapshot, not rewind the mutable source");
    assert_eq!(fs::read_dir(store.root().join("tmp")).unwrap().count(), 0);
}
