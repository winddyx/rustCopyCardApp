//! 命令行参数解析。
//!
//! 这里用 clap 的 derive 方式：给结构体派生 `Parser`，每个字段就是一个选项，
//! 字段上的 `#[arg(...)]` 属性描述选项的行为。
//!
//! 需要注意 clap 的两条隐式规则，否则容易把注释写错地方：
//!
//! 1. 字段上的 `///` 文档注释会被 clap 当作 `--help` 里该选项的帮助文字；
//! 2. `#[derive(ValueEnum)]` 的枚举变体上，文档注释会变成该选项可取值的说明。
//!
//! 因此下面这些对 clap 可见的文档注释保持英文（改动它们等于改动命令行输出），
//! 中文讲解放在普通 `//` 注释里。clap 不读普通注释，普通注释也不会进入 `--help`。

// 路径类型统一用 `PathBuf`（拥有所有权的版本），因为解析结果要在整个运行期间保存；
// 相比之下 `&Path` 是借用版本，装不进结构体字段。
use std::path::PathBuf;

// 只引入用到的两个 trait：`Parser` 提供 `parse()` / `try_parse_from()`，
// `ValueEnum` 让枚举可以被 `--color` 这样的选项直接解析。
use clap::{Parser, ValueEnum};

/// Colour policy for the live progress line.
///
/// 变体顺序决定 `--color` 帮助里列出的顺序。派生的 trait 各有用途：
/// `Clone`/`Copy` 让这个很小的枚举可以按值复制（`Copy` 要求所有字段都能按位复制，
/// 无形段枚举天然满足）；`PartialEq`/`Eq` 让测试可以直接用 `assert_eq!` 比较；
/// `Debug` 用于测试失败时的打印；`ValueEnum` 则让 clap 负责解析 `--color never`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    // 没有任何字段的枚举变体叫"无形段变体"（unit variant），大小为零字节。
    // 每个 `///` 都在 `--help` 里显示为该取值的说明，所以保持英文。
    /// Decide from the environment (NO_COLOR, TERM, TTY)
    Auto,
    /// Always use colour
    Always,
    /// Never use colour
    Never,
}

/// Camera card copy tool: sy mirror copy plus XXH64 verification.
///
/// 结构体上派生了 `Parser`，由此获得 `Cli::parse()`（失败时打印帮助并退出）
/// 和 `Cli::try_parse_from(...)`（返回 `Result`，给测试用）。
///
/// `#[command(...)]` 里的字段：`name` 是程序名，`version` 取 `Cargo.toml` 的版本号，
/// `about` 是一行简介，`long_about = None` 表示不额外提供长简介，
/// `after_help` 会在 `--help` 末尾追加退出码说明。
#[derive(Debug, Parser)]
#[command(
    name = "rcca",
    version,
    about = "Camera card copy tool: sy mirror copy plus XXH64 verification",
    long_about = None,
    after_help = "Exit codes: 0 success; 1 copy or verify failure; 2 usage error; 3 preflight failure; 130 interrupted."
)]
pub struct Cli {
    // 字段声明顺序就是解析顺序。没有 `#[arg]` 属性的字段成为位置参数，
    // 因此 source 是第一个必填参数、target 是第二个，顺序不能随意调换。
    /// Source directory (card root)
    pub source: PathBuf,
    /// Target directory (created when missing)
    pub target: PathBuf,

    // 每个布尔字段派生出的选项默认关闭；出现即视为 true，不需要跟值。
    // `#[arg(long)]` 把字段名按 kebab-case 转成选项名：keep_source_name 对应
    // `--keep-source-name`（下划线在命令行里写成短横线）。
    /// Keep the source directory name in the target layout (target/<source name>/...)
    #[arg(long)]
    pub keep_source_name: bool,

    /// Skip unreadable source files instead of failing the preflight
    #[arg(long)]
    pub skip_unreadable: bool,

    /// Continue even when the target looks too small
    #[arg(long)]
    pub force: bool,

    /// Abort on the first failure
    #[arg(long)]
    pub fail_fast: bool,

    // `conflicts_with` 让 clap 在同时出现两个互斥选项时报用法错误，
    // 这样互斥规则由解析器保证，业务代码里不需要再判断一次。
    // 两处属性互相指向，任一侧都可以触发冲突检查。
    /// Do not draw the progress line, print the summary only
    #[arg(long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Keep and print the complete failure list instead of the first few
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Keep mismatching target files as .badhash instead of deleting them
    #[arg(long)]
    pub keep_bad: bool,

    /// Bypass the page cache while hashing so hashes come from the device
    #[arg(long)]
    pub verify_no_cache: bool,

    /// Remove leftover .sy-*.tmp files from the target before starting
    #[arg(long)]
    pub clean_stale: bool,

    // 这一组属性的作用：`short = 'j'` 提供短选项，`value_name = "N"` 决定帮助里的
    // 占位符写法，`default_value_t` 指定默认值（这里写成 1），
    // `value_parser = parse_jobs` 指定自定义解析函数，解析失败时报用法错误。
    // 这些说明写成普通注释：写进文档注释会多出一段 `--help` 文字。
    /// Number of parallel file transfers passed to sy (sy's -j), 1-64, 1 means serial
    #[arg(short = 'j', long, value_name = "N", default_value_t = 1, value_parser = parse_jobs)]
    pub jobs: usize,

    // `value_enum` 声明取值范围来自 `ValueEnum` 的派生实现；
    // `default_value_t = ColorChoice::Auto` 用类型自身的值作默认值。
    /// Colour policy for the progress line (defaults to NO_COLOR, TERM and TTY)
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,
}

