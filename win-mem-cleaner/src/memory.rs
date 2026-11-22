//! 内存信息查询（GlobalMemoryStatusEx）。

use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::error::{Error, Result};

/// 一块内存（物理内存或页面文件提交内存）的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryStats {
    /// 总量（字节）。
    pub total_bytes: u64,
    /// 可用量（字节）。
    pub free_bytes: u64,
    /// 已用量（字节）。
    pub used_bytes: u64,
    /// 使用百分比（0-100）。
    pub used_percent: u8,
    /// 可用百分比（0-100）。
    pub free_percent: u8,
}

impl MemoryStats {
    /// 由总量/可用量/使用百分比（可选）构造，与 C# MemoryStats 语义一致：
    /// 物理内存使用 dwMemoryLoad，页面文件按 used/total 计算。
    pub(crate) fn new(free: u64, total: u64, used_percent_override: Option<u8>) -> Self {
        let used = total.saturating_sub(free);
        let used_percent = match used_percent_override {
            Some(p) => p,
            None => (used * 100).checked_div(total).map(|v| v.min(100) as u8).unwrap_or(0),
        };
        Self {
            total_bytes: total,
            free_bytes: free,
            used_bytes: used,
            used_percent,
            free_percent: 100 - used_percent,
        }
    }
}

/// 当前内存信息：物理内存 + 页面文件（提交内存）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryInfo {
    /// 物理 RAM。
    pub physical: MemoryStats,
    /// 页面文件提交内存。
    pub page_file: MemoryStats,
}

/// 查询当前内存信息。
pub fn query() -> Result<MemoryInfo> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..MEMORYSTATUSEX::default()
    };

    unsafe {
        GlobalMemoryStatusEx(&mut status)
            .map_err(|e| Error::win32(e, "GlobalMemoryStatusEx"))?;
    }

    Ok(MemoryInfo {
        // dwMemoryLoad：系统报告的物理内存使用百分比。
        physical: MemoryStats::new(
            status.ullAvailPhys,
            status.ullTotalPhys,
            Some(status.dwMemoryLoad as u8),
        ),
        page_file: MemoryStats::new(status.ullAvailPageFile, status.ullTotalPageFile, None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_uses_reported_load() {
        let s = MemoryStats::new(2_000, 10_000, Some(80));
        assert_eq!(s.used_percent, 80);
        assert_eq!(s.free_percent, 20);
        assert_eq!(s.used_bytes, 8_000);
    }

    #[test]
    fn page_file_computes_percentage() {
        let s = MemoryStats::new(2_500, 10_000, None);
        assert_eq!(s.used_percent, 75);
        assert_eq!(s.free_percent, 25);
    }

    #[test]
    fn zero_total_is_safe() {
        let s = MemoryStats::new(0, 0, None);
        assert_eq!(s.used_percent, 0);
        assert_eq!(s.free_percent, 100);
    }

    #[test]
    fn free_larger_than_total_clamps() {
        let s = MemoryStats::new(12_000, 10_000, None);
        assert_eq!(s.used_bytes, 0);
        assert_eq!(s.used_percent, 0);
    }

    #[test]
    fn query_roundtrip() {
        let info = query().expect("普通用户权限即可查询内存");
        assert!(info.physical.total_bytes > 0);
        assert!(info.physical.used_percent <= 100);
    }
}
