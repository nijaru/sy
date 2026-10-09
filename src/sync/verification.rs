use super::stats::SyncError;
use serde::Serialize;
use std::fmt::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

// Details are examples, not the authority for totals or verification success.
// Bound both cardinality and payload: native paths and error messages vary in size.
const MAX_EXAMPLES_PER_CATEGORY: usize = 32;
const MAX_DETAIL_BYTES: usize = 64 * 1024;
const MAX_ERROR_BYTES: usize = 2 * 1024;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct VerificationCounts {
    #[serde(rename = "files_mismatched_count")]
    pub files_mismatched: usize,
    #[serde(rename = "files_only_in_source_count")]
    pub files_only_in_source: usize,
    #[serde(rename = "files_only_in_dest_count")]
    pub files_only_in_dest: usize,
    #[serde(rename = "errors_count")]
    pub errors: usize,
}

#[derive(Debug, Default)]
pub struct VerificationResult {
    pub files_matched: usize,
    pub counts: VerificationCounts,
    pub files_mismatched: Vec<PathBuf>,
    pub files_only_in_source: Vec<PathBuf>,
    pub files_only_in_dest: Vec<PathBuf>,
    pub errors: Vec<SyncError>,
    pub duration: Duration,
    detail_bytes: usize,
}

pub(super) enum Difference {
    Mismatch,
    SourceOnly,
    DestinationOnly,
}

impl VerificationResult {
    /// Errors take precedence over differences, including those without a retained example.
    pub fn exit_code(&self) -> i32 {
        if self.counts.errors > 0 {
            2
        } else if self.counts.files_mismatched > 0
            || self.counts.files_only_in_source > 0
            || self.counts.files_only_in_dest > 0
        {
            1
        } else {
            0
        }
    }

    pub(super) fn record_difference(&mut self, kind: Difference, root: &Path, relative: &Path) {
        let (count, examples) = match kind {
            Difference::Mismatch => (
                &mut self.counts.files_mismatched,
                &mut self.files_mismatched,
            ),
            Difference::SourceOnly => (
                &mut self.counts.files_only_in_source,
                &mut self.files_only_in_source,
            ),
            Difference::DestinationOnly => (
                &mut self.counts.files_only_in_dest,
                &mut self.files_only_in_dest,
            ),
        };
        *count += 1;
        // Check before joining, so omitted examples do not allocate a full path.
        let path_bytes = root
            .as_os_str()
            .len()
            .saturating_add(relative.as_os_str().len())
            .saturating_add(1);
        if examples.len() < MAX_EXAMPLES_PER_CATEGORY
            && path_bytes <= MAX_DETAIL_BYTES - self.detail_bytes
        {
            let path = root.join(relative);
            self.detail_bytes += path.as_os_str().len();
            examples.push(path);
        }
    }

    pub(super) fn record_error(&mut self, root: &Path, relative: &Path, error: &dyn fmt::Display) {
        self.counts.errors += 1;
        const ACTION: &str = "verify";
        let path_bytes = root
            .as_os_str()
            .len()
            .saturating_add(relative.as_os_str().len())
            .saturating_add(1);
        // Reserve a bounded message before formatting; never collect unbounded error strings.
        let reservation = path_bytes
            .saturating_add(ACTION.len())
            .saturating_add(MAX_ERROR_BYTES);
        if self.errors.len() >= MAX_EXAMPLES_PER_CATEGORY
            || reservation > MAX_DETAIL_BYTES - self.detail_bytes
        {
            return;
        }
        let path = root.join(relative);
        let error = bounded_error_text(error);
        self.detail_bytes += path.as_os_str().len() + error.len() + ACTION.len();
        self.errors.push(SyncError {
            path,
            error,
            action: ACTION.into(),
        });
    }
}

