//! `-l` and `-L` listeners: Unix sockets published through a private
//! staging directory, and TCP sockets for every resolved address.

use super::config::{DEFAULT_PORT, ListenAddress, ListenerSpec};
use super::{errno_text, error_text};
use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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

/// A published Unix socket file. Dropping it removes the directory entry,
/// through the directory descriptor held since publication, if it is still
/// the inode this process bound.
#[derive(Debug)]
pub(crate) struct SocketFile {
    directory: Option<std::fs::File>,
    name: CString,
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
        directory: Some(open_directory(path.parent().unwrap()).unwrap()),
        name: CString::new(path.file_name().unwrap().as_encoded_bytes()).unwrap(),
        identity: Some((metadata.dev(), metadata.ino())),
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let (Some(identity), Some(directory)) = (self.identity, &self.directory) else {
            return;
        };
        let ours =
            stat_at(directory, &self.name).is_ok_and(|found| identity_of(&found) == identity);
        if ours {
            // SAFETY: the descriptor is open and the name NUL terminated;
            // unlinkat removes the entry and never follows a link.
            unsafe { libc::unlinkat(directory.as_raw_fd(), self.name.as_ptr(), 0) };
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
///
/// `before_threads` says the process has a single thread yet (see
/// `open_unix`).
pub(crate) fn open_all(
    specs: &[ListenerSpec],
    notes: &mut dyn Write,
    before_threads: bool,
) -> Result<Vec<Listener>, ()> {
    let mut opened = Vec::new();
    let mut failed = false;
    for spec in specs {
        let permissions: Option<Arc<[String]>> = spec.permissions.as_deref().map(Arc::from);
        let sockets = match &spec.address {
            ListenAddress::Unix(path) => {
                { open_unix(path, spec.access.mode(), spec.access.group, before_threads) }
                    .map(|(listener, file)| vec![BoundSocket::Unix(listener, file)])
                    .map_err(|reason| vec![reason])
            }
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
/// listener and left in place for the caller to remove. Called before the
/// JSON daemon starts any thread.
pub(crate) fn open_json_listener(path: &Path) -> std::io::Result<UnixListener> {
    let (listener, file) = open_unix(path, 0o600, None, true).map_err(std::io::Error::other)?;
    file.keep();
    Ok(listener)
}

/// Binds a Unix socket and publishes it at `path` with its final mode and
/// group already set.
///
/// `before_threads` says the process has a single thread, so the process
/// umask may be narrowed around the bind.
// Rondi policy: the socket is bound inside a private 0700 directory, given
// its group and mode there and renamed into place, so it is never reachable
// with a wider mode; every step after the directory checks goes through
// directory descriptors and checks the socket's identity, so no chown,
// chmod, rename or unlink can follow a path swapped in by another user; a
// live socket at the path is refused rather than replaced, as is any file
// that is not a socket; and the socket's directory and its ancestors must
// not be writable by another user (RCD-5, RCD-11).
fn open_unix(
    path: &Path,
    mode: u32,
    group: Option<u32>,
    before_threads: bool,
) -> Result<(UnixListener, SocketFile), String> {
    let shown = path.display();
    let bind_failed = |reason: String| format!("bind({shown}) failed: {reason}.");
    let io_failed = |error: std::io::Error| bind_failed(error_text(&error));
    if path.as_os_str().is_empty() {
        return Err(bind_failed(errno_text(libc::ENOENT)));
    }
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io_failed)?.join(path)
    };
    let (Some(directory), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(bind_failed(errno_text(libc::EISDIR)));
    };
    let directory = if directory.as_os_str().is_empty() {
        Path::new("/")
    } else {
        directory
    };
    let name =
        CString::new(name.as_encoded_bytes()).map_err(|_| bind_failed(errno_text(libc::EINVAL)))?;
    // Compatibility: a missing socket directory is created (opt-050).
    // Provenance: black-box compatibility test.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(directory)
        .map_err(io_failed)?;
    let parent = check_directory_chain(directory)?;
    match stat_at(&parent, &name) {
        Ok(found) if found.st_mode & libc::S_IFMT == libc::S_IFSOCK => {
            if UnixStream::connect(&target).is_ok() {
                return Err(format!("socket is already active: {shown}"));
            }
        }
        Ok(_) => return Err(format!("refusing to replace {shown}: not a socket")),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(error) => return Err(io_failed(error)),
    }
    let stage = Stage::create(&parent, directory).map_err(io_failed)?;
    let listener = stage.bind(before_threads).map_err(io_failed)?;
    let bound = stage.socket_identity().map_err(io_failed)?;
    if let Some(gid) = group {
        // SAFETY: the descriptor is open, the name is NUL terminated, and
        // AT_SYMLINK_NOFOLLOW keeps a swapped-in link from being followed.
        let changed = unsafe {
            libc::fchownat(
                stage.directory.as_raw_fd(),
                SOCKET_NAME.as_ptr(),
                u32::MAX,
                gid,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if changed != 0 || stage.socket_identity().ok() != Some(bound) {
            let error = std::io::Error::last_os_error();
            return Err(format!("chown({shown}) failed: {}", error_text(&error)));
        }
    }
    stage
        .set_mode(mode, bound)
        .map_err(|error| format!("chmod({shown}) failed: {}", error_text(&error)))?;
    // SAFETY: both descriptors are open directories and both names are NUL
    // terminated; renameat replaces a stale socket entry atomically.
    let renamed = unsafe {
        libc::renameat(
            stage.directory.as_raw_fd(),
            SOCKET_NAME.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
        )
    };
    if renamed != 0 {
        return Err(io_failed(std::io::Error::last_os_error()));
    }
    let published = stat_at(&parent, &name).map_err(io_failed)?;
    if identity_of(&published) != bound {
        // What was renamed into place is not the bound socket; take the
        // entry back out rather than publish it.
        // SAFETY: the descriptor is open and the name NUL terminated;
        // unlinkat never follows a link.
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
        return Err(bind_failed(
            "socket was replaced while it was published".to_owned(),
        ));
    }
    drop(stage);
    Ok((
        listener,
        SocketFile {
            directory: Some(parent),
            name,
            identity: Some(bound),
        },
    ))
}

/// The name a socket is bound under inside its staging directory.
const SOCKET_NAME: &CStr = c"s";

/// `fstatat` without following a final symbolic link.
pub(super) fn stat_at(directory: &std::fs::File, name: &CStr) -> std::io::Result<libc::stat> {
    // SAFETY: an all-zero stat is a valid out-parameter.
    let mut found: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open, the name is NUL terminated and
    // `found` is valid for writing.
    let status = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut found,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(found)
}

/// Device and inode, the identity compared before acting on an entry.
// The field types differ between platforms (st_dev is 32 bits on macOS).
#[allow(clippy::unnecessary_cast)]
pub(super) fn identity_of(found: &libc::stat) -> (u64, u64) {
    (found.st_dev as u64, found.st_ino as u64)
}

/// Opens a directory without following a final symbolic link.
pub(super) fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// A private directory, made by this process and held open, in which a
/// socket is bound. Dropping it removes whatever is left in it.
struct Stage {
    parent: std::fs::File,
    name: CString,
    directory: std::fs::File,
    path: PathBuf,
}

impl Stage {
    fn create(parent: &std::fs::File, shown: &Path) -> std::io::Result<Self> {
        let parent = parent.try_clone()?;
        let pid = std::process::id();
        for attempt in 0..100u32 {
            let name = CString::new(format!(".rrdcached-{pid}-{attempt}"))
                .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
            // SAFETY: the descriptor is open and the name NUL terminated.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: as above; O_NOFOLLOW refuses a link swapped in.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: fd is a new descriptor owned by nothing else.
            let directory = unsafe { std::fs::File::from_raw_fd(fd) };
            let path = shown.join(name.to_string_lossy().as_ref());
            let stage = Self {
                parent,
                name,
                directory,
                path,
            };
            let metadata = stage.directory.metadata()?;
            // SAFETY: geteuid has no preconditions.
            let owner = unsafe { libc::geteuid() };
            if metadata.uid() != owner || metadata.mode() & 0o7777 != 0o700 {
                return Err(std::io::Error::other(format!(
                    "staging directory in {} is not private",
                    shown.display()
                )));
            }
            return Ok(stage);
        }
        Err(std::io::Error::from_raw_os_error(libc::EEXIST))
    }

    /// Binds `s` in the stage. With `before_threads` the umask is narrowed
    /// so the socket is created 0600 rather than widened later.
    fn bind(&self, before_threads: bool) -> std::io::Result<UnixListener> {
        // SAFETY: umask only swaps the process mask; it is restored below.
        let previous_mask = before_threads.then(|| unsafe { libc::umask(0o077) });
        let bound = self.bind_by_name(before_threads);
        if let Some(mask) = previous_mask {
            // SAFETY: as above.
            unsafe { libc::umask(mask) };
        }
        bound
    }

    /// Binds `s` in the stage by path when the path fits a socket address.
    /// A longer path is bound by name from inside the stage, which changes
    /// the process directory and so is done only before any thread starts.
    /// Either way the caller then checks, through the stage descriptor, that
    /// the socket landed in this stage.
    fn bind_by_name(&self, before_threads: bool) -> std::io::Result<UnixListener> {
        let staged = self.path.join(SOCKET_NAME.to_string_lossy().as_ref());
        // SAFETY: an all-zero sockaddr_un is valid; only its size is read.
        let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        if staged.as_os_str().len() < capacity {
            return UnixListener::bind(&staged);
        }
        if !before_threads {
            return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        let previous = std::fs::File::open(".")?;
        // SAFETY: the stage descriptor is an open directory.
        if unsafe { libc::fchdir(self.directory.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let bound = UnixListener::bind(SOCKET_NAME.to_string_lossy().as_ref());
        // SAFETY: `previous` is an open directory descriptor.
        if unsafe { libc::fchdir(previous.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        bound
    }

    /// Device and inode of the bound socket, which must be a socket owned
    /// by this process's user.
    fn socket_identity(&self) -> std::io::Result<(u64, u64)> {
        let found = stat_at(&self.directory, SOCKET_NAME)?;
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        if found.st_mode & libc::S_IFMT != libc::S_IFSOCK || found.st_uid != owner {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(identity_of(&found))
    }

    /// Sets the socket's mode after checking it is still the bound socket.
    // Linux cannot change a mode through a name without following a link,
    // so it opens the entry itself (O_PATH, no follow), checks that this is
    // the bound socket, and changes the mode of that inode through its
    // /proc descriptor link. There is no fallback that follows a link.
    #[cfg(target_os = "linux")]
    fn set_mode(&self, mode: u32, bound: (u64, u64)) -> std::io::Result<()> {
        // SAFETY: the descriptor is open and the name NUL terminated.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                SOCKET_NAME.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd is a new descriptor owned by nothing else.
        let entry = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero stat is a valid out-parameter.
        let mut found: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open and `found` is valid for writing.
        if unsafe { libc::fstat(entry.as_raw_fd(), &mut found) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if found.st_mode & libc::S_IFMT != libc::S_IFSOCK || identity_of(&found) != bound {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        let link = CString::new(format!("/proc/self/fd/{}", entry.as_raw_fd()))
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: the name is NUL terminated; the /proc link names the
        // inode the descriptor holds, not the directory entry.
        if unsafe { libc::chmod(link.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sets the socket's mode after checking it is still the bound socket.
    #[cfg(not(target_os = "linux"))]
    fn set_mode(&self, mode: u32, bound: (u64, u64)) -> std::io::Result<()> {
        if self.socket_identity()? != bound {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        // SAFETY: the descriptor is open and the name NUL terminated;
        // AT_SYMLINK_NOFOLLOW changes a link itself, never its target.
        let changed = unsafe {
            libc::fchmodat(
                self.directory.as_raw_fd(),
                SOCKET_NAME.as_ptr(),
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if changed != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A link swapped in after the check took the mode instead; refuse
        // to publish then.
        if self.socket_identity()? != bound {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        // SAFETY: descriptors are open and names NUL terminated; unlinkat
        // removes the entry itself and never follows a link.
        unsafe {
            libc::unlinkat(self.directory.as_raw_fd(), SOCKET_NAME.as_ptr(), 0);
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            );
        }
    }
}

/// Refuses a socket directory that a user other than root or the daemon's
/// own could write, checking every component of its real path: each must
/// belong to root or the daemon's user, and must not be writable by others
/// or by a group other than root's unless it is sticky. Returns the
/// directory, opened without following a link and checked to be the one
/// examined.
fn check_directory_chain(directory: &Path) -> Result<std::fs::File, String> {
    let failed = |path: &Path, error: std::io::Error| {
        format!("bind({}) failed: {}.", path.display(), error_text(&error))
    };
    let real = std::fs::canonicalize(directory).map_err(|error| failed(directory, error))?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let mut last = None;
    for component in real.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata =
            std::fs::symlink_metadata(component).map_err(|error| failed(component, error))?;
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
        last = Some((metadata.dev(), metadata.ino()));
    }
    let opened = open_directory(&real).map_err(|error| failed(directory, error))?;
    let held = opened
        .metadata()
        .map_err(|error| failed(directory, error))?;
    if last != Some((held.dev(), held.ino())) {
        return Err(format!(
            "refusing socket directory {}: it changed while it was checked",
            real.display()
        ));
    }
    Ok(opened)
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
    use std::os::unix::fs::PermissionsExt;

    // Tests run on many threads, so they never narrow the umask.
    fn open_all(specs: &[ListenerSpec], notes: &mut dyn Write) -> Result<Vec<Listener>, ()> {
        super::open_all(specs, notes, false)
    }

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

    /// Another process keeps replacing the staged socket with a link to a
    /// victim file between the bind and the chown and chmod. Same-user
    /// races are the worst case: the staging directory keeps out every
    /// other user. The victim must never change, and a published socket
    /// must be the one that was bound.
    #[test]
    fn swapping_the_staged_socket_never_touches_its_target() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (_dir, root) = private_dir();
        let victim = root.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let racer = {
            let (stop, root, victim) = (Arc::clone(&stop), root.clone(), victim.clone());
            std::thread::spawn(move || {
                let pid = std::process::id();
                while !stop.load(Ordering::Relaxed) {
                    for attempt in 0..2 {
                        let staged = root.join(format!(".rrdcached-{pid}-{attempt}/s"));
                        if std::fs::remove_file(&staged).is_ok() {
                            let _ = std::os::unix::fs::symlink(&victim, &staged);
                        }
                    }
                }
            })
        };
        // SAFETY: getegid has no preconditions.
        let gid = unsafe { libc::getegid() };
        let socket = root.join("s.sock");
        for _ in 0..300 {
            if let Ok((listener, file)) = open_unix(&socket, 0o666, Some(gid), false) {
                let metadata = std::fs::symlink_metadata(&socket).unwrap();
                assert!(std::os::unix::fs::FileTypeExt::is_socket(
                    &metadata.file_type()
                ));
                assert_eq!(metadata.mode() & 0o7777, 0o666);
                drop((listener, file));
            }
            assert_eq!(std::fs::metadata(&victim).unwrap().mode() & 0o7777, 0o600);
        }
        stop.store(true, Ordering::Relaxed);
        racer.join().unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(!socket.exists());
    }

    #[test]
    fn empty_unix_path_fails_like_bind() {
        let reason = open_unix(Path::new(""), 0o600, None, false).unwrap_err();
        assert_eq!(reason, "bind() failed: No such file or directory.");
    }
}
