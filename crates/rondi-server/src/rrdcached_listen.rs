//! rrdcached listener setup and the post-bind privilege drop, ported from
//! RRDtool 1.11.0 `src/rrd_daemon.c` (`open_listen_socket_unix`,
//! `open_listen_socket_network`, `open_listen_socket`, and `daemonize`).
//! Both run before the async runtime starts any thread, matching upstream's
//! rule that threads are created only at the final privilege level.

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

const DEFAULT_PORT: &CStr = c"42217";
const NI_MAXHOST: usize = 1025;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
const LISTEN_BACKLOG: libc::c_int = -1;
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const LISTEN_BACKLOG: libc::c_int = 511;

/// One `-l` or `-L` entry with the `-m`, `-s`, and `-P` values in effect
/// when it was named.
#[derive(Debug, Clone)]
pub struct RrdcachedListenAddress {
    pub address: String,
    pub mode: Option<u32>,
    pub group: Option<u32>,
    pub commands: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum RrdcachedSocket {
    Unix {
        listener: std::os::unix::net::UnixListener,
        path: PathBuf,
    },
    Tcp(std::net::TcpListener),
}

/// A bound, listening socket and the commands its clients may use.
#[derive(Debug)]
pub struct RrdcachedListener {
    pub socket: RrdcachedSocket,
    pub commands: Option<Vec<String>>,
}

/// RRDtool prints `rrd_strerror(errno)`, which is plain `strerror` text.
fn strerror(errno: i32) -> String {
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    match text.rfind(" (os error ") {
        Some(index) => text[..index].to_owned(),
        None => text,
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// POSIX `dirname` for the socket path.
fn dirname(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.starts_with('/') { "/" } else { "." };
    }
    match trimmed.rfind('/') {
        None => ".",
        Some(index) => match trimmed[..index].trim_end_matches('/') {
            "" => "/",
            parent => parent,
        },
    }
}

fn set_cloexec(fd: &OwnedFd) -> bool {
    // SAFETY: fd is an open descriptor owned by the caller.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0 {
        return true;
    }
    eprintln!("rrdcached: fcntl(FD_CLOEXEC) failed: {}", strerror(errno()));
    false
}

/// Open one listener as `open_listen_socket` does: `unix:` and absolute
/// paths are Unix sockets, everything else is a network address. Each
/// failure is reported with upstream's text. Upstream then carries on with
/// whatever opened; Rondi's caller instead refuses to start when this
/// returns false, so a daemon never serves with a listener missing.
pub fn open_rrdcached_listener(
    address: &RrdcachedListenAddress,
    listeners: &mut Vec<RrdcachedListener>,
) -> bool {
    if address.address.starts_with("unix:") || address.address.starts_with('/') {
        open_unix(address, listeners)
    } else {
        open_network(address, listeners)
    }
}

fn open_unix(sock: &RrdcachedListenAddress, listeners: &mut Vec<RrdcachedListener>) -> bool {
    let path = sock.address.strip_prefix("unix:").unwrap_or(&sock.address);
    let dir = dirname(path);
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(error) = std::fs::DirBuilder::new()
            .recursive(true)
            // Upstream asks for 0777 and lets the umask trim it; Rondi never
            // creates a directory writable beyond its owner.
            .mode(0o755)
            .create(dir)
        {
            eprintln!(
                "Failed to create socket directory '{dir}': {}",
                strerror(error.raw_os_error().unwrap_or(0))
            );
            return false;
        }
    }
    // Upstream unlinks whatever is at the path because the pid file proves
    // no other daemon owns it. Rondi refuses a live socket or a non-socket,
    // and otherwise renames the new socket over a stale one.
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            eprintln!("rrdcached: socket is already active: {path}");
            return false;
        }
        if !metadata.file_type().is_socket() {
            eprintln!("rrdcached: refusing to replace non-socket path {path}");
            return false;
        }
    }
    let Some(staging) = StagingDir::create(dir) else {
        eprintln!("rrdcached: bind({path}) failed: {}.", strerror(errno()));
        return false;
    };
    let Some(fd) = bind_staged(sock, path, &staging) else {
        return false;
    };
    // The socket is complete and listening before its name appears, and
    // rename(2) replaces a stale socket in one step.
    if std::fs::rename(staging.socket(), path).is_err() {
        eprintln!("rrdcached: bind({path}) failed: {}.", strerror(errno()));
        return false;
    }
    listeners.push(RrdcachedListener {
        socket: RrdcachedSocket::Unix {
            listener: fd.into(),
            path: PathBuf::from(path),
        },
        commands: sock.commands.clone(),
    });
    true
}

