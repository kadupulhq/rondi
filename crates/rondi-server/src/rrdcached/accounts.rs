//! `-U`, `-G` and `-s` accounts: lookup while the options are read, and
//! the change of process identity after the sockets are bound.

use super::config::Account;
use std::ffi::{CStr, CString};

/// Resolves `-U`: a user name, or else a uid the account database knows.
// Compatibility: the numeric form is read like `-G` (C `strtoul` over the
// whole value, wrapped to 32 bits). An unknown name or id is refused with
// status 5 by the caller (RCD-1).
// Provenance: black-box compatibility test.
pub fn lookup_user(value: &str) -> Option<Account> {
    let name = CString::new(value).ok()?;
    if let Some(account) = query_user(|entry, buffer, found| {
        // SAFETY: every pointer refers to live, correctly sized storage.
        unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                found,
            )
        }
    }) {
        return Some(account);
    }
    let uid = wrap_to_id(c_strtoul_whole(value)?);
    query_user(|entry, buffer, found| {
        // SAFETY: as above.
        unsafe { libc::getpwuid_r(uid, entry, buffer.as_mut_ptr().cast(), buffer.len(), found) }
    })
}

/// Resolves `-G`: a group name, or else a gid the group database knows.
// Compatibility: the numeric form is read like C `strtoul` over the whole
// value (white space and a sign allowed, an empty value is 0), wrapped to
// 32 bits. Provenance: black-box compatibility test.
pub fn lookup_group(value: &str) -> Option<u32> {
    if let Some(gid) = group_by_name(value) {
        return Some(gid);
    }
    let gid = wrap_to_id(c_strtoul_whole(value)?);
    group_exists(gid).then_some(gid)
}

/// Resolves `-s`: a group name, or else a gid the group database knows.
// Compatibility: the numeric form is read like C `atoi`: leading digits
// after optional white space and sign, the rest ignored, and a result of
// zero refused (so `-s 0` and `-s ''` fail while `-s 20x` is group 20).
// Provenance: black-box compatibility test.
pub fn lookup_socket_group(value: &str) -> Option<u32> {
    if let Some(gid) = group_by_name(value) {
        return Some(gid);
    }
    let number = c_atoi(value);
    if number == 0 {
        return None;
    }
    let gid = number as u32;
    group_exists(gid).then_some(gid)
}

type Lookup<'a, T> = dyn FnMut(&mut T, &mut Vec<u8>, &mut *mut T) -> libc::c_int + 'a;

/// Runs a `get*_r` query, growing the buffer on ERANGE.
fn query<T>(query: &mut Lookup<'_, T>) -> Option<(T, Vec<u8>)> {
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: passwd and group are plain C structs; all-zero is a
        // valid out-parameter for the get*_r functions.
        let mut entry: T = unsafe { std::mem::zeroed() };
        let mut found: *mut T = std::ptr::null_mut();
        let status = query(&mut entry, &mut buffer, &mut found);
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            let larger = buffer.len() * 4;
            buffer.resize(larger, 0);
            continue;
        }
        return (status == 0 && !found.is_null()).then_some((entry, buffer));
    }
}

fn query_user(
    mut lookup: impl FnMut(&mut libc::passwd, &mut Vec<u8>, &mut *mut libc::passwd) -> libc::c_int,
) -> Option<Account> {
    let (entry, buffer) = query(&mut lookup)?;
    // SAFETY: a successful lookup leaves pw_name pointing at a
    // NUL-terminated string inside `buffer`, which is still alive.
    let name = unsafe { CStr::from_ptr(entry.pw_name) }.to_owned();
    drop(buffer);
    Some(Account {
        name,
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    })
}

fn group_by_name(value: &str) -> Option<u32> {
    let name = CString::new(value).ok()?;
    let mut lookup =
        |entry: &mut libc::group, buffer: &mut Vec<u8>, found: &mut *mut libc::group| {
            // SAFETY: every pointer refers to live, correctly sized storage.
            unsafe {
                libc::getgrnam_r(
                    name.as_ptr(),
                    entry,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    found,
                )
            }
        };
    query(&mut lookup).map(|(entry, _)| entry.gr_gid)
}

fn group_exists(gid: u32) -> bool {
    let mut lookup =
        |entry: &mut libc::group, buffer: &mut Vec<u8>, found: &mut *mut libc::group| {
            // SAFETY: as above.
            unsafe { libc::getgrgid_r(gid, entry, buffer.as_mut_ptr().cast(), buffer.len(), found) }
        };
    query(&mut lookup).is_some()
}

/// An unsigned long cut to an id, as a C cast to `uid_t`/`gid_t` does.
fn wrap_to_id(value: u64) -> u32 {
    value as u32
}

const C_SPACE: [char; 6] = [' ', '\t', '\n', '\x0b', '\x0c', '\r'];

