// 错误类型：把失败分成三类，每一类直接对应一个进程退出码。
//
// Rust 里常见的做法有两种：用 `thiserror` 派生实现样板代码，或者手写。
// 本文件手写，因为只有三个变体，手写更直观，也不必为三个变体引入派生依赖。

// `use` 只是把名字带进当前作用域，不产生运行时代码。这里引入 `fmt` 模块本身，
// 下面用到 `fmt::Display`、`fmt::Formatter`、`fmt::Result` 三个名字。
use std::fmt;

/// 顶层错误分类，与退出码一一对应。
///
/// 退出码：0 成功；1 复制或校验失败；2 用法错误；3 前置检查失败；130 被中断。
///
/// `#[derive(Debug)]` 是 `Result::unwrap` 等方法的要求，出错时打印的就是 Debug 形式。
/// 这里没有派生 `Clone`：错误一旦构造出来就只在错误路径上传递一次，不需要复制。
#[derive(Debug)]
pub enum Error {
    /// 变体的载荷是 `String`，也就是失败详情。用 `String` 而不是 `&str`，
    /// 是因为错误信息多为运行时拼装，需要拥有自己的内存，不能借用临时值。
    Usage(String),
    // 前置检查失败：磁盘空间不足、目标不可写、清单冲突等。载荷同样是 `String` 详情。
    Preflight(String),
    // 复制阶段失败，包括 sy 引擎返回的错误。它在 `main` 里对应退出码 1。
    Copy(String),
}

// 为 `Error` 实现方法。Rust 的数据与行为分开：`enum Error` 定义数据结构，
// `impl Error { ... }` 定义它能做什么。同一个类型可以写多个 `impl` 块。
impl Error {
    /// 把错误类别映射为进程退出码，见模块上面的约定。
    ///
    /// `&self` 借用而不是夺取所有权：调用方之后可能还要打印错误内容。
    /// `match` 里的 `Error::Usage(_)` 用 `_` 忽略载荷，因为我们只关心类别。
    pub fn exit_code(&self) -> i32 {
        // 被匹配的是 `&self`，但模式里不必写 `&`：Rust 的默认绑定模式（binding mode）
        // 会在按引用匹配时自动把子模式也当成引用，写起来更简洁。
        match self {
            // 每个分支是"模式 => 表达式"，最后一个分支的值就是 `match` 的值，
            // 也就是本函数的返回值；三个类别合计覆盖全部变体，因此不需要 `_` 兜底。
            Error::Usage(_) => 2,
            Error::Preflight(_) => 3,
            Error::Copy(_) => 1,
        }
    }

    // 下面三个构造器都接受 `impl Into<String>`，这是一种省略写法的泛型参数：
    // 传 `&str`、`String` 都能编译，`&str` 会自动转成 `String`。
    // 调用点因此不必关心自己手里是借来的字面量还是拼好的 `String`。

    /// 构造一个用法错误（退出码 2）。
    pub fn usage(msg: impl Into<String>) -> Self {
        // `Self` 表示当前 `impl` 的类型，这里等价于 `Error`，写 `Self` 便于将来改名。
        // `msg.into()` 的转换目标由字段类型推断出来：字段是 `String`，所以转成 `String`。
        Error::Usage(msg.into())
    }

    /// 构造一个前置检查错误（退出码 3）。
    pub fn preflight(msg: impl Into<String>) -> Self {
        // 与 `usage` 结构相同，只是换了一个变体，因此退出码也随之不同。
        Error::Preflight(msg.into())
    }

    /// 构造一个复制错误（退出码 1）。
    pub fn copy(msg: impl Into<String>) -> Self {
        Error::Copy(msg.into())
    }

    /// 前置检查里最常见的失败是 IO 失败，这里把上下文和原始 `io::Error` 拼成一条信息。
    ///
    /// 参数 `context: impl fmt::Display` 而不是 `&str`，因此 `String`、`Path` 的显示
    /// 包装等都能直接传入；`{context}` 走 `Display`，`{:?}` 才是带字段名的调试形式。
    pub fn preflight_io(context: impl fmt::Display, err: std::io::Error) -> Self {
        // `format!` 返回 `String`（不是打印）；两处 `{}` 分别调用各自的 `Display`。
        // `err` 按值传入，因为它拥有自己的操作系统错误码，借用还得处理生命周期。
        Error::Preflight(format!("{context}: {err}"))
    }
}

/// 为自定义错误实现 `Display` 后，`eprintln!("{err}")` 与 `to_string()` 才能工作。
///
/// `Formatter<'_>` 的 `'_` 是省略的生命周期参数：写全了是 `Formatter<'a>`，
/// 这里无需给生命周期命名，交给编译器推断。
impl fmt::Display for Error {
    // `&self` 借用错误本身，`f` 是格式化输出目标；
    // `fmt::Result` 是 `Result<(), fmt::Error>` 的别名，成功时不携带值。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 每个分支都返回 `write!` 的结果：它把文本写入 `f` 并返回 `Result`，
        // 与会 panic 的 `print!` 不同，`Display` 的契约要求把错误向上传递。
        match self {
            // `{m}` 调用载荷 `String` 的 `Display`，把内容嵌进固定前缀后面。
            Error::Usage(m) => write!(f, "usage error: {m}"),
            Error::Preflight(m) => write!(f, "preflight failed: {m}"),
            Error::Copy(m) => write!(f, "copy failed: {m}"),
        }
    }
}

/// `std::error::Error` 是标准库的错误 trait，此处没有额外方法要覆盖，用空实现即可。
///
/// 它的意义在于让本类型可以被当作 `Box<dyn Error>` 使用，也让下游能用统一的
/// `Error` trait 接口处理错误。若某个函数返回 `Result<T, Box<dyn Error>>`，
/// 就能用 `?` 直接把本类型转成盒装错误（标准库里已有对应的 `From` 实现）。
impl std::error::Error for Error {}

/// 类型别名：本项目内部一律写 `Result<T>`，省掉每次重复错误类型。
///
/// 注意它在名字上遮蔽了标准库的 `std::result::Result`，等价写法是
/// `Result<T, Error>`；需要指定别的错误类型时写全路径 `std::result::Result<T, E>`。
pub type Result<T> = std::result::Result<T, Error>;
