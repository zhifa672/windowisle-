//! Windows 服务（SCM）宿主：安装/卸载/运行「窗屿内存清理服务」。
//!
//! 服务以 LocalSystem 运行，**按需启动**：启动类型为手动，开机不运行；
//! 宿主（windowisle）启动时用 [`start`] 拉起它（安装时给交互用户只开放
//! SERVICE_START，见 [`SERVICE_SDDL`]），宿主全部退出后服务空闲一段时间
//! 自行停止（见 host.rs）。于是「清理服务是否开机运行」自动跟随 windowisle
//! 自己的开机自启，无需另设开关。崩溃由 SCM 按失败动作自动重启。
//! windowisle 主进程保持普通权限，通过 [`crate::pipe`] 的命名管道下达清理命令；
//! 首次安装需要一次 UAC（runas 本程序 `--service-install`），之后全程无感。

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
    KEY_WOW64_64KEY,
};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, QueryServiceStatusEx, RegisterServiceCtrlHandlerExW, SC_HANDLE,
    SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICES_ACTIVE_DATABASE, SERVICE_STATUS_PROCESS,
    StartServiceW,
    SetServiceObjectSecurity, SERVICE_ACCEPT_STOP, SERVICE_DEMAND_START,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_WIN32_OWN_PROCESS,
};

use crate::error::{Error, Result};

const DISPLAY_NAME: PCWSTR = w!("窗屿内存清理服务（WindowIsle）");

/// 服务访问权组合（0.58 里是裸 u32 位标志）。
/// SERVICE_CHANGE_CONFIG = 0x0002，DELETE（通用访问权）= 0x00010000，
/// WRITE_DAC = 0x00040000（设置服务 DACL）。
const SERVICE_RIGHTS_MANAGE: u32 = SERVICE_START
    | SERVICE_STOP
    | SERVICE_QUERY_STATUS
    | 0x0001_0000 /* DELETE */
    | 0x0002 /* CHANGE_CONFIG */
    | 0x0004_0000 /* WRITE_DAC */;

/// 服务 DACL：在 Windows 默认服务权限基础上，只给交互登录用户（IU）
/// 加了 RP（SERVICE_START），好让普通权限的 windowisle 按需拉起服务。
/// 刻意**不**给 DC（CHANGE_CONFIG）/WP（STOP）/SD（DELETE）/WD/WO：
/// 能改配置就能把程序路径换成任意程序并以 SYSTEM 运行，等于提权。
/// 启动一个只做内存清理、只听本会话管道的服务，没有可滥用的面。
pub const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
const SERVICE_RIGHTS_UNINSTALL: u32 = SERVICE_STOP | SERVICE_QUERY_STATUS | 0x0001_0000 /* DELETE */;

// ---------------------------------------------------------------- 服务程序位置

/// 复制到 Program Files 后的服务程序文件名。
pub const SERVICE_EXE_NAME: &str = "windowisle-clean-service.exe";

/// 服务程序的固定安装位置：`%ProgramFiles%\WindowIsle\windowisle-clean-service.exe`
/// （安装时复制的是宿主程序本身——通常就是 windowisle.exe）。
///
/// 窗屿本体装在用户可写的 `%LOCALAPPDATA%`，若直接把那里的程序注册成
/// SYSTEM 服务，任何以当前用户身份运行的程序都能替换它来提权。因此安装时
/// 把程序复制到只有管理员可写的 Program Files 再注册；客户端也只认
/// 这个路径上的服务进程。目录取自 HKLM 注册表（而非可被用户变量覆盖的
/// 环境变量），读不到时回退 `C:\Program Files`。
pub fn installed_exe_path() -> PathBuf {
    program_files_dir().join("WindowIsle").join(SERVICE_EXE_NAME)
}

/// 服务日志目录（与服务程序同在受保护目录下，避免 SYSTEM 往用户可控的
/// 目录写文件）。
pub fn log_dir() -> PathBuf {
    program_files_dir().join("WindowIsle").join("logs")
}

