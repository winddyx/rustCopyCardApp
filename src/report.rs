//! 结束时的汇总输出。

use std::path::PathBuf;
use std::time::Duration;

use crate::stats::{human_bytes, human_duration, human_speed};

#[derive(Debug, Clone)]
pub struct Summary {
    pub source: PathBuf,
    pub target: PathBuf,
    pub files_total: usize,
    pub bytes_total: u64,
    pub copied: usize,
    pub copy_failed: usize,
    pub skipped: usize,
    pub committed: usize,
    pub commit_failed: usize,
    pub verified: usize,
    pub verify_failed: usize,
    pub elapsed: Duration,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub failures: Vec<String>,
    pub verbose: bool,
    pub interrupted: bool,
    pub committed_unverified: usize,
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

    let seconds = summary.elapsed.as_secs_f64().max(0.001);
    lines.push(format!(
        "耗时 {}，平均读速 {}，平均写速 {}",
        human_duration(summary.elapsed.as_secs()),
        human_speed(summary.read_bytes as f64 / seconds),
        human_speed(summary.written_bytes as f64 / seconds)
    ));

    lines.push(format!(
        "复制成功 {} 个，失败 {} 个，跳过 {} 个",
        summary.copied, summary.copy_failed, summary.skipped
    ));
    lines.push(format!(
        "提交成功 {} 个，失败 {} 个",
        summary.committed, summary.commit_failed
    ));
    lines.push(format!(
        "校验通过 {} 个，失败 {} 个",
        summary.verified, summary.verify_failed
    ));

    if summary.interrupted {
        if summary.committed_unverified > 0 {
            lines.push(format!(
                "已中断：已提交但未校验 {} 个文件",
                summary.committed_unverified
            ));
        } else {
            lines.push("已中断：未提交任何文件".to_string());
        }
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
        "结论：任务被中断，目标目录中不存在未完成的最终文件名".to_string()
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
            copied: 2,
            copy_failed: 0,
            skipped: 0,
            committed: 2,
            commit_failed: 0,
            verified: 2,
            verify_failed: 0,
            elapsed: Duration::from_secs(3),
            read_bytes: 2048,
            written_bytes: 2048,
            failures: Vec::new(),
            verbose: false,
            interrupted: false,
            committed_unverified: 0,
        }
    }

    #[test]
    fn success_summary_states_success() {
        let lines = render(&base());
        assert!(lines.iter().any(|l| l.contains("校验通过 2 个")));
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
        summary.committed_unverified = 3;
        let lines = render(&summary);
        assert!(lines.iter().any(|l| l.contains("已提交但未校验 3 个")));
    }
}
