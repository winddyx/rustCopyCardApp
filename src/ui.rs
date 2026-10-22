//! 运行期间绘制的那一行进度：阶段、整体百分比、已完成/总量字节和当前文件。
//!
//! 没有进度条、没有速度、没有剩余时间估计。这一行在 stderr 上原地重绘。
//! 当 stderr 不是终端或指定了 `--quiet` 时完全不绘制，此时 stdout 只有结果汇总。

// 绘制工作放在独立线程里定时进行，业务线程只负责更新几个原子变量。
// 这样复制和校验的热路径上不必做任何 IO，也不会被终端写入速度拖慢。

// `IsTerminal` 提供 `is_terminal()`；`Write` 让 `write!` 能写入 stderr 句柄。
use std::io::{self, IsTerminal, Write};
// 三个原子整数类型分别承载阶段码、总字节和已扫描数；`Ordering` 是内存序。
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
// `Arc` 让同一个原子变量能被业务线程与绘制线程共享。
use std::sync::Arc;
// 绘制线程的句柄，用于在收尾时 join。
use std::thread::JoinHandle;
// 绘制线程的刷新间隔用 `Duration` 表示。
use std::time::Duration;

// 按"显示宽度"而不是字符个数计算列宽，中日韩等全角字符算两列。
use unicode_width::UnicodeWidthStr;

// 统计状态与单位表来自 stats 模块，保证进度行与汇总使用同一套单位。
use crate::stats::{Stats, BYTE_UNITS};

/// 标签列显示的阶段。
///
/// 派生 `Copy` 让这个枚举可以按值传来传去，`PartialEq`/`Eq` 让 `phase == Phase::Scan`
/// 这样的比较成立；没有载荷的枚举实现这些 trait 成本为零。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    // 扫描源目录：此时还不知道总字节数，只显示文件数。
    Scan,
    // 复制：进度的一半。
    Copy,
    // 校验：另外一半。
    Verify,
}

// 阶段与显示文本、存储编码之间的转换。
impl Phase {
    /// 返回 `&'static str`：标签来自字符串字面量，无需分配内存。
    pub fn label(self) -> &'static str {
        // 这里 `self` 是按值接收，因为 `Phase` 实现了 `Copy`。
        match self {
            Phase::Scan => "SCAN",
            Phase::Copy => "COPY",
            // 三个标签都补齐到 6 列宽（`VERIFY` 本身就是 6 个字符）。
            Phase::Verify => "VERIFY",
        }
    }

    /// 编码成 `u8`，因为原子类型里只有 `AtomicU8` 能装下它（跨线程共享的字段）。
    fn code(self) -> u8 {
        match self {
            Phase::Scan => 0,
            Phase::Copy => 1,
            Phase::Verify => 2,
        }
    }

    /// 由编码还原阶段。`_` 分支兜住所有未知值，默认回到 `Scan`。
    fn from_code(code: u8) -> Self {
        match code {
            1 => Phase::Copy,
            2 => Phase::Verify,
            _ => Phase::Scan,
        }
    }
}

/// 标签列宽度：`VERIFY` 最长，固定成 6 列后文件列的位置才不会跳动。
const LABEL_W: usize = 6;
/// 百分比列宽度，含 `%`。
const PCT_W: usize = 4;
/// 固定列之间的空隙。
const GAP: usize = 2;
/// 留给文件名的最小宽度；比这更窄时干脆不显示文件名。
const MIN_FILE: usize = 8;

/// 这一行的绘制方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    // 只有一个开关：是否输出颜色转义序列。
    pub color: bool,
}

impl Style {
    /// 纯文本样式，测试与管道输出使用。
    pub const PLAIN: Self = Self { color: false };
}

/// 变暗的 SGR 参数。ANSI 转义 `\x1b[2m ... \x1b[0m` 表示"暗色显示、随后复位"。
const SGR_DIM: &str = "2";

/// 写入侧：只更新几个数字，真正绘制交给定时线程。
///
/// 内部字段用 `Arc`（原子引用计数指针）包裹，才能被绘制线程和业务线程同时持有。
/// 这个结构体本身可以克隆，克隆只是把 `Arc` 的引用计数加一，代价很小。
#[derive(Clone)]
pub struct LiveHandle {
    // 当前阶段，存 `u8` 编码。
    phase: Arc<AtomicU8>,
    // 源文件总字节数，扫描结束后写入一次。
    total: Arc<AtomicU64>,
    // 扫描阶段已发现的文件数。
    scanned: Arc<AtomicUsize>,
}

