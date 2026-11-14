#![cfg(unix)]
use rondi::open_output_file;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

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

#[test]
fn character_devices_open_directly_and_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let device = open_output_file(std::path::Path::new("/dev/null"), false).unwrap();
    assert!(device.metadata().unwrap().file_type().is_char_device());
    let linked = dir.path().join("null");
    std::os::unix::fs::symlink("/dev/null", &linked).unwrap();
    let device = open_output_file(&linked, false).unwrap();
    assert!(device.metadata().unwrap().file_type().is_char_device());
    // Blocking mode is restored so a terminal write cannot return EAGAIN.
    // SAFETY: the descriptor is open.
    let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&device), libc::F_GETFL) };
    assert_eq!(flags & libc::O_NONBLOCK, 0);
}

#[test]
fn symlinks_to_other_inodes_stay_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let to_fifo = dir.path().join("to_fifo");
    std::os::unix::fs::symlink(&fifo, &to_fifo).unwrap();
    assert!(open_output_file(&to_fifo, false).is_err());
    let to_dir = dir.path().join("to_dir");
    std::os::unix::fs::symlink(dir.path(), &to_dir).unwrap();
    assert!(open_output_file(&to_dir, false).is_err());
    let missing = dir.path().join("missing");
    let dangling = dir.path().join("dangling");
    std::os::unix::fs::symlink(&missing, &dangling).unwrap();
    assert!(open_output_file(&dangling, false).is_err());
    assert!(!missing.exists());
}

/// A new pseudo-terminal: its controller, its terminal and the terminal's path.
fn terminal_pair() -> (std::fs::File, std::fs::File, std::path::PathBuf) {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let (mut controller, mut terminal) = (-1, -1);
    // SAFETY: both descriptor pointers are valid; the optional settings are null.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut controller,
                &mut terminal,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned two distinct descriptors owned by this scope.
    let controller = unsafe { std::fs::File::from_raw_fd(controller) };
    let terminal = unsafe { std::fs::File::from_raw_fd(terminal) };
    let mut name = [0 as libc::c_char; 256];
    // SAFETY: the terminal is open and the buffer is writable for its length.
    assert_eq!(
        unsafe { libc::ttyname_r(terminal.as_raw_fd(), name.as_mut_ptr(), name.len()) },
        0
    );
    // SAFETY: a successful ttyname_r terminates the buffer.
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes()));
    (controller, terminal, path)
}

#[test]
fn foreign_owned_terminal_output_is_refused() {
    // Only root can give a terminal to another account; the Linux oracle
    // suite runs as root.
    if !rustix::process::geteuid().is_root() {
        return;
    }
    let (_controller, terminal, path) = terminal_pair();
    let nobody = Some(rustix::fs::Uid::from_raw(65534));
    rustix::fs::fchown(&terminal, nobody, None).unwrap();
    rustix::fs::fchmod(&terminal, rustix::fs::Mode::from_raw_mode(0o666)).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().uid(), 65534);
    let dir = tempfile::tempdir().unwrap();
    let linked = dir.path().join("foreign-terminal");
    std::os::unix::fs::symlink(&path, &linked).unwrap();
    assert!(open_output_file(&path, false).is_err());
    assert!(open_output_file(&linked, false).is_err());
}

#[test]
fn owned_terminal_output_stays_available() {
    let (_controller, _terminal, path) = terminal_pair();
    assert_eq!(
        std::fs::metadata(&path).unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    assert!(open_output_file(&path, false).is_ok());
    let dir = tempfile::tempdir().unwrap();
    let linked = dir.path().join("own-terminal");
    std::os::unix::fs::symlink(&path, &linked).unwrap();
    assert!(open_output_file(&linked, false).is_ok());
}

#[test]
fn null_output_stays_available_to_unprivileged_users() {
    if !rustix::process::geteuid().is_root() {
        return;
    }
    use std::os::unix::process::CommandExt;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "character_devices_open_directly_and_through_a_symlink",
        ])
        .uid(65534)
        .gid(65534)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
