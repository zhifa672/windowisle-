//! 内存清理实现：8 个内存区域（对应 C# ComputerService.Optimize）。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, GetDriveTypeW, GetLogicalDrives, FILE_FLAG_NO_BUFFERING,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Memory::SetSystemFileCacheSize;
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
};

use crate::error::{Error, Result};
use crate::native::{
    MemoryCombineInformationEx, SystemFileCacheInformation64, DRIVE_FIXED,
    FSCTL_DISCARD_VOLUME_CACHE, FSCTL_RESET_WRITE_ORDER, MEMORY_EMPTY_WORKING_SETS,
    MEMORY_FLUSH_MODIFIED_LIST, MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST,
    MEMORY_PURGE_STANDBY_LIST, SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION,
    SYSTEM_FILE_CACHE_INFORMATION, SYSTEM_MEMORY_LIST_INFORMATION,
    SYSTEM_REGISTRY_RECONCILIATION_INFORMATION,
};
use crate::privilege::{
    enable_privilege, SE_DEBUG_PRIVILEGE, SE_INCREASE_QUOTA_PRIVILEGE,
    SE_PROFILE_SINGLE_PROCESS_PRIVILEGE,
};

/// 可清理的内存区域（位标志组合使用）。
///
/// 位值与 C# `Enums.Memory.Areas` 完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryAreas(u32);

impl MemoryAreas {
    pub const NONE: Self = Self(0);
    pub const COMBINED_PAGE_LIST: Self = Self(1);
    pub const MODIFIED_FILE_CACHE: Self = Self(2);
    pub const MODIFIED_PAGE_LIST: Self = Self(4);
    pub const REGISTRY_CACHE: Self = Self(8);
    pub const STANDBY_LIST: Self = Self(16);
    pub const STANDBY_LIST_LOW_PRIORITY: Self = Self(32);
    pub const SYSTEM_FILE_CACHE: Self = Self(64);
    pub const WORKING_SET: Self = Self(128);

    /// 默认全选（不含低优先级备用列表），与 C# Settings 默认值一致。
    pub const ALL: Self = Self(
        Self::COMBINED_PAGE_LIST.0
            | Self::MODIFIED_FILE_CACHE.0
            | Self::MODIFIED_PAGE_LIST.0
            | Self::REGISTRY_CACHE.0
            | Self::STANDBY_LIST.0
            | Self::SYSTEM_FILE_CACHE.0
            | Self::WORKING_SET.0,
    );

    /// 推荐（默认）组合：清工作集 + 低优先级备用列表 + 合并页 + 注册表缓存。
    ///
    /// 刻意不含：完整备用列表（被换出的页面会被直接丢弃，随后全系统从磁盘
    /// 硬缺页读回，表现为整机卡死数秒到数十秒）、已修改页列表（把脏页全部
    /// 写盘）、系统文件缓存与已修改文件缓存（刷新/丢弃整卷缓存）。这几项
    /// 只在明确需要「深度清理」时使用。
    pub const RECOMMENDED: Self = Self(
        Self::WORKING_SET.0
            | Self::STANDBY_LIST_LOW_PRIORITY.0
            | Self::COMBINED_PAGE_LIST.0
            | Self::REGISTRY_CACHE.0,
    );

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for MemoryAreas {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for MemoryAreas {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// 单个内存区域（枚举形式，用于报告展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryArea {
    CombinedPageList,
    ModifiedFileCache,
    ModifiedPageList,
    RegistryCache,
    StandbyList,
    StandbyListLowPriority,
    SystemFileCache,
    WorkingSet,
}

impl MemoryArea {
    /// 英文名（与 C# Localizer 键一致）。
    pub fn name(self) -> &'static str {
        match self {
            MemoryArea::CombinedPageList => "Combined Page List",
            MemoryArea::ModifiedFileCache => "Modified File Cache",
            MemoryArea::ModifiedPageList => "Modified Page List",
            MemoryArea::RegistryCache => "Registry Cache",
            MemoryArea::StandbyList => "Standby List",
            MemoryArea::StandbyListLowPriority => "Standby List (Low Priority)",
            MemoryArea::SystemFileCache => "System File Cache",
            MemoryArea::WorkingSet => "Working Set",
        }
    }

    /// 中文名。
    pub fn name_cn(self) -> &'static str {
        match self {
            MemoryArea::CombinedPageList => "合并页列表",
            MemoryArea::ModifiedFileCache => "已修改文件缓存",
            MemoryArea::ModifiedPageList => "已修改页列表",
            MemoryArea::RegistryCache => "注册表缓存",
            MemoryArea::StandbyList => "备用列表",
            MemoryArea::StandbyListLowPriority => "低优先级备用列表",
            MemoryArea::SystemFileCache => "系统文件缓存",
            MemoryArea::WorkingSet => "工作集",
        }
    }

