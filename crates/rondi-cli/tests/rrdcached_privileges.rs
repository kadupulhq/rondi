#![cfg(target_os = "linux")]

//! Root-only checks of rrdcached `-U`/`-G`: sockets and the pid file are
//! created as root, then every process id changes before anything is served
//! or written. docker/Dockerfile.test runs the suite as root and creates the
//! `rondi-test` account used here.

#[macro_use]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Account {
    uid: u32,
    gid: u32,
    /// `id -G rondi-test`, sorted: what initgroups installs.
    groups: Vec<u32>,
}

/// None, with a visible message, when the test cannot run here.
fn test_account(test: &str) -> Option<Account> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        oracle_skip!("skipping {test}: needs root (docker/Dockerfile.test runs it as root)");
        return None;
    }
    // SAFETY: the name is NUL terminated; the entry is copied immediately.
    let user = unsafe { libc::getpwnam(c"rondi-test".as_ptr()) };
    assert!(
        !user.is_null(),
        "{test} runs as root but the rondi-test user is missing; create it as docker/Dockerfile.test does"
    );
    // SAFETY: non-null passwd entry.
    let (uid, gid) = unsafe { ((*user).pw_uid, (*user).pw_gid) };
    let output = Command::new("id")
        .args(["-G", "rondi-test"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut groups = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|group| group.parse().unwrap())
        .collect::<Vec<u32>>();
    groups.sort_unstable();
    assert!(groups.len() >= 2, "rondi-test needs a supplementary group");
    Some(Account { uid, gid, groups })
}

fn daemon(name: &str, dir: &Path) -> PathBuf {
    if name == "upstream" {
        return PathBuf::from("rrdcached");
    }
    let alias = dir.join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    alias
}

fn request(socket: &Path, command: &str) -> Option<String> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(common::io_timeout())).unwrap();
    stream.write_all(command.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    Some(line)
}

fn wait_until_serving(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + common::io_timeout();
    while request(socket, "PING\n").is_none() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before serving: {status}");
        }
        assert!(Instant::now() < deadline, "daemon never answered PING");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The real, effective, saved, and filesystem ids from `/proc`.
fn proc_ids(pid: u32, field: &str) -> Vec<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .unwrap_or_else(|| panic!("no {field} in /proc/{pid}/status"));
    let mut ids = line
        .split_whitespace()
        .map(|id| id.parse().unwrap())
        .collect::<Vec<u32>>();
    if field == "Groups:" {
        ids.sort_unstable();
    }
    ids
}

/// A privileged port nothing is using, so binding it proves root.
fn privileged_port() -> u16 {
    (600..1000)
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .expect("no free privileged port")
}