// 业务线程通过这三个 setter 更新状态，全部是 `Relaxed`：
// 进度显示对时序没有要求，读到一个稍旧的值无妨。
impl LiveHandle {
    /// 扫描阶段目前已看到的文件数。
    pub fn set_scanned(&self, files: usize) {
        self.scanned.store(files, Ordering::Relaxed);
    }

    /// 源文件总字节数；它的两倍就是整个运行的分母。扫描结束后调用一次。
    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    /// 切换阶段。当前文件名来自 `Stats`，所以不用在这里重复设置。
    pub fn set_phase(&self, phase: Phase) {
        // 先转成 `u8` 编码再存入原子变量。
        self.phase.store(phase.code(), Ordering::Relaxed);
    }
}

/// 绘制侧：它运行期间是 stderr 上唯一的写入者。
///
/// 持有停止标志和绘制线程的句柄。句柄用 `Option<JoinHandle<()>>` 包装，
/// 是为了让 `finish` 能通过 `take()` 把线程"取走"并只 join 一次：
/// `finish` 可能被显式调用，也可能由 `Drop` 调用，重复 join 会出错。
pub struct Live {
    // 置为 true 后绘制线程在下一轮循环退出。
    stop: Arc<AtomicBool>,
    // `None` 表示从未启动过线程，或已经被 `finish` 取走。
    handle: Option<JoinHandle<()>>,
}

impl Live {
    /// 当 `enabled` 为假（stderr 不是终端或指定了 `--quiet`）时不绘制，也不启动线程。
    ///
    /// 返回一个二元组：`Live` 负责停止与清理，`LiveHandle` 交给业务代码更新数值。
    pub fn start(stats: Arc<Stats>, enabled: bool, style: Style) -> (Self, LiveHandle) {
        // 初始状态：阶段是扫描，总数与已扫描数都是 0。
        let state = LiveHandle {
            phase: Arc::new(AtomicU8::new(Phase::Scan.code())),
            total: Arc::new(AtomicU64::new(0)),
            scanned: Arc::new(AtomicUsize::new(0)),
        };
        // 停止标志先建成 `Arc`，稍后克隆一份给线程。
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if enabled {
            // 线程闭包必须满足 `'static`：它可能比当前栈帧活得更久，
            // 因此不能借用局部变量。做法是先把需要的 `Arc` 克隆进闭包，
            // 由闭包自己持有一份引用计数。`move` 关键字正是让闭包夺取这些变量。
            let stop_flag = Arc::clone(&stop);
            let ticker_state = state.clone();
            // `spawn` 返回 `JoinHandle`，线程立刻开始运行闭包体。
            Some(std::thread::spawn(move || {
                // `drawn` 记录是否画过内容，退出时只在画过的情况下才清除这一行。
                let mut drawn = false;
                // 缓存上一帧的文本，内容没变就不重写，减少终端闪烁和 IO。
                let mut last = String::new();
                loop {
                    // 取不到终端宽度时按 80 列处理；`unwrap_or` 让 `Option` 变成具体值。
                    // 每轮都重新取，用户在运行中调整窗口大小时能跟上。
                    let width = terminal_width().unwrap_or(80);
                    // `render` 是纯函数：同样的输入必然得到同样的行，便于测试。
                    let line = render(&ticker_state, &stats, width, style);
                    // 只有内容变化才写终端，静止时不做无意义的写入。
                    if line != last {
                        // 锁住 stderr 拿到一个可写句柄；`\r` 回到行首，`\x1b[2K` 清除整行。
                        let mut out = io::stderr().lock();
                        let _ = write!(out, "\r\x1b[2K{line}");
                        // 终端写入通常是行缓冲，必须显式 flush 才能立刻看到。
                        let _ = out.flush();
                        drawn = true;
                        // 移动语义：`line` 的所有权交给 `last`，下一轮重新构造新的行。
                        last = line;
                    }
                    // 收到停止信号就退出循环，不再刷新。
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    // 每 100 毫秒刷新一次；比起事件驱动，这样实现最简单，也不会忙等。
                    std::thread::sleep(Duration::from_millis(100));
                }
                if drawn {
                    // 收尾时把这一行清空，避免和随后打印的汇总混在一起。
                    let mut out = io::stderr().lock();
                    let _ = write!(out, "\r\x1b[2K");
                    let _ = out.flush();
                }
            }))
        } else {
            // 不需要绘制：不启动线程，句柄保持 `None`。
            None
        };
        // 返回绘制侧与写入侧，两者共享同一批原子变量。
        (Self { stop, handle }, state)
    }