    /// 序列化 id（等于对应的 MemoryAreas 位值）。
    pub fn id(self) -> u32 {
        match self {
            MemoryArea::CombinedPageList => MemoryAreas::COMBINED_PAGE_LIST.bits(),
            MemoryArea::ModifiedFileCache => MemoryAreas::MODIFIED_FILE_CACHE.bits(),
            MemoryArea::ModifiedPageList => MemoryAreas::MODIFIED_PAGE_LIST.bits(),
            MemoryArea::RegistryCache => MemoryAreas::REGISTRY_CACHE.bits(),
            MemoryArea::StandbyList => MemoryAreas::STANDBY_LIST.bits(),
            MemoryArea::StandbyListLowPriority => MemoryAreas::STANDBY_LIST_LOW_PRIORITY.bits(),
            MemoryArea::SystemFileCache => MemoryAreas::SYSTEM_FILE_CACHE.bits(),
            MemoryArea::WorkingSet => MemoryAreas::WORKING_SET.bits(),
        }
    }

    /// 由序列化 id 还原。
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            x if x == MemoryAreas::COMBINED_PAGE_LIST.bits() => Some(MemoryArea::CombinedPageList),
            x if x == MemoryAreas::MODIFIED_FILE_CACHE.bits() => Some(MemoryArea::ModifiedFileCache),
            x if x == MemoryAreas::MODIFIED_PAGE_LIST.bits() => Some(MemoryArea::ModifiedPageList),
            x if x == MemoryAreas::REGISTRY_CACHE.bits() => Some(MemoryArea::RegistryCache),
            x if x == MemoryAreas::STANDBY_LIST.bits() => Some(MemoryArea::StandbyList),
            x if x == MemoryAreas::STANDBY_LIST_LOW_PRIORITY.bits() => {
                Some(MemoryArea::StandbyListLowPriority)
            }
            x if x == MemoryAreas::SYSTEM_FILE_CACHE.bits() => Some(MemoryArea::SystemFileCache),
            x if x == MemoryAreas::WORKING_SET.bits() => Some(MemoryArea::WorkingSet),
            _ => None,
        }
    }
}

/// 清理触发原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanReason {
    /// 手动触发。
    Manual,
    /// 定时清理到点触发。
    Schedule,
    /// 可用内存低于阈值触发。
    LowMemory,
}

impl CleanReason {
    pub fn name(self) -> &'static str {
        match self {
            CleanReason::Manual => "Manual",
            CleanReason::Schedule => "Schedule",
            CleanReason::LowMemory => "LowMemory",
        }
    }
}

/// 单个区域的清理结果。
#[derive(Debug, Clone)]
pub struct AreaResult {
    pub area: MemoryArea,
    pub duration: Duration,
    pub error: Option<Error>,
    /// 逐区域模式（[`optimize_stepwise`]）下，该区域清理前/后的
    /// 物理内存可用字节数；普通模式为 `None`。
    pub free_before: Option<u64>,
    pub free_after: Option<u64>,
}

