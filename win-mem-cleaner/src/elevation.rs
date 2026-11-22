//! 自动提权：检测当前进程是否已提权；未提权时通过 `runas`（UAC）启动
//! helper 进程执行清理，并把报告传回。
//!
//! Windows 不允许运行中的进程原地提权，唯一途径是 ShellExecuteExW 的
//! runas 动词启动一个新的提权进程。因此本 crate 提供：
//! - [`is_elevated`]：检测令牌是否已提升；
//! - [`optimize_elevated`]：已提权则本进程直接清理；否则弹 UAC 启动
//!   helper（默认为当前 exe 同目录下的 `win-mem-cleaner-helper.exe`），
//!   等待其完成后读取 JSON 报告；
//! - [`helper_main`]：helper 进程的入口逻辑（bin 包装器只需调用它）。
//!
//! 注意：**每次**未提权清理都会弹一次 UAC 确认框；用户取消会返回
//! `Error::Elevation`。若不希望定时清理反复弹窗，应让宿主程序以管理员
//! 身份运行（或仅对手动清理启用自动提权）。

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{CloseHandle, WAIT_EVENT};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    OpenProcessToken, GetCurrentProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

use crate::cleaner::{
    optimize, AreaResult, CleanOptions, CleanReason, CleanReport, MemoryArea, MemoryAreas,
};
use crate::error::{Error, Result};

/// UAC 取消的错误码。
const ERROR_CANCELLED: i32 = 1223;
/// WaitForSingleObject 的 WAIT_TIMEOUT。
const WAIT_TIMEOUT_CODE: u32 = 258;
/// SW_HIDE（避免闪出 helper 控制台窗口）。
const SW_HIDE: i32 = 0;

/// helper 进程默认文件名（宿主可自带同逻辑的 helper 并用自定义路径）。
pub const HELPER_EXE_NAME: &str = "win-mem-cleaner-helper.exe";

/// 提权清理选项。
#[derive(Debug, Clone)]
pub struct ElevateOptions {
    /// helper 可执行文件路径；`None` 时使用当前 exe 同目录下的
    /// [`HELPER_EXE_NAME`]。
    pub helper_path: Option<PathBuf>,
    /// 等待 helper 完成的超时（含用户在 UAC 对话框上停留的时间），
    /// 默认 5 分钟。
    pub timeout: Duration,
    /// 逐区域模式：每个区域清理前后采样物理内存可用量（报告中的
    /// `free_before`/`free_after`），用于观察单区域效果。
    pub stepwise: bool,
}

impl Default for ElevateOptions {
    fn default() -> Self {
        Self {
            helper_path: None,
            timeout: Duration::from_secs(5 * 60),
            stepwise: false,
        }
    }
}

