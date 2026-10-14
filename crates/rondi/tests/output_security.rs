#![cfg(unix)]
use rondi::open_output_file;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn output_rejects_fifo_without_waiting_and_preserves_links() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(open_output_file(&fifo, false).is_err());
    let target = dir.path().join("target");
    std::fs::write(&target, b"unchanged").unwrap();
    let linked = dir.path().join("linked");
    std::fs::hard_link(&target, &linked).unwrap();
    assert!(open_output_file(&linked, false).is_err());
    std::fs::remove_file(&linked).unwrap();
    std::os::unix::fs::symlink(&target, &linked).unwrap();
    assert!(open_output_file(&linked, false).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
}

#[test]
fn output_is_private_and_exclusive_creation_preserves_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out");
    let file = open_output_file(&path, true).unwrap();
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
    std::fs::write(&path, b"keep").unwrap();
    assert!(open_output_file(&path, true).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"keep");
}

#[test]
fn foreign_owned_output_is_rejected_without_truncation() {
    // Ownership changes require root; the Linux oracle job runs as root.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign");
    std::fs::write(&path, b"keep").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::chown(name.as_ptr(), 65534, 65534) }, 0);
    assert_eq!(std::fs::metadata(&path).unwrap().uid(), 65534);
    assert!(open_output_file(&path, false).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"keep");
}