impl AreaResult {
    /// 该区域清理前后物理内存可用量的净变化（逐区域模式且前后采样
    /// 成功时可用；可能为负——例如清备用列表时系统正把页换回工作集）。
    pub fn freed_bytes(&self) -> Option<i64> {
        Some(self.free_after? as i64 - self.free_before? as i64)
    }
}

/// 一次清理的整体报告。
#[derive(Debug, Clone)]
pub struct CleanReport {
    pub reason: CleanReason,
    pub areas: Vec<AreaResult>,
    pub total_duration: Duration,
}

impl CleanReport {
    /// 全部区域是否都成功。
    pub fn is_success(&self) -> bool {
        self.areas.iter().all(|a| a.error.is_none())
    }
}

/// 清理选项。
#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    /// 工作集清理的进程排除列表（进程名，带不带 .exe、大小写均可）。
    /// 非空时逐进程调用 EmptyWorkingSet（排除列表之外）；
    /// 为空时使用系统级 MemoryEmptyWorkingSets 命令，一次清空所有工作集。
    pub process_exclusions: Vec<String>,
}

/// 单个区域的清理函数。
type AreaFn = fn(&CleanOptions) -> Result<()>;

/// 区域执行顺序表（与 C# ComputerService.Optimize 一致），供逐区域
/// 执行模式（stepwise）复用。
pub(crate) fn area_steps() -> &'static [(MemoryAreas, MemoryArea, AreaFn)] {
    &[
        (MemoryAreas::WORKING_SET, MemoryArea::WorkingSet, optimize_working_set),
        (MemoryAreas::SYSTEM_FILE_CACHE, MemoryArea::SystemFileCache, optimize_system_file_cache),
        (MemoryAreas::MODIFIED_PAGE_LIST, MemoryArea::ModifiedPageList, optimize_modified_page_list),
        (MemoryAreas::STANDBY_LIST, MemoryArea::StandbyList, optimize_standby_list),
        (
            MemoryAreas::STANDBY_LIST_LOW_PRIORITY,
            MemoryArea::StandbyListLowPriority,
            optimize_standby_list_low_priority,
        ),
        (MemoryAreas::COMBINED_PAGE_LIST, MemoryArea::CombinedPageList, optimize_combined_page_list),
        (MemoryAreas::REGISTRY_CACHE, MemoryArea::RegistryCache, optimize_registry_cache),
        (MemoryAreas::MODIFIED_FILE_CACHE, MemoryArea::ModifiedFileCache, optimize_modified_file_cache),
    ]
}

/// 执行一次内存清理。
///
/// 每个选中区域独立尝试：单个区域失败不影响其他区域，
/// 结果统一记录在 [`CleanReport`] 中。大部分区域需要管理员权限。
/// 永不清空工作集的系统关键进程：桌面合成（dwm）、会话/登录、音频等。
/// 清掉它们的工作集会让整个桌面卡顿、音频爆音，收益却很小。
pub const PROTECTED_PROCESSES: &[&str] = &[
    "System", "Registry", "Memory Compression", "smss", "csrss", "wininit", "winlogon",
    "services", "lsass", "dwm", "fontdrvhost", "audiodg", "explorer", "ctfmon",
    "sihost", "ShellExperienceHost", "StartMenuExperienceHost", "SearchHost",
    "TextInputHost", "windowisle", "windowisle-clean-service",
];

/// 清理期间把当前线程切到后台模式（降低 CPU / IO / 内存优先级），
/// 清理引发的缺页与写盘让位给前台程序；离开作用域恢复。
struct BackgroundMode;

impl BackgroundMode {
    fn enter() -> Self {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        Self
    }
}

impl Drop for BackgroundMode {
    fn drop(&mut self) {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END};
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}