#[test]
fn rrdcached_binds_as_root_then_runs_as_the_requested_account() {
    let Some(account) = test_account("rrdcached privilege drop") else {
        return;
    };
    let unprivileged_start =
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok())
            .unwrap_or(1024);
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut status_lines = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = base.join(name);
        // root:root 0755, so only root can create the socket and pid file.
        let sockets = run.join("sockets");
        let data = run.join("data");
        let journal = run.join("journal");
        for directory in [&sockets, &data, &journal] {
            std::fs::create_dir_all(directory).unwrap();
        }
        for directory in [&data, &journal] {
            std::os::unix::fs::chown(directory, Some(account.uid), Some(account.gid)).unwrap();
        }
        let socket = sockets.join("rrdcached.sock");
        let pid_file = run.join("rrdcached.pid");
        let port = privileged_port();
        let mut child = Command::new(daemon(name, &run))
            .args(["-g", "-B", "-b", data.to_str().unwrap()])
            .args(["-U", "rondi-test", "-G", "rondi-test", "-j"])
            .arg(&journal)
            .arg("-p")
            .arg(&pid_file)
            .arg("-l")
            .arg(format!("unix:{}", socket.display()))
            .args(["-l", &format!("127.0.0.1:{port}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_until_serving(&mut child, &socket);
        let pid = child.id();

        // Every id, including the saved ones that would let root come back.
        let uids = proc_ids(pid, "Uid:");
        let gids = proc_ids(pid, "Gid:");
        let groups = proc_ids(pid, "Groups:");
        assert_eq!(uids, [account.uid; 4], "{name} Uid");
        assert_eq!(gids, [account.gid; 4], "{name} Gid");
        // Upstream keeps the starting root process's supplementary groups;
        // Rondi installs the account's own list with initgroups.
        let expected_groups = if name == "upstream" {
            proc_ids(std::process::id(), "Groups:")
        } else {
            account.groups.clone()
        };
        assert_eq!(groups, expected_groups, "{name} Groups");
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let task = task.unwrap().file_name().into_string().unwrap();
            let status =
                std::fs::read_to_string(format!("/proc/{pid}/task/{task}/status")).unwrap();
            let thread_uids = status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .unwrap()
                .split_whitespace()
                .map(|id| id.parse().unwrap())
                .collect::<Vec<u32>>();
            assert!(
                thread_uids == [account.uid; 4],
                "{name} thread {task} kept another uid"
            );
        }
        status_lines.push((name, uids, gids, groups));

        // Bound before the drop: root owns the socket inode in a directory
        // only root can write, and the privileged port answers.
        let socket_metadata = std::fs::symlink_metadata(&socket).unwrap();
        assert_eq!(socket_metadata.uid(), 0, "{name} socket owner");
        assert_eq!(std::fs::metadata(&pid_file).unwrap().uid(), 0);
        if port < unprivileged_start {
            assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        }

        // Created after the drop: the RRD and the journal belong to the account.
        assert_eq!(
            request(
                &socket,
                "CREATE owned.rrd -b 1000000000 -s 10 DS:v:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
            )
            .as_deref(),
            Some("0 RRD created OK\n"),
            "{name} CREATE"
        );
        assert_eq!(
            std::fs::metadata(data.join("owned.rrd")).unwrap().uid(),
            account.uid
        );
        assert_eq!(
            request(&socket, "UPDATE owned.rrd 1000000010:1\n").as_deref(),
            Some("0 errors, enqueued 1 value(s).\n"),
            "{name} UPDATE"
        );
        let journal_files = std::fs::read_dir(&journal)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .collect::<Vec<_>>();
        assert!(!journal_files.is_empty(), "{name} wrote no journal");
        for metadata in journal_files {
            assert_eq!((metadata.uid(), metadata.gid()), (account.uid, account.gid));
        }

        // SAFETY: the child is a daemon started by this test.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
    }
    // Uid and Gid are identical to upstream's in this container.
    assert_eq!(status_lines[1].1, status_lines[0].1);
    assert_eq!(status_lines[1].2, status_lines[0].2);
}

