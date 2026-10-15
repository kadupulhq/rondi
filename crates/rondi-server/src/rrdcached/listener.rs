//! `-l` and `-L` listeners: Unix sockets published through a private
//! staging directory, and TCP sockets for every resolved address.

use super::config::{DEFAULT_PORT, ListenAddress, ListenerSpec};
use super::{errno_text, error_text};
use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An open listener with the commands its clients may use.
#[derive(Debug)]
pub(crate) struct Listener {
    pub(crate) socket: BoundSocket,
    pub(crate) permissions: Option<Arc<[String]>>,
}

#[derive(Debug)]
pub(crate) enum BoundSocket {
    /// The socket file is removed when the listener is dropped.
    Unix(UnixListener, SocketFile),
    Tcp(std::net::TcpListener),
}

/// A published Unix socket file. Dropping it removes the file if it is
/// still the inode this process bound.
#[derive(Debug)]
pub(crate) struct SocketFile {
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

impl SocketFile {
    /// Leaves the file in place when dropped.
    fn keep(mut self) {
        self.identity = None;
    }
}

#[cfg(test)]
pub(crate) fn tests_socket_file(path: &Path) -> SocketFile {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    SocketFile {
        path: path.to_path_buf(),
        identity: Some((metadata.dev(), metadata.ino())),
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let Some(identity) = self.identity else {
            return;
        };
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Opens every listener, printing one `rrdcached: <reason>` line per
/// failure to `notes`.
// Compatibility: the failure lines and, when nothing opened, the FATAL
// line (RCD-4). Provenance: black-box compatibility test.
// Rondi policy: a listener that fails stops startup even when others
// opened, and the sockets already published are removed (RCD-5), so a
// configured listener and its `-P` scope are never silently missing.
pub(crate) fn open_all(specs: &[ListenerSpec], notes: &mut dyn Write) -> Result<Vec<Listener>, ()> {
    let mut opened = Vec::new();
    let mut failed = false;
    for spec in specs {
        let permissions: Option<Arc<[String]>> = spec.permissions.as_deref().map(Arc::from);
        let sockets = match &spec.address {
            ListenAddress::Unix(path) => open_unix(path, spec.access.mode(), spec.access.group)
                .map(|(listener, file)| vec![BoundSocket::Unix(listener, file)])
                .map_err(|reason| vec![reason]),
            ListenAddress::Network(text) => open_network(text),
        };
        match sockets {
            Ok(sockets) => opened.extend(sockets.into_iter().map(|socket| Listener {
                socket,
                permissions: permissions.clone(),
            })),
            Err(reasons) => {
                failed = true;
                for reason in reasons {
                    let _ = writeln!(notes, "rrdcached: {reason}");
                }
            }
        }
    }
    if opened.is_empty() {
        let _ = writeln!(notes, "rrdcached: FATAL: cannot open any listen sockets");
        return Err(());
    }
    if failed {
        return Err(());
    }
    Ok(opened)
}

/// The JSON daemon's socket: mode 0600, published like an rrdcached Unix
/// listener and left in place for the caller to remove.
pub(crate) fn open_json_listener(path: &Path) -> std::io::Result<UnixListener> {
    let (listener, file) = open_unix(path, 0o600, None).map_err(std::io::Error::other)?;
    file.keep();
    Ok(listener)
}

/// Binds a Unix socket and publishes it at `path` with its final mode and
/// group already set.
// Rondi policy: the socket is bound and given its mode inside a private
// directory and then renamed into place, so it is never reachable with a
// wider mode and no chmod or chown can follow a path swapped in by
// another user; a live socket at the path is refused rather than replaced,
// as is any file that is not a socket; and the socket's directory and its
// ancestors must not be writable by another user (RCD-5, RCD-11).
fn open_unix(
    path: &Path,
    mode: u32,
    group: Option<u32>,
) -> Result<(UnixListener, SocketFile), String> {
    let shown = path.display();
    let bind_failed = |reason: String| format!("bind({shown}) failed: {reason}.");
    if path.as_os_str().is_empty() {
        return Err(bind_failed(errno_text(libc::ENOENT)));
    }
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| bind_failed(error_text(&error)))?
            .join(path)
    };
    let directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("/"))
        .to_path_buf();
    // Compatibility: a missing socket directory is created (opt-050).
    // Provenance: black-box compatibility test.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&directory)
        .map_err(|error| bind_failed(error_text(&error)))?;
    check_directory_chain(&directory)?;
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if UnixStream::connect(&target).is_ok() {
                return Err(format!("socket is already active: {shown}"));
            }
        }
        Ok(_) => return Err(format!("refusing to replace {shown}: not a socket")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(bind_failed(error_text(&error))),
    }
    let stage = Stage::create(&directory).map_err(|error| bind_failed(error_text(&error)))?;
    let staged = stage.path.join("s");
    let listener = bind_unix(&stage.path).map_err(|error| bind_failed(error_text(&error)))?;
    if let Some(gid) = group {
        std::os::unix::fs::chown(&staged, None, Some(gid))
            .map_err(|error| format!("chown({shown}) failed: {}", error_text(&error)))?;
    }
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod({shown}) failed: {}", error_text(&error)))?;
    std::fs::rename(&staged, &target).map_err(|error| bind_failed(error_text(&error)))?;
    let metadata =
        std::fs::symlink_metadata(&target).map_err(|error| bind_failed(error_text(&error)))?;
    drop(stage);
    Ok((
        listener,
        SocketFile {
            path: target,
            identity: Some((metadata.dev(), metadata.ino())),
        },
    ))
}

