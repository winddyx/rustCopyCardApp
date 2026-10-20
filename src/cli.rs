//! 命令行参数解析。

use std::path::PathBuf;

use clap::Parser;

/// 相机存储卡复制工具：sy 镜像复制 + XXH64 校验。
#[derive(Debug, Parser)]
#[command(
    name = "rcca",
    version,
    about = "相机存储卡复制工具：sy 镜像复制 + XXH64 校验",
    long_about = None,
    after_help = "退出码：0 成功；1 复制或校验失败；2 用法错误；3 预检失败；130 被中断。"
)]
pub struct Cli {
    /// 源目录（存储卡根目录）
    pub source: PathBuf,

    /// 目标目录（不存在会自动创建）
    pub target: PathBuf,

    /// 目标布局保留源目录名（target/<源目录名>/...）
    #[arg(long)]
    pub keep_source_name: bool,

    /// 跳过不可读的源文件，而不是预检失败
    #[arg(long)]
    pub skip_unreadable: bool,

    /// 目标空间不足时仍然继续
    #[arg(long)]
    pub force: bool,

    /// 首个失败即中止
    #[arg(long)]
    pub fail_fast: bool,

    /// 不绘制终端界面，只输出汇总
    #[arg(long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// 输出逐文件细节与完整失败清单
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// 校验失败时保留目标文件为 .badhash 而不是删除
    #[arg(long)]
    pub keep_bad: bool,

    /// 校验时绕过页缓存，使哈希来自设备
    #[arg(long)]
    pub verify_no_cache: bool,

    /// 开始前清理目标卷上遗留的 .sy-*.tmp 暂存文件
    #[arg(long)]
    pub clean_stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults() {
        let cli = Cli::try_parse_from(["rcca", "src", "dst"]).unwrap();
        assert!(!cli.keep_source_name);
        assert!(!cli.skip_unreadable);
        assert!(!cli.fail_fast);
        assert!(!cli.quiet);
        assert!(!cli.verbose);
    }

    #[test]
    fn conflicting_flags_rejected() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--quiet", "--verbose"]).is_err());
    }

    #[test]
    fn removed_engine_flags_are_rejected() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--chunk", "4M"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--serial-io"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--no-fsync"]).is_err());
    }
}
