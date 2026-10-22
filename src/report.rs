//! 运行结束后的结果汇总，打印到 stdout。
//!
//! 版式：两行路径、一个空行、一段带缩进且对齐的计数块、一个空行和一行结果。
//! 若有失败详情，则作为缩进列表放在最后，这样上面的计数块仍然便于扫读。

// `Write` 是写入 trait，`writeln!` 需要它在作用域内；`io` 用来取得 stdout 句柄。
use std::io::{self, Write};
// 汇总里的路径字段需要拥有所有权，所以用 `PathBuf`。
use std::path::PathBuf;
// 运行耗时用 `Duration` 承载，打印时再转成秒。
use std::time::Duration;

// 复用人性化的字节与时长格式化，保证这一块和进度行口径一致。
use crate::stats::{human_bytes, human_duration};

/// 汇总所需的数据快照。
///
/// 这个结构体只承载数据，不做计算也不做 IO：`render` 负责排版，`print` 负责输出。
/// 分开之后，版式就能用纯函数测试，不必真的往终端写东西。
#[derive(Debug, Clone)]
pub struct Summary {
    // 源与目标的绝对路径，直接打印在第一、二行。
    pub source: PathBuf,
    pub target: PathBuf,
    // 清单里的文件数与总字节数。
    pub files_total: usize,
    pub bytes_total: u64,
    // 整个运行（含扫描、复制、校验）的耗时。
    pub elapsed: Duration,
    /// sy 上报的计数，包含目录条目。
    pub created: u64,
    // 被更新与删除的条目数，同样来自 sy。
    pub updated: u64,
    // `skipped` 是复制阶段跳过与 preflight 跳过之和，因此在 main 里累加。
    pub skipped: usize,
    pub deleted: usize,
    /// sy 实际复制的普通文件数与搬运的字节数。
    pub transferred_files: usize,
    pub bytes_transferred: u64,
    /// 通过校验的文件数，以及所有被检查文件的字节数。
    pub verified: usize,
    // 未通过校验的文件数：不一致、缺失与读取错误三类之和。
    pub verify_failed: usize,
    pub checked_bytes: u64,
    /// 问题总数，可能大于 `failures.len()`：保留的样例有上限，计数没有。
    pub failure_count: usize,
    // 失败样例文本，最多几十条，打印时可能再截断。
    pub failures: Vec<String>,
    // `--verbose` 时打印全部样例而不是前若干条。
    pub verbose: bool,
    // 是否被 Ctrl-C 中断；中断时结果行要给出明确提示。
    pub interrupted: bool,
}

/// 普通模式下最多打印的失败条数。
const MAX_PRINTED_FAILURES: usize = 20;
/// 计数块的标签列宽度：`Transfer` 是最长的标签。
const LABEL_W: usize = 11;
/// 同一行内各数值之间的分隔符。
const SEP: &str = " · ";

/// 把汇总写到 stdout。
///
/// 写入错误一律忽略（`let _ =`）：像 `rcca source target | head` 这样下游提前关闭管道时，
/// 报错或 panic 都没有意义。
pub fn print(summary: &Summary) {
    // 先锁住 stdout 拿到句柄，整块输出期间不会和其他线程的写入交错。
    let mut out = io::stdout().lock();
    // `render` 是纯函数，返回要打印的所有行；这里只负责逐行写出。
    for line in render(summary) {
        // `writeln!` 写一行并补换行；返回值 `Result` 被 `let _ =` 丢弃。
        let _ = writeln!(out, "{line}");
    }
    // 显式 flush：stdout 是行缓冲，进程即将退出时也无害，但这样行为更确定。
    let _ = out.flush();
}

