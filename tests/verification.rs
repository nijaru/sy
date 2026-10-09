use std::process::Command;

#[cfg(target_os = "linux")]
#[test]
fn native_names_do_not_discard_json_verification_or_operation_events() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&destination).unwrap();
    let name = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
    std::fs::write(source.join(&name), b"a").unwrap();
    std::fs::write(destination.join(&name), b"b").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source.join(""))
        .arg(&destination)
        .args(["--verify=only", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["files_mismatched_count"], 1);
    assert_eq!(
        event["files_mismatched"][0],
        serde_json::json!({
            "encoding": "unix_bytes", "bytes": source.join(&name).as_os_str().as_bytes()
        })
    );
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source.join(""))
        .arg(&destination)
        .args(["--checksum", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let update = events
        .iter()
        .find(|event| event["type"] == "update")
        .unwrap();
    assert_eq!(
        update["path"],
        serde_json::json!({"encoding": "unix_bytes", "bytes": [102, 255]})
    );
    assert_eq!(std::fs::read(destination.join(&name)).unwrap(), b"a");
}

#[test]
fn verify_only_reports_exact_totals_after_bounded_examples_without_writes() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&destination).unwrap();
    for i in 0..35 {
        let name = format!("different-{i:02}");
        std::fs::write(source.join(&name), b"source").unwrap();
        std::fs::create_dir(destination.join(&name)).unwrap();
        std::fs::write(source.join(format!("source-only-{i:02}")), b"source").unwrap();
        std::fs::write(
            destination.join(format!("dest-only-{i:02}")),
            b"destination",
        )
        .unwrap();
    }
    for root in [&source, &destination] {
        std::fs::write(root.join("same"), b"identical").unwrap();
    }
    // A content comparison beyond the detail limit must still run and count.
    std::fs::write(source.join("zz-last"), b"a").unwrap();
    std::fs::write(destination.join("zz-last"), b"b").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source.join(""))
        .arg(&destination)
        .args(["--verify=only", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(event["type"], "verification_result");
    assert_eq!(event["files_matched"], 1);
    assert_eq!(event["files_mismatched_count"], 36);
    assert_eq!(event["files_only_in_source_count"], 35);
    assert_eq!(event["files_only_in_dest_count"], 35);
    assert_eq!(event["errors_count"], 0);
    for category in [
        "files_mismatched",
        "files_only_in_source",
        "files_only_in_dest",
    ] {
        assert_eq!(event[category].as_array().unwrap().len(), 32);
    }
    assert_eq!(event["exit_code"], 1);
    assert_eq!(std::fs::read(destination.join("zz-last")).unwrap(), b"b");
    assert_eq!(std::fs::read(source.join("zz-last")).unwrap(), b"a");
    assert_eq!(std::fs::read_dir(&source).unwrap().count(), 72);
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 72);

    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source.join(""))
        .arg(&destination)
        .arg("--verify=only")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("36 ✗"));
    assert!(text.contains("4 additional entries not shown"));
    assert_eq!(text.matches("3 additional entries not shown").count(), 2);
}
