use super::{parse_size, Cli, CompressionDetection};
use crate::path::SyncPath;
use std::path::PathBuf;
use std::str::FromStr;

/// Native argv binding. The application fields skipped by `Cli` are resolved
/// here so custom UTF-8 conversions never touch filesystem path bytes.
#[derive(usage::Cli, Debug)]
#[usage(
    bin = "sy",
    version,
    about = "Modern file synchronization tool",
    completion,
    unknown_flags = "error",
    args_override_self = false
)]
#[usage(after_help = "For more information: https://github.com/nijaru/sy")]
#[usage(example("sy /source /destination", header = "Basic sync"))]
#[usage(example(
    "sy /source /destination --dry-run",
    header = "Preview changes without applying"
))]
#[usage(example(
    "sy /source /destination --delete",
    header = "Mirror mode (delete extra destination files)"
))]
#[usage(example(
    "sy /source /destination -j 20",
    header = "Parallel transfers (20 workers)"
))]
#[usage(example("sy /path/to/file.txt /dest/file.txt", header = "Sync single file"))]
#[usage(example("sy /local user@host:/remote", header = "Remote sync (SSH push)"))]
#[usage(example("sy user@host:/remote /local", header = "Remote sync (SSH pull)"))]
#[usage(example("sy /source /destination --quiet", header = "Quiet mode (only errors)"))]
#[usage(example(
    "sy /source /destination --verify=after",
    header = "Verify staged integrity before commit"
))]
pub(super) struct ParsedArguments {
    /// Source path (local: /path or remote: user@host:/path)
    /// Optional when using --profile
    #[usage(value_hint = usage::ValueHint::FilePath)]
    source: Option<PathBuf>,

    /// Destination path (local: /path or remote: user@host:/path)
    /// Optional when using --profile
    #[usage(value_hint = usage::ValueHint::FilePath)]
    destination: Option<PathBuf>,

    /// Minimum file size to sync (e.g., "1MB", "500KB")
    #[usage(long)]
    min_size: Option<ByteSize>,

    /// Maximum file size to sync (e.g., "100MB", "1GB")
    #[usage(long)]
    max_size: Option<ByteSize>,

    /// Bandwidth limit in bytes per second (e.g., "1MB", "500KB")
    #[usage(long)]
    bwlimit: Option<ByteSize>,

    #[usage(flatten)]
    options: Cli,
}

impl ParsedArguments {
    pub(super) fn into_cli(self) -> Result<Cli, PathError> {
        Ok(Cli {
            source: self.source.map(sync_path).transpose()?,
            destination: self.destination.map(sync_path).transpose()?,
            min_size: self.min_size.map(|size| size.0),
            max_size: self.max_size.map(|size| size.0),
            bwlimit: self.bwlimit.map(|size| size.0),
            ..self.options
        })
    }
}

#[derive(Debug)]
struct ByteSize(u64);

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_size(value).map(Self)
    }
}

// Keep CLI vocabulary out of the compression implementation. This is also the
// sole source for accepted compression values in parsing, help and completions.
impl usage::spec::ValueEnum for CompressionDetection {
    const CHOICES: &'static [&'static str] = &["auto", "extension", "always", "never"];

    fn from_choice(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "extension" => Some(Self::Extension),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("object-store URLs require UTF-8: {0:?}")]
    ObjectUrl(PathBuf),
    #[error("remote host/user must be UTF-8 (use ./ for a local path): {0:?}")]
    RemoteAuthority(PathBuf),
}

fn sync_path(path: PathBuf) -> Result<SyncPath, PathError> {
    if let Some(text) = path.to_str() {
        return Ok(SyncPath::parse(text));
    }
    if path.as_os_str().as_encoded_bytes().starts_with(b"s3://")
        || path.as_os_str().as_encoded_bytes().starts_with(b"gs://")
    {
        return Err(PathError::ObjectUrl(path));
    }

    // Remote roots are native paths too. Parse only the UTF-8 host prefix;
    // retain the bytes after ':' and the original trailing-slash semantics.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = path.as_os_str().as_bytes();
        let has_trailing_slash = bytes.ends_with(b"/") || bytes.ends_with(b"\\");
        if let Some(colon) = bytes.iter().position(|&byte| byte == b':') {
            let prefix = &bytes[..colon];
            let is_drive = matches!(prefix, [b'a'..=b'z' | b'A'..=b'Z']);
            if !is_drive && !prefix.is_empty() && !prefix.contains(&b'/') {
                if let Ok(prefix) = std::str::from_utf8(prefix) {
                    let (user, host) = match prefix.split_once('@') {
                        Some((user, host)) => (Some(user.to_owned()), host.to_owned()),
                        None => (None, prefix.to_owned()),
                    };
                    return Ok(SyncPath::Remote {
                        user,
                        host,
                        path: PathBuf::from(std::ffi::OsString::from_vec(
                            bytes[colon + 1..].to_vec(),
                        )),
                        has_trailing_slash,
                    });
                }
                return Err(PathError::RemoteAuthority(path));
            }
        }
        Ok(SyncPath::Local {
            path,
            has_trailing_slash,
        })
    }
    #[cfg(not(unix))]
    {
        let has_trailing_slash = path.as_os_str().as_encoded_bytes().ends_with(b"/")
            || path.as_os_str().as_encoded_bytes().ends_with(b"\\");
        Ok(SyncPath::Local {
            path,
            has_trailing_slash,
        })
    }
}
