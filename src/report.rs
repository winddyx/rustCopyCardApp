//! Final summary, printed to stdout once the run is over.
//!
//! Layout: the two paths, a blank line, an indented block of aligned counters, a blank
//! line and one result line. Failure detail, when there is any, follows as an indented
//! list so the block above stays scannable.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::stats::{human_bytes, human_duration};

#[derive(Debug, Clone)]
pub struct Summary {
    pub source: PathBuf,
    pub target: PathBuf,
    pub files_total: usize,
    pub bytes_total: u64,
    pub elapsed: Duration,
    /// Counters reported by sy; they include directory entries.
    pub created: u64,
    pub updated: u64,
    pub skipped: usize,
    pub deleted: usize,
    /// Regular files sy actually copied, and the bytes it moved.
    pub transferred_files: usize,
    pub bytes_transferred: u64,
    /// Files that passed verification and the bytes of every file that was checked.
    pub verified: usize,
    pub verify_failed: usize,
    pub checked_bytes: u64,
    /// How many problems the run hit, which can exceed `failures.len()`: the stored
    /// examples are bounded, the count is not.
    pub failure_count: usize,
    pub failures: Vec<String>,
    pub verbose: bool,
    pub interrupted: bool,
}

const MAX_PRINTED_FAILURES: usize = 20;
/// Label column of the counter block: `Transfer` is the longest label.
const LABEL_W: usize = 11;
/// Separator between values on one row.
const SEP: &str = " · ";

pub fn print(summary: &Summary) {
    // A closed stdout (rcca source target | head) is not an error worth a panic.
    let mut out = io::stdout().lock();
    for line in render(summary) {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

pub fn render(summary: &Summary) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("SRC  {}", summary.source.display()));
    lines.push(format!("DST  {}", summary.target.display()));
    lines.push(String::new());

    lines.push(row(
        "Files",
        &format!(
            "{}{SEP}{}{SEP}{}",
            file_count(summary.files_total),
            human_bytes(summary.bytes_total),
            human_duration(summary.elapsed.as_secs())
        ),
    ));
    lines.push(row(
        "Copy",
        &format!(
            "{} created{SEP}{} updated{SEP}{} skipped{SEP}{} deleted",
            summary.created, summary.updated, summary.skipped, summary.deleted
        ),
    ));
    lines.push(row(
        "Transfer",
        &format!(
            "{}{SEP}{}",
            file_count(summary.transferred_files),
            human_bytes(summary.bytes_transferred)
        ),
    ));

    let verified_total = summary.verified + summary.verify_failed;
    lines.push(row(
        "Verify",
        &format!(
            "{}{SEP}{}{SEP}{}/{} passed",
            file_count(verified_total),
            human_bytes(summary.checked_bytes),
            summary.verified,
            verified_total
        ),
    ));

    lines.push(String::new());
    lines.push(format!("  {}", result_line(summary)));

    if !summary.failures.is_empty() {
        let limit = if summary.verbose {
            summary.failures.len()
        } else {
            MAX_PRINTED_FAILURES.min(summary.failures.len())
        };
        lines.push(String::new());
        for failure in summary.failures.iter().take(limit) {
            lines.push(format!("    - {}", one_line(failure)));
        }
        // The count is authoritative: stored examples are capped, so a long failure list
        // would otherwise read as complete.
        if limit < summary.failure_count {
            let tail = if summary.verbose {
                "more, not kept"
            } else {
                "more, see --verbose"
            };
            lines.push(format!("    ... {} {tail}", summary.failure_count - limit));
        }
    }

    lines
}

fn row(label: &str, value: &str) -> String {
    format!("  {label:<LABEL_W$}{value}")
}

/// `1 file` reads better than `1 files`.
fn file_count(files: usize) -> String {
    if files == 1 {
        "1 file".to_string()
    } else {
        format!("{files} files")
    }
}

/// Failure text can carry newlines (sy wraps its errors), which would break the list.
fn one_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

