#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

/// Read a small regular file without following a final symlink.
///
/// Policy and configuration files live in agent-writable workspaces. Opening
/// them through this helper prevents a FIFO/device from blocking the manager,
/// prevents symlink substitution on Unix, and bounds memory use before a read.
pub(crate) fn read_regular_file_limited(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself so a workspace-controlled symlink or
        // junction cannot redirect policy reads outside the workspace.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening regular file {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("statting open file {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= max_bytes,
        "{} is too large ({} bytes; maximum is {} bytes)",
        path.display(),
        metadata.len(),
        max_bytes
    );

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= max_bytes,
        "{} grew beyond the {} byte limit while being read",
        path.display(),
        max_bytes
    );
    Ok(Some(bytes))
}

/// Render a host path without leaking Windows' internal extended-length prefix
/// into user-facing text. `Path::canonicalize` returns `\\?\C:\...` (or
/// `\\?\UNC\server\share`) on Windows even for ordinary paths.
pub(crate) fn display_host_path(path: &Path) -> String {
    normalize_windows_extended_path(&path.as_os_str().to_string_lossy())
}

pub(crate) fn normalize_windows_extended_path(path: &str) -> String {
    if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else if let Some(local) = path.strip_prefix("\\\\?\\") {
        local.to_string()
    } else {
        path.to_string()
    }
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    set_private_file_permissions(path)?;
    Ok(())
}

pub(crate) fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    let _ = path;
    Ok(())
}

pub(crate) fn write_env_file_entry<W: Write>(
    writer: &mut W,
    key: &str,
    value: impl AsRef<str>,
) -> Result<()> {
    let value = value.as_ref();
    validate_env_file_entry(key, value)?;
    writeln!(writer, "{key}={value}")?;
    Ok(())
}

pub(crate) fn validate_env_file_entry(key: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        is_valid_env_name(key),
        "invalid environment variable name for Docker env file: {key}"
    );
    anyhow::ensure!(
        !value.contains('\n') && !value.contains('\r'),
        "environment variable value for {key} must not contain newlines"
    );
    Ok(())
}

pub(crate) fn is_valid_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::{normalize_windows_extended_path, read_regular_file_limited, write_env_file_entry};

    #[test]
    fn windows_extended_paths_render_as_standard_paths() {
        assert_eq!(
            normalize_windows_extended_path(r"\\?\C:\Users\example\repo"),
            r"C:\Users\example\repo"
        );
        assert_eq!(
            normalize_windows_extended_path(r"\\?\UNC\server\share\repo"),
            r"\\server\share\repo"
        );
        assert_eq!(
            normalize_windows_extended_path(r"C:\Users\example\repo"),
            r"C:\Users\example\repo"
        );
    }

    #[test]
    fn env_file_entry_rejects_newline_values() {
        let mut out = Vec::new();
        let err = write_env_file_entry(&mut out, "TOKEN", "abc\nINJECTED=1")
            .expect_err("newlines must be rejected");

        assert!(err.to_string().contains("must not contain newlines"));
    }

    #[test]
    fn env_file_entry_rejects_invalid_names() {
        let mut out = Vec::new();
        let err = write_env_file_entry(&mut out, "BAD-NAME", "value")
            .expect_err("invalid names must be rejected");

        assert!(
            err.to_string()
                .contains("invalid environment variable name")
        );
    }

    #[test]
    fn env_file_entry_writes_valid_line() {
        let mut out = Vec::new();

        write_env_file_entry(&mut out, "GOOD_NAME", "value").expect("write env entry");

        assert_eq!(String::from_utf8(out).unwrap(), "GOOD_NAME=value\n");
    }

    #[test]
    fn limited_regular_file_read_rejects_oversized_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, b"123456789").unwrap();

        let error = read_regular_file_limited(&path, 8).unwrap_err();
        assert!(error.to_string().contains("too large"));
    }

    #[cfg(unix)]
    #[test]
    fn limited_regular_file_read_rejects_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.pipe");
        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);

        let error = read_regular_file_limited(&path, 1024).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn limited_regular_file_read_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        let link = dir.path().join("rules.toml");
        std::fs::write(&target, b"version = 1\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(read_regular_file_limited(&link, 1024).is_err());
    }
}
