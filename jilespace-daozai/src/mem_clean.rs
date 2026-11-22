//! 内存清理：通过命名管道与 SYSTEM 清理服务（windowisle.exe `--service-run`，
//! 服务逻辑见 ../win-mem-cleaner）通信。
//!
//! 架构反转后的分工：windowisle 主进程保持普通权限（拖放、剪贴板等交互
//! 全部正常），清理动作由服务在 SYSTEM 侧执行；本模块只做：
//! - 后台客户端线程：连接/断线重连、下发配置、发起手动清理、收事件；
//! - 界面状态：服务是否连上、手动清理进行中、最近结果。
//!
//! 清理服务就是 windowisle.exe 自己（`--service-*` 参数，见 main.rs 开头与
//! `win_mem_cleaner::service_cli`），不需要额外的 helper 程序。首次使用时
//! runas 运行 `windowisle.exe --service-install`（一次 UAC），它把自己复制到
//! `%ProgramFiles%\WindowIsle\windowisle-clean-service.exe` 再注册为服务（窗屿本体
//! 所在的 `%LOCALAPPDATA%` 普通用户可写，不能直接当 SYSTEM 服务程序）。
//! 服务按需启动：开机不运行，窗屿启动时由本模块以普通权限拉起（安装时
//! 只给交互用户开放了 SERVICE_START），窗屿退出后服务空闲 60 秒自行停止。
//! 于是清理服务是否随开机运行，自动跟随窗屿自己的开机自启，全程无弹窗。
//! 非 Windows（开发/录制）为空实现。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// 清理服务的对外事件（外壳在帧循环里 `try_recv` 排空）。
#[derive(Clone, Debug)]
pub enum Event {
    /// 一次清理完成（`manual` 区分手动/自动触发）。
    Finished {
        manual: bool,
        /// 清理前后可用物理内存的净变化（字节）。
        freed_bytes: Option<i64>,
        duration: Duration,
    },
    /// 一次清理失败（自动的静默记录，手动的会提示）。
    Failed { manual: bool, error: String },
    /// 与服务的连接状态变化（用于界面提示）。
    Connected(bool),
    /// 连不上服务的原因（变化时才发）：`installed` 区分「没装」与
    /// 「装了但连不上」（如旧版安装路径、服务未启动），界面据此显示
    /// 「安装」还是「修复」。
    Unavailable { installed: bool, reason: String },
    /// 当前窗屿与已安装的服务副本不一致（窗屿升级后），
    /// 界面提示「更新清理服务」。
    Outdated(bool),
}

/// 界面线程 → 客户端线程的命令。
enum Command {
    Configure { interval_hours: u32, threshold: u8, exclusions: Vec<String> },
    CleanNow,
    /// UAC 已同意、安装进程已启动（原始进程句柄）：客户端线程等它结束，
    /// 检查退出码后立即重连，不再等指数退避。
    #[cfg_attr(not(windows), allow(dead_code))]
    Installing(isize),
    Shutdown,
}

/// 界面侧状态。
#[derive(Default)]
pub struct State {
    /// 手动清理进行中（按钮/圆环显示动画）。
    pub busy: bool,
    /// 最近一次清理的释放量、耗时与完成时刻。
    pub last: Option<(i64, Duration, Instant)>,
    /// 最近一次失败原因。
    pub last_error: Option<String>,
    /// 清理服务是否已连上。
    pub connected: bool,
    /// 服务已在 SCM 注册（未连上时区分「安装」与「修复」）。
    pub installed: bool,
    /// 未连上的原因（界面直接显示）。
    pub unavailable: Option<String>,
    /// 已安装的服务程序比窗屿自带的旧，需要更新。
    pub outdated: bool,
    /// 安装进程运行中（UAC 已同意，等服务起来）。
    pub installing: bool,
    /// 懒安装中：服务连上后自动补发一次清理（取走即清）。
    pub pending_clean: bool,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置页/卡片上的安装按钮文字：没装 → 安装；装了连不上或过期 → 修复/更新。
    pub fn install_label(&self) -> &'static str {
        if self.outdated && self.connected {
            "更新清理服务"
        } else if self.installed {
            "修复清理服务"
        } else {
            "安装清理服务"
        }
    }
}