pub fn optimize(reason: CleanReason, areas: MemoryAreas, options: &CleanOptions) -> CleanReport {
    let _background = BackgroundMode::enter();
    let mut report = CleanReport { reason, areas: Vec::new(), total_duration: Duration::ZERO };
    let started = Instant::now();

    for (flag, area, f) in area_steps() {
        if !areas.contains(*flag) {
            continue;
        }
        let area_started = Instant::now();
        let result = f(options);
        report.areas.push(AreaResult {
            area: *area,
            duration: area_started.elapsed(),
            error: result.err(),
            free_before: None,
            free_after: None,
        });
    }

    report.total_duration = started.elapsed();
    report
}

/// 逐区域清理：每个区域执行前后采样物理内存可用量，用于观察单个区域的
/// 实际效果（普通 [`optimize`] 不采样）。区域顺序与 C# 一致。
pub fn optimize_stepwise(reason: CleanReason, areas: MemoryAreas, options: &CleanOptions) -> CleanReport {
    let _background = BackgroundMode::enter();
    let mut report = CleanReport { reason, areas: Vec::new(), total_duration: Duration::ZERO };
    let started = Instant::now();

    for (flag, area, f) in area_steps() {
        if !areas.contains(*flag) {
            continue;
        }
        // 采样需要一点时间；空闲内存的自然波动在所难免，仅供观察量级。
        let free_before = crate::memory::query().map(|m| m.physical.free_bytes).ok();
        let area_started = Instant::now();
        let result = f(options);
        let duration = area_started.elapsed();
        let free_after = crate::memory::query().map(|m| m.physical.free_bytes).ok();

        report.areas.push(AreaResult {
            area: *area,
            duration,
            error: result.err(),
            free_before,
            free_after,
        });
    }

    report.total_duration = started.elapsed();
    report
}

/// 清理工作集。
///
/// 排除列表非空：启用 SeDebugPrivilege 后逐进程 EmptyWorkingSet
/// （跳过排除进程，忽略拒绝访问的受保护进程）。
/// 排除列表为空：启用 SeProfileSingleProcessPrivilege 后用系统级命令
/// MemoryEmptyWorkingSets 一次清空。
fn optimize_working_set(options: &CleanOptions) -> Result<()> {
    if !options.process_exclusions.is_empty() {
        enable_privilege(SE_DEBUG_PRIVILEGE)?;

        let snapshot = unsafe {
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(|e| Error::win32(e, "CreateToolhelp32Snapshot"))?
        };

        let mut errors: Vec<String> = Vec::new();
        let result = (|| -> Result<()> {
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
                return Ok(()); // 无进程可枚举
            }
            loop {
                let name = process_entry_name(&entry);
                if !is_excluded(&name, &options.process_exclusions) {
                    if let Err(e) = empty_process_working_set(entry.th32ProcessID) {
                        // 拒绝访问（受保护进程）与 C# 一致地忽略。
                        let skip = matches!(
                            &e,
                            // 拒绝访问（受保护进程）与参数错误（枚举后进程已退出）都忽略。
                            Error::Win32 { code, .. } if *code == crate::native::ERROR_ACCESS_DENIED || *code == 87
                        );
                        if !skip {
                            errors.push(format!("{name}: {e}"));
                        }
                    }
                }
                if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                    break;
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(Error::Partial(errors))
            }
        })();

        let _ = unsafe { CloseHandle(snapshot) };
        result
    } else {
        enable_privilege(SE_PROFILE_SINGLE_PROCESS_PRIVILEGE)?;
        nt_memory_list_command(MEMORY_EMPTY_WORKING_SETS)
    }
}

/// 清理系统文件缓存（需要 SeIncreaseQuotaPrivilege）。
fn optimize_system_file_cache(_options: &CleanOptions) -> Result<()> {
    enable_privilege(SE_INCREASE_QUOTA_PRIVILEGE)?;

    let mut info = SystemFileCacheInformation64::purge();
    unsafe {
        crate::native::nt_set_system_information(
            SYSTEM_FILE_CACHE_INFORMATION,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of_val(&info) as u32,
        )?;
    }

    // 追加一次 SetSystemFileCacheSize(-1, -1, 0) 触发刷新。
    unsafe {
        SetSystemFileCacheSize(usize::MAX, usize::MAX, 0)
            .map_err(|e| Error::win32(e, "SetSystemFileCacheSize"))?;
    }
    Ok(())
}