/// 当前进程令牌是否已提升（管理员）。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 清理内存，必要时自动提权。
///
/// - 已提权：等价于 [`crate::optimize`]（进程内直接执行）；
/// - 未提权：弹出 UAC，以管理员启动 helper 进程执行清理，等待完成并
///   回传 [`CleanReport`]。
///
/// 失败情形（返回 Err）：用户取消 UAC、helper 不存在、等待超时、
/// helper 崩溃或报告损坏。
pub fn optimize_elevated(
    reason: CleanReason,
    areas: MemoryAreas,
    options: &CleanOptions,
    elevate: &ElevateOptions,
) -> Result<CleanReport> {
    if is_elevated() {
        return Ok(if elevate.stepwise {
            crate::cleaner::optimize_stepwise(reason, areas, options)
        } else {
            optimize(reason, areas, options)
        });
    }

    let helper = match &elevate.helper_path {
        Some(p) => p.clone(),
        None => default_helper_path()?,
    };
    if !helper.exists() {
        return Err(Error::Elevation(format!(
            "未找到提权 helper：{}（请随宿主程序一并分发）",
            helper.display()
        )));
    }

    // 配置/报告都放在临时目录（helper 端也强制校验这一点）。
    let config_path = temp_file("config")?;
    let report_path = temp_file("report")?;

    let result = (|| -> Result<CleanReport> {
        std::fs::write(&config_path, config_to_json(reason, areas, options, elevate.stepwise).to_string())
            .map_err(|e| Error::Elevation(format!("写入临时配置失败: {e}")))?;

        let parameters = HSTRING::from(format!(
            "--config {} --report {}",
            quote(&config_path),
            quote(&report_path)
        ));
        let file = HSTRING::from(helper.as_os_str());
        let verb = w!("runas");

        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: verb,
            lpFile: windows::core::PCWSTR::from_raw(file.as_ptr()),
            lpParameters: windows::core::PCWSTR::from_raw(parameters.as_ptr()),
            nShow: SW_HIDE,
            ..SHELLEXECUTEINFOW::default()
        };

        unsafe {
            ShellExecuteExW(&mut info as *mut SHELLEXECUTEINFOW).map_err(|e| {
                let code = crate::error::Error::win32(e, "ShellExecuteExW(runas)").code().unwrap_or(0);
                if code == ERROR_CANCELLED {
                    Error::Elevation("用户取消了 UAC 提权请求".into())
                } else {
                    Error::Elevation(format!("启动提权 helper 失败（Win32 错误 {code}）"))
                }
            })?;
        }

        // 等待 helper 结束（含用户盯着 UAC 对话框的时间）。
        let process = info.hProcess;
        if process.is_invalid() {
            return Err(Error::Elevation("未取得 helper 进程句柄".into()));
        }
        let wait_result = unsafe {
            WaitForSingleObject(process, elevate.timeout.as_millis().min(u32::MAX as u128) as u32)
        };
        let _ = unsafe { CloseHandle(process) };

        if wait_result == WAIT_EVENT(WAIT_TIMEOUT_CODE) {
            return Err(Error::Elevation(format!(
                "等待提权清理超时（{:?}）；helper 可能仍在运行",
                elevate.timeout
            )));
        }

        let report = std::fs::read_to_string(&report_path)
            .map_err(|e| Error::Elevation(format!("读取清理报告失败: {e}")))?;
        let value: Value = serde_json::from_str(&report)
            .map_err(|e| Error::Elevation(format!("清理报告格式损坏: {e}")))?;

        if let Some(err) = value.get("error").and_then(Value::as_str) {
            return Err(Error::Elevation(format!("helper 失败: {err}")));
        }
        report_from_json(reason, &value)
    })();

    // 清理临时文件（尽力而为）。
    let _ = std::fs::remove_file(&config_path);
    let _ = std::fs::remove_file(&report_path);
    result
}

/// helper 进程入口：解析 `--config <path> --report <path>`，执行清理并把
/// JSON 报告写入 report 文件。返回进程退出码（0 = 全部区域成功）。
///
/// 出于安全考虑（helper 以管理员身份运行），config/report 路径必须位于
/// 系统临时目录之下，避免被滥用于读写任意文件。
pub fn helper_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match arg_value(&args, "--config") {
        Some(p) => p,
        None => {
            eprintln!("用法: --config <配置文件> --report <报告文件>");
            return 2;
        }
    };
    let report_path = arg_value(&args, "--report").unwrap_or_default();

    if let Some(code) = validate_under_temp_dir(&[&config_path, &report_path]) {
        eprintln!("拒绝执行：路径必须位于系统临时目录下");
        return code;
    }

    let run = || -> Result<CleanReport> {
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| Error::Elevation(format!("读取配置失败: {e}")))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Elevation(format!("配置格式损坏: {e}")))?;
        let (reason, areas, options, stepwise) = config_from_json(&value)?;
        Ok(if stepwise {
            crate::cleaner::optimize_stepwise(reason, areas, &options)
        } else {
            optimize(reason, areas, &options)
        })
    };

    let report_json = match run() {
        Ok(report) => report_to_json(&report),
        Err(e) => json!({ "error": e.to_string() }),
    };

    let write_failed = report_path.is_empty()
        || std::fs::write(&report_path, report_json.to_string()).is_err();
    if write_failed {
        eprintln!("写入报告失败: {}", report_path.display());
        return 2;
    }

    // 0 = 全部成功；1 = 有区域失败（报告里仍有明细）。
    match report_json.get("error") {
        Some(_) => 1,
        None => {
            let all_ok = report_json
                .get("areas")
                .and_then(Value::as_array)
                .map(|areas| areas.iter().all(|a| a.get("error").map(Value::is_null).unwrap_or(false)))
                .unwrap_or(false);
            if all_ok {
                0
            } else {
                1
            }
        }
    }
}