/// 以 runas（UAC）运行本程序的 `--service-install`：安装（或修复/更新）
/// 并启动清理服务。阻塞到用户在 UAC 上做出选择；同意后返回安装进程句柄，
/// 安装本身在后台完成。
#[cfg(windows)]
fn launch_install() -> Result<isize, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let exe = std::env::current_exe().map_err(|e| format!("定位窗屿程序失败：{e}"))?;
    let mut wide: Vec<u16> = exe.as_os_str().encode_wide().collect();
    wide.push(0);
    let parameters: Vec<u16> = "--service-install\0".encode_utf16().collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(wide.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: 0, // SW_HIDE：安装是静默的
        ..SHELLEXECUTEINFOW::default()
    };
    match unsafe { ShellExecuteExW(&mut info as *mut SHELLEXECUTEINFOW) } {
        Ok(()) if !info.hProcess.is_invalid() => Ok(info.hProcess.0 as isize),
        Ok(()) => Err("无法启动清理服务安装程序".into()),
        // ERROR_CANCELLED(1223)：用户在 UAC 上点了「否」。
        Err(e) if (e.code().0 as u32 & 0xFFFF) == 1223 => Err("已取消安装（UAC 被拒绝）".into()),
        Err(e) => Err(format!("无法启动清理服务安装程序：{e}")),
    }
}

/// 后台客户端服务（Shell 持有；界面只发命令、收事件）。
pub struct Service {
    commands: Option<Sender<Command>>,
    events: Receiver<Event>,
}

impl Service {
    pub fn start() -> Self {
        let (command_tx, command_rx) = channel::<Command>();
        let (event_tx, events) = channel::<Event>();
        #[cfg(windows)]
        std::thread::Builder::new()
            .name("windowisle-mem-clean".into())
            .spawn(move || client_loop(command_rx, event_tx))
            .ok();
        #[cfg(not(windows))]
        let _ = (command_rx, event_tx);
        Self { commands: Some(command_tx), events }
    }

    /// 设置变化时重新下发调度配置。
    pub fn reconfigure(&self, enabled: bool, interval_hours: u32, threshold: u8, exclusions: &[String]) {
        let Some(tx) = &self.commands else { return };
        let _ = tx.send(Command::Configure {
            // 服务未连上时也照发：客户端线程会在连上后带过去最新配置。
            interval_hours: if enabled { interval_hours.min(24) } else { 0 },
            threshold: if enabled { threshold.min(50) } else { 0 },
            exclusions: exclusions.to_vec(),
        });
    }

    /// 手动清理：发命令给客户端线程（由服务执行）。
    pub fn clean_now(&self) {
        let Some(tx) = &self.commands else { return };
        let _ = tx.send(Command::CleanNow);
    }

    /// 安装/修复/更新清理服务：弹一次 UAC（在调用线程上等用户选择），
    /// 同意后交给客户端线程等待安装结束并立即重连；结果经事件回传
    /// （失败为 `Failed { manual: true }`，成功为 `Connected(true)`）。
    pub fn install(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            let process = launch_install()?;
            if let Some(tx) = &self.commands {
                let _ = tx.send(Command::Installing(process));
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err("仅 Windows 支持内存清理".into())
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(Command::Shutdown);
        }
    }
}

/// 安装进程结束后，最多再等这么久让服务起来并建好管道。
#[cfg(windows)]
const INSTALL_CONNECT_GRACE: Duration = Duration::from_secs(15);

/// 拉起服务后快速重连（每秒一次）的时长；过期仍没连上再尝试启动。
#[cfg(windows)]
const START_GRACE: Duration = Duration::from_secs(10);

/// 客户端线程的跨连接状态。
#[cfg(windows)]
struct Session {
    configure: Option<(u32, u8, Vec<String>)>,
    /// 安装刚完成：截止前每秒重连一次，过期仍连不上则报告失败。
    install_deadline: Option<Instant>,
    /// 刚拉起服务：截止前每秒重连一次（不打扰用户，过期后再次尝试启动）。
    starting_until: Option<Instant>,
    /// 最近一次发出的 Unavailable（去重）。
    last_unavailable: Option<(bool, String)>,
}

