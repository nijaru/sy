from pathlib import Path

path = Path("src/rooted_fs/scan.rs")
text = path.read_text()

old = """fn send_error(
    sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
    error: RootedScanError,
) {
"""
new = """fn send_error(sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>, error: RootedScanError) {
"""
if old not in text:
    raise SystemExit("send_error formatting marker missing")
text = text.replace(old, new, 1)

marker = "fn symlink_snapshot(stat: &libc::stat, path: &Path) -> Result<SymlinkSnapshot, RootedScanError> {\n"
helpers = """#[cfg(target_os = "linux")]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode
}

#[cfg(target_os = "macos")]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode as u32
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode as u32
}

#[cfg(target_os = "linux")]
const fn stat_device_u64(stat: &libc::stat) -> u64 {
    stat.st_dev
}

#[cfg(target_os = "macos")]
const fn stat_device_u64(stat: &libc::stat) -> u64 {
    stat.st_dev as u64
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn stat_device_u64(stat: &libc::stat) -> u64 {
    stat.st_dev as u64
}

"""
if "const fn stat_mode_u32" not in text:
    if marker not in text:
        raise SystemExit("symlink snapshot marker missing")
    text = text.replace(marker, helpers + marker, 1)

replacements = {
    "    let mode = stat.st_mode as u32;\n": "    let mode = stat_mode_u32(stat);\n",
    "    hasher.update(&(stat.st_dev as u64).to_le_bytes());\n": "    hasher.update(&stat_device_u64(stat).to_le_bytes());\n",
    "    hasher.update(&(stat.st_ino as u64).to_le_bytes());\n": "    hasher.update(&stat.st_ino.to_le_bytes());\n",
    """        let mut request = ScanRequest::default();
        request.max_depth = Some(0);
""": """        let request = ScanRequest {
            max_depth: Some(0),
            ..ScanRequest::default()
        };
""",
    """        let mut request = ScanRequest::default();
        request.respect_gitignore = true;
""": """        let request = ScanRequest {
            respect_gitignore: true,
            ..ScanRequest::default()
        };
""",
}
for old, new in replacements.items():
    if old not in text:
        raise SystemExit(f"cleanup marker missing: {old!r}")
    text = text.replace(old, new, 1)

path.write_text(text)
