//! 命令行参数解析。

use std::path::PathBuf;

use clap::Parser;

/// 相机存储卡复制工具：四阶段复制、XXH64 校验、tmp 原子提交。
#[derive(Debug, Parser)]
#[command(
    name = "rcca",
    version,
    about = "相机存储卡复制工具：四阶段复制、XXH64 校验、tmp 原子提交",
    long_about = None,
    after_help = "退出码：0 成功；1 复制或校验失败；2 用法错误；3 预检失败；130 被中断。"
)]
pub struct Cli {
    /// 源目录（存储卡根目录）
    pub source: PathBuf,

    /// 目标目录（不存在会自动创建）
    pub target: PathBuf,

    /// 读写流水线的缓冲预算（字节预算，例 128M、512M）
    #[arg(long, value_name = "SIZE", default_value = "128M", value_parser = parse_size)]
    pub buffer: u64,

    /// 数据块大小（例 1M、4M，允许 1M 至 8M）
    #[arg(long, value_name = "SIZE", default_value = "1M", value_parser = parse_size)]
    pub chunk: u64,

    /// 关闭读写重叠：读一块、同步写一块（同盘机械盘适用）
    #[arg(long)]
    pub serial_io: bool,

    /// 不执行 fsync（性能优先，断电时校验结论不再可信）
    #[arg(long)]
    pub no_fsync: bool,

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

    /// 不绘制进度条，只输出逐文件结果与汇总
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

    /// 开始前清理目标卷上遗留的 .rcptmp 文件
    #[arg(long)]
    pub clean_stale: bool,
}

impl Cli {
    /// 修正并返回实际生效的块大小与缓冲预算（附带提示消息）。
    pub fn effective_io(&self) -> (usize, u64, Vec<String>) {
        let mut notes = Vec::new();
        let mut chunk = self.chunk;
        if chunk < MIN_CHUNK {
            notes.push(format!(
                "块大小 {} 小于下限 {}，已按 {} 运行",
                crate::stats::human_bytes(chunk),
                crate::stats::human_bytes(MIN_CHUNK),
                crate::stats::human_bytes(MIN_CHUNK)
            ));
            chunk = MIN_CHUNK;
        }
        if chunk > MAX_CHUNK {
            notes.push(format!(
                "块大小 {} 超过上限 {}，已按 {} 运行",
                crate::stats::human_bytes(chunk),
                crate::stats::human_bytes(MAX_CHUNK),
                crate::stats::human_bytes(MAX_CHUNK)
            ));
            chunk = MAX_CHUNK;
        }
        let mut buffer = self.buffer;
        if buffer < MIN_BUFFER {
            notes.push(format!(
                "缓冲 {} 小于下限 {}，已按 {} 运行",
                crate::stats::human_bytes(buffer),
                crate::stats::human_bytes(MIN_BUFFER),
                crate::stats::human_bytes(MIN_BUFFER)
            ));
            buffer = MIN_BUFFER;
        }
        if buffer > MAX_BUFFER {
            notes.push(format!(
                "缓冲 {} 超过上限 {}，已按 {} 运行",
                crate::stats::human_bytes(buffer),
                crate::stats::human_bytes(MAX_BUFFER),
                crate::stats::human_bytes(MAX_BUFFER)
            ));
            buffer = MAX_BUFFER;
        }
        if buffer < chunk {
            buffer = chunk;
        }
        (chunk as usize, buffer, notes)
    }
}

pub const MIN_CHUNK: u64 = 1024 * 1024;
pub const MAX_CHUNK: u64 = 8 * 1024 * 1024;
pub const MIN_BUFFER: u64 = 8 * 1024 * 1024;
pub const MAX_BUFFER: u64 = 1024 * 1024 * 1024;

/// 解析带单位的大小：128M、512MB、1G、134217728。
pub fn parse_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("大小不能为空".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let (digits, unit) = if let Some(stripped) = lower
        .strip_suffix("kib")
        .or_else(|| lower.strip_suffix("kb"))
    {
        (stripped, 1024u64)
    } else if let Some(stripped) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix("mb"))
    {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = lower
        .strip_suffix("gib")
        .or_else(|| lower.strip_suffix("gb"))
    {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('k') {
        (stripped, 1024)
    } else if let Some(stripped) = lower.strip_suffix('m') {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('g') {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('b') {
        (stripped, 1)
    } else {
        (lower.as_str(), 1)
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("无法解析大小：{text}"))?;
    value
        .checked_mul(unit)
        .ok_or_else(|| format!("大小超出范围：{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("128M").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_size("128mb").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("4096b").unwrap(), 4096);
        assert_eq!(parse_size("8KiB").unwrap(), 8192);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn cli_parses_defaults() {
        let cli = Cli::try_parse_from(["rcca", "src", "dst"]).unwrap();
        assert_eq!(cli.buffer, 128 * 1024 * 1024);
        assert_eq!(cli.chunk, 1024 * 1024);
        assert!(!cli.serial_io);
    }

    #[test]
    fn conflicting_flags_rejected() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--quiet", "--verbose"]).is_err());
    }

    #[test]
    fn effective_io_clamps() {
        let cli = Cli::try_parse_from(["rcca", "src", "dst", "--chunk", "64K", "--buffer", "2M"])
            .unwrap();
        let (chunk, buffer, notes) = cli.effective_io();
        assert_eq!(chunk as u64, MIN_CHUNK);
        assert_eq!(buffer, MIN_BUFFER);
        assert!(!notes.is_empty());
    }
}