/// 等安装进程结束并检查退出码；成功返回 true。
#[cfg(windows)]
fn wait_install(process: isize, events: &Sender<Event>) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    let handle = HANDLE(process as *mut std::ffi::c_void);
    let mut code = 1u32;
    unsafe {
        let _ = WaitForSingleObject(handle, 120_000);
        if GetExitCodeProcess(handle, &mut code).is_err() {
            code = 1;
        }
        let _ = CloseHandle(handle);
    }
    if code == 0 {
        return true;
    }
    let log = win_mem_cleaner::service_log_dir().join("install.log");
    let _ = events.send(Event::Failed {
        manual: true,
        error: format!("安装清理服务失败（详见 {}）", log.display()),
    });
    false
}

/// 当前窗屿与已安装的服务副本是否不同（窗屿升级后需要更新服务）。
/// 安装用 CopyFile 复制，大小与修改时间会原样保留，比较这两项即可，
/// 不必每次读完整个 exe。读不到任一文件时不提示。
#[cfg(windows)]
fn service_outdated() -> bool {
    let (Ok(exe), installed) = (std::env::current_exe(), win_mem_cleaner::service_exe_path()) else {
        return false;
    };
    if exe == installed {
        return false;
    }
    match (std::fs::metadata(&exe), std::fs::metadata(&installed)) {
        (Ok(current), Ok(service)) => {
            current.len() != service.len() || current.modified().ok() != service.modified().ok()
        }
        _ => false,
    }
}

