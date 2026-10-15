//! Directories and files the options name, checked while they are read.

use super::config::LogFile;
use super::{OptionFailure, errno_text, error_text};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Creates `-b` if needed and returns its real path.
// Compatibility: exit statuses 3 (cannot create), 4 (only slashes) and 5
// (not the real path) and their texts (RCD-3).
// Provenance: rrdcached(1) and black-box compatibility test.
pub fn prepare_base_directory(value: &str) -> Result<PathBuf, OptionFailure> {
    let cannot_create = |text: String| {
        OptionFailure::new(
            3,
            format!("Failed to create base directory '{value}': {text}"),
        )
    };
    if value.is_empty() {
        return Err(cannot_create(errno_text(libc::EINVAL)));
    }
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(OptionFailure::new(
            4,
            format!("Invalid base directory: {value}"),
        ));
    }
    let path = Path::new(value);
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(cannot_create(errno_text(libc::ENOTDIR))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Rondi policy: new components are 0750, not 0777 less the
            // umask (RCD-3, opt-037).
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(path)
                .map_err(|error| cannot_create(error_text(&error)))?;
        }
        Err(error) => return Err(cannot_create(error_text(&error))),
    }
    let real = std::fs::canonicalize(path).map_err(|error| cannot_create(error_text(&error)))?;
    if real.as_os_str() != trimmed {
        return Err(OptionFailure::new(
            5,
            format!(
                "Base directory (-b) resolved via file system links!\n\
                 Please consult rrdcached '-b' documentation!\n\
                 Consider specifying the real directory ({})",
                real.display()
            ),
        ));
    }
    Ok(real)
}

/// Resolves `-j` to the real path of an existing directory.
// Compatibility: status 6 with these texts (RCD-30, opt-031, opt-032).
// Provenance: black-box compatibility test.
pub fn resolve_journal_directory(value: &str) -> Result<PathBuf, OptionFailure> {
    let real = std::fs::canonicalize(value).map_err(|error| {
        OptionFailure::new(
            6,
            format!(
                "Unable to resolve journal path ({value},{})",
                error_text(&error)
            ),
        )
    })?;
    if !real.is_dir() {
        return Err(OptionFailure::new(
            6,
            format!(
                "Failed to create journal directory '{value}': {}",
                errno_text(libc::ENOTDIR)
            ),
        ));
    }
    Ok(real)
}

/// Opens (creating if needed) the `-o` file for appending.
// Compatibility: status 6 and `Failed to open log file '<file>': <reason>`.
// Provenance: black-box compatibility test.
// Rondi policy: the file must be a regular file with one link, opened
// without following a symbolic link, so a log path cannot redirect writes
// elsewhere; a FIFO is refused instead of blocking startup.
pub fn open_log_file(value: &str) -> Result<LogFile, OptionFailure> {
    let failure = |reason: String| {
        OptionFailure::new(6, format!("Failed to open log file '{value}': {reason}"))
    };
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(value)
        .map_err(|error| failure(error_text(&error)))?;
    let metadata = file
        .metadata()
        .map_err(|error| failure(error_text(&error)))?;
    if !metadata.is_file() {
        return Err(failure("not a regular file".to_owned()));
    }
    if metadata.nlink() > 1 {
        return Err(failure(errno_text(libc::EMLINK)));
    }
    clear_nonblocking(&file).map_err(|error| failure(error_text(&error)))?;
    Ok(LogFile {
        path: PathBuf::from(value),
        file,
    })
}

fn clear_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fd is an open descriptor owned by `file` for both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        (dir, real)
    }

    #[test]
    fn base_directory_is_created_private_and_must_be_its_real_path() {
        let (_dir, root) = temp();
        let base = root.join("new/base");
        let text = base.to_str().unwrap();
        assert_eq!(prepare_base_directory(text).unwrap(), base);
        for created in [root.join("new"), base.clone()] {
            let mode = std::fs::metadata(&created).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o750, "{}", created.display());
        }
        // A trailing slash is accepted; doubled or dotted spellings are not.
        assert_eq!(prepare_base_directory(&format!("{text}//")).unwrap(), base);
        let failure =
            prepare_base_directory(&format!("{}/./new/base", root.display())).unwrap_err();
        assert_eq!(failure.status, 5);
        assert!(failure.message.ends_with(&format!("({})", base.display())));
    }

    #[test]
    fn base_directory_errors_carry_their_statuses() {
        let (_dir, root) = temp();
        let file = root.join("file");
        std::fs::write(&file, b"").unwrap();
        for (value, status, text) in [
            (
                String::new(),
                3,
                "Failed to create base directory '': Invalid argument".to_owned(),
            ),
            ("/".to_owned(), 4, "Invalid base directory: /".to_owned()),
            ("//".to_owned(), 4, "Invalid base directory: //".to_owned()),
            (
                file.to_str().unwrap().to_owned(),
                3,
                format!(
                    "Failed to create base directory '{}': Not a directory",
                    file.display()
                ),
            ),
            (
                format!("{}/below", file.display()),
                3,
                format!(
                    "Failed to create base directory '{}/below': Not a directory",
                    file.display()
                ),
            ),
        ] {
            let failure = prepare_base_directory(&value).unwrap_err();
            assert_eq!(
                (failure.status, failure.message.as_str()),
                (status, text.as_str())
            );
        }
    }

    #[test]
    fn journal_directory_must_exist_and_be_a_directory() {
        let (_dir, root) = temp();
        let file = root.join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(
            resolve_journal_directory(root.to_str().unwrap()).unwrap(),
            root
        );
        let missing = root.join("missing");
        let failure = resolve_journal_directory(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(failure.status, 6);
        assert_eq!(
            failure.message,
            format!(
                "Unable to resolve journal path ({},No such file or directory)",
                missing.display()
            )
        );
        let failure = resolve_journal_directory(file.to_str().unwrap()).unwrap_err();
        assert_eq!(
            failure.message,
            format!(
                "Failed to create journal directory '{}': Not a directory",
                file.display()
            )
        );
    }

    #[test]
    fn log_file_refuses_links_and_special_files() {
        let (_dir, root) = temp();
        let log = root.join("log");
        let opened = open_log_file(log.to_str().unwrap()).unwrap();
        assert_eq!(opened.path, log);
        drop(opened);
        let alias = root.join("alias");
        std::fs::hard_link(&log, &alias).unwrap();
        let failure = open_log_file(alias.to_str().unwrap()).unwrap_err();
        assert_eq!(
            (failure.status, failure.message),
            (
                6,
                format!(
                    "Failed to open log file '{}': Too many links",
                    alias.display()
                )
            )
        );
        let link = root.join("link");
        std::os::unix::fs::symlink(&log, &link).unwrap();
        assert_eq!(open_log_file(link.to_str().unwrap()).unwrap_err().status, 6);
        let failure = open_log_file(root.to_str().unwrap()).unwrap_err();
        assert_eq!(
            failure.message,
            format!(
                "Failed to open log file '{}': Is a directory",
                root.display()
            )
        );
    }
}
