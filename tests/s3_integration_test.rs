use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir(name: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    // Create some structure
    fs::write(dir.path().join(format!("{}_file1.txt", name)), "content1").unwrap();
    fs::create_dir_all(dir.path().join("subdir")).unwrap();
    fs::write(
        dir.path().join(format!("subdir/{}_file2.txt", name)),
        "content2",
    )
    .unwrap();
    dir
}

// Helper to get S3 configuration from environment
struct S3Config {
    bucket: String,
    region: Option<String>,
    endpoint: Option<String>,
    prefix: String,
}

impl S3Config {
    fn from_env() -> Option<Self> {
        let bucket = std::env::var("SY_TEST_S3_BUCKET").ok()?;
        let region = std::env::var("SY_TEST_S3_REGION").ok();
        let endpoint = std::env::var("SY_TEST_S3_ENDPOINT").ok();
        // Use a random prefix to avoid collisions
        let prefix = format!(
            "sy-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );

        Some(Self {
            bucket,
            region,
            endpoint,
            prefix,
        })
    }

    fn to_url(&self, path: &str) -> String {
        let key = if path.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, path)
        };

        let mut url = format!("s3://{}/{}", self.bucket, key);
        let mut params = Vec::new();

        if let Some(ref region) = self.region {
            params.push(format!("region={}", region));
        }
        if let Some(ref endpoint) = self.endpoint {
            params.push(format!("endpoint={}", endpoint));
        }

        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        url
    }
}

#[test]
fn test_s3_upload_download() {
    let config = match S3Config::from_env() {
        Some(c) => c,
        None => {
            println!("Skipping S3 test: SY_TEST_S3_BUCKET not set");
            return;
        }
    };

    let source = setup_test_dir("upload");
    let dest = TempDir::new().unwrap();

    println!("Testing S3 sync with prefix: {}", config.prefix);

    // 1. Sync Local -> S3
    let s3_url = config.to_url(""); // Root of prefix
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &s3_url,
            "--use-cache=false", // Disable cache to force scan
        ])
        .output()
        .expect("Failed to run sy");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "Local -> S3 sync failed\nStderr: {}\nStdout: {}",
            stderr, stdout
        );
    }

    // 2. Sync S3 -> Local (different directory)
    let output = Command::new(sy_bin())
        .args([&s3_url, dest.path().to_str().unwrap(), "--use-cache=false"])
        .output()
        .expect("Failed to run sy");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "S3 -> Local sync failed\nStderr: {}\nStdout: {}",
            stderr, stdout
        );
    }

    // 3. Verify files
    let file1_name = "upload_file1.txt";
    let file2_name = "subdir/upload_file2.txt";

    assert!(dest.path().join(file1_name).exists());
    assert!(dest.path().join(file2_name).exists());

    assert_eq!(
        fs::read_to_string(dest.path().join(file1_name)).unwrap(),
        "content1"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join(file2_name)).unwrap(),
        "content2"
    );

    // 4. Cleanup (Delete from S3)
    // We can use --delete with an empty source to delete everything in the prefix
    let empty_source = TempDir::new().unwrap();
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", empty_source.path().display()),
            &s3_url,
            "--delete",
            "--use-cache=false",
            "--force-delete", // Force delete since we're deleting everything
        ])
        .output()
        .expect("Failed to run sy cleanup");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("Warning: Failed to cleanup S3: {}", stderr);
    }
}

#[test]
fn test_s3_multipart_upload() {
    let config = match S3Config::from_env() {
        Some(c) => c,
        None => {
            println!("Skipping S3 test: SY_TEST_S3_BUCKET not set");
            return;
        }
    };

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let large_file_name = "large_file.bin";
    let large_file_path = source.path().join(large_file_name);

    // Create a 6MB file (threshold is 5MB)
    let size = 6 * 1024 * 1024;
    let data = vec![b'A'; size];
    fs::write(&large_file_path, &data).unwrap();

    println!("Testing S3 multipart upload with prefix: {}", config.prefix);

    // 1. Sync Local -> S3
    let s3_url = config.to_url("");
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &s3_url,
            "--use-cache=false",
        ])
        .output()
        .expect("Failed to run sy");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("Local -> S3 sync failed: {}", stderr);
    }

    // 2. Sync S3 -> Local
    let output = Command::new(sy_bin())
        .args([&s3_url, dest.path().to_str().unwrap(), "--use-cache=false"])
        .output()
        .expect("Failed to run sy");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("S3 -> Local sync failed: {}", stderr);
    }

    // 3. Verify file size and content
    let dest_file = dest.path().join(large_file_name);
    assert!(dest_file.exists());
    let metadata = fs::metadata(&dest_file).unwrap();
    assert_eq!(metadata.len(), size as u64);

    // 4. Cleanup
    let empty_source = TempDir::new().unwrap();
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", empty_source.path().display()),
            &s3_url,
            "--delete",
            "--use-cache=false",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy cleanup");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("Warning: Failed to cleanup S3: {}", stderr);
    }
}