fn result_line(summary: &Summary) -> String {
    if summary.interrupted {
        return "! Interrupted: verification incomplete, the target may hold unverified files"
            .to_string();
    }
    if summary.failure_count == 0 {
        return "✓ All files copied and verified".to_string();
    }
    let suffix = if summary.failure_count == 1 { "" } else { "s" };
    format!(
        "! Failed: {} problem{suffix}, listed below",
        summary.failure_count
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Summary {
        Summary {
            source: PathBuf::from("/card"),
            target: PathBuf::from("/out"),
            files_total: 2,
            bytes_total: 2048,
            elapsed: Duration::from_secs(3),
            created: 2,
            updated: 0,
            skipped: 0,
            deleted: 0,
            transferred_files: 2,
            bytes_transferred: 2048,
            verified: 2,
            verify_failed: 0,
            checked_bytes: 2048,
            failure_count: 0,
            failures: Vec::new(),
            verbose: false,
            interrupted: false,
        }
    }

    #[test]
    fn renders_the_reference_layout() {
        let mut summary = base();
        summary.files_total = 91;
        summary.bytes_total = 6_010_000_000;
        summary.created = 0;
        summary.skipped = 91;
        summary.transferred_files = 0;
        summary.bytes_transferred = 0;
        summary.verified = 91;
        summary.checked_bytes = 6_010_000_000;

        let lines = render(&summary);
        assert_eq!(lines[0], "SRC  /card");
        assert_eq!(lines[1], "DST  /out");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "  Files      91 files · 5.6 GiB · 00:03");
        assert_eq!(
            lines[4],
            "  Copy       0 created · 0 updated · 91 skipped · 0 deleted"
        );
        assert_eq!(lines[5], "  Transfer   0 files · 0 B");
        assert_eq!(lines[6], "  Verify     91 files · 5.6 GiB · 91/91 passed");
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "  ✓ All files copied and verified");
    }

    #[test]
    fn one_file_is_not_pluralised() {
        let mut summary = base();
        summary.files_total = 1;
        summary.transferred_files = 1;
        summary.verified = 1;
        let lines = render(&summary);
        assert!(lines[3].contains("1 file ·"), "{lines:?}");
        assert!(lines[5].contains("1 file ·"), "{lines:?}");
        assert!(lines[6].contains("1 file ·"), "{lines:?}");
    }

    #[test]
    fn counter_values_start_at_the_same_column() {
        let lines = render(&base());
        let columns: Vec<usize> = ["Files", "Copy", "Transfer", "Verify"]
            .iter()
            .map(|label| {
                let line = lines
                    .iter()
                    .find(|l| l.trim_start().starts_with(label))
                    .expect("row");
                let value_start = line.find(|c: char| c.is_ascii_digit()).expect("value");
                line[..value_start].chars().count()
            })
            .collect();
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "columns must line up: {columns:?}"
        );
        assert_eq!(columns[0], 2 + LABEL_W);
    }

    #[test]
    fn failures_are_limited_unless_verbose() {
        let mut summary = base();
        summary.failures = (0..30).map(|i| format!("f{i}")).collect();
        summary.failure_count = 30;
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 20);
        summary.verbose = true;
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 30);
    }

    #[test]
    fn the_trailing_count_comes_from_the_failure_count_not_the_examples() {
        let mut summary = base();
        // The store keeps at most 32 examples, the count keeps every problem.
        summary.failures = (0..32).map(|i| format!("f{i}")).collect();
        summary.failure_count = 100;
        let lines = render(&summary);
        assert_eq!(
            lines.last().unwrap(),
            "    ... 80 more, see --verbose",
            "{lines:?}"
        );
    }

    #[test]
    fn wrapped_failure_text_stays_on_one_line() {
        let mut summary = base();
        summary.failure_count = 1;
        summary.failures = vec!["copying a.bin: permission denied\nCause: 13\nretry later".into()];
        let lines = render(&summary);
        assert_eq!(
            lines.last().unwrap(),
            "    - copying a.bin: permission denied; Cause: 13; retry later"
        );
    }

    #[test]
    fn failed_run_says_so_and_lists_the_detail() {
        let mut summary = base();
        summary.verified = 1;
        summary.verify_failed = 1;
        summary.failure_count = 2;
        summary.failures = vec![
            "a.bin: copy failed".to_string(),
            "a.bin: target file missing".to_string(),
        ];
        let lines = render(&summary);
        let result = lines
            .iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.starts_with("  ! Failed: 2 problems"), "{result}");
        assert_eq!(
            lines.last().unwrap(),
            "    - a.bin: target file missing",
            "failure detail follows the result line, in order: {lines:?}"
        );
    }

    #[test]
    fn interrupted_run_is_explicit() {
        let mut summary = base();
        summary.interrupted = true;
        let lines = render(&summary);
        assert!(
            lines.iter().any(|l| l.starts_with("  ! Interrupted")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_single_problem_is_not_pluralised() {
        let mut summary = base();
        summary.verify_failed = 1;
        summary.failure_count = 1;
        summary.failures = vec!["a.bin: read failed".to_string()];
        let result = render(&summary)
            .into_iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.contains("1 problem,"), "{result}");
    }

    #[test]
    fn a_clean_run_reports_success_even_with_no_examples() {
        let summary = base();
        let result = render(&summary)
            .into_iter()
            .find(|l| l.trim_start().starts_with('✓'))
            .expect("result line");
        assert!(result.contains("All files copied and verified"), "{result}");
    }
}
