//! ntdll 原生 API（NtSetSystemInformation）与相关常量/结构体。
//!
//! 这些是未文档化的系统调用，`windows` crate 不提供，需在运行时从
//! ntdll.dll 动态解析。命令值与结构布局与 WinMemoryCleaner 的 C# 实现
//! （IgorMundstein/WinMemoryCleaner）保持一致。

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{s, w};
use windows::Win32::Foundation::{GetLastError, WIN32_ERROR};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::error::{Error, Result};

/// SYSTEM_INFORMATION_CLASS
pub(crate) const SYSTEM_FILE_CACHE_INFORMATION: i32 = 21; // 0x15
pub(crate) const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 80; // 0x50
pub(crate) const SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION: i32 = 130; // 0x82
pub(crate) const SYSTEM_REGISTRY_RECONCILIATION_INFORMATION: i32 = 155; // 0x9B

/// SYSTEM_MEMORY_LIST_COMMAND
pub(crate) const MEMORY_EMPTY_WORKING_SETS: i32 = 2;
pub(crate) const MEMORY_FLUSH_MODIFIED_LIST: i32 = 3;
pub(crate) const MEMORY_PURGE_STANDBY_LIST: i32 = 4;
pub(crate) const MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST: i32 = 5;

/// 卷设备控制码
// 注意：旧版这里写成了十进制 589828 / 589832，实为 0x90004 / 0x90008，即
// FSCTL_REQUEST_OPLOCK_LEVEL_2 / FSCTL_REQUEST_BATCH_OPLOCK——在同步打开的
// 系统卷句柄上请求 oplock 会阻塞到 oplock 被打破，可能拖住整卷 IO。
pub(crate) const FSCTL_DISCARD_VOLUME_CACHE: u32 = 0x0009_0054;
pub(crate) const FSCTL_RESET_WRITE_ORDER: u32 = 0x0009_00F8;

/// 错误码
pub(crate) const ERROR_ACCESS_DENIED: i32 = 5;
pub(crate) const ERROR_NOT_ALL_ASSIGNED: i32 = 1300;
pub(crate) const ERROR_SUCCESS: i32 = 0;

/// GetDriveTypeW 返回值：固定磁盘。
pub(crate) const DRIVE_FIXED: u32 = 3;

type NtSetSystemInformationFn = unsafe extern "system" fn(i32, *mut c_void, u32) -> i32;

/// SYSTEM_FILECACHE_INFORMATION（x64）：9 个 8 字节字段。
/// 清理系统文件缓存时仅设置 Minimum/MaximumWorkingSet 为 -1，其余为 0。
#[repr(C)]
pub(crate) struct SystemFileCacheInformation64 {
    pub current_size: i64,
    pub peak_size: i64,
    pub page_fault_count: i64,
    pub minimum_working_set: i64,
    pub maximum_working_set: i64,
    pub current_size_including_transition_in_pages: i64,
    pub peak_size_including_transition_in_pages: i64,
    pub transition_re_purpose_count: i64,
    pub flags: i64,
}

impl SystemFileCacheInformation64 {
    pub fn purge() -> Self {
        Self {
            current_size: 0,
            peak_size: 0,
            page_fault_count: 0,
            minimum_working_set: -1,
            maximum_working_set: -1,
            current_size_including_transition_in_pages: 0,
            peak_size_including_transition_in_pages: 0,
            transition_re_purpose_count: 0,
            flags: 0,
        }
    }
}

/// MEMORY_COMBINE_INFORMATION_EX（x64）：句柄、合并页数、标志全 0 即可触发合并。
#[repr(C)]
#[derive(Default)]
pub(crate) struct MemoryCombineInformationEx {
    pub handle: usize,
    pub pages_combined: usize,
    pub flags: i64,
}

/// 解析并缓存 ntdll!NtSetSystemInformation。
fn resolve_nt_set_system_information() -> Option<NtSetSystemInformationFn> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        // GetProcAddress 返回 FARPROC = Option<unsafe extern "system" fn() -> isize>。
        let proc = GetProcAddress(ntdll, s!("NtSetSystemInformation"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            NtSetSystemInformationFn,
        >(proc))
    }
}

/// 调用 NtSetSystemInformation；返回 0（STATUS_SUCCESS）视为成功。
pub(crate) unsafe fn nt_set_system_information(
    class: i32,
    info: *mut c_void,
    len: u32,
) -> Result<()> {
    static NT_SET_SYSTEM_INFORMATION: OnceLock<Option<NtSetSystemInformationFn>> = OnceLock::new();

    let f = NT_SET_SYSTEM_INFORMATION
        .get_or_init(resolve_nt_set_system_information)
        .ok_or_else(|| Error::Unsupported("NtSetSystemInformation（无法从 ntdll.dll 解析）"))?;

    let status = f(class, info, len);
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(Error::NtStatus(status))
    }
}

/// 调整令牌特权后取回真实的 GetLastError（AdjustTokenPrivileges 成功也可能
/// 残留 ERROR_NOT_ALL_ASSIGNED，表示特权未真正生效）。
pub(crate) unsafe fn last_win32_error() -> WIN32_ERROR {
    GetLastError()
}