/// A fresh mode-0700 directory beside the socket path. The socket is bound,
/// chowned, and chmodded inside it, where no other user can swap the name
/// for a symlink or hard link between bind(2) and chown/chmod.
struct StagingDir(PathBuf);

impl StagingDir {
    const SOCKET: &str = "s";

    fn create(dir: &str) -> Option<Self> {
        let template = CString::new(format!("{dir}/.rrdcached.XXXXXX")).ok()?;
        let mut template = template.into_bytes_with_nul();
        // SAFETY: template is a writable NUL-terminated buffer ending in
        // XXXXXX, as mkdtemp requires.
        let created = unsafe { libc::mkdtemp(template.as_mut_ptr().cast::<libc::c_char>()) };
        if created.is_null() {
            return None;
        }
        template.pop();
        Some(Self(PathBuf::from(String::from_utf8(template).ok()?)))
    }

    fn socket(&self) -> PathBuf {
        self.0.join(Self::SOCKET)
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        // After a successful rename only the empty directory remains.
        let _ = std::fs::remove_file(self.socket());
        if let Err(error) = std::fs::remove_dir(&self.0) {
            eprintln!("rrdcached: rmdir({}) failed: {error}", self.0.display());
        }
    }
}

/// Restores the working directory when dropped.
struct WorkingDirectory(OwnedFd);

impl WorkingDirectory {
    /// Enter `dir`. Only for single-threaded startup: the working directory
    /// is process-wide.
    fn enter(dir: &std::path::Path) -> Option<Self> {
        let previous = std::fs::File::open(".").ok()?;
        std::env::set_current_dir(dir).ok()?;
        Some(Self(previous.into()))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        // SAFETY: the descriptor is an open directory owned by self.
        if unsafe { libc::fchdir(self.0.as_raw_fd()) } != 0 {
            // Relative -b, -j, and -p paths would now resolve elsewhere.
            panic!("rrdcached: cannot restore the working directory");
        }
    }
}

/// Bind, set ownership and mode, and listen on the staged socket, binding
/// the short relative name so a long `path` still fits `sun_path`. Errors
/// use upstream's text with the final `path`.
fn bind_staged(sock: &RrdcachedListenAddress, path: &str, staging: &StagingDir) -> Option<OwnedFd> {
    let Some(_cwd) = WorkingDirectory::enter(&staging.0) else {
        eprintln!("rrdcached: bind({path}) failed: {}.", strerror(errno()));
        return None;
    };
    let name = c"s";
    // SAFETY: socket(2) has no memory preconditions.
    let fd = unsafe { libc::socket(libc::PF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        eprintln!("rrdcached: unix socket(2) failed: {}", strerror(errno()));
        return None;
    }
    // SAFETY: fd was just returned by socket(2) and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if !set_cloexec(&fd) {
        return None;
    }
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    address.sun_path[0] = StagingDir::SOCKET.as_bytes()[0] as libc::c_char;
    // SAFETY: address is a fully initialized sockaddr_un and the length
    // passed is its size.
    let status = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if status != 0 {
        eprintln!("rrdcached: bind({path}) failed: {}.", strerror(errno()));
        return None;
    }
    // Upstream reports a failure here and listens anyway; Rondi reports it
    // and does not start.
    if let Some(group) = sock.group {
        // SAFETY: name is NUL terminated and static.
        let failed = unsafe {
            libc::chown(name.as_ptr(), libc::getuid(), group as libc::gid_t) != 0
                || libc::chmod(name.as_ptr(), 0o760) != 0
        };
        if failed {
            eprintln!(
                "rrdcached: failed to set socket group permissions ({})",
                strerror(errno())
            );
            return None;
        }
    }
    if let Some(mode) = sock.mode {
        // SAFETY: name is NUL terminated and static.
        if unsafe { libc::chmod(name.as_ptr(), mode as libc::mode_t) } != 0 {
            eprintln!(
                "rrdcached: failed to set socket file permissions ({mode:o}): {}",
                strerror(errno())
            );
            return None;
        }
    }
    // SAFETY: fd is a bound socket owned by this function.
    if unsafe { libc::listen(fd.as_raw_fd(), LISTEN_BACKLOG) } != 0 {
        eprintln!("rrdcached: listen({path}) failed: {}.", strerror(errno()));
        return None;
    }
    Some(fd)
}

