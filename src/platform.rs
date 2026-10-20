//! 平台相关的少量判定：把系统错误码归类为"空间不足"与"设备 IO 错误"。

/// 目标卷空间不足或只读。
pub fn is_disk_full(raw: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        matches!(raw, Some(code) if code == libc::ENOSPC || code == libc::EROFS || code == libc::EDQUOT)
    }
    #[cfg(windows)]
    {
        // ERROR_HANDLE_DISK_FULL = 39，ERROR_DISK_FULL = 112，ERROR_WRITE_PROTECT = 19
        matches!(raw, Some(19) | Some(39) | Some(112))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = raw;
        false
    }
}

/// 传输层 IO 错误：通常意味着设备已断开或介质损坏。
pub fn is_device_io_error(raw: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        matches!(raw, Some(code) if code == libc::EIO)
    }
    #[cfg(windows)]
    {
        // ERROR_CRC = 23，ERROR_IO_DEVICE = 1117，ERROR_DEVICE_NOT_CONNECTED = 1167
        matches!(raw, Some(23) | Some(1167) | Some(1117))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = raw;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_codes() {
        #[cfg(unix)]
        {
            assert!(is_disk_full(Some(libc::ENOSPC)));
            assert!(is_disk_full(Some(libc::EROFS)));
            assert!(!is_disk_full(Some(libc::EIO)));
            assert!(is_device_io_error(Some(libc::EIO)));
            assert!(!is_device_io_error(Some(libc::ENOSPC)));
        }
        assert!(!is_disk_full(None));
        assert!(!is_device_io_error(None));
    }
}
