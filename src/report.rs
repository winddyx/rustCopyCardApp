//! Final summary, printed to stdout once the run is over.

use std::path::PathBuf;
use std::time::Duration;

use crate::stats::{human_bytes, human_duration};

#[derive(Debug, Clone)]
pub struct Summary {
    pub source: PathBuf,
    pub target: PathBuf,
    pub files_total: usize,
    pub bytes_total: u64,
    /// Counters reported by sy.
    pub created: u64,
    pub updated: u64,
    pub skipped: usize,
    pub deleted: usize,
    pub bytes_transferred: u64,
    pub copy_errors: usize,
    /// Results of rcca's own verification.
    pub verified: usize,
    pub verify_failed: usize,
    pub elapsed: Duration,
    pub failures: Vec<String>,
    pub verbose: bool,
    pub interrupted: bool,
}

const MAX_PRINTED_FAILURES: usize = 20;

pub fn print(summary: &Summary) {
    let lines = render(summary);
    for line in lines {
        println!("{line}");
    }
}

pub fn render(summary: &Summary) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("source: {}", summary.source.display()));
    lines.push(format!("target: {}", summary.target.display()));
    lines.push(format!(
        "files {}, {} total, elapsed {}",
        summary.files_total,
        human_bytes(summary.bytes_total),
        human_duration(summary.elapsed.as_secs())
    ));

    // sy counts directory entries too; say so here so it cannot be confused with
    // the file count above.
    lines.push(format!(
        "copy (sy, includes directories): {} created, {} updated, {} skipped, {} deleted, {} transferred",
        summary.created,
        summary.updated,
        summary.skipped,
        summary.deleted,
        human_bytes(summary.bytes_transferred)
    ));
    if summary.copy_errors > 0 {
        lines.push(format!("copy errors: {}", summary.copy_errors));
    }
    lines.push(format!(
        "verify (XXH64): {} passed, {} failed",
        summary.verified, summary.verify_failed
    ));

    if summary.interrupted {
        lines.push(
            "interrupted: verification incomplete, the target may hold unverified files"
                .to_string(),
        );
    }

    if !summary.failures.is_empty() {
        let limit = if summary.verbose {
            summary.failures.len()
        } else {
            MAX_PRINTED_FAILURES.min(summary.failures.len())
        };
        lines.push(format!("failures ({}):", summary.failures.len()));
        for failure in summary.failures.iter().take(limit) {
            lines.push(format!("  - {failure}"));
        }
        if limit < summary.failures.len() {
            lines.push(format!(
                "  ... {} more, see --verbose",
                summary.failures.len() - limit
            ));
        }
    }

    let ok = summary.failures.is_empty() && !summary.interrupted;
    lines.push(if ok {
        "result: all files copied and verified with XXH64".to_string()
    } else if summary.interrupted {
        "result: interrupted".to_string()
    } else {
        "result: failed, see the failure list above".to_string()
    });

    lines
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
            created: 2,
            updated: 0,
            skipped: 0,
            deleted: 0,
            bytes_transferred: 2048,
            copy_errors: 0,
            verified: 2,
            verify_failed: 0,
            elapsed: Duration::from_secs(3),
            failures: Vec::new(),
            verbose: false,
            interrupted: false,
        }
    }

    #[test]
    fn copy_line_marks_sy_counting_includes_directories() {
        let lines = render(&base());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("copy (sy, includes directories): 2 created")),
            "{lines:?}"
        );
    }

    #[test]
    fn file_count_and_elapsed_share_one_line() {
        let lines = render(&base());
        let line = lines
            .iter()
            .find(|l| l.starts_with("files "))
            .expect("a files line");
        assert_eq!(line, "files 2, 2.0 KiB total, elapsed 00:03");
        assert!(
            !lines.iter().any(|l| l.starts_with("elapsed ")),
            "elapsed must not get a line of its own: {lines:?}"
        );
    }

    #[test]
    fn success_summary_states_success() {
        let lines = render(&base());
        assert!(lines.iter().any(|l| l.contains("verify (XXH64): 2 passed")));
        assert!(lines.last().unwrap().contains("all files copied"));
    }

    #[test]
    fn failures_are_limited_unless_verbose() {
        let mut summary = base();
        summary.failures = (0..30).map(|i| format!("f{i}")).collect();
        let listed = |lines: &Vec<String>| {
            lines
                .iter()
                .filter(|l| l.trim_start().starts_with("- f"))
                .count()
        };
        let lines = render(&summary);
        assert_eq!(listed(&lines), 20);
        summary.verbose = true;
        let lines = render(&summary);
        assert_eq!(listed(&lines), 30);
    }

    #[test]
    fn interrupted_summary_is_explicit() {
        let mut summary = base();
        summary.interrupted = true;
        let lines = render(&summary);
        assert!(lines
            .iter()
            .any(|l| l.contains("interrupted: verification")));
        assert!(lines.last().unwrap().contains("result: interrupted"));
    }

    #[test]
    fn copy_errors_are_reported_separately() {
        let mut summary = base();
        summary.copy_errors = 3;
        let lines = render(&summary);
        assert!(lines.iter().any(|l| l.contains("copy errors: 3")));
    }
}
