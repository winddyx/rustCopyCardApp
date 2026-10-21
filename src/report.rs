//! Final summary, printed to stdout once the run is over.
//!
//! Layout: the two paths, a blank line, an indented block of aligned counters, a blank
//! line and one result line. Failure detail, when there is any, follows as an indented
//! list so the block above stays scannable.

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
    pub copy_errors: usize,
    /// Files that passed verification, the bytes they hold, and the failures.
    pub verified: usize,
    pub verify_failed: usize,
    pub verified_bytes: u64,
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
    let lines = render(summary);
    for line in lines {
        println!("{line}");
    }
}

pub fn render(summary: &Summary) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("SRC  {}", summary.source.display()));
    lines.push(format!("DST  {}", summary.target.display()));
    lines.push(String::new());

    lines.push(row(
        "Files",
        &format!(
            "{} files{SEP}{}{SEP}{}",
            summary.files_total,
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
            "{} files{SEP}{}",
            summary.transferred_files,
            human_bytes(summary.bytes_transferred)
        ),
    ));

    let verified_total = summary.verified + summary.verify_failed;
    lines.push(row(
        "Verify",
        &format!(
            "{verified_total} files{SEP}{}{SEP}{}/{} passed",
            human_bytes(summary.verified_bytes),
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
            lines.push(format!("    - {failure}"));
        }
        if limit < summary.failures.len() {
            lines.push(format!(
                "    ... {} more, see --verbose",
                summary.failures.len() - limit
            ));
        }
    }

    lines
}

fn row(label: &str, value: &str) -> String {
    format!("  {label:<LABEL_W$}{value}")
}

fn result_line(summary: &Summary) -> String {
    if summary.interrupted {
        return "! Interrupted: verification incomplete, the target may hold unverified files"
            .to_string();
    }
    let problems = summary.failures.len() + summary.copy_errors;
    if problems == 0 && summary.verify_failed == 0 {
        return "✓ All files copied and verified".to_string();
    }
    let suffix = if problems == 1 { "" } else { "s" };
    format!("! Failed: {problems} problem{suffix}, listed below")
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
            copy_errors: 0,
            verified: 2,
            verify_failed: 0,
            verified_bytes: 2048,
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
        summary.verified_bytes = 6_010_000_000;

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
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 20);
        summary.verbose = true;
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 30);
    }

    #[test]
    fn failed_run_says_so_and_lists_the_detail() {
        let mut summary = base();
        summary.verified = 1;
        summary.verify_failed = 1;
        summary.copy_errors = 1;
        summary.failures = vec!["a.bin: hash mismatch".to_string()];
        let lines = render(&summary);
        let result = lines
            .iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.starts_with("  ! Failed: 2 problems"), "{result}");
        assert_eq!(
            lines.last().unwrap(),
            "    - a.bin: hash mismatch",
            "failure detail follows the result line: {lines:?}"
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
        summary.failures = vec!["a.bin: read failed".to_string()];
        let result = render(&summary)
            .into_iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.contains("1 problem,"), "{result}");
    }

    #[test]
    fn copy_errors_alone_do_not_claim_success() {
        let mut summary = base();
        summary.copy_errors = 2;
        let lines = render(&summary);
        assert!(lines.last().unwrap().starts_with("  ! Failed: 2 problems"));
    }
}