fn program_files_dir() -> PathBuf {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion"),
            0,
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
            &mut key,
        )
        .0
            == 0
        {
            let mut buffer = [0u16; 520];
            let mut size = (buffer.len() * 2) as u32;
            let rc = RegQueryValueExW(
                key,
                w!("ProgramFilesDir"),
                None,
                None,
                Some(buffer.as_mut_ptr() as *mut u8),
                Some(&mut size),
            );
            let _ = RegCloseKey(key);
            if rc.0 == 0 {
                let len = (size as usize / 2).min(buffer.len());
                let text = String::from_utf16_lossy(&buffer[..len]);
                let text = text.trim_end_matches('\0').trim();
                if !text.is_empty() {
                    return PathBuf::from(text);
                }
            }
        }
    }
    PathBuf::from(r"C:\Program Files")
}

// ---------------------------------------------------------------- 安装 / 卸载 / 查询

/// 安装并启动服务（需要管理员）。`source` 是宿主程序自身（windowisle.exe），先复制到
/// [`installed_exe_path`] 再注册。幂等：服务已存在时停止 → 覆盖程序 →
/// 改正程序路径 → 重新启动，用于升级 helper 或修复旧版本（旧版把用户
/// 目录里的 helper 直接注册成了服务）。
pub fn install(source: &Path) -> Result<()> {
    let target = installed_exe_path();
    unsafe {
        let manager = OpenSCManagerW(None, SERVICES_ACTIVE_DATABASE, SC_MANAGER_ALL_ACCESS)
            .map_err(|e| Error::win32(e, "OpenSCManager（需要管理员权限）"))?;
        let result = (|| -> Result<()> {
            // 改名迁移：先卸掉旧版「岛仔」的清理服务（这里已有管理员权限）。
            remove_legacy_service(manager);
            let existing = OpenServiceW(manager, w!("WindowIsleClean"), SERVICE_RIGHTS_MANAGE).ok();
            if let Some(service) = existing {
                // 运行中的服务锁着程序文件：先停下再覆盖。
                stop_and_wait(service);
            }
            let copied = copy_helper(source, &target);
            if let Err(error) = copied {
                if let Some(service) = existing {
                    let _ = CloseServiceHandle(service);
                }
                return Err(error);
            }
            let binary = format!("\"{}\" --service-run", target.display());
            let wide = windows::core::HSTRING::from(binary);
            let service = match existing {
                Some(service) => {
                    ChangeServiceConfigW(
                        service,
                        SERVICE_WIN32_OWN_PROCESS,
                        // 旧版是开机自动启动；更新时改成按需启动。
                        SERVICE_DEMAND_START,
                        SERVICE_ERROR_NORMAL,
                        PCWSTR(wide.as_ptr()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        DISPLAY_NAME,
                    )
                    .map_err(|e| {
                        let _ = CloseServiceHandle(service);
                        Error::win32(e, "ChangeServiceConfig（更新服务程序路径）")
                    })?;
                    service
                }
                None => CreateServiceW(
                    manager,
                    w!("WindowIsleClean"),
                    DISPLAY_NAME,
                    SERVICE_RIGHTS_MANAGE,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_DEMAND_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(wide.as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .map_err(|e| Error::win32(e, "CreateService"))?,
            };
            let _ = configure_failure_restart(service);
            if let Err(error) = apply_service_dacl(service) {
                let _ = CloseServiceHandle(service);
                return Err(error);
            }
            let started = match StartServiceW(service, None) {
                Ok(()) => Ok(()),
                // 已在运行：视为成功（幂等）。
                Err(e) if (e.code().0 as u32 & 0xFFFF) == ERROR_SERVICE_ALREADY_RUNNING.0 => Ok(()),
                Err(e) => Err(Error::win32(e, "StartService")),
            };
            let _ = CloseServiceHandle(service);
            started
        })();
        let _ = CloseServiceHandle(manager);
        result
    }
}

/// 把宿主程序复制到受保护目录。源与目标相同（直接从安装位置运行）时跳过；
/// 服务进程刚停止时文件可能仍短暂被占用，重试几次。
fn copy_helper(source: &Path, target: &Path) -> Result<()> {
    let same = std::fs::canonicalize(source).ok().zip(std::fs::canonicalize(target).ok()).is_some_and(|(a, b)| a == b);
    if same {
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Service(format!("创建 {} 失败：{e}", dir.display())))?;
    }
    let mut last = None;
    for _ in 0..20 {
        match std::fs::copy(source, target) {
            Ok(_) => return Ok(()),
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(Error::Service(format!(
        "复制服务程序到 {} 失败：{}",
        target.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

/// 请求停止并等待服务进入 STOPPED（最多约 10 秒）。
fn stop_and_wait(service: SC_HANDLE) {
    unsafe {
        let mut status = SERVICE_STATUS::default();
        if QueryServiceStatus(service, &mut status).is_ok() && status.dwCurrentState == SERVICE_STOPPED {
            return;
        }
        let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
        for _ in 0..40 {
            if QueryServiceStatus(service, &mut status).is_err() || status.dwCurrentState == SERVICE_STOPPED {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// 写入 [`SERVICE_SDDL`]：允许普通用户启动服务（不能改、停、删）。
fn apply_service_dacl(service: SC_HANDLE) -> Result<()> {
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &windows::core::HSTRING::from(SERVICE_SDDL),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|e| Error::win32(e, "ConvertStringSecurityDescriptor(服务)"))?;
        let result = SetServiceObjectSecurity(service, DACL_SECURITY_INFORMATION, descriptor)
            .map_err(|e| Error::win32(e, "SetServiceObjectSecurity"));
        let _ = windows::Win32::Foundation::LocalFree(HLOCAL(descriptor.0));
        result
    }
}

/// 以当前（普通）权限启动服务：windowisle 启动时调用。已在运行视为成功。
/// 旧版安装没有放开 SERVICE_START，会返回拒绝访问——需要「修复清理服务」。
pub fn start() -> Result<()> {
    unsafe {
        let manager = OpenSCManagerW(None, SERVICES_ACTIVE_DATABASE, SC_MANAGER_CONNECT)
            .map_err(|e| Error::win32(e, "OpenSCManager"))?;
        let opened = OpenServiceW(manager, w!("WindowIsleClean"), SERVICE_START | SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let service = opened.map_err(|e| Error::win32(e, "OpenService(启动)"))?;
        let started = match StartServiceW(service, None) {
            Ok(()) => Ok(()),
            Err(e) if (e.code().0 as u32 & 0xFFFF) == ERROR_SERVICE_ALREADY_RUNNING.0 => Ok(()),
            Err(e) => Err(Error::win32(e, "StartService")),
        };
        let _ = CloseServiceHandle(service);
        started
    }
}

/// 服务自行停止（空闲退出）：让 service_main 上报 STOPPED 并结束进程。
pub(crate) fn request_stop() {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 崩溃自动重启：失败动作 = 5 秒后重启，三次；计数每天归零。
fn configure_failure_restart(service: SC_HANDLE) -> Result<()> {
    unsafe {
        let actions = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 }; 3];
        let info = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_ptr().cast_mut(),
        };
        windows::Win32::System::Services::ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&info as *const SERVICE_FAILURE_ACTIONSW as *const std::ffi::c_void),
        )
        .map_err(|e| Error::win32(e, "设置失败重启动作"))
    }
}

/// 改名前（岛仔）的清理服务：服务名 DaozaiClean，程序在 `%ProgramFiles%\Daozai`。
/// 安装新服务时顺手停掉并删除，不留一个没人用的 SYSTEM 服务。只删旧的服务程序和
/// 日志，目录空了才移除。失败静默（没装过旧版时什么也不做）。
unsafe fn remove_legacy_service(manager: SC_HANDLE) {
    if let Ok(service) = OpenServiceW(manager, w!("DaozaiClean"), SERVICE_RIGHTS_UNINSTALL) {
        stop_and_wait(service);
        let _ = DeleteService(service);
        let _ = CloseServiceHandle(service);
    }
    let dir = program_files_dir().join("Daozai");
    let exe = dir.join("daozai-clean-service.exe");
    for _ in 0..20 {
        if !exe.exists() || std::fs::remove_file(&exe).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_dir_all(dir.join("logs"));
    let _ = std::fs::remove_dir(&dir);
}

/// 停止并删除服务，再删掉受保护目录里的服务程序（需要管理员）。
pub fn uninstall() -> Result<()> {
    unsafe {
        let manager = OpenSCManagerW(None, SERVICES_ACTIVE_DATABASE, SC_MANAGER_ALL_ACCESS)
            .map_err(|e| Error::win32(e, "OpenSCManager（需要管理员权限）"))?;
        let result = (|| -> Result<()> {
            let service = OpenServiceW(manager, w!("WindowIsleClean"), SERVICE_RIGHTS_UNINSTALL)
                .map_err(|e| match e.code().0 as u32 & 0xFFFF {
                    code if code == ERROR_SERVICE_DOES_NOT_EXIST.0 => Error::Service("服务未安装".into()),
                    _ => Error::win32(e, "OpenService"),
                })?;
            stop_and_wait(service);
            let deleted = DeleteService(service).is_ok();
            let _ = CloseServiceHandle(service);
            if !deleted {
                // 删除标记已下；正在停止的服务稍后被 SCM 摘除。
                return Err(Error::Service("服务将在停止后删除（如仍存在请稍后重试）".into()));
            }
            Ok(())
        })();
        let _ = CloseServiceHandle(manager);
        if result.is_ok() {
            remove_installed_files();
        }
        result
    }
}

/// 删除受保护目录里的服务程序与日志；目录为空才移除。失败静默。
fn remove_installed_files() {
    let exe = installed_exe_path();
    for _ in 0..20 {
        if !exe.exists() || std::fs::remove_file(&exe).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_dir_all(log_dir());
    if let Some(dir) = exe.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// 服务在 SCM 中的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// 未安装（或无权查询）。
    NotInstalled,
    /// 已安装但没在运行（已停止、启动中、停止中等）。
    Stopped,
    Running,
}

/// 查询服务状态（普通权限即可）。
pub fn query_state() -> ServiceState {
    unsafe {
        let Ok(manager) = OpenSCManagerW(None, SERVICES_ACTIVE_DATABASE, SC_MANAGER_CONNECT) else {
            return ServiceState::NotInstalled;
        };
        let opened = OpenServiceW(manager, w!("WindowIsleClean"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let Ok(service) = opened else { return ServiceState::NotInstalled };
        let mut status = SERVICE_STATUS::default();
        let queried = QueryServiceStatus(service, &mut status).is_ok();
        let _ = CloseServiceHandle(service);
        if queried && status.dwCurrentState == SERVICE_RUNNING {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        }
    }
}

/// 服务正在运行时返回其进程号（普通权限即可：SCM 默认给交互用户
/// SERVICE_QUERY_STATUS）。客户端用它核对管道对端身份——普通用户打不开
/// SYSTEM 服务进程（OpenProcess 报错误 5），没法靠读进程路径来校验。
pub fn query_pid() -> Option<u32> {
    unsafe {
        let manager = OpenSCManagerW(None, SERVICES_ACTIVE_DATABASE, SC_MANAGER_CONNECT).ok()?;
        let opened = OpenServiceW(manager, w!("WindowIsleClean"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let service = opened.ok()?;
        // 缓冲区必须按 SERVICE_STATUS_PROCESS（36 字节）给足，否则调用失败。
        let mut buffer = [0u8; std::mem::size_of::<SERVICE_STATUS_PROCESS>()];
        let mut needed = 0u32;
        let queried =
            QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(&mut buffer[..]), &mut needed).is_ok();
        let _ = CloseServiceHandle(service);
        if !queried {
            return None;
        }
        let status = std::ptr::read_unaligned(buffer.as_ptr() as *const SERVICE_STATUS_PROCESS);
        (status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0).then_some(status.dwProcessId)
    }
}

/// 查询服务是否处于运行状态（供 UI 提示）。
pub fn query_running() -> bool {
    query_state() == ServiceState::Running
}

// ---------------------------------------------------------------- 服务入口

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 服务逻辑闭包：dispatcher 的 service_main 没有上下文参数，经此传递。
static SERVE: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
/// 最近一次服务错误（service_main 无法返回 Result，经此带回）。
static SERVICE_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// SCM 服务主入口（`--service-run`）。正确流程：主线程先连
/// `StartServiceCtrlDispatcher`（SCM 要求服务进程最先做这件事，否则
/// 「服务没有及时响应启动」30 秒超时），再在 service_main 里注册控制
/// 处理器、上报状态并启动 `serve` 逻辑线程。
pub fn run<F>(serve: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    if let Ok(mut slot) = SERVE.lock() {
        *slot = Some(Box::new(serve));
    }
    unsafe extern "system" fn control(code: u32, _: u32, _: *mut std::ffi::c_void, _: *mut std::ffi::c_void) -> u32 {
        if code == SERVICE_CONTROL_STOP {
            STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        0
    }
    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows::core::PWSTR) {
        let result = (|| -> Result<()> {
            let handle = RegisterServiceCtrlHandlerExW(w!("WindowIsleClean"), Some(control), None)
                .map_err(|_| Error::Service("RegisterServiceCtrlHandler 失败（需由 SCM 启动）".into()))?;
            report(handle, SERVICE_START_PENDING)?;
            let serve = SERVE.lock().ok().and_then(|mut slot| slot.take());
            if let Some(serve) = serve {
                std::thread::spawn(serve);
            }
            report(handle, SERVICE_RUNNING)?;
            // 等待停止请求；服务逻辑线程随进程退出而被带走。
            while !STOP.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(300));
            }
            report(handle, SERVICE_STOP_PENDING)?;
            report(handle, SERVICE_STOPPED)?;
            Ok(())
        })();
        if let Err(error) = result {
            if let Ok(mut slot) = SERVICE_ERROR.lock() {
                *slot = Some(error.to_string());
            }
        }
    }
    unsafe {
        let mut name: Vec<u16> = "WindowIsleClean\0".encode_utf16().collect();
        let table = [
            windows::Win32::System::Services::SERVICE_TABLE_ENTRYW {
                lpServiceName: windows::core::PWSTR(name.as_mut_ptr()),
                lpServiceProc: Some(service_main),
            },
            windows::Win32::System::Services::SERVICE_TABLE_ENTRYW {
                lpServiceName: windows::core::PWSTR::null(),
                lpServiceProc: None,
            },
        ];
        // 阻塞直到服务停止；失败（如非 SCM 环境）带出错误。
        windows::Win32::System::Services::StartServiceCtrlDispatcherW(table.as_ptr())
            .map_err(|e| Error::win32(e, "StartServiceCtrlDispatcher（需由 SCM 启动）"))?;
        if let Ok(mut slot) = SERVICE_ERROR.lock() {
            if let Some(error) = slot.take() {
                return Err(Error::Service(error));
            }
        }
        Ok(())
    }
}

fn report(handle: SERVICE_STATUS_HANDLE, state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE) -> Result<()> {
    unsafe {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: SERVICE_ACCEPT_STOP,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 3000,
        };
        windows::Win32::System::Services::SetServiceStatus(handle, &status)
            .map_err(|e| Error::win32(e, "SetServiceStatus"))
    }
}