/// `-j` 的上限。每个校验线程持有一块 1 MiB 缓冲区，sy 侧也会同时发起同样多的传输，
/// 过大的取值只会把内存与线程数推高，对复制速度没有帮助。
pub const MAX_JOBS: usize = 64;

/// 拒绝 0、非数字和超过上限的取值：sy 至少需要一个传输槽位，而线程数必须有界。
///
/// 这个函数要交给 clap 当 `value_parser`，所以签名是固定的：
/// 输入 `&str`，输出 `Result<T, String>`，`Err` 里的文字会作为报错原因打印。
///
/// 流程分三步：先 `parse()` 把字符串转成 `usize`（`map_err` 把解析错误改写成
/// 带原值的友好提示），再判断 0 与上限。分开判断比拼一条复杂的错误信息更直观。
fn parse_jobs(raw: &str) -> Result<usize, String> {
    // `raw.parse()` 的目标类型由左边的 `let jobs: usize` 推断出来，这里显式标注了类型，
    // 否则链式调用上的 `map_err` 无法确定要解析成什么。
    // `map_err` 只处理 `Err` 分支：把底层解析错误替换成带原值的提示文本。
    // 末尾的 `?` 表示失败就带 `Err` 提前返回，成功则解包出 `usize` 赋给 `jobs`。
    let jobs: usize = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a positive number"))?;
    // `?` 只能出现在返回 `Result`（或 `Option`）的函数里；上面那一行如果失败，
    // 就会带着改写后的错误信息直接返回，下面的判断因此只处理数值合法的情况。
    if jobs == 0 {
        // clap 会把这里返回的 `String` 打印成"invalid value ...: jobs must be at least 1"。
        return Err("jobs must be at least 1".to_string());
    }
    if jobs > MAX_JOBS {
        // 超过上限时明确拒绝：几百个线程同时各持一块缓冲区没有意义。
        return Err(format!("jobs must be at most {MAX_JOBS}"));
    }
    // 包成 `Ok` 返回；最后一个表达式就是函数返回值，所以不写 `return`。
    Ok(jobs)
}

// `#[cfg(test)]` 表示这个模块只在 `cargo test` 时编译，正式构建里完全不存在，
// 因此测试代码不会增大二进制体积。
#[cfg(test)]
mod tests {
    // 单元测试写在同文件的 `mod tests` 里，能访问父模块的私有条目。
    // `use super::*` 把父模块（也就是本文件）的所有条目引入作用域。

    use super::*;

    // `#[test]` 标记的函数由测试运行器逐个调用；函数无参数、无返回值，
    // 靠 panic 表示失败（`assert!` 系列就是"不成立就 panic"）。
    #[test]
    fn cli_parses_defaults() {
        // 用 `try_parse_from` 而不是 `parse`：前者返回 `Result`，测试里出错会失败而不是退出进程。
        // 参数数组的第一个元素是程序名，clap 约定如此；`unwrap()` 在这里可以接受，
        // 因为解析失败就说明测试该失败。
        let cli = Cli::try_parse_from(["rcca", "src", "dst"]).unwrap();
        // `assert_eq!` 在不相等时打印两侧的值，比 `assert!(a == b)` 更好定位问题。
        assert_eq!(cli.color, ColorChoice::Auto);
        assert_eq!(cli.jobs, 1);
        // 布尔选项默认 false，这里逐个确认默认值没有被写反；
        // `assert!` 只需要真假值，`!x` 表示取反。
        assert!(!cli.keep_source_name);
        assert!(!cli.skip_unreadable);
        assert!(!cli.fail_fast);
        assert!(!cli.quiet);
        assert!(!cli.verbose);
    }

    #[test]
    fn color_flag_parses() {
        // 选项和取值分两个数组元素写，与命令行里空格分隔一致。
        let cli = Cli::try_parse_from(["rcca", "src", "dst", "--color", "never"]).unwrap();
        assert_eq!(cli.color, ColorChoice::Never);
        // 非法取值应当被 clap 拒绝：`is_err()` 说明解析没有成功。
        // 这里不断言具体错误文字，只要求"解析失败"，避免测试耦合 clap 的措辞。
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--color", "sometimes"]).is_err());
    }

    #[test]
    fn jobs_flag_parses_both_spellings() {
        // 同一个选项的两个写法都要能解析，且解析成同一个字段。
        let long = Cli::try_parse_from(["rcca", "src", "dst", "--jobs", "8"]).unwrap();
        assert_eq!(long.jobs, 8);
        let short = Cli::try_parse_from(["rcca", "src", "dst", "-j", "4"]).unwrap();
        assert_eq!(short.jobs, 4);
    }

    #[test]
    fn jobs_rejects_zero_and_garbage() {
        // 0 由 parse_jobs 拒绝，非数字由 usize 的 parse 拒绝，两条路径都要覆盖。
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "0"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "many"]).is_err());
    }

    #[test]
    fn jobs_is_capped() {
        // 每个工作线程各持一块 1 MiB 缓冲区，因此上限必须写死并被拒绝，而不是悄悄夹紧。
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "64"]).is_ok());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "65"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "100000"]).is_err());
    }

    #[test]
    fn conflicting_flags_rejected() {
        // 由 conflicts_with 声明的互斥关系在解析阶段生效，不会留到业务代码里。
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--quiet", "--verbose"]).is_err());
    }

    #[test]
    fn removed_engine_flags_are_rejected() {
        // 旧版本手写引擎的选项已删除，这里确认它们不会再被悄悄接受：
        // 未知选项会被 clap 报错，用户因此不会误以为参数生效了。
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--chunk", "4M"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--serial-io"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--no-fsync"]).is_err());
    }
}