/// C `strtoul` that must consume the whole value: `None` if anything but
/// white space, one sign and digits is present. Negative values wrap and
/// overlong ones saturate, as in C.
fn c_strtoul_whole(value: &str) -> Option<u64> {
    let text = value.trim_start_matches(C_SPACE);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.is_empty() {
        // No digits: nothing is converted and the end pointer stays at the
        // start, so only an empty value counts as wholly read.
        return value.is_empty().then_some(0);
    }
    let magnitude = digits.parse::<u64>().unwrap_or(u64::MAX);
    Some(if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

/// C `atoi`: the leading number, with the rest of the text ignored.
fn c_atoi(value: &str) -> i32 {
    let text = value.trim_start_matches(C_SPACE);
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let magnitude = rest[..digits]
        .parse::<i64>()
        .unwrap_or(if digits == 0 { 0 } else { i64::MAX });
    let number = if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    };
    number as i32
}

/// Whether the daemon would keep serving with root's identity.
pub(crate) fn serves_as_root(user: Option<&Account>) -> bool {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    euid == 0 && user.is_none_or(|account| account.uid == 0)
}

/// Changes to the `-G` group and `-U` user. Runs after the sockets and the
/// pid file exist and before any thread starts, so every thread inherits
/// the new identity.
///
/// Returns the line to print when a change fails.
// Compatibility: `daemonize: failed to setgid(<gid>)` and
// `daemonize: failed to setuid(<uid>)`, the group first.
// Provenance: black-box compatibility test.
// Rondi policy: started as root, the daemon installs the target account's
// supplementary groups (or only the `-G` group) and, for `-U` without
// `-G`, switches to the account's primary group, so no root group
// survives the drop.
pub(crate) fn change_identity(user: Option<&Account>, group: Option<u32>) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    let privileged = unsafe { libc::geteuid() } == 0;
    let gid = group.or_else(|| user.filter(|_| privileged).map(|account| account.gid));
    if let Some(gid) = gid {
        let failed = || format!("daemonize: failed to setgid({gid})");
        if privileged {
            let installed = match user {
                // SAFETY: the name is NUL terminated and outlives the call.
                Some(account) => unsafe { libc::initgroups(account.name.as_ptr(), gid as _) },
                // SAFETY: one valid gid_t is passed with its length.
                None => unsafe { libc::setgroups(1, &gid) },
            };
            if installed != 0 {
                return Err(failed());
            }
        }
        if !set_group(gid) {
            return Err(failed());
        }
    }
    if let Some(account) = user {
        let uid = account.uid;
        let failed = || format!("daemonize: failed to setuid({uid})");
        if !set_user(uid) {
            return Err(failed());
        }
        // Rondi policy: after leaving root, the ids are read back and root
        // must be out of reach.
        if privileged && uid != 0 {
            // SAFETY: getuid, geteuid and setuid have no memory preconditions.
            let kept_root =
                unsafe { libc::getuid() != uid || libc::geteuid() != uid || libc::setuid(0) == 0 };
            if kept_root {
                return Err(failed());
            }
        }
    }
    Ok(())
}

/// Sets the real, effective and saved group ids.
fn set_group(gid: u32) -> bool {
    #[cfg(target_os = "linux")]
    // SAFETY: setresgid has no memory preconditions.
    let changed = unsafe { libc::setresgid(gid, gid, gid) == 0 };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: setgid has no memory preconditions; as root it sets all three.
    let changed = unsafe { libc::setgid(gid) == 0 };
    changed
}

/// Sets the real, effective and saved user ids.
fn set_user(uid: u32) -> bool {
    #[cfg(target_os = "linux")]
    // SAFETY: setresuid has no memory preconditions.
    let changed = unsafe { libc::setresuid(uid, uid, uid) == 0 };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: setuid has no memory preconditions; as root it sets all three.
    let changed = unsafe { libc::setuid(uid) == 0 };
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_account_resolves_by_name_and_number() {
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let by_number = lookup_user(&uid.to_string()).expect("current uid has an entry");
        assert_eq!(by_number.uid, uid);
        let by_name = lookup_user(by_number.name.to_str().unwrap()).unwrap();
        assert_eq!(by_name, by_number);
        assert_eq!(lookup_group(&gid.to_string()), Some(gid));
        assert_eq!(lookup_group(&format!(" +{gid}")), Some(gid));
        assert_eq!(
            lookup_socket_group(&format!("{gid}x")),
            (gid != 0).then_some(gid)
        );
    }

    #[test]
    fn numeric_ids_follow_each_options_grammar() {
        // Root (uid and gid 0) exists on every supported system.
        for accepted in ["", "0", " +0", "-0", "4294967296"] {
            assert_eq!(
                lookup_user(accepted).map(|account| account.uid),
                Some(0),
                "{accepted:?}"
            );
        }
        for refused in ["0x", " ", "a\0b", "rondi-no-such-user-x"] {
            assert!(lookup_user(refused).is_none(), "{refused:?}");
        }
        for accepted in ["", "0", " +0", "4294967296"] {
            assert_eq!(lookup_group(accepted), Some(0), "{accepted:?}");
        }
        for refused in ["0x", "a\0b", "rondi-no-such-group-x"] {
            assert!(lookup_group(refused).is_none(), "{refused:?}");
        }
        for refused in ["", "0", "4294967296", "rondi-no-such-group-x"] {
            assert!(lookup_socket_group(refused).is_none(), "{refused:?}");
        }
    }

    #[test]
    fn c_number_readers() {
        assert_eq!(c_strtoul_whole(" +12"), Some(12));
        assert_eq!(c_strtoul_whole("-1"), Some(u64::MAX));
        assert_eq!(c_strtoul_whole(""), Some(0));
        assert_eq!(c_strtoul_whole(" "), None);
        assert_eq!(c_strtoul_whole("+"), None);
        assert_eq!(c_strtoul_whole("1x"), None);
        assert_eq!(c_atoi(" 20x"), 20);
        assert_eq!(c_atoi("-1"), -1);
        assert_eq!(c_atoi("x"), 0);
        assert_eq!(c_atoi("4294967296"), 0);
    }

    #[test]
    fn keeping_the_current_identity_succeeds() {
        // SAFETY: as above.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        if uid == 0 {
            return;
        }
        let account = lookup_user(&uid.to_string()).unwrap();
        assert_eq!(change_identity(Some(&account), Some(gid)), Ok(()));
        assert_eq!(change_identity(None, None), Ok(()));
    }
}