/// 默认 helper 路径：优先当前 exe 同目录（发布形态，helper 与宿主放一起）；
/// 其次 exe 目录的父目录（cargo 开发形态，例如 example 在
/// `target/release/examples/` 下而 helper 在 `target/release/`）。
fn default_helper_path() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Elevation(format!("定位当前程序目录失败: {e}")))?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let sibling = dir.join(HELPER_EXE_NAME);
    if sibling.exists() {
        return Ok(sibling);
    }
    if let Some(parent) = dir.parent() {
        let uncle = parent.join(HELPER_EXE_NAME);
        if uncle.exists() {
            return Ok(uncle);
        }
    }
    Ok(sibling)
}

/// 唯一临时文件路径（父进程负责删除）。
fn temp_file(tag: &str) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(std::env::temp_dir().join(format!(
        "win-mem-cleaner-{}-{nanos}-{seq}-{tag}.json",
        std::process::id()
    )))
}

/// 给路径加引号（命令行参数传递）。
fn quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// 从参数列表取 `--flag <value>`。
fn arg_value(args: &[String], flag: &str) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(PathBuf::from);
        }
    }
    None
}

/// 校验所有路径都位于系统临时目录之下（对尚不存在的文件按其父目录
/// canonicalize 后前缀匹配），返回 Some(退出码) 表示校验失败。
fn validate_under_temp_dir(paths: &[&PathBuf]) -> Option<i32> {
    let temp = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    for path in paths {
        if path.as_os_str().is_empty() {
            continue;
        }
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::env::current_dir().ok()?,
        };
        let canonical_parent = std::fs::canonicalize(&parent).ok()?;
        let file_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        let canonical = canonical_parent.join(file_name);
        if !canonical.starts_with(&temp) {
            return Some(2);
        }
    }
    None
}

// ---------- JSON 线格式（公共 API 不暴露 serde 类型） ----------

fn reason_to_string(reason: CleanReason) -> &'static str {
    match reason {
        CleanReason::Manual => "Manual",
        CleanReason::Schedule => "Schedule",
        CleanReason::LowMemory => "LowMemory",
    }
}

fn reason_from_string(s: &str) -> Option<CleanReason> {
    match s {
        "Manual" => Some(CleanReason::Manual),
        "Schedule" => Some(CleanReason::Schedule),
        "LowMemory" => Some(CleanReason::LowMemory),
        _ => None,
    }
}

fn config_to_json(reason: CleanReason, areas: MemoryAreas, options: &CleanOptions, stepwise: bool) -> Value {
    json!({
        "reason": reason_to_string(reason),
        "areas": areas.bits(),
        "exclusions": options.process_exclusions,
        "stepwise": stepwise,
    })
}