    /// 停止刷新并在打印汇总之前清除这一行。
    pub fn finish(&mut self) {
        // 先置停止标志，让线程在下一轮检查时退出。
        self.stop.store(true, Ordering::Relaxed);
        // `take()` 把 `Option` 里的句柄取出，原位置变成 `None`，因此重复调用不会重复 join。
        // `join()` 返回 `Result`：线程 panic 时是 `Err`，这里忽略即可。
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 实现 `Drop` 让清理自动发生（RAII）：即使调用方忘了写 `finish()`，
/// 离开作用域时也会停止线程，不会留下 100 毫秒周期的空转线程。
///
/// 注意 `Drop` 里的 `finish` 需要 `&mut self`，而 `drop(&mut self)` 正好提供它。
impl Drop for Live {
    fn drop(&mut self) {
        self.finish();
    }
}

/// 到目前为止已完成的字节数：复制阶段处理的字节，加上已校验覆盖的字节
/// （校验时两侧都要读，所以计数要减半）。
pub fn done_bytes(stats: &Stats) -> u64 {
    // 两处 `load` 都是 `Relaxed`：这里只需要"大致当前"的数值。
    stats.copied_bytes.load(Ordering::Relaxed) + stats.hashed_bytes.load(Ordering::Relaxed) / 2
}

/// 把当前状态渲染成一整行文本。这个函数是纯函数，便于单元测试。
fn render(state: &LiveHandle, stats: &Stats, width: usize, style: Style) -> String {
    // 读阶段码并还原成枚举，再取出标签并补到固定宽度。
    let phase = Phase::from_code(state.phase.load(Ordering::Relaxed));
    // `paint` 在需要颜色时给标签加上 SGR 序列；纯文本样式下原样返回。
    let label = paint(&pad_label(phase.label()), SGR_DIM, style);
    if phase == Phase::Scan {
        // 扫描阶段还没有总数，只显示文件数，也没有百分比可言。
        let scanned = state.scanned.load(Ordering::Relaxed);
        // 扫描阶段还不知道总字节数，因此只显示文件数，也没有百分比。
        return format!("{label} {scanned} files");
    }

    // 总字节数在扫描结束时写入；两倍作为整个运行的分母。
    let total = state.total.load(Ordering::Relaxed);
    // 分母是总字节数的两倍（复制一半、校验一半）；`saturating_mul` 溢出时饱和到最大值而不是 panic。
    let denom = total.saturating_mul(2);
    // 完成的字节不能超过分母，否则百分比会出现大于 100 的怪值。
    let done = done_bytes(stats).min(denom);
    // `checked_div` 在分母为 0 时返回 `None`，此时按 100% 处理（清单为空即视为完成）。
    let percent = done.saturating_mul(100).checked_div(denom).unwrap_or(100) as u32;
    // `{:>3}` 表示右对齐、最少三位，这样 5% 和 95% 的百分号仍然对齐。
    let pct = format!("{percent:>3}%");
    // 字节列自己决定单位与宽度，保证分子分母同一单位。
    let bytes = bytes_columns(done, denom);
    // 当前文件名从 `Stats` 读取（那里是跨线程共享的）。
    let current = stats.current();

    // 依次尝试四种布局：完整、省略文件名、只留百分比、极端情况下截断。
    // `fixed` 是除文件名之外所有列的宽度之和。
    let fixed = LABEL_W + 1 + PCT_W + GAP + bytes.width() + GAP;
    if width >= fixed + MIN_FILE {
        // 空间够放文件名：超出部分从开头截断，保留扩展名。
        // `" ".repeat(GAP)` 生成 GAP 个空格，作为固定间隔。
        let shown = truncate_start(&current, width - fixed);
        format!("{label} {pct}{}{bytes}  {shown}", " ".repeat(GAP))
    } else if width >= fixed {
        // 放不下文件名时去掉它，并去掉行尾多余的空格。
        format!("{label} {pct}{}{bytes}", " ".repeat(GAP))
            .trim_end()
            .to_string()
    } else if width >= LABEL_W + 1 + PCT_W {
        // 再窄就连字节列都放不下，只保留标签与百分比。
        format!("{label} {pct}")
    } else {
        // 终端窄到连标签都放不下时，直接按列宽截断。
        truncate_end(&format!("{label} {pct}"), width)
    }
}

/// 分子和分母使用同一个单位，这样重绘时数字不会左右跳动。
///
/// `format!` 里的 `{num:>width$}` 用命名参数 `width` 作为最小宽度，
/// 这是运行时才知道宽度时的写法（`{:>8}` 只能写死常量）。
fn bytes_columns(done: u64, denom: u64) -> String {
    // 单位只由分母决定，分子沿用同一个单位。
    let index = unit_index(denom);
    let unit = BYTE_UNITS[index];
    // 分母决定宽度，分子按同样的宽度右对齐。
    let den = format_value(denom, index);
    let num = format_value(done.min(denom), index);
    // `width = den.width()` 是命名参数，供上面 `>width$` 使用。
    format!("{num:>width$}/{den} {unit}", width = den.width())
}

/// 挑单位的分档规则与 `human_bytes` 一致，保证这一行和汇总里的单位相同。
fn unit_index(bytes: u64) -> usize {
    let mut value = bytes as f64;
    let mut index = 0;
    // 超过 1024 就换下一级单位；`index + 1 < len` 防止越界。
    while value >= 1024.0 && index + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        index += 1;
    }
    index
}

/// 按单位下标格式化数值：字节本身是整数，更大的单位保留一位小数。
fn format_value(bytes: u64, index: usize) -> String {
    if index == 0 {
        // 单位是 B 时直接用整数，避免出现 "512.0"。
        bytes.to_string()
    } else {
        // `1024f64.powi(index as i32)` 计算 1024 的 index 次方；
        // `as i32` 是显式转换，`powi` 的指数要求是有符号整数。
        format!("{:.1}", bytes as f64 / 1024f64.powi(index as i32))
    }
}

/// 把标签补齐到 `LABEL_W` 列。
///
/// `{label:<LABEL_W$}` 里的 `LABEL_W$` 表示"宽度取名为 LABEL_W 的常量或变量"，
/// 这样宽度只在常量定义处维护一次。
fn pad_label(label: &str) -> String {
    format!("{label:<LABEL_W$}")
}

/// 给文本加上 SGR 颜色，返回普通 `String`。
fn paint(text: &str, code: &str, style: Style) -> String {
    if !style.color || text.trim().is_empty() {
        // 不需要着色，或者内容全是空白（没有可着色的字符）。
        return text.to_string();
    }
    // 尾部的空格不能被转义序列包住，否则某些终端会把宽度算错，
    // 所以先切出有效内容，再单独把填充拼回末尾。
    let trimmed = text.trim_end();
    // 用有效内容的长度切出尾部填充：这里的字节偏移恰好落在填充的起点。
    let padding = &text[trimmed.len()..];
    // `\x1b[{code}m` 开始着色，`\x1b[0m` 复位；填充留在复位之后。
    format!("\x1b[{code}m{trimmed}\x1b[0m{padding}")
}

/// 从开头截断，保留尾部。
///
/// 文件名被截断时保留尾部，是为了让扩展名（.MOV、.MP4）仍然可见。
/// 这里按 `char` 逐个处理而不是按字节切片，否则中文等多字节字符会被切开。
fn truncate_start(s: &str, max_width: usize) -> String {
    // 放得下就原样返回，`width()` 是显示宽度而不是字符数。
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        // 连省略号都放不下，返回空串。
        return String::new();
    }
    // 留一列给省略号。
    let target = max_width - 1;
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    // 从后往前收集字符，此时顺序是反的，最后需要 `reverse`。
    for ch in s.chars().rev() {
        // `UnicodeWidthStr::width` 用显示宽度而不是字符个数，全角字符算两列。
        // `ch.to_string()` 会分配一次小字符串，这里为了简洁可以接受。
        let w = ch.to_string().width();
        if used + w > target {
            break;
        }
        used += w;
        tail.push(ch);
    }
    // 收集时是反向的，先翻回来才符合阅读顺序。
    tail.reverse();
    // `collect::<String>()` 把字符向量拼成字符串，turbofish 语法指定目标类型。
    format!("…{}", tail.into_iter().collect::<String>())
}

