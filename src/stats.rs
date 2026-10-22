//! 各工作线程共享的运行状态：原子字节计数、有上限的失败列表、取消标志和当前文件名。

// 这里同时用到原子类型和互斥锁，原因是两类数据的需求不同：
// 计数器只做加法，用原子变量最快；字符串列表和当前文件名需要整体读写，
// 用 `Mutex` 保护更简单。

// 三个原子类型分别承载 64 位、机器字大小的无符号整数和一个布尔标志；
// `Ordering` 是内存序枚举，决定原子操作对其它线程的可见性强度。
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
// `Mutex` 提供互斥访问：同一时刻只有一个线程能拿到数据。
use std::sync::Mutex;

/// 保留的失败样例条数上限。
pub const MAX_FAILURE_EXAMPLES: usize = 32;
/// 保留的失败详情字节数上限。
pub const MAX_FAILURE_DETAIL_BYTES: usize = 64 * 1024;
/// --verbose 时尽量保留全部失败，实际上等于不设限。
pub const VERBOSE_FAILURE_EXAMPLES: usize = 4096;
// 单独列出字节预算常量，便于和上面的条数上限一起调整。
pub const VERBOSE_FAILURE_DETAIL_BYTES: usize = 4 * 1024 * 1024;

/// 失败记录：计数不设上限，样例与详情字节数有上限。
///
/// 这个类型只在持有 `Mutex` 时被访问，所以内部字段都是普通的 `usize`/`Vec`，
/// 不需要原子类型。锁被释放前不会有第二个线程读它，因此无需额外同步。
#[derive(Default)]
struct FailureLog {
    // 记录了多少条失败，与 `examples.len()` 不是一回事：样例会被截断，计数不会。
    count: usize,
    // 已保存样例占用的字节总数，用来执行 `MAX_FAILURE_DETAIL_BYTES` 预算。
    detail_bytes: usize,
    // 真正保留下来的失败文本，按记录顺序排列。
    examples: Vec<String>,
}

// 这个 `impl` 块只有私有方法，因此只有本文件内部能调用。
impl FailureLog {
    /// 记录一条失败。计数总会加一，但样例满了或超出字节预算就只记数不存内容。
    ///
    /// 参数用 `String` 而不是 `&str`：调用方拼好的信息需要被保存下来，
    /// 借用的话生命周期撑不过本次调用。这里也不克隆，直接移动进 `Vec`。
    fn record(&mut self, message: String, max_examples: usize, max_bytes: usize) {
        // `&mut self` 允许修改自身字段；锁保证同时只有一个线程能做到这一点。
        self.count += 1;
        // 样例条数到上限：只累加计数，立刻返回。
        if self.examples.len() >= max_examples {
            return;
        }
        // 再加一条就会超出字节预算：同样只累加计数。
        // `message.len()` 是 UTF-8 字节数，与预算口径一致。
        if self.detail_bytes + message.len() > max_bytes {
            return;
        }
        // 记账后再保存，两处口径因此始终一致。
        self.detail_bytes += message.len();
        // `push` 把 `message` 的所有权移入向量，不发生复制。
        self.examples.push(message);
    }
}

/// 复制回调、校验循环和进度行之间共享的状态。
///
/// 之所以全是原子类型，是因为这些计数器会被多处并发读写，而统计不需要强一致：
/// 进度显示差一个字节无所谓，因此一律用开销最小的 `Ordering::Relaxed`。
/// 真正需要严格顺序的地方（取消标志）才用 `SeqCst`。
pub struct Stats {
    /// 复制阶段处理的字节：文件被创建、更新或跳过时都计入。
    pub copied_bytes: AtomicU64,
    /// 校验阶段读到的字节。源和目标都要读，因此这个值约为文件大小的两倍。
    pub hashed_bytes: AtomicU64,
    /// sy 实际复制的普通文件数（被跳过的文件不计）。
    pub copied_files: AtomicUsize,
    // preflight 阶段因不可读而被跳过的文件数。
    pub files_skipped: AtomicUsize,
    /// Ctrl-C 置位的取消标志。
    pub cancel: AtomicBool,
    // 下面几个字段需要整体读写，用 `Mutex` 保护。字段私有，只能通过方法访问，
    // 这样锁不会被外部忘记获取。
    // 进度行显示的当前文件名。
    current: Mutex<String>,
    // 失败记录本体。
    failures: Mutex<FailureLog>,
    /// 失败列表的上限（样例条数, 详情字节数），在构造时确定。
    // 用二元组而不是两个字段：两者总是一起传递，打包后不易错配。
    failure_limits: (usize, usize),
}