fn config_from_json(value: &Value) -> Result<(CleanReason, MemoryAreas, CleanOptions, bool)> {
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .and_then(reason_from_string)
        .ok_or_else(|| Error::Elevation("配置缺少有效的 reason".into()))?;
    let bits = value.get("areas").and_then(Value::as_u64).unwrap_or(0) as u32;
    let areas = MemoryAreas::from_bits_truncate(bits);
    let exclusions = value
        .get("exclusions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let stepwise = value.get("stepwise").and_then(Value::as_bool).unwrap_or(false);
    Ok((reason, areas, CleanOptions { process_exclusions: exclusions }, stepwise))
}

fn report_to_json(report: &CleanReport) -> Value {
    json!({
        "reason": reason_to_string(report.reason),
        "total_ms": report.total_duration.as_millis() as u64,
        "areas": report
            .areas
            .iter()
            .map(|a| {
                json!({
                    "id": a.area.id(),
                    "ms": a.duration.as_millis() as u64,
                    "error": a.error.as_ref().map(|e| e.to_string()),
                    "free_before": a.free_before,
                    "free_after": a.free_after,
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn report_from_json(reason: CleanReason, value: &Value) -> Result<CleanReport> {
    let mut report = CleanReport {
        reason,
        areas: Vec::new(),
        total_duration: Duration::from_millis(
            value.get("total_ms").and_then(Value::as_u64).unwrap_or(0),
        ),
    };
    if let Some(arr) = value.get("areas").and_then(Value::as_array) {
        for item in arr {
            let Some(area) = item.get("id").and_then(Value::as_u64).and_then(|id| {
                MemoryArea::from_id(id as u32)
            }) else {
                continue;
            };
            report.areas.push(AreaResult {
                area,
                duration: Duration::from_millis(item.get("ms").and_then(Value::as_u64).unwrap_or(0)),
                error: item
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|msg| Error::Elevation(msg.to_string())),
                free_before: item.get("free_before").and_then(Value::as_u64),
                free_after: item.get("free_after").and_then(Value::as_u64),
            });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_json_roundtrip() {
        let reason = CleanReason::LowMemory;
        let areas = MemoryAreas::WORKING_SET | MemoryAreas::STANDBY_LIST;
        let options =
            CleanOptions { process_exclusions: vec!["windowisle".into(), "chrome.exe".into()] };

        let json = config_to_json(reason, areas, &options, false);
        let (r2, a2, o2, s2) = config_from_json(&json).expect("解析配置");
        assert_eq!(r2, reason);
        assert_eq!(a2, areas);
        assert_eq!(o2.process_exclusions, options.process_exclusions);
        assert!(!s2);

        let json = config_to_json(reason, areas, &options, true);
        let (_, _, _, s2) = config_from_json(&json).expect("解析配置");
        assert!(s2);
    }

    #[test]
    fn report_json_roundtrip() {
        let report = CleanReport {
            reason: CleanReason::Manual,
            areas: vec![
                AreaResult {
                    area: MemoryArea::WorkingSet,
                    duration: Duration::from_millis(12),
                    error: None,
                    free_before: Some(1_000),
                    free_after: Some(1_400),
                },
                AreaResult {
                    area: MemoryArea::StandbyList,
                    duration: Duration::from_millis(34),
                    error: Some(Error::PrivilegeRequired("SeDebugPrivilege")),
                    free_before: None,
                    free_after: None,
                },
            ],
            total_duration: Duration::from_millis(46),
        };

        let json = report_to_json(&report);
        let back = report_from_json(CleanReason::Manual, &json).expect("解析报告");
        assert_eq!(back.reason, report.reason);
        assert_eq!(back.total_duration, report.total_duration);
        assert_eq!(back.areas.len(), 2);
        assert_eq!(back.areas[0].area, MemoryArea::WorkingSet);
        assert!(back.areas[0].error.is_none());
        assert_eq!(back.areas[0].freed_bytes(), Some(400));
        assert_eq!(back.areas[1].area, MemoryArea::StandbyList);
        assert!(back.areas[1].error.is_some());
        assert_eq!(back.areas[1].freed_bytes(), None);
    }

    #[test]
    fn is_elevated_runs() {
        // 只验证可调用，不断言具体值（取决于运行环境）。
        let _ = is_elevated();
    }

    #[test]
    fn arg_parsing() {
        let args = vec!["--config".to_string(), "a.json".to_string(), "--report".to_string(), "b.json".to_string()];
        assert_eq!(arg_value(&args, "--config"), Some(PathBuf::from("a.json")));
        assert_eq!(arg_value(&args, "--report"), Some(PathBuf::from("b.json")));
        assert_eq!(arg_value(&args, "--missing"), None);
    }
}