fn bounded_error_text(error: &dyn fmt::Display) -> String {
    const SUFFIX: &str = "… [truncated]";
    struct LimitedText(String);
    impl Write for LimitedText {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            let remaining = MAX_ERROR_BYTES - SUFFIX.len() - self.0.len();
            if value.len() <= remaining {
                self.0.push_str(value);
                return Ok(());
            }
            let mut end = remaining;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&value[..end]);
            Err(fmt::Error)
        }
    }
    let mut text = LimitedText(String::new());
    // A formatter error stops rendering at the byte cap. Preserve an explicit
    // incomplete-message marker rather than making the shortened text look complete.
    if write!(&mut text, "{error}").is_err() {
        text.0.push_str(SUFFIX);
    }
    text.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_status_survive_example_limits() {
        let mut result = VerificationResult::default();
        for _ in 0..MAX_EXAMPLES_PER_CATEGORY + 3 {
            for kind in [
                Difference::Mismatch,
                Difference::SourceOnly,
                Difference::DestinationOnly,
            ] {
                result.record_difference(kind, Path::new("root"), Path::new("file"));
            }
            result.record_error(Path::new("root"), Path::new("file"), &"denied");
        }
        assert_eq!(
            result.counts.files_mismatched,
            MAX_EXAMPLES_PER_CATEGORY + 3
        );
        assert_eq!(
            result.counts.files_only_in_source,
            MAX_EXAMPLES_PER_CATEGORY + 3
        );
        assert_eq!(
            result.counts.files_only_in_dest,
            MAX_EXAMPLES_PER_CATEGORY + 3
        );
        assert_eq!(result.counts.errors, MAX_EXAMPLES_PER_CATEGORY + 3);
        assert_eq!(result.files_mismatched.len(), MAX_EXAMPLES_PER_CATEGORY);
        assert_eq!(result.files_only_in_source.len(), MAX_EXAMPLES_PER_CATEGORY);
        assert_eq!(result.files_only_in_dest.len(), MAX_EXAMPLES_PER_CATEGORY);
        assert_eq!(result.errors.len(), MAX_EXAMPLES_PER_CATEGORY);
        assert_eq!(result.exit_code(), 2);
    }

    #[test]
    fn path_and_unicode_error_payloads_share_a_byte_bound_without_hiding_failures() {
        let mut result = VerificationResult::default();
        let long_error = "é".repeat(MAX_ERROR_BYTES);
        result.record_error(Path::new("root"), Path::new("file"), &long_error);
        assert!(result.errors[0].error.ends_with("[truncated]"));
        assert!(result.errors[0].error.len() <= MAX_ERROR_BYTES);
        let path = PathBuf::from("component/".repeat(400));
        for _ in 0..MAX_EXAMPLES_PER_CATEGORY {
            result.record_difference(Difference::SourceOnly, Path::new("root"), &path);
        }
        let oversized = PathBuf::from("x".repeat(MAX_DETAIL_BYTES));
        result.record_error(Path::new("root"), &oversized, &"another error");
        assert_eq!(result.counts.errors, 2);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.counts.files_only_in_source,
            MAX_EXAMPLES_PER_CATEGORY
        );
        assert!(result.files_only_in_source.len() < MAX_EXAMPLES_PER_CATEGORY);
        let retained = result
            .files_only_in_source
            .iter()
            .map(|p| p.as_os_str().len())
            .sum::<usize>()
            + result
                .errors
                .iter()
                .map(|e| e.path.as_os_str().len() + e.error.len() + e.action.len())
                .sum::<usize>();
        assert_eq!(retained, result.detail_bytes);
        assert!(retained <= MAX_DETAIL_BYTES);
        assert_eq!(result.exit_code(), 2);
        let mut no_examples = VerificationResult::default();
        no_examples.record_difference(Difference::Mismatch, Path::new("root"), &oversized);
        assert!(no_examples.files_mismatched.is_empty());
        assert_eq!(no_examples.exit_code(), 1);
        no_examples.record_error(Path::new("root"), &oversized, &"denied");
        assert!(no_examples.errors.is_empty());
        assert_eq!(no_examples.exit_code(), 2);
    }
}