/// 清理已修改页列表（需要 SeProfileSingleProcessPrivilege）。
fn optimize_modified_page_list(_options: &CleanOptions) -> Result<()> {
    enable_privilege(SE_PROFILE_SINGLE_PROCESS_PRIVILEGE)?;
    nt_memory_list_command(MEMORY_FLUSH_MODIFIED_LIST)
}

/// 清理备用列表（需要 SeProfileSingleProcessPrivilege）。
fn optimize_standby_list(_options: &CleanOptions) -> Result<()> {
    enable_privilege(SE_PROFILE_SINGLE_PROCESS_PRIVILEGE)?;
    nt_memory_list_command(MEMORY_PURGE_STANDBY_LIST)
}

/// 仅清理低优先级备用列表。
fn optimize_standby_list_low_priority(_options: &CleanOptions) -> Result<()> {
    enable_privilege(SE_PROFILE_SINGLE_PROCESS_PRIVILEGE)?;
    nt_memory_list_command(MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST)
}

/// 合并物理内存页（页合并，需要 SeProfileSingleProcessPrivilege，Win8+）。
fn optimize_combined_page_list(_options: &CleanOptions) -> Result<()> {
    enable_privilege(SE_PROFILE_SINGLE_PROCESS_PRIVILEGE)?;

    let mut info = MemoryCombineInformationEx::default();
    unsafe {
        crate::native::nt_set_system_information(
            SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of_val(&info) as u32,
        )?;
    }
    Ok(())
}

/// 清理注册表缓存（Win8.1+，无需额外特权）。
fn optimize_registry_cache(_options: &CleanOptions) -> Result<()> {
    unsafe {
        crate::native::nt_set_system_information(
            SYSTEM_REGISTRY_RECONCILIATION_INFORMATION,
            std::ptr::null_mut(),
            0,
        )?;
    }
    Ok(())
}

/// 清理已修改文件缓存：遍历所有固定磁盘卷，重置写入顺序、丢弃卷缓存并
/// 刷新文件缓冲。前两步为尽力而为（与 C# 一致），FlushFileBuffers 失败才报错。
fn optimize_modified_file_cache(_options: &CleanOptions) -> Result<()> {
    let drives = unsafe { GetLogicalDrives() };
    if drives == 0 {
        return Err(Error::Win32 { code: 0, context: "GetLogicalDrives" });
    }

    let mut first_error: Option<Error> = None;

    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }
        let letter = char::from_u32(b'A' as u32 + i).unwrap_or('A');
        let root = HSTRING::from(format!("{letter}:\\"));
        if unsafe { GetDriveTypeW(&root) } != DRIVE_FIXED {
            continue;
        }

        let volume = HSTRING::from(format!(r"\\.\{letter}:"));
        let handle = unsafe {
            CreateFileW(
                &volume,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING,
                None,
            )
        };
        let handle = match handle {
            Ok(h) => h,
            Err(e) => {
                // 打不开的卷跳过，记录第一个错误。
                if first_error.is_none() {
                    first_error = Some(Error::win32(e, "CreateFileW(卷句柄)"));
                }
                continue;
            }
        };

        unsafe {
            // FSCTL_RESET_WRITE_ORDER：需要 1 字节输入缓冲，尽力而为。
            let mut dummy: u8 = 0;
            let mut returned: u32 = 0;
            let _ = DeviceIoControl(
                handle,
                FSCTL_RESET_WRITE_ORDER,
                Some(&mut dummy as *mut u8 as *const c_void),
                1,
                None,
                0,
                Some(&mut returned),
                None,
            );

            // FSCTL_DISCARD_VOLUME_CACHE：尽力而为。
            let _ = DeviceIoControl(
                handle,
                FSCTL_DISCARD_VOLUME_CACHE,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            );

            // FlushFileBuffers：失败则记录。
            if let Err(e) = FlushFileBuffers(handle) {
                if first_error.is_none() {
                    first_error = Some(Error::win32(e, "FlushFileBuffers"));
                }
            }

            let _ = CloseHandle(handle);
        }
    }

    match first_error {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// SystemMemoryListInformation 命令的公共封装。
fn nt_memory_list_command(command: i32) -> Result<()> {
    let mut command = command;
    unsafe {
        crate::native::nt_set_system_information(
            SYSTEM_MEMORY_LIST_INFORMATION,
            &mut command as *mut i32 as *mut c_void,
            std::mem::size_of::<i32>() as u32,
        )
    }
}

/// 打开进程并清空其工作集（排除模式用）。
fn empty_process_working_set(pid: u32) -> Result<()> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
        .map_err(|e| Error::win32(e, "OpenProcess"))?;

        let result = EmptyWorkingSet(handle).map_err(|e| Error::win32(e, "EmptyWorkingSet"));
        let _ = CloseHandle(handle);
        result
    }
}