#[test]
fn rrdcached_group_only_drop_replaces_root_supplementary_groups() {
    let Some(account) = test_account("rrdcached -G without -U") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let run = std::fs::canonicalize(dir.path()).unwrap();
    let socket = run.join("rrdcached.sock");
    let mut child = Command::new(daemon("rondi", &run))
        .args(["-g", "-b", run.to_str().unwrap(), "-G", "rondi-test", "-l"])
        .arg(&socket)
        .arg("-p")
        .arg(run.join("rrdcached.pid"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until_serving(&mut child, &socket);
    let pid = child.id();
    // The user stays root, as upstream; root's supplementary groups are
    // replaced by the target group alone.
    assert_eq!(proc_ids(pid, "Uid:"), [0; 4]);
    assert_eq!(proc_ids(pid, "Gid:"), [account.gid; 4]);
    assert_eq!(proc_ids(pid, "Groups:"), [account.gid]);
    // SAFETY: the child is a daemon started by this test.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();
}

#[test]
fn rrdcached_unknown_account_exits_before_binding_like_upstream() {
    if test_account("rrdcached unknown account").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    for (option, value) in [("-U", "rondi-no-such-user"), ("-G", "rondi-no-such-group")] {
        let mut outputs = Vec::new();
        for name in ["upstream", "rondi"] {
            let run = base.join(format!("{option}-{name}"));
            std::fs::create_dir_all(&run).unwrap();
            let socket = run.join("rrdcached.sock");
            let output = Command::new(daemon(name, &run))
                .args(["-g", "-b", run.to_str().unwrap(), option, value, "-l"])
                .arg(&socket)
                .arg("-p")
                .arg(run.join("rrdcached.pid"))
                .output()
                .unwrap();
            assert!(!socket.exists(), "{name} {option} bound before failing");
            outputs.push((output.status.code(), output.stdout, output.stderr));
        }
        assert_eq!(outputs[0].0, Some(5));
        assert_eq!(outputs[1], outputs[0], "{option}");
    }
}

#[test]
fn rrdcached_systemd_unit_never_runs_with_group_zero() {
    let Some(account) = test_account("rrdcached systemd unit identity") else {
        return;
    };
    let unit = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/systemd/rondi-rrdcached.service"
    ))
    .unwrap();
    let setting = |key: &str| {
        unit.lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::to_owned)
    };
    let exec = setting("ExecStart=").expect("unit has ExecStart");
    let args = exec.split_whitespace().skip(1).collect::<Vec<_>>();
    // With -U alone upstream keeps the starting egid, so a root start would
    // run with gid 0. The unit must either drop with both or start unprivileged.
    if args.contains(&"-U") {
        assert!(args.contains(&"-G"), "unit passes -U without -G");
    } else {
        assert!(setting("User=").is_some() && setting("Group=").is_some());
    }

    // Run the unit's command as the unit would: as its unprivileged account
    // (rondi-test stands in for User=/Group=), with its directories moved
    // under a temporary root.
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let remap = |arg: &str| -> String {
        let arg = arg.replace("/var/lib/", &format!("{}/var/lib/", base.display()));
        arg.replace("/run/", &format!("{}/run/", base.display()))
    };
    let mut command_args = Vec::new();
    let mut previous = "";
    for arg in &args {
        command_args.push(match previous {
            "-s" | "-U" | "-G" => "rondi-test".to_owned(),
            _ => remap(arg),
        });
        previous = arg;
    }
    for directory in ["var/lib/rondi", "var/lib/rondi-journal", "run/rondi"] {
        let path = base.join(directory);
        std::fs::create_dir_all(&path).unwrap();
        std::os::unix::fs::chown(&path, Some(account.uid), Some(account.gid)).unwrap();
    }
    let socket = base.join("run/rondi/rrdcached.sock");
    let mut command = Command::new(daemon("rondi", &base));
    if !args.contains(&"-U") {
        use std::os::unix::process::CommandExt;
        command.uid(account.uid).gid(account.gid);
    }
    let mut child = command
        .args(&command_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until_serving(&mut child, &socket);
    let pid = child.id();
    assert!(!proc_ids(pid, "Gid:").contains(&0), "unit runs with gid 0");
    assert!(!proc_ids(pid, "Uid:").contains(&0), "unit runs as root");
    // SAFETY: the child is a daemon started by this test.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();
}

#[test]
fn rrdcached_root_refuses_a_socket_directory_another_user_can_write() {
    let Some(account) = test_account("rrdcached socket directory ownership") else {
        return;
    };
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    // (directory owner, mode, expected to start)
    let cases = [
        (account.uid, 0o755, false),
        (0, 0o777, false),
        (0, 0o1777, true),
        (0, 0o755, true),
    ];
    for (index, (owner, mode, starts)) in cases.into_iter().enumerate() {
        let run = base.join(format!("case-{index}"));
        let sockets = run.join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        std::os::unix::fs::chown(&sockets, Some(owner), Some(0)).unwrap();
        std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(mode)).unwrap();
        let socket = sockets.join("rrdcached.sock");
        let mut child = Command::new(daemon("rondi", &run))
            .args(["-g", "-b", run.to_str().unwrap(), "-m", "0660", "-l"])
            .arg(&socket)
            .arg("-p")
            .arg(run.join("rrdcached.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if starts {
            wait_until_serving(&mut child, &socket);
            let metadata = std::fs::symlink_metadata(&socket).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o660, "case {index}");
            // Only the socket remains; the staging directory is gone.
            let entries = std::fs::read_dir(&sockets).unwrap().count();
            assert_eq!(entries, 1, "case {index} left a staging directory");
            // SAFETY: the child is a daemon started by this test.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            child.wait().unwrap();
        } else {
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1), "case {index}");
            assert!(
                String::from_utf8_lossy(&output.stderr).starts_with(&format!(
                    "rrdcached: refusing socket directory {}: another user can write it\n",
                    sockets.display()
                )),
                "case {index}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_dir(&sockets).unwrap().count(), 0);
        }
    }
}