/// 手写 `Default` 让它与 `new()` 行为一致。
///
/// 不能直接派生 `Default`，因为派生出来的版本会把所有字段取默认值（上限变成 0），
/// 与 `new()` 的语义不同。Clippy 的 `new_without_default` 规则要求"有 `new` 就要有
/// `Default`"，这个实现同时满足该规则和语义一致性。
impl Default for Stats {
    fn default() -> Self {
        // 转调 `new()`，保证两条入口得到同样的默认值。
        Self::new()
    }
}

// 对外（crate 内）的接口：构造、更新计数、读取汇总。
impl Stats {
    pub fn new() -> Self {
        // 默认上限：32 条样例、64 KiB 详情。
        Self::with_failure_limits(MAX_FAILURE_EXAMPLES, MAX_FAILURE_DETAIL_BYTES)
    }

    /// 与 `new` 相同，但保留完整的失败列表而不是前 32 条。
    pub fn verbose() -> Self {
        // 换一组宽松的上限即可，其余初始化逻辑完全复用。
        Self::with_failure_limits(VERBOSE_FAILURE_EXAMPLES, VERBOSE_FAILURE_DETAIL_BYTES)
    }

    /// 构造并指定失败列表上限；上面的两个构造器都转调到它。
    pub fn with_failure_limits(examples: usize, detail_bytes: usize) -> Self {
        // 结构体字面量一次性初始化所有字段；原子类型必须显式给出初值。
        Self {
            copied_bytes: AtomicU64::new(0),
            hashed_bytes: AtomicU64::new(0),
            copied_files: AtomicUsize::new(0),
            files_skipped: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            // 空字符串表示"还没有当前文件"，进度行据此决定是否显示文件名。
            current: Mutex::new(String::new()),
            // `FailureLog` 派生了 `Default`，这里用它构造零值。
            failures: Mutex::new(FailureLog::default()),
            failure_limits: (examples, detail_bytes),
        }
    }

    /// 更新"当前文件"。参数 `impl Into<String>` 让调用方传 `&str` 也能通过。
    pub fn set_current(&self, name: impl Into<String>) {
        // `lock()` 返回 `Result`：持锁线程 panic 会导致锁中毒（poisoned）。
        // 进度显示不必为此中断，因此用 `if let Ok(...)` 忽略中毒的情况。
        if let Ok(mut guard) = self.current.lock() {
            // `*guard = ...` 通过 `DerefMut` 解引用到被保护的 `String` 再赋值；
            // `guard` 必须声明为 `mut` 才能这样写。
            *guard = name.into();
        }
    }