/// Frees a getaddrinfo result list on every exit path.
struct AddrInfo(*mut libc::addrinfo);

impl Drop for AddrInfo {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a successful getaddrinfo call and
            // is freed exactly once.
            unsafe { libc::freeaddrinfo(self.0) };
        }
    }
}

fn set_int_option(fd: &OwnedFd, level: libc::c_int, name: libc::c_int) -> bool {
    let one: libc::c_int = 1;
    // SAFETY: the option value points to a live c_int of the given size.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const one).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

fn open_network(sock: &RrdcachedListenAddress, listeners: &mut Vec<RrdcachedListener>) -> bool {
    let bytes = sock.address.as_bytes();
    // strncpy into a NI_MAXHOST buffer keeps at most NI_MAXHOST - 1 bytes.
    let addr_copy = &bytes[..bytes.len().min(NI_MAXHOST - 1)];
    let (host, port): (&[u8], Option<&[u8]>) = if addr_copy.first() == Some(&b'[') {
        let rest = &addr_copy[1..];
        let Some(close) = rest.iter().position(|byte| *byte == b']') else {
            eprintln!("rrdcached: Malformed address: {}", sock.address);
            return false;
        };
        let after = &rest[close + 1..];
        let port = match after.first() {
            Some(b':') => Some(&after[1..]),
            None => None,
            Some(_) => {
                eprintln!(
                    "rrdcached: Garbage after address: {}",
                    String::from_utf8_lossy(after)
                );
                return false;
            }
        };
        (&rest[..close], port)
    } else {
        match addr_copy.iter().rposition(|byte| *byte == b':') {
            Some(index) => (&addr_copy[..index], Some(&addr_copy[index + 1..])),
            None => (addr_copy, None),
        }
    };
    let host_text = String::from_utf8_lossy(host);
    let wildcard = host.is_empty();

    // SAFETY: addrinfo is plain old data; all-zero is a valid hints value.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_flags = libc::AI_ADDRCONFIG;
    hints.ai_family = libc::AF_UNSPEC;
    hints.ai_socktype = libc::SOCK_STREAM;
    if wildcard {
        hints.ai_flags |= libc::AI_PASSIVE;
    }
    let (Ok(c_host), Ok(c_port)) = (CString::new(host), port.map(CString::new).transpose()) else {
        eprintln!("rrdcached: Malformed address: {}", sock.address);
        return false;
    };
    let mut result = std::ptr::null_mut();
    // SAFETY: node and service are NUL-terminated strings (or null for a
    // wildcard node) that outlive the call; hints is initialized.
    let status = unsafe {
        libc::getaddrinfo(
            if wildcard {
                std::ptr::null()
            } else {
                c_host.as_ptr()
            },
            c_port.as_deref().unwrap_or(DEFAULT_PORT).as_ptr(),
            &hints,
            &mut result,
        )
    };
    if status != 0 {
        // SAFETY: gai_strerror returns a static NUL-terminated string.
        let detail = unsafe { CStr::from_ptr(libc::gai_strerror(status)) };
        eprintln!(
            "rrdcached: getaddrinfo({host_text}) failed: {}",
            detail.to_string_lossy()
        );
        return false;
    }
    let result = AddrInfo(result);

    let mut opened = true;
    let mut entry = result.0;
    // SAFETY: every node is part of the list `result` owns until it drops
    // after this loop; as_ref yields None at the terminating null.
    while let Some(info) = unsafe { entry.as_ref() } {
        entry = info.ai_next;
        // SAFETY: socket(2) has no memory preconditions.
        let fd = unsafe { libc::socket(info.ai_family, info.ai_socktype, info.ai_protocol) };
        if fd < 0 {
            eprintln!(
                "rrdcached: network socket(2) failed: {}.",
                strerror(errno())
            );
            opened = false;
            continue;
        }
        // SAFETY: fd was just returned by socket(2) and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if !set_cloexec(&fd) {
            return false;
        }
        if !set_int_option(&fd, libc::SOL_SOCKET, libc::SO_REUSEADDR) {
            eprintln!(
                "rrdcached: setsockopt(SO_REUSEADDR) failed: {}",
                strerror(errno())
            );
            return false;
        }
        if !set_int_option(&fd, libc::IPPROTO_TCP, libc::TCP_NODELAY) {
            eprintln!(
                "rrdcached: setsockopt(TCP_NODELAY) failed: {}",
                strerror(errno())
            );
            return false;
        }
        if info.ai_family == libc::AF_INET6
            && !set_int_option(&fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)
        {
            eprintln!(
                "rrdcached: setsockopt(IPV6_V6ONLY) failed: {}",
                strerror(errno())
            );
            return false;
        }
        // SAFETY: ai_addr and ai_addrlen describe a valid address from
        // getaddrinfo.
        if unsafe { libc::bind(fd.as_raw_fd(), info.ai_addr, info.ai_addrlen) } != 0 {
            eprintln!(
                "rrdcached: bind({}) failed: {}.",
                sock.address,
                strerror(errno())
            );
            opened = false;
            continue;
        }
        // SAFETY: fd is a bound socket owned by this loop iteration.
        if unsafe { libc::listen(fd.as_raw_fd(), LISTEN_BACKLOG) } != 0 {
            // The stray "\n." is upstream's format string.
            eprintln!(
                "rrdcached: listen({}) failed: {}\n.",
                sock.address,
                strerror(errno())
            );
            return false;
        }
        listeners.push(RrdcachedListener {
            socket: RrdcachedSocket::Tcp(fd.into()),
            commands: sock.commands.clone(),
        });
    }
    opened
}