/// 从尾部截断，保留开头。
fn truncate_end(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        // 只有一列时只能放下省略号本身。
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    // 正向收集，直到再加一个字符就会超出 `max_width - 1`（留位给省略号）。
    for ch in s.chars() {
        let w = ch.to_string().width();
        if used + w > max_width - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// 当前终端的列数。
///
/// `terminal_size()` 返回 `Option<(Width, Height)>`，这里用 `map` 取出宽度并转成 `usize`。
fn terminal_width() -> Option<usize> {
    // `|(w, _)|` 解构元组，忽略高度；`w.0` 取出 `Width` 内部的 `u16`。
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
}

/// 判断是否需要绘制：stderr 是终端且没有指定 `--quiet`。
///
/// `IsTerminal` 是标准库的 trait，`is_terminal()` 用来区分交互终端与重定向、
/// 管道的情况；重定向时不绘制，日志文件里就不会留下转义序列。
pub fn interactive(quiet: bool) -> bool {
    // 两个条件都很重要：重定向时即使不 quiet 也不应画，quiet 时即使是终端也不画。
    io::stderr().is_terminal() && !quiet
}

/// 颜色策略：显式开关优先，其次看 NO_COLOR、TERM=dumb 和是否为终端。
pub fn color_enabled(choice: crate::cli::ColorChoice) -> bool {
    // 在函数内引入枚举，避免文件顶部多一个仅此处使用的名字。
    use crate::cli::ColorChoice;
    match choice {
        // 显式要求着色时不看环境变量：用户的明确选择优先。
        ColorChoice::Always => true,
        // 显式要求不着色时同理。
        ColorChoice::Never => false,
        // `Always` 和 `Never` 已经返回，因此这里不再需要单独的 `if` 保护。
        ColorChoice::Auto => {
            // `var_os` 不要求值必须是 UTF-8，只要变量存在就算设置了 NO_COLOR。
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            // `as_deref()` 把 `Result<String, _>` 变成 `Result<&str, _>`，
            // 这样 `matches!` 就能和字符串字面量比较，不必分配新的 `String`。
            if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
                return false;
            }
            // 自动模式下最后看 stderr 是不是终端。
            io::stderr().is_terminal()
        }
    }
}

// 测试模块只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    use super::*;

    /// 直接构造一个绘制状态，绕开 `start`，测试里就不需要真的启动线程。
    fn view(phase: Phase, total: u64) -> LiveHandle {
        // 字段是私有的，但测试模块在本文件内，可以直接构造。
        LiveHandle {
            phase: Arc::new(AtomicU8::new(phase.code())),
            total: Arc::new(AtomicU64::new(total)),
            scanned: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn scan_phase_counts_files() {
        let handle = view(Phase::Scan, 0);
        // 扫描阶段只关心文件数。
        handle.set_scanned(128);
        let line = render(&handle, &Stats::new(), 80, Style::PLAIN);
        // 标签补到 6 列后再跟一个空格，所以 "SCAN" 后面是三个空格。
        assert_eq!(line, "SCAN   128 files");
    }

    #[test]
    fn line_shows_percent_bytes_and_file() {
        let stats = Stats::new();
        // 直接写原子计数，模拟"复制阶段已经处理完全部源字节"。
        stats.copied_bytes.store(3_000_000_000, Ordering::Relaxed);
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 3_000_000_000);
        let line = render(&handle, &stats, 100, Style::PLAIN);
        assert!(line.starts_with("COPY  "), "{line}");
        // 复制完成等于整体进度的一半。
        assert!(line.contains("50%"), "the copy half is 50%: {line}");
        assert!(line.ends_with("A001_06141325_C026.mov"), "{line}");
        // 任何情况下都不能超出终端宽度。
        assert!(line.width() <= 100, "{line}");
    }

    #[test]
    fn verify_counts_both_reads_as_one_pass() {
        let stats = Stats::new();
        stats.copied_bytes.store(1000, Ordering::Relaxed);
        // 源和目标各读 2000 字节，合计 4000，减半后只算 2000。
        stats.hashed_bytes.store(4000, Ordering::Relaxed);
        // 1000（复制）+ 4000/2（校验）= 3000。
        assert_eq!(done_bytes(&stats), 3000);
    }

    #[test]
    fn complete_run_reaches_one_hundred_percent() {
        let stats = Stats::new();
        // 复制 5000 字节，校验两侧合计 10000 字节，恰好等于分母的两倍。
        stats.copied_bytes.store(5_000, Ordering::Relaxed);
        stats.hashed_bytes.store(10_000, Ordering::Relaxed);
        let handle = view(Phase::Verify, 5_000);
        let line = render(&handle, &stats, 80, Style::PLAIN);
        assert!(line.contains("100%"), "{line}");
    }

    #[test]
    fn numbers_never_exceed_one_hundred_percent() {
        let stats = Stats::new();
        // 计数异常偏高时，`min(denom)` 保证不会显示超过 100%。
        stats.copied_bytes.store(9_999, Ordering::Relaxed);
        stats.hashed_bytes.store(99_999, Ordering::Relaxed);
        let handle = view(Phase::Verify, 5_000);
        let line = render(&handle, &stats, 80, Style::PLAIN);
        assert!(line.contains("100%"), "{line}");
    }

    #[test]
    fn both_phases_align_the_file_name() {
        let stats = Stats::new();
        stats.set_current("x.mov");
        // 同一份数据在 COPY 与 VERIFY 两个阶段各渲染一次。
        let copy = render(&view(Phase::Copy, 0), &stats, 80, Style::PLAIN);
        let verify = render(&view(Phase::Verify, 0), &stats, 80, Style::PLAIN);
        // 计算文件名所在列：截取到 `x` 之前的部分，再取显示宽度。
        // 闭包在测试里复用两次，省得写两遍表达式。
        let column = |line: &str| line[..line.find('x').unwrap()].width();
        assert_eq!(column(&copy), column(&verify), "the file column must align");
        assert!(verify.starts_with("VERIFY"), "{verify}");
    }

    #[test]
    fn long_names_keep_their_tail() {
        let stats = Stats::new();
        stats.set_current("session_01_cameraA_0001.MOV");
        // 52 列宽足以放下固定列，但放不下完整文件名，触发截断。
        let line = render(&view(Phase::Verify, 1_000_000), &stats, 52, Style::PLAIN);
        assert!(line.width() <= 52, "{line}");
        // 截断保留尾部，扩展名因此还在。
        assert!(line.ends_with("_0001.MOV"), "keep the extension: {line}");
    }

    #[test]
    fn narrow_terminals_drop_fields_in_order() {
        let stats = Stats::new();
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 1_000_000);
        // 扫一遍不同宽度，任何宽度下渲染结果都不能超宽。
        // 数组里的 `10usize` 给出元素类型，后面的数字沿用 `usize`。
        for width in [10usize, 12, 16, 20, 24, 30, 40, 60, 120] {
            let line = render(&handle, &stats, width, Style::PLAIN);
            assert!(line.width() <= width, "too wide at {width}: {line}");
        }
    }

    #[test]
    fn color_style_only_paints_the_label() {
        let stats = Stats::new();
        stats.set_current("x.mov");
        let handle = view(Phase::Copy, 1_000);
        let plain = render(&handle, &stats, 80, Style::PLAIN);
        // `\x1b` 是转义符：纯文本输出里不应出现任何 SGR 序列。
        assert!(!plain.contains('\x1b'), "plain text must not carry SGR");

        // 结构体字面量只覆盖一个字段，`Style` 只有这一个字段，所以不必写 `..`。
        let colored = render(&handle, &stats, 80, Style { color: true });
        // 只有标签被着色，文件名的部分保持原样。
        assert!(colored.starts_with("\x1b[2mCOPY\x1b[0m"), "{colored}");
        assert!(colored.ends_with("x.mov"), "{colored}");
    }

    #[test]
    fn byte_columns_share_one_unit() {
        // 分母是 11.0 GiB：分子沿用同一单位和宽度，所以前面补了空格。
        assert_eq!(
            bytes_columns(0, 11 * 1024 * 1024 * 1024),
            " 0.0/11.0 GiB".to_string()
        );
        assert_eq!(
            bytes_columns(7 * 1024 * 1024 * 1024, 11 * 1024 * 1024 * 1024),
            " 7.0/11.0 GiB".to_string()
        );
    }

    #[test]
    fn small_totals_use_their_own_unit() {
        // 分档规则与 human_bytes 一致：1500 字节显示为 1.5 KiB。
        assert_eq!(bytes_columns(0, 1500), "0.0/1.5 KiB".to_string());
        // 900 字节仍在 B 档，整数显示，宽度按分母对齐后前面补了两个空格。
        assert_eq!(bytes_columns(0, 900), "  0/900 B".to_string());
    }
}
