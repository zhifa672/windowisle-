//! # win-mem-cleaner
//!
//! Windows 内存清理库，移植自
//! [WinMemoryCleaner](https://github.com/IgorMundstein/WinMemoryCleaner)（C# / GPL-3.0）。
//!
//! 功能：
//! - [`query_memory`]：查询物理内存/页面文件使用情况；
//! - [`optimize`]：手动清理 8 个内存区域（工作集、系统文件缓存、已修改页
//!   列表、备用列表、低优先级备用列表、合并页列表、注册表缓存、已修改
//!   文件缓存）；
//! - [`start_auto_cleaner`]：自动清理调度器——**定时清理**（每 N 小时）与
//!   **可用内存低于阈值自动清理**（带 5 分钟默认冷却）。
//!
//! 系统要求：Windows 8.1 及以上；清理操作需要管理员权限（仅查询内存不需要）。
//!
//! ```no_run
//! use win_mem_cleaner::{SchedulerConfig, start_auto_cleaner, CleanEvent};
//!
//! let mut config = SchedulerConfig {
//!     interval_hours: 6,                    // 每 6 小时定时清理一次
//!     free_threshold_percent: 15,           // 可用内存低于 15% 时也触发清理
//!     ..SchedulerConfig::default()
//! };
//! let mut handle = start_auto_cleaner(config, |event| match event {
//!     CleanEvent::Tick { memory } => {
//!         println!("内存: {:.1}% 可用", memory.physical.free_percent);
//!     }
//!     CleanEvent::Cleaned { reason, report } => {
//!         println!("清理完成 ({:?}): 成功={}", reason, report.is_success());
//!     }
//!     CleanEvent::CleanFailed { reason, error } => {
//!         eprintln!("清理失败 ({reason:?}): {error}");
//!     }
//!     CleanEvent::Stopped => {}
//! });
//! // ……需要停止时：handle.stop();
//! ```

#![cfg(windows)]

mod cleaner;
mod elevation;
mod error;
mod host;
mod memory;
mod native;
mod pipe;
mod privilege;
mod scheduler;
mod service;

pub use cleaner::{
    optimize, optimize_stepwise, AreaResult, CleanOptions, CleanReason, CleanReport, MemoryArea,
    MemoryAreas, PROTECTED_PROCESSES,
};
pub use elevation::{
    helper_main, is_elevated, optimize_elevated, ElevateOptions, HELPER_EXE_NAME,
};
pub use error::{Error, Result};
pub use host::service_cli;
pub use memory::{query as query_memory, MemoryInfo, MemoryStats};
pub use pipe::{
    current_session_id, pipe_trace, service_running, service_state, Client, Message, Request, Server as PipeServer,
    TryRecv, SERVICE_NAME,
};
pub use scheduler::{start_auto_cleaner, AutoCleanHandle, CleanEvent, SchedulerConfig};
pub use service::{
    install as install_service, installed_exe_path as service_exe_path, log_dir as service_log_dir,
    run as run_service, start as start_service, uninstall as uninstall_service, ServiceState,
};