/// 生成要输出的所有行。
///
/// 返回 `Vec<String>` 而不是直接打印，好处是测试可以逐行断言，也不用捕获 stdout。
pub fn render(summary: &Summary) -> Vec<String> {
    // 逐行累积：先固定头部，再计数块，最后是结果行与失败详情。
    let mut lines = Vec::new();
    // 两行路径用固定前缀 `SRC` / `DST`，便于脚本按行首过滤。
    lines.push(format!("SRC  {}", summary.source.display()));
    lines.push(format!("DST  {}", summary.target.display()));
    // `String::new()` 就是空字符串，用来输出空行，把路径与计数块隔开。
    lines.push(String::new());

    // 计数块固定四行，标签用 `row` 补齐，所以各行的数值从同一列开始。
    // 每行都用 `format!` 先拼出数值部分，再交给 `row` 加标签与缩进。
    lines.push(row(
        "Files",
        &format!(
            // 三个字段用 `SEP` 连接：文件数、总字节、总耗时。
            "{}{SEP}{}{SEP}{}",
            file_count(summary.files_total),
            human_bytes(summary.bytes_total),
            human_duration(summary.elapsed.as_secs())
        ),
    ));
    lines.push(row(
        "Copy",
        &format!(
            // 这一行的数字来自 sy 的计数器，四个短语各自带单位词。
            "{} created{SEP}{} updated{SEP}{} skipped{SEP}{} deleted",
            summary.created, summary.updated, summary.skipped, summary.deleted
        ),
    ));
    lines.push(row(
        "Transfer",
        &format!(
            // 实际搬运的文件数与字节数，不含被跳过的文件。
            "{}{SEP}{}",
            file_count(summary.transferred_files),
            human_bytes(summary.bytes_transferred)
        ),
    ));

    // 通过数与失败数相加才是参与校验的总数。
    let verified_total = summary.verified + summary.verify_failed;
    lines.push(row(
        "Verify",
        &format!(
            // `{}/{} passed` 让读者一眼看到比例，例如 "91/91 passed"。
            "{}{SEP}{}{SEP}{}/{} passed",
            file_count(verified_total),
            human_bytes(summary.checked_bytes),
            summary.verified,
            verified_total
        ),
    ));

    // 空行把计数块与结果行分开。
    lines.push(String::new());
    // 结果行前面缩进两格，与块内的标签列保持一致。
    lines.push(format!("  {}", result_line(summary)));

    // 只有存在失败样例时才追加详情列表。
    if !summary.failures.is_empty() {
        // `--verbose` 打印全部样例，否则只打印前若干条。
        let limit = if summary.verbose {
            summary.failures.len()
        } else {
            // `min` 取两者较小值：样例本就不足 20 条时不会多打印。
            MAX_PRINTED_FAILURES.min(summary.failures.len())
        };
        lines.push(String::new());
        // `take(limit)` 截取前 limit 项；`iter()` 保持借用，不会移动列表内容。
        for failure in summary.failures.iter().take(limit) {
            // 每条缩进四格，`one_line` 保证多行文本不会破坏版式。
            lines.push(format!("    - {}", one_line(failure)));
        }
        // 计数是权威值：样例本身有上限，若不补这一行，长列表看起来就像已经完整。
        if limit < summary.failure_count {
            // 两种收尾措辞：普通模式提示去看 `--verbose`，verbose 模式说明确实没保存。
            let tail = if summary.verbose {
                "more, not kept"
            } else {
                "more, see --verbose"
            };
            lines.push(format!("    ... {} {tail}", summary.failure_count - limit));
        }
    }

    // 最后把所有行交给调用方写出。
    lines
}

/// 拼一行"标签 + 数值"，标签左对齐补到固定宽度。
///
/// `{label:<LABEL_W$}` 里的 `LABEL_W$` 表示宽度取名为 `LABEL_W` 的常量，
/// 常量改了这里自动跟着变，不必满文件搜索硬编码的列宽。
fn row(label: &str, value: &str) -> String {
    // 两个空格缩进 + 左对齐的标签 + 数值。
    format!("  {label:<LABEL_W$}{value}")
}

/// `1 file` 比 `1 files` 读起来更自然，因此单数单独处理。
fn file_count(files: usize) -> String {
    if files == 1 {
        "1 file".to_string()
    } else {
        // 其余情况（含 0）都用复数形式。
        format!("{files} files")
    }
}

