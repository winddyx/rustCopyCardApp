//! 输出设备准备：让 Windows 控制台按 UTF-8 解释 rcca 打印的字节。
//!
//! rcca 的 stdout 与 stderr 都是 UTF-8 字节流（中文目录名、`·` 分隔符、`✓` 都在其中）。
//! Unix 的终端直接按 UTF-8 解码，不存在额外步骤；Windows 控制台则按"输出代码页"
//! 解码字节，而简体中文系统的默认代码页是 936（GBK）。此时一行 `sub\照片_0001.JPG`
//! 会变成乱码。
//!
//! 因此程序启动时把控制台输出代码页切到 UTF-8，并在退出前恢复原值：代码页是控制台的
//! 全局状态，同一个窗口里随后运行的其他程序仍按原来的代码页工作，rcca 不应留下副作用。
//! 输出不是控制台（管道、文件、重定向）时什么也不做，字节流本来就按 UTF-8 传递。

// `Ordering` 只有 Windows 分支用到，因此按平台引入。
#[cfg(windows)]
use std::sync::atomic::Ordering;

/// 把控制台输出代码页切到 UTF-8；调用方应在打印路径与结果之前调用它。
///
/// Windows 上的实现只在 stdout 确实是控制台时才动手：被重定向时字节原样写出，
/// 代码页不参与解码，改它只会影响别的程序。
#[cfg(windows)]
pub fn use_utf8() {
    // 只声明用到的四个函数，避免为此引入 windows-sys 依赖。
    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "GetStdHandle"]
        fn get_std_handle(which: u32) -> isize;
        #[link_name = "GetConsoleMode"]
        fn get_console_mode(handle: isize, mode: *mut u32) -> i32;
        #[link_name = "GetConsoleOutputCP"]
        fn get_console_output_cp() -> u32;
        #[link_name = "SetConsoleOutputCP"]
        fn set_console_output_cp(code_page: u32) -> i32;
    }
    // `STD_OUTPUT_HANDLE` 是 -11，`INVALID_HANDLE_VALUE` 是 -1，都按位模式写成常量。
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const INVALID_HANDLE_VALUE: isize = -1;
    // 65001 是 UTF-8 的代码页编号。
    const CP_UTF8: u32 = 65001;

    // 安全性：四个函数只收发句柄与整数，`mode` 指向本栈帧里可写的 `u32`，
    // 失败时返回 0，没有需要释放的资源。
    unsafe {
        let handle = get_std_handle(STD_OUTPUT_HANDLE);
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return;
        }
        // 取不到控制台模式说明不是控制台：保持原样，别去改别人的代码页。
        let mut mode: u32 = 0;
        if get_console_mode(handle, &mut mode) == 0 {
            return;
        }
        let current = get_console_output_cp();
        // 已经是 UTF-8（或读取失败返回 0）时不需要记录，也没有可恢复的东西。
        if current == 0 || current == CP_UTF8 {
            return;
        }
        if set_console_output_cp(CP_UTF8) != 0 {
            // 记住原值，退出时用它恢复；`Relaxed` 足够，这里只有先后关系要求。
            ORIGINAL_CODE_PAGE.store(current, Ordering::Relaxed);
        }
    }
}

/// 非 Windows 平台不需要准备输出设备：终端一律按 UTF-8 解码。
#[cfg(not(windows))]
pub fn use_utf8() {}

/// 退出前把控制台输出代码页恢复成启动时的值。
///
/// 没有记录过原值（不是控制台、本来就是 UTF-8，或者其他平台）时什么也不做。
#[cfg(windows)]
pub fn restore() {
    // 与 `use_utf8` 中相同的声明；两个函数各自独立，避免共享可变全局状态。
    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "SetConsoleOutputCP"]
        fn set_console_output_cp(code_page: u32) -> i32;
    }
    // `swap` 把记录清零，所以重复调用只会真正恢复一次。
    let original = ORIGINAL_CODE_PAGE.swap(0, Ordering::Relaxed);
    if original != 0 {
        // 安全性：只传一个整数参数，返回值表示成功与否，失败也不需要补救。
        unsafe {
            set_console_output_cp(original);
        }
    }
}

/// 非 Windows 平台没有需要恢复的状态。
#[cfg(not(windows))]
pub fn restore() {}

/// 启动时记录的代码页，0 表示"没有改过，不需要恢复"。
///
/// 用原子变量而不是 `static mut`：`restore` 也可能从 Ctrl-C 处理线程调用，
/// 原子操作让这里不需要 `unsafe` 也不需要锁。
#[cfg(windows)]
static ORIGINAL_CODE_PAGE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