/// 取出 PROCESSENTRY32W 中的进程名（截断到 NUL）。
fn process_entry_name(entry: &PROCESSENTRY32W) -> String {
    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..len])
}

/// 判断进程名是否在排除列表中（忽略大小写与 .exe 后缀）。
fn is_excluded(process_name: &str, exclusions: &[String]) -> bool {
    let bare = strip_exe_suffix(process_name);
    exclusions
        .iter()
        .any(|ex| bare.eq_ignore_ascii_case(strip_exe_suffix(ex)))
}

/// 大小写不敏感地去掉 .exe 后缀。
fn strip_exe_suffix(name: &str) -> &str {
    if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_bits_match_csharp() {
        assert_eq!(MemoryAreas::COMBINED_PAGE_LIST.bits(), 1);
        assert_eq!(MemoryAreas::MODIFIED_FILE_CACHE.bits(), 2);
        assert_eq!(MemoryAreas::MODIFIED_PAGE_LIST.bits(), 4);
        assert_eq!(MemoryAreas::REGISTRY_CACHE.bits(), 8);
        assert_eq!(MemoryAreas::STANDBY_LIST.bits(), 16);
        assert_eq!(MemoryAreas::STANDBY_LIST_LOW_PRIORITY.bits(), 32);
        assert_eq!(MemoryAreas::SYSTEM_FILE_CACHE.bits(), 64);
        assert_eq!(MemoryAreas::WORKING_SET.bits(), 128);
    }

    #[test]
    fn areas_union_and_contains() {
        let areas = MemoryAreas::WORKING_SET | MemoryAreas::STANDBY_LIST;
        assert!(areas.contains(MemoryAreas::WORKING_SET));
        assert!(areas.contains(MemoryAreas::STANDBY_LIST));
        assert!(!areas.contains(MemoryAreas::SYSTEM_FILE_CACHE));
        assert!(!areas.is_empty());
        assert!(MemoryAreas::NONE.is_empty());
    }

    #[test]
    fn default_all_excludes_low_priority() {
        let all = MemoryAreas::ALL;
        assert!(all.contains(MemoryAreas::WORKING_SET));
        assert!(all.contains(MemoryAreas::STANDBY_LIST));
        assert!(!all.contains(MemoryAreas::STANDBY_LIST_LOW_PRIORITY));
    }

    #[test]
    fn exclusion_matching() {
        let exclusions = vec!["windowisle".to_string(), "Chrome.EXE".to_string()];
        assert!(is_excluded("windowisle.exe", &exclusions));
        assert!(is_excluded("WINDOWISLE", &exclusions));
        assert!(is_excluded("chrome.exe", &exclusions));
        assert!(!is_excluded("explorer.exe", &exclusions));
    }

    #[test]
    fn empty_areas_report_is_empty() {
        let report = optimize(CleanReason::Manual, MemoryAreas::NONE, &CleanOptions::default());
        assert!(report.areas.is_empty());
        assert!(report.is_success());
    }
}