/// 失败信息里可能带换行（sy 会折行输出），直接打印会破坏列表结构，
/// 因此把多行压成一行：逐行去空白、丢掉空行，再用分号连接。
fn one_line(message: &str) -> String {
    // `lines()` 按行切分且不含换行符；`str::trim` 作为函数指针传入 `map`。
    // `filter` 去掉空行，避免连接后出现多余的分号。
    // `collect::<Vec<_>>()` 的 turbofish 指明收集目标，`join` 才能使用。
    message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// 结果行：中断、成功、失败三种情况各一句。
///
/// 中断优先判断：即使同时存在失败，也要先说明"结果不完整"这一更重要的事实。
fn result_line(summary: &Summary) -> String {
    if summary.interrupted {
        // 中断时目标里可能留着未校验的文件，这句话把风险说清楚。
        return "! Interrupted: verification incomplete, the target may hold unverified files"
            .to_string();
    }
    if summary.failure_count == 0 {
        // 成功路径：所有文件都复制并校验过。
        return "✓ All files copied and verified".to_string();
    }
    // 只有一个问题时不加复数 s。
    let suffix = if summary.failure_count == 1 { "" } else { "s" };
    // 样例全部被丢弃时（单条信息超过字节预算）不能承诺"列在下面"，否则只有一个空列表。
    let tail = if summary.failures.is_empty() {
        ""
    } else {
        ", listed below"
    };
    // `{suffix}` 直接把变量插进句子，`{}` 则是显式的第一个位置参数。
    format!("! Failed: {} problem{suffix}{tail}", summary.failure_count)
}

// 测试模块只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    use super::*;

    /// 一份全部通过的基础数据，测试按需覆盖个别字段。
    fn base() -> Summary {
        // `PathBuf::from("/card")` 从字面量构造路径，测试里不要求路径真实存在。
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
        // 造一组"全部跳过"的数据，用来固定版式：列宽、分隔符与顺序都在断言里写明。
        // 这类测试相当于给输出格式拍照，改动版式时必须同步更新。
        let mut summary = base();
        summary.files_total = 91;
        summary.bytes_total = 6_010_000_000;
        summary.created = 0;
        summary.skipped = 91;
        summary.transferred_files = 0;
        summary.bytes_transferred = 0;
        summary.verified = 91;
        summary.checked_bytes = 6_010_000_000;

        // 逐行断言比整体字符串比较多一条信息：能直接看出是哪一行变了。
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
        // 三行都应当写成 `1 file`；`{lines:?}` 在断言失败时打印全部行。
        assert!(lines[3].contains("1 file ·"), "{lines:?}");
        assert!(lines[5].contains("1 file ·"), "{lines:?}");
        assert!(lines[6].contains("1 file ·"), "{lines:?}");
    }

    #[test]
    fn counter_values_start_at_the_same_column() {
        let lines = render(&base());
        // 对每个标签找到对应行，量出"第一个数字之前"有多少个字符。
        // `map` 里用闭包，`expect` 在找不到时给出固定的失败信息。
        let columns: Vec<usize> = ["Files", "Copy", "Transfer", "Verify"]
            .iter()
            .map(|label| {
                let line = lines
                    .iter()
                    .find(|l| l.trim_start().starts_with(label))
                    .expect("row");
                // `find` 的闭包参数类型是 `char`，因此要显式标注 `|c: char|`。
                let value_start = line.find(|c: char| c.is_ascii_digit()).expect("value");
                // 用字符数而不是字节数，避免多字节字符影响列宽计算。
                line[..value_start].chars().count()
            })
            .collect();
        // `windows(2)` 给出相邻两两组合，全部相等就说明所有数值对齐。
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "columns must line up: {columns:?}"
        );
        // 两格缩进加标签列宽。
        assert_eq!(columns[0], 2 + LABEL_W);
    }

    #[test]
    fn failures_are_limited_unless_verbose() {
        let mut summary = base();
        // 造 30 条失败样例，源码里用区间迭代再 `format!` 生成。
        summary.failures = (0..30).map(|i| format!("f{i}")).collect();
        summary.failure_count = 30;
        let lines = render(&summary);
        // 普通模式只打印前 20 条；这里数的是形如 "- f..." 的行数。
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 20);
        // 打开 verbose 后全部打印。
        summary.verbose = true;
        let lines = render(&summary);
        assert_eq!(lines.iter().filter(|l| l.contains("- f")).count(), 30);
    }

    #[test]
    fn the_trailing_count_comes_from_the_failure_count_not_the_examples() {
        let mut summary = base();
        // 样例存储最多保留 32 条，计数则完整记录了 100 个问题。
        summary.failures = (0..32).map(|i| format!("f{i}")).collect();
        summary.failure_count = 100;
        let lines = render(&summary);
        // `lines.last()` 返回 `Option<&String>`，`unwrap` 后与字面量比较。
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
        // 字符串里含转义换行 `\n`，模拟 sy 折行的错误信息。
        summary.failures = vec!["copying a.bin: permission denied\nCause: 13\nretry later".into()];
        let lines = render(&summary);
        // `.into()` 把 `&str` 转成 `String`；多行内容应当用分号连成一行。
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
        // 结果行以 `!` 开头，用 `trim_start` 去掉缩进后再判断。
        let result = lines
            .iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.starts_with("  ! Failed: 2 problems"), "{result}");
        // 失败详情按原顺序追加在结果行之后。
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
        // 中断时的结果行必须点明"结果不完整"。
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
        // 这里用 `into_iter()` 消费这些行，因此后面还能继续借用 `summary`。
        let result = render(&summary)
            .into_iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert!(result.contains("1 problem,"), "{result}");
    }

    #[test]
    fn a_clean_run_reports_success_even_with_no_examples() {
        let summary = base();
        // 没有失败样例时，结果行依然是成功提示。
        let result = render(&summary)
            .into_iter()
            .find(|l| l.trim_start().starts_with('✓'))
            .expect("result line");
        assert!(result.contains("All files copied and verified"), "{result}");
    }

    #[test]
    fn a_failure_without_kept_examples_does_not_promise_a_list() {
        let mut summary = base();
        // 计数存在但样例全被丢弃（单条信息超过字节预算）：不能写"列在下面"。
        summary.failure_count = 3;
        let lines = render(&summary);
        let result = lines
            .iter()
            .find(|l| l.trim_start().starts_with('!'))
            .expect("result line");
        assert_eq!(result, "  ! Failed: 3 problems");
        // 结果行之后不应再出现空的失败列表。
        assert_eq!(lines.last().unwrap(), result, "{lines:?}");
    }
}