/// The account `-U` named, kept for its supplementary group list.
#[derive(Debug, Clone)]
pub struct RrdcachedUser {
    pub uid: libc::uid_t,
    pub name: CString,
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn set_all_gids(gid: libc::gid_t) -> bool {
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    // SAFETY: the out-pointers refer to live locals.
    let changed = unsafe {
        libc::setresgid(gid, gid, gid) == 0
            && libc::getresgid(&mut real, &mut effective, &mut saved) == 0
    };
    changed && [real, effective, saved] == [gid; 3]
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
fn set_all_gids(gid: libc::gid_t) -> bool {
    // A root caller's setgid sets the real, effective, and saved ids.
    // SAFETY: setgid and the getters have no memory preconditions.
    unsafe { libc::setgid(gid) == 0 && libc::getgid() == gid && libc::getegid() == gid }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn set_all_uids(uid: libc::uid_t) -> bool {
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    // SAFETY: the out-pointers refer to live locals.
    let changed = unsafe {
        libc::setresuid(uid, uid, uid) == 0
            && libc::getresuid(&mut real, &mut effective, &mut saved) == 0
    };
    changed && [real, effective, saved] == [uid; 3]
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
fn set_all_uids(uid: libc::uid_t) -> bool {
    // A root caller's setuid sets the real, effective, and saved ids.
    // SAFETY: setuid and the getters have no memory preconditions.
    unsafe { libc::setuid(uid) == 0 && libc::getuid() == uid && libc::geteuid() == uid }
}

/// The privilege change at the end of `daemonize`: setgid when the
/// effective group differs, then setuid when the effective user differs,
/// with upstream's log line as the error text.
///
/// Two protections clients cannot observe are added. A root caller first
/// replaces its supplementary groups (the `-U` account's list via
/// `initgroups`, otherwise just the target group), where upstream keeps
/// root's. Every id is set with the real, effective, and saved values
/// together, read back, and root must be unrecoverable afterwards.
pub fn drop_rrdcached_privileges(
    user: Option<&RrdcachedUser>,
    gid: libc::gid_t,
) -> Result<(), String> {
    // SAFETY: the get*id calls have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let uid = user.map_or(euid, |user| user.uid);
    if euid == 0 && (uid != euid || gid != egid) {
        match user.filter(|user| user.uid != euid) {
            Some(user) => {
                // SAFETY: the name is NUL terminated and outlives the call.
                if unsafe { libc::initgroups(user.name.as_ptr(), gid as _) } != 0 {
                    return Err(format!(
                        "daemonize: failed to initgroups({}, {gid})",
                        user.name.to_string_lossy()
                    ));
                }
            }
            None => {
                let mut groups = [0 as libc::gid_t; 2];
                // SAFETY: setgroups reads one gid; getgroups writes at most
                // two into the live array.
                let replaced = unsafe {
                    libc::setgroups(1, &gid) == 0 && libc::getgroups(2, groups.as_mut_ptr()) == 1
                };
                if !replaced || groups[0] != gid {
                    return Err(format!("daemonize: failed to setgroups({gid})"));
                }
            }
        }
    }
    if egid != gid && !set_all_gids(gid) {
        return Err(format!("daemonize: failed to setgid({gid})"));
    }
    if euid != uid {
        // SAFETY: setuid has no memory preconditions; it must fail here.
        if !set_all_uids(uid) || (uid != 0 && unsafe { libc::setuid(0) } == 0) {
            return Err(format!("daemonize: failed to setuid({uid})"));
        }
    }
    Ok(())
}

fn shutdown_signal_mask(how: libc::c_int) {
    // SAFETY: the set is initialized by sigemptyset before use and the old
    // mask pointer may be null.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        // pthread_sigmask fails only for an invalid `how`, which the two
        // callers below never pass.
        let status = libc::pthread_sigmask(how, &set, std::ptr::null_mut());
        assert_eq!(status, 0, "pthread_sigmask failed");
    }
}

/// Hold SIGINT and SIGTERM from before the pid file and sockets exist until
/// the daemon's handlers are installed, so an early signal cannot kill the
/// process with the default action and leave both behind. Threads spawned
/// meanwhile inherit the mask, so delivery goes to the unblocking thread.
pub fn block_shutdown_signals() {
    shutdown_signal_mask(libc::SIG_BLOCK);
}

pub(crate) fn unblock_shutdown_signals() {
    shutdown_signal_mask(libc::SIG_UNBLOCK);
}

#[cfg(test)]
mod tests {
    use super::dirname;

    #[test]
    fn dirname_follows_posix() {
        assert_eq!(dirname("/tmp/rrdcached.sock"), "/tmp");
        assert_eq!(dirname("/rrdcached.sock"), "/");
        assert_eq!(dirname("rrdcached.sock"), ".");
        assert_eq!(dirname("/a//b/"), "/a");
        assert_eq!(dirname(""), ".");
        assert_eq!(dirname("///"), "/");
    }
}
