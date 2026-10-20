//! 结束时的汇总输出。

use std::path::PathBuf;
use std::time::Duration;

use crate::stats::{human_bytes, human_duration};

#[derive(Debug, Clone)]
pub struct Summary {
    pub source: PathBuf,
    pub target: PathBuf,
    pub files_total: usize,
    pub bytes_total: u64,
    /// sy 复制的统计。
    pub created: u64,
    pub updated: u64,
    pub skipped: usize,
    pub deleted: usize,
    pub bytes_transferred: u64,
    pub copy_errors: usize,
    /// rcca 校验的结果。
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
    lines.push(format!("源：{}", summary.source.display()));
    lines.push(format!("目标：{}", summary.target.display()));
    lines.push(format!(
        "文件 {} 个，共 {}",
        summary.files_total,
        human_bytes(summary.bytes_total)
    ));
    lines.push(format!(
        "耗时 {}",
        human_duration(summary.elapsed.as_secs())
    ));

    lines.push(format!(
        "复制（sy）：新建 {} 个，更新 {} 个，跳过 {} 个，删除 {} 个，传输 {}",
        summary.created,
        summary.updated,
        summary.skipped,
        summary.deleted,
        human_bytes(summary.bytes_transferred)
    ));
    if summary.copy_errors > 0 {
        lines.push(format!("复制报错 {} 个", summary.copy_errors));
    }
    lines.push(format!(
        "校验（XXH64）：通过 {} 个，失败 {} 个",
        summary.verified, summary.verify_failed
    ));

    if summary.interrupted {
        lines.push("已中断：校验未完成，目标目录中可能存在未校验的文件".to_string());
    }

    if !summary.failures.is_empty() {
        let limit = if summary.verbose {
            summary.failures.len()
        } else {
            MAX_PRINTED_FAILURES.min(summary.failures.len())
        };
        lines.push(format!("失败明细（{} 条）：", summary.failures.len()));
        for failure in summary.failures.iter().take(limit) {
            lines.push(format!("  - {failure}"));
        }
        if limit < summary.failures.len() {
            lines.push(format!(
                "  ... 其余 {} 条请用 --verbose 查看",
                summary.failures.len() - limit
            ));
        }
    }

    let ok = summary.failures.is_empty() && !summary.interrupted;
    lines.push(if ok {
        "结论：全部文件已复制并通过 XXH64 校验".to_string()
    } else if summary.interrupted {
        "结论：任务被中断".to_string()
    } else {
        "结论：任务失败，详见上面的失败明细".to_string()
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
    fn success_summary_states_success() {
        let lines = render(&base());
        assert!(lines.iter().any(|l| l.contains("校验（XXH64）：通过 2 个")));
        assert!(lines.last().unwrap().contains("全部文件已复制"));
    }

    #[test]
    fn failures_are_limited_unless_verbose() {
        let mut summary = base();
        summary.failures = (0..30).map(|i| format!("f{i}")).collect();
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("f")).count(), 20);
        summary.verbose = true;
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("f")).count(), 30);
    }

    #[test]
    fn interrupted_summary_is_explicit() {
        let mut summary = base();
        summary.interrupted = true;
        let lines = render(&summary);
        assert!(lines.iter().any(|l| l.contains("已中断")));
        assert!(lines.last().unwrap().contains("被中断"));
    }

    #[test]
    fn copy_errors_are_reported_separately() {
        let mut summary = base();
        summary.copy_errors = 3;
        let lines = render(&summary);
        assert!(lines.iter().any(|l| l.contains("复制报错 3 个")));
    }
}