/// A private directory for binding a socket, removed with whatever is
/// left in it.
struct Stage {
    path: PathBuf,
}

impl Stage {
    fn create(directory: &Path) -> std::io::Result<Self> {
        let pid = std::process::id();
        for attempt in 0..100u32 {
            let path = directory.join(format!(".rrdcached-{pid}-{attempt}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::from_raw_os_error(libc::EEXIST))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.join("s"));
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Binds `<stage>/s`. A staging path too long for a socket address is
/// bound by name from inside the directory, which is safe only while the
/// process has one thread; the daemon binds before its runtime starts.
fn bind_unix(stage: &Path) -> std::io::Result<UnixListener> {
    let staged = stage.join("s");
    // SAFETY: an all-zero sockaddr_un is valid; only its size is read.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    if staged.as_os_str().len() < capacity {
        return UnixListener::bind(&staged);
    }
    let previous = std::fs::File::open(".")?;
    let stage_name = CString::new(stage.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: the name is NUL terminated and outlives the call.
    if unsafe { libc::chdir(stage_name.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bound = UnixListener::bind("s");
    // SAFETY: `previous` is an open directory descriptor.
    let restored = unsafe { libc::fchdir(previous.as_raw_fd()) };
    if restored != 0 {
        return Err(std::io::Error::last_os_error());
    }
    bound
}

/// Refuses a socket directory that a user other than root or the daemon's
/// own could write, checking every component of its real path: each must
/// belong to root or the daemon's user, and must not be writable by others
/// or by a group other than root's unless it is sticky.
fn check_directory_chain(directory: &Path) -> Result<(), String> {
    let real = std::fs::canonicalize(directory).map_err(|error| {
        format!(
            "bind({}) failed: {}.",
            directory.display(),
            error_text(&error)
        )
    })?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    for component in real.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = std::fs::symlink_metadata(component).map_err(|error| {
            format!(
                "bind({}) failed: {}.",
                component.display(),
                error_text(&error)
            )
        })?;
        let trusted_owner = metadata.uid() == 0 || metadata.uid() == euid;
        // Group write is shared unless the group is root's own.
        let writable =
            metadata.mode() & 0o002 != 0 || (metadata.mode() & 0o020 != 0 && metadata.gid() != 0);
        let shared = writable && metadata.mode() & 0o1000 == 0;
        if !trusted_owner || shared {
            return Err(format!(
                "refusing socket directory {}: another user can write it",
                component.display()
            ));
        }
    }
    Ok(())
}

/// Splits a network `-l` argument into host and port text.
// Compatibility: `[host]` and `[host]:port`; otherwise the last colon
// separates the port, so `::1` is host `:` and port `1`; a missing or
// empty port means 42217 (RCD-4).
// Provenance: rrdcached(1) and black-box compatibility test.
fn split_network_address(text: &str) -> Result<(&str, &str), String> {
    if let Some(bracketed) = text.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return Err(format!("Malformed address: {text}"));
        };
        let host = &bracketed[..close];
        let after = &bracketed[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => port,
            None if after.is_empty() => "",
            None => return Err(format!("Garbage after address: {after}")),
        };
        return Ok((host, port));
    }
    Ok(match text.rfind(':') {
        Some(colon) => (&text[..colon], &text[colon + 1..]),
        None => (text, ""),
    })
}

/// Binds every address `getaddrinfo` returns for a network listener.
fn open_network(text: &str) -> Result<Vec<BoundSocket>, Vec<String>> {
    let (host, port) = split_network_address(text).map_err(|reason| vec![reason])?;
    let port = if port.is_empty() { DEFAULT_PORT } else { port };
    let unresolvable = |reason: String| vec![format!("getaddrinfo({host}) failed: {reason}")];
    let host_name = if host.is_empty() {
        None
    } else {
        Some(CString::new(host).map_err(|_| unresolvable(errno_text(libc::EINVAL)))?)
    };
    let service = CString::new(port).map_err(|_| unresolvable(errno_text(libc::EINVAL)))?;
    let addresses = AddressList::resolve(host_name.as_deref(), &service).map_err(unresolvable)?;
    let mut sockets = Vec::new();
    let mut reasons = Vec::new();
    for address in addresses.iter() {
        match bind_tcp(address, text) {
            Ok(listener) => sockets.push(BoundSocket::Tcp(listener)),
            Err(reason) => reasons.push(reason),
        }
    }
    if reasons.is_empty() {
        Ok(sockets)
    } else {
        Err(reasons)
    }
}

/// A `getaddrinfo` result list, freed on drop.
struct AddressList(*mut libc::addrinfo);

impl AddressList {
    fn resolve(host: Option<&CStr>, service: &CStr) -> Result<Self, String> {
        // SAFETY: an all-zero addrinfo is a valid hints value.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_flags = libc::AI_PASSIVE | libc::AI_ADDRCONFIG;
        let mut list = std::ptr::null_mut();
        // SAFETY: the strings are NUL terminated, hints is initialized and
        // `list` receives the allocation that Drop frees.
        let status = unsafe {
            libc::getaddrinfo(
                host.map_or(std::ptr::null(), CStr::as_ptr),
                service.as_ptr(),
                &hints,
                &mut list,
            )
        };
        if status != 0 {
            if status == libc::EAI_SYSTEM {
                return Err(error_text(&std::io::Error::last_os_error()));
            }
            // SAFETY: gai_strerror returns a static NUL-terminated string.
            let text = unsafe { CStr::from_ptr(libc::gai_strerror(status)) };
            return Err(text.to_string_lossy().into_owned());
        }
        Ok(Self(list))
    }

    fn iter(&self) -> impl Iterator<Item = &libc::addrinfo> {
        // SAFETY: each node of a getaddrinfo list is valid until freeaddrinfo.
        std::iter::successors(unsafe { self.0.as_ref() }, |node| unsafe {
            node.ai_next.as_ref()
        })
    }
}

impl Drop for AddressList {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the list came from getaddrinfo and is freed once.
            unsafe { libc::freeaddrinfo(self.0) };
        }
    }
}

fn bind_tcp(address: &libc::addrinfo, text: &str) -> Result<std::net::TcpListener, String> {
    // SAFETY: socket has no memory preconditions.
    let fd = unsafe { libc::socket(address.ai_family, address.ai_socktype, address.ai_protocol) };
    if fd < 0 {
        return Err(format!(
            "socket() failed: {}",
            error_text(&std::io::Error::last_os_error())
        ));
    }
    // SAFETY: fd is a new descriptor owned by nothing else.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let enable: libc::c_int = 1;
    let set = |level, name| {
        // SAFETY: the option value is a live c_int of the given size.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                (&raw const enable).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    // SAFETY: fcntl on an owned descriptor.
    unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    set(libc::SOL_SOCKET, libc::SO_REUSEADDR);
    if address.ai_family == libc::AF_INET6 {
        // Keeps `[::]` and `0.0.0.0` from claiming each other's port.
        set(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY);
    }
    // SAFETY: ai_addr points at ai_addrlen bytes of a valid address.
    if unsafe { libc::bind(socket.as_raw_fd(), address.ai_addr, address.ai_addrlen) } != 0 {
        return Err(format!(
            "bind({text}) failed: {}.",
            error_text(&std::io::Error::last_os_error())
        ));
    }
    // SAFETY: listen on a bound, owned descriptor.
    if unsafe { libc::listen(socket.as_raw_fd(), libc::SOMAXCONN) } != 0 {
        return Err(format!(
            "listen({text}) failed: {}.",
            error_text(&std::io::Error::last_os_error())
        ));
    }
    Ok(std::net::TcpListener::from(socket))
}

#[cfg(test)]
mod tests {
    use super::super::config::SocketAccess;
    use super::*;

    fn unix_spec(path: &Path, access: SocketAccess) -> ListenerSpec {
        ListenerSpec {
            address: ListenAddress::Unix(path.to_path_buf()),
            access,
            permissions: None,
        }
    }

    fn private_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, real)
    }

    #[test]
    fn network_addresses_split_like_the_reference() {
        for (text, host, port) in [
            ("127.0.0.1:5000", "127.0.0.1", "5000"),
            ("localhost", "localhost", ""),
            ("::1", ":", "1"),
            ("127.0.0.1:5000:5", "127.0.0.1:5000", "5"),
            (":42217", "", "42217"),
            ("127.0.0.1:", "127.0.0.1", ""),
            ("[::1]", "::1", ""),
            ("[::1]:7", "::1", "7"),
            ("", "", ""),
        ] {
            assert_eq!(split_network_address(text), Ok((host, port)), "{text}");
        }
        assert_eq!(
            split_network_address("[::1"),
            Err("Malformed address: [::1".to_owned())
        );
        assert_eq!(
            split_network_address("[::1]x"),
            Err("Garbage after address: x".to_owned())
        );
    }

    #[test]
    fn unix_socket_is_published_with_its_mode_and_removed_on_drop() {
        let (_dir, root) = private_dir();
        let path = root.join("missing/s.sock");
        let mut notes = Vec::new();
        let specs = [
            unix_spec(&path, SocketAccess::default()),
            unix_spec(
                &root.join("t.sock"),
                SocketAccess {
                    mode: Some(0o640),
                    group: None,
                },
            ),
        ];
        let listeners = open_all(&specs, &mut notes).unwrap();
        assert!(notes.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(
            std::fs::metadata(root.join("t.sock")).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(root.join("missing").is_dir());
        // Only the sockets remain; the staging directories are gone.
        assert_eq!(std::fs::read_dir(root.join("missing")).unwrap().count(), 1);
        UnixStream::connect(&path).unwrap();
        drop(listeners);
        assert!(!path.exists());
        assert!(!root.join("t.sock").exists());
    }

    #[test]
    fn live_sockets_and_other_files_are_not_replaced() {
        let (_dir, root) = private_dir();
        let path = root.join("s.sock");
        let live = UnixListener::bind(&path).unwrap();
        let mut notes = Vec::new();
        assert!(open_all(&[unix_spec(&path, SocketAccess::default())], &mut notes).is_err());
        assert_eq!(
            String::from_utf8(notes).unwrap(),
            format!(
                "rrdcached: socket is already active: {}\nrrdcached: FATAL: cannot open any listen sockets\n",
                path.display()
            )
        );
        drop(live);
        // A stale socket is replaced.
        let listeners = open_all(
            &[unix_spec(&path, SocketAccess::default())],
            &mut Vec::new(),
        )
        .unwrap();
        drop(listeners);
        let victim = root.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        assert!(
            open_all(
                &[unix_spec(
                    &path,
                    SocketAccess {
                        mode: Some(0o666),
                        group: None
                    }
                )],
                &mut Vec::new()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::metadata(&victim).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn shared_socket_directories_are_refused_unless_sticky() {
        let (_dir, root) = private_dir();
        for (mode, accepted) in [
            (0o777, false),
            (0o770, false),
            (0o1777, true),
            (0o755, true),
        ] {
            let shared = root.join(format!("d{mode:o}"));
            std::fs::create_dir(&shared).unwrap();
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(mode)).unwrap();
            // Write access for root's own group is not shared.
            let accepted =
                accepted || (mode == 0o770 && std::fs::metadata(&shared).unwrap().gid() == 0);
            let mut notes = Vec::new();
            let result = open_all(
                &[unix_spec(&shared.join("s.sock"), SocketAccess::default())],
                &mut notes,
            );
            assert_eq!(result.is_ok(), accepted, "{mode:o}");
            if !accepted {
                assert!(String::from_utf8(notes).unwrap().starts_with(&format!(
                    "rrdcached: refusing socket directory {}: another user can write it\n",
                    shared.display()
                )));
            }
        }
    }

    #[test]
    fn a_failing_listener_removes_the_sockets_already_published() {
        let (_dir, root) = private_dir();
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = busy.local_addr().unwrap().to_string();
        let socket = root.join("first.sock");
        let specs = [
            unix_spec(&socket, SocketAccess::default()),
            ListenerSpec {
                address: ListenAddress::Network(address.clone()),
                access: SocketAccess::default(),
                permissions: None,
            },
        ];
        let mut notes = Vec::new();
        assert!(open_all(&specs, &mut notes).is_err());
        assert_eq!(
            String::from_utf8(notes).unwrap(),
            format!("rrdcached: bind({address}) failed: Address already in use.\n")
        );
        assert!(!socket.exists());
    }

    #[test]
    fn network_listener_binds_loopback_and_reports_lookup_failures() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let sockets = open_network(&format!("127.0.0.1:{port}")).unwrap();
        assert_eq!(sockets.len(), 1);
        std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let reasons = open_network("[::1").unwrap_err();
        assert_eq!(reasons, ["Malformed address: [::1"]);
        let reasons = open_network("rondi-no-such-host.invalid:1").unwrap_err();
        assert!(reasons[0].starts_with("getaddrinfo(rondi-no-such-host.invalid) failed: "));
    }

    #[test]
    fn empty_unix_path_fails_like_bind() {
        let reason = open_unix(Path::new(""), 0o600, None).unwrap_err();
        assert_eq!(reason, "bind() failed: No such file or directory.");
    }
}