/// 客户端线程：连接 → 配置/清理命令 → 事件回传；断线后指数退避重连。
#[cfg(windows)]
fn client_loop(commands: Receiver<Command>, events: Sender<Event>) {
    use std::sync::mpsc::RecvTimeoutError;
    use win_mem_cleaner::{Message, Request, ServiceState, TryRecv};

    // 只认 Program Files 里那份服务程序（防冒充；也是 SYSTEM 实际运行的那份）。
    let expected = win_mem_cleaner::service_exe_path();
    let mut session = Session { configure: None, install_deadline: None, starting_until: None, last_unavailable: None };
    let mut backoff = Duration::from_secs(2);

    loop {
        let mut client = match connect_client(&expected) {
            Ok(client) => client,
            Err(reason) => {
                let state = win_mem_cleaner::service_state();
                let installed = state != ServiceState::NotInstalled;
                let mut reason = if installed { reason } else { "清理服务未安装".to_string() };
                // 按需启动：服务已装但没运行（开机后首次、或空闲退出后）→ 直接拉起，
                // 随后每秒重连。旧版安装没放开启动权限会被拒绝，提示修复。
                let now = Instant::now();
                if state == ServiceState::Stopped && session.starting_until.map_or(true, |until| now >= until) {
                    match win_mem_cleaner::start_service() {
                        Ok(()) => session.starting_until = Some(now + START_GRACE),
                        Err(error) => {
                            session.starting_until = None;
                            reason = format!("无法启动清理服务：{error}（请点「修复清理服务」）");
                        }
                    }
                }
                if session.install_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    session.install_deadline = None;
                    let _ = events.send(Event::Failed {
                        manual: true,
                        error: format!("清理服务已安装，但连接失败：{reason}"),
                    });
                }
                let unavailable = (installed, reason.clone());
                // 正在拉起服务时不报「连不上」，免得启动瞬间界面闪出「修复」。
                let starting = session.starting_until.is_some_and(|until| Instant::now() < until);
                if !starting && session.last_unavailable.as_ref() != Some(&unavailable) {
                    let _ = events.send(Event::Unavailable { installed, reason: reason.clone() });
                    session.last_unavailable = Some(unavailable);
                }
                let fast = session.install_deadline.is_some()
                    || session.starting_until.is_some_and(|until| Instant::now() < until);
                let wait = if fast { Duration::from_secs(1) } else { backoff };
                match commands.recv_timeout(wait) {
                    Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(Command::Configure { interval_hours, threshold, exclusions }) => {
                        session.configure = Some((interval_hours, threshold, exclusions));
                    }
                    Ok(Command::CleanNow) => {
                        let _ = events.send(Event::Failed { manual: true, error: reason });
                    }
                    Ok(Command::Installing(process)) => {
                        if wait_install(process, &events) {
                            session.install_deadline = Some(Instant::now() + INSTALL_CONNECT_GRACE);
                            backoff = Duration::from_secs(2);
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if !fast {
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                continue;
            }
        };
        backoff = Duration::from_secs(2);
        session.install_deadline = None;
        session.starting_until = None;
        session.last_unavailable = None;
        let _ = events.send(Event::Connected(true));
        let _ = events.send(Event::Outdated(service_outdated()));
        let mut pending_clean: Option<u64> = None;
        // 新连接的服务端（可能刚重启）没有配置：签名清零，下面必然补发一次。
        let mut configure_signature = 0u64;
        let mut alive = true;
        while alive {
            // 服务端重启会丢配置：每次连上后补发一次最新配置。
            if configure_signature != signature(&session.configure) {
                if let Some((hours, threshold, exclusions)) = &session.configure {
                    if client
                        .send(&Request::Configure { interval_hours: *hours, threshold: *threshold, exclusions: exclusions.clone() })
                        .is_err()
                    {
                        break;
                    }
                    configure_signature = signature(&session.configure);
                }
            }
            match commands.recv_timeout(Duration::from_millis(1000)) {
                Ok(Command::Configure { interval_hours, threshold, exclusions }) => {
                    // 下一轮循环开头按签名差异下发。
                    session.configure = Some((interval_hours, threshold, exclusions));
                }
                Ok(Command::CleanNow) => match client.send(&Request::Clean) {
                    Ok(id) => pending_clean = Some(id),
                    Err(_) => {
                        let _ = events.send(Event::Failed { manual: true, error: "管道已断开".into() });
                        alive = false;
                    }
                },
                Ok(Command::Installing(process)) => {
                    // 更新服务会先停掉当前服务：等安装结束，管道随后断开并重连。
                    wait_install(process, &events);
                    session.install_deadline = Some(Instant::now() + INSTALL_CONNECT_GRACE);
                }
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = events.send(Event::Connected(false));
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            for _ in 0..8 {
                match client.try_recv() {
                    TryRecv::Message(Message::Reply { id, ok, freed_bytes, duration_ms, error, .. }) => {
                        if pending_clean == Some(id) {
                            pending_clean = None;
                            if ok {
                                let _ = events.send(Event::Finished {
                                    manual: true,
                                    freed_bytes,
                                    duration: Duration::from_millis(duration_ms.unwrap_or(0)),
                                });
                            } else {
                                let _ = events.send(Event::Failed {
                                    manual: true,
                                    error: error.unwrap_or_else(|| "清理失败".into()),
                                });
                            }
                        }
                    }
                    TryRecv::Message(Message::Cleaned { duration_ms }) => {
                        let _ = events.send(Event::Finished {
                            manual: false,
                            freed_bytes: None,
                            duration: Duration::from_millis(duration_ms),
                        });
                    }
                    TryRecv::Message(Message::CleanFailed { error }) => {
                        let _ = events.send(Event::Failed { manual: false, error });
                    }
                    TryRecv::Empty => break,
                    TryRecv::Closed => {
                        alive = false;
                        break;
                    }
                }
            }
        }
        // 断线时还有手动清理在等结果：如实告知，别让界面一直「清理中」。
        if pending_clean.is_some() {
            let _ = events.send(Event::Failed { manual: true, error: "清理服务连接中断".into() });
        }
        let _ = events.send(Event::Connected(false));
    }
}

/// FNV-1a 配置签名：判断重连后是否需要补发配置。
#[cfg(windows)]
fn signature(configure: &Option<(u32, u8, Vec<String>)>) -> u64 {
    let Some((hours, threshold, exclusions)) = configure else { return 0 };
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in hours.to_le_bytes().iter().chain(threshold.to_le_bytes().iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    for text in exclusions {
        for byte in text.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

#[cfg(windows)]
fn connect_client(expected: &std::path::Path) -> Result<win_mem_cleaner::Client, String> {
    let client = win_mem_cleaner::Client::connect(expected).map_err(|e| e.to_string())?;
    client.set_nowait();
    Ok(client)
}