    /// 读取当前文件名；取不到（锁中毒）时返回空串。
    pub fn current(&self) -> String {
        // 注意这里返回的是 `String` 的克隆：锁必须尽快释放，
        // 不能把 `MutexGuard` 或它内部数据的引用交给调用方。
        // 方法解析先尝试 `MutexGuard`，没有 `clone` 才自动解引用到 `String`。
        self.current.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 记录一次失败；计数和详情都受上限约束。
    pub fn note_failure(&self, message: String) {
        // 上限是 `Copy` 的二元组，先复制出来，避免持锁期间再去读 `self` 的字段。
        let (examples, detail_bytes) = self.failure_limits;
        // 锁中毒时丢弃这条失败，比让整个运行 panic 更合适。
        if let Ok(mut log) = self.failures.lock() {
            log.record(message, examples, detail_bytes);
        }
    }

    /// 失败总条数，含未保存详情的那些。
    pub fn failure_count(&self) -> usize {
        // `map` 在 `Ok` 时取字段，`unwrap_or(0)` 处理中毒的情形。
        self.failures.lock().map(|l| l.count).unwrap_or(0)
    }

    /// 已保存的失败样例（可能少于总数）。
    pub fn failure_examples(&self) -> Vec<String> {
        // 同样克隆一份出去，避免把锁的生命周期泄露给调用方。
        self.failures
            .lock()
            .map(|l| l.examples.clone())
            .unwrap_or_default()
    }

    /// 是否已收到停止请求（Ctrl-C），供工作线程在循环里检查。
    pub fn stop_requested(&self) -> bool {
        // `Relaxed` 足够：这里只关心"最终能看到置位"，不关心与其他变量的先后。
        self.cancel.load(Ordering::Relaxed)
    }
}

/// 二进制单位前缀。
pub const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// 把字节数拆成数值和单位，便于调用方自行对齐列宽。
///
/// 返回 `(f64, &'static str)`：单位来自常量数组，因此返回的引用必须标注
/// `'static`，表示它指向的数据活得和程序一样久。
pub fn human_bytes_parts(bytes: u64) -> (f64, &'static str) {
    // 转成浮点再连续除以 1024，整数除法会丢掉小数部分。
    let mut value = bytes as f64;
    // 单位下标从 0（字节）开始。
    let mut unit = 0;
    // 上限判断放在循环条件里，防止索引越界；`unit + 1 < len` 保证下标始终有效。
    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // 返回元组：数值和单位名一起交给调用方。
    (value, BYTE_UNITS[unit])
}

/// 按二进制前缀格式化字节数。
pub fn human_bytes(bytes: u64) -> String {
    let (value, unit) = human_bytes_parts(bytes);
    // 元组解构先把两个值分开，后面按单位选择不同的小数位。
    if unit == "B" {
        // 字节数本身是整数，`round()` 后转成 `u64` 打印，避免出现 "512.0 B"。
        // `as u64` 是显式数值转换：`round()` 先把浮点四舍五入，再截断成整数。
        format!("{} {unit}", value.round() as u64)
    } else {
        // `{value:.1}` 表示保留一位小数。
        format!("{value:.1} {unit}")
    }
}

/// 把秒数格式化成 mm:ss 或 hh:mm:ss。
pub fn human_duration(seconds: u64) -> String {
    // 三段取余与整除：`% 60` 得到秒和分，`/ 3600` 得到小时。
    let s = seconds % 60;
    let m = (seconds / 60) % 60;
    let h = seconds / 3600;
    if h > 0 {
        // `{:02}` 表示至少两位、左侧补零，凑够固定宽度。
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

// 测试模块只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    // 引入父模块的常量与函数。
    use super::*;

    #[test]
    fn failure_log_is_bounded() {
        let stats = Stats::new();
        // 记录远多于上限的失败，确认计数完整、样例被截断。
        for i in 0..100 {
            // `format!` 生成每条不同的文本，`note_failure` 接管所有权。
            stats.note_failure(format!("failure {i}"));
        }
        assert_eq!(stats.failure_count(), 100);
        assert_eq!(stats.failure_examples().len(), MAX_FAILURE_EXAMPLES);
    }

    #[test]
    fn verbose_stats_keep_a_long_failure_list() {
        // 只有构造方式不同，统计逻辑一致。
        let stats = Stats::verbose();
        for i in 0..100 {
            stats.note_failure(format!("failure {i}"));
        }
        // --verbose 下的上限远大于 100，因此样例不会丢。
        assert_eq!(stats.failure_count(), 100);
        assert_eq!(stats.failure_examples().len(), 100);
    }

    #[test]
    fn failure_detail_bytes_are_bounded() {
        let stats = Stats::new();
        // `repeat` 生成一条 4096 字节的信息，用来触发字节数上限。
        let long = "x".repeat(4096);
        for _ in 0..100 {
            // `String` 是一次性的，所以每条都要 `clone()`；`_` 表示不使用循环变量。
            stats.note_failure(long.clone());
        }
        // 上限是 64 KiB，每条 4 KiB，因此最多能存下 16 条左右，这里留一点余量。
        assert!(stats.failure_examples().len() <= MAX_FAILURE_DETAIL_BYTES / 4096 + 1);
    }

    #[test]
    fn human_units() {
        // 边界与进位各测一个：0 字节、刚进到 KiB、不足一分钟、超过一小时。
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_duration(65), "01:05");
        assert_eq!(human_duration(3725), "01:02:05");
    }
}
