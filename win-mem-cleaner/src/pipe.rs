//! windowisle（普通权限 UI）与清理服务（SYSTEM）之间的命名管道协议与传输。
//!
//! 安全设计（与 windowisle 侧约定一致）：
//! - 管道名带会话 ID：[`PIPE_PREFIX`]`{sessionId}`，多用户会话互不串扰；
//! - DACL 用 SDDL 显式收紧：仅 SYSTEM 与该会话登录用户，显式拒绝网络
//!   登录（`NU`），并设 `PIPE_REJECT_REMOTE_CLIENTS` 挡远程；
//! - 服务端创建首个实例时带 `FILE_FLAG_FIRST_PIPE_INSTANCE`：低权限进程
//!   抢先注册同名管道冒充服务会直接失败；
//! - 客户端连接后用 `GetNamedPipeServerProcessId` + 进程映像路径校验
//!   对端确实是 helper，防止连到冒充者；
//! - 协议只暴露 configure / clean / status 高层命令，**不接受任何路径或
//!   任意命令**；服务端不做任何按路径的文件删除（内存清理器本身没有
//!   文件删除面）。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_BUSY,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, SetNamedPipeHandleState,
    PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTSActive,
    WTS_SESSION_INFOW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::error::{Error, Result};

/// Windows 服务名（安装/卸载/查询用）。
pub const SERVICE_NAME: &str = "WindowIsleClean";
/// 管道名前缀，完整名为 `\\.\pipe\windowisle-clean-{sessionId}`。
pub const PIPE_PREFIX: &str = r"\\.\pipe\windowisle-clean-";

/// 可跨线程传递的管道句柄：命名管道句柄本身线程安全，windows 0.58 的
/// `HANDLE` 含裸指针没有 `Send`，这里显式声明。
#[derive(Clone, Copy, PartialEq)]
struct Sh(HANDLE);

unsafe impl Send for Sh {}

// ---------------------------------------------------------------- 协议

/// 客户端 → 服务端的请求。只暴露高层命令。
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// 更新自动清理配置（服务端调度器热更新）。
    Configure { interval_hours: u32, threshold: u8, exclusions: Vec<String> },
    /// 立即手动清理一次（服务端进程内执行并采样释放量）。
    Clean,
    /// 查询服务状态。
    Status,
}

/// 服务端 → 客户端的消息：请求响应或主动事件。
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// 请求完成（对应某条请求，`id` 与请求一致）。
    Reply { id: u64, ok: bool, freed_bytes: Option<i64>, duration_ms: Option<u64>, error: Option<String>, status_running: Option<bool> },
    /// 服务端自动清理完成（广播）。
    Cleaned { duration_ms: u64 },
    /// 服务端自动清理失败（广播）。
    CleanFailed { error: String },
}

fn request_to_json(id: u64, request: &Request) -> Value {
    match request {
        Request::Configure { interval_hours, threshold, exclusions } => json!({
            "id": id, "cmd": "configure",
            "interval_hours": interval_hours, "threshold": threshold, "exclusions": exclusions,
        }),
        Request::Clean => json!({ "id": id, "cmd": "clean" }),
        Request::Status => json!({ "id": id, "cmd": "status" }),
    }
}

fn request_from_json(value: &Value) -> Option<Request> {
    match value.get("cmd")?.as_str()? {
        "configure" => Some(Request::Configure {
            interval_hours: value.get("interval_hours").and_then(Value::as_u64).unwrap_or(0).min(24) as u32,
            threshold: value.get("threshold").and_then(Value::as_u64).unwrap_or(0).min(50) as u8,
            exclusions: value
                .get("exclusions")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        }),
        "clean" => Some(Request::Clean),
        "status" => Some(Request::Status),
        _ => None,
    }
}

fn message_to_json(message: &Message) -> Value {
    match message {
        Message::Reply { id, ok, freed_bytes, duration_ms, error, status_running } => json!({
            "id": id, "ok": ok, "freed_bytes": freed_bytes, "duration_ms": duration_ms,
            "error": error, "running": status_running,
        }),
        Message::Cleaned { duration_ms } => json!({ "event": "cleaned", "duration_ms": duration_ms }),
        Message::CleanFailed { error } => json!({ "event": "clean_failed", "error": error }),
    }
}

fn message_from_json(value: &Value) -> Option<Message> {
    if let Some(event) = value.get("event").and_then(Value::as_str) {
        return match event {
            "cleaned" => Some(Message::Cleaned {
                duration_ms: value.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
            }),
            "clean_failed" => Some(Message::CleanFailed {
                error: value.get("error").and_then(Value::as_str).unwrap_or_default().to_string(),
            }),
            _ => None,
        };
    }
    let ok = value.get("ok").and_then(Value::as_bool)?;
    Some(Message::Reply {
        id: value.get("id").and_then(Value::as_u64).unwrap_or(0),
        ok,
        freed_bytes: value.get("freed_bytes").and_then(Value::as_i64),
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        error: value.get("error").and_then(Value::as_str).map(str::to_string),
        status_running: value.get("running").and_then(Value::as_bool),
    })
}

// ---------------------------------------------------------------- 会话与 SDDL

/// 当前进程所在的终端服务会话 ID。
pub fn current_session_id() -> u32 {
    unsafe {
        let mut session = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        session
    }
}

/// 某个会话对应的管道名。
pub fn session_pipe_name(session: u32) -> String {
    format!("{PIPE_PREFIX}{session}")
}

/// 列出当前活跃（可交互）的会话 ID。
fn active_sessions() -> Vec<u32> {
    unsafe {
        let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(HANDLE::default(), 0, 1, &mut sessions, &mut count).is_err() {
            return Vec::new();
        }
        let list = std::slice::from_raw_parts(sessions, count as usize);
        let active: Vec<u32> = list
            .iter()
            .filter(|info| info.State == WTSActive)
            .map(|info| info.SessionId)
            .collect();
        WTSFreeMemory(sessions.cast());
        active
    }
}

/// 查询某个会话登录用户的 SID 字符串（服务端以 SYSTEM 运行才能调用）。
fn session_user_sid(session: u32) -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        if WTSQueryUserToken(session, &mut token).is_err() {
            trace(&format!("WTSQueryUserToken({session}) 失败"));
            return None;
        }
        // 两段式：先查所需长度（TOKEN_USER 后面跟着 SID 本体），再取数。
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            trace(&format!("GetTokenInformation 长度查询失败 session={session}"));
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
            needed,
            &mut needed,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            trace(&format!("GetTokenInformation 取数失败 session={session}"));
            return None;
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid = windows::core::PWSTR::null();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid).is_err() {
            trace(&format!("ConvertSidToStringSid 失败 session={session}"));
            return None;
        }
        let text = sid.to_string().unwrap_or_default();
        let _ = windows::Win32::Foundation::LocalFree(HLOCAL(sid.as_ptr().cast()));
        Some(text)
    }
}

// ---------------------------------------------------------------- 诊断

/// 服务端关键路径的滚动跟踪（排查服务起不来的问题）。
static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn trace(line: &str) {
    if let Ok(mut log) = TRACE.lock() {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        log.push(format!("{seconds} {line}"));
        if log.len() > 200 {
            log.remove(0);
        }
    }
}

// 取走并清空服务端跟踪日志（诊断用）。
pub fn pipe_trace() -> Vec<String> {
    match TRACE.lock() {
        Ok(mut log) => std::mem::take(&mut *log),
        Err(poisoned) => std::mem::take(&mut poisoned.into_inner()),
    }
}

/// 仅 SYSTEM + 指定用户 + 显式拒绝网络登录的 DACL（SDDL）。
fn sddl_for(user_sid: &str) -> String {
    format!("D:(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;{user_sid})")
}

fn security_attributes(sddl: &str) -> Result<windows::Win32::Security::SECURITY_ATTRIBUTES> {
    unsafe {
        let mut descriptor = windows::Win32::Security::PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &windows::core::HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|e| Error::win32(e, "ConvertStringSecurityDescriptor"))?;
        Ok(windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        })
    }
}

// ---------------------------------------------------------------- 服务端

/// 管道服务端：为每个活跃会话维护监听线程，处理连接并广播事件。
pub struct Server {
    stop: Arc<AtomicBool>,
    /// 已连接客户端的写句柄（广播用；连接线程退出时移除）。
    clients: Arc<Mutex<Vec<Sh>>>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// 启动服务端。`handler` 在连接线程里同步处理请求（Clean 等长操作
    /// 直接在连接线程执行），返回要写回客户端的 [`Message::Reply`]。
    pub fn spawn<F>(handler: F) -> Arc<Self>
    where
        F: Fn(u64, Request) -> Message + Send + Sync + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let clients: Arc<Mutex<Vec<Sh>>> = Arc::new(Mutex::new(Vec::new()));
        let supervisor = {
            let stop = stop.clone();
            let clients = clients.clone();
            let handler = Arc::new(handler);
            std::thread::Builder::new()
                .name("windowisle-clean-pipe".into())
                .spawn(move || supervise(stop, clients, handler))
                .ok()
        };
        Arc::new(Self { stop, clients, supervisor })
    }

    /// 当前已连接的客户端数（连接线程在断开时注销，崩溃的客户端同样会被注销）。
    pub fn client_count(&self) -> usize {
        self.clients.lock().map(|list| list.len()).unwrap_or(0)
    }

    /// 向所有已连接客户端广播（自动清理事件）。
    pub fn broadcast(&self, message: &Message) {
        let text = message_to_json(message).to_string();
        let mut clients = self.clients.lock().map(|c| c.clone()).unwrap_or_default();
        clients.retain(|handle| write_all(handle.0, text.as_bytes()) != 0);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.supervisor.take() {
            let _ = join.join();
        }
    }
}

/// 会话监听管理：枚举活跃会话，为每个新会话启动 accept 线程。
fn supervise<F>(stop: Arc<AtomicBool>, clients: Arc<Mutex<Vec<Sh>>>, handler: Arc<F>)
where
    F: Fn(u64, Request) -> Message + Send + Sync + 'static,
{
    let mut watched: Vec<u32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let sessions = active_sessions();
        trace(&format!("wts sessions: {:?} (watched {:?})", sessions, watched));
        for session in sessions {
            if watched.contains(&session) {
                continue;
            }
            // 取到 SID 才算接管该会话：开机时服务先于用户登录完成启动，
            // WTSQueryUserToken 可能暂时失败，必须留到下一轮（30 秒后）重试，
            // 否则该会话永远没有管道，窗屿一直连不上。
            let Some(user_sid) = session_user_sid(session) else { continue };
            watched.push(session);
            trace(&format!("session {session} user sid {user_sid}"));
            let (stop_flag, client_list, handler_clone) = (stop.clone(), clients.clone(), handler.clone());
            let _ = std::thread::Builder::new()
                .name(format!("windowisle-clean-accept-{session}"))
                .spawn(move || accept_loop(session, user_sid, stop_flag, client_list, handler_clone));
        }
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// 单会话 accept 循环：创建实例 → 等待连接 → 交给处理线程。
fn accept_loop<F>(session: u32, user_sid: String, stop: Arc<AtomicBool>, clients: Arc<Mutex<Vec<Sh>>>, handler: Arc<F>)
where
    F: Fn(u64, Request) -> Message + Send + Sync + 'static,
{
    let name = session_pipe_name(session);
    let wide = windows::core::HSTRING::from(name.as_str());
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        let attributes = match security_attributes(&sddl_for(&user_sid)) {
            Ok(attributes) => attributes,
            Err(error) => {
                trace(&format!("session {session} SDDL 失败: {error}"));
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let open_mode = PIPE_ACCESS_DUPLEX
            | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0) };
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                open_mode,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        if pipe.is_invalid() {
            // 首个实例创建失败：可能被冒充或权限问题，稍后重试。
            let error = unsafe { windows::Win32::Foundation::GetLastError() };
            trace(&format!("session {session} CreateNamedPipe 失败 err={} first={first}", error.0));
            first = false;
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        first = false;
        trace(&format!("session {session} pipe created, waiting for client"));
        let connected = unsafe { ConnectNamedPipe(pipe, None) }.is_ok()
            || unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_PIPE_CONNECTED;
        if !connected {
            unsafe { let _ = CloseHandle(pipe); };
            continue;
        }
        // 连接建立：注册到广播列表，交给处理线程；本线程继续创建下一实例。
        let pipe = Sh(pipe);
        if let Ok(mut list) = clients.lock() {
            list.push(pipe);
        }
        let (client_list, handler_clone) = (clients.clone(), handler.clone());
        let _ = std::thread::Builder::new()
            .name(format!("windowisle-clean-conn-{session}"))
            .spawn(move || serve_client(pipe, client_list, handler_clone));
    }
}

/// 单连接处理：读请求 → 处理 → 写响应；断开时注销句柄。
fn serve_client<F>(pipe: Sh, clients: Arc<Mutex<Vec<Sh>>>, handler: Arc<F>)
where
    F: Fn(u64, Request) -> Message + Send + Sync + 'static,
{
    while let Some(text) = read_message(pipe.0) {
        let value: Option<Value> = serde_json::from_str(&text).ok();
        let id = value.as_ref().and_then(|v| v.get("id")).and_then(Value::as_u64).unwrap_or(0);
        let reply = match value.as_ref().and_then(request_from_json) {
            Some(request) => handler(id, request),
            None => Message::Reply {
                id,
                ok: false,
                freed_bytes: None,
                duration_ms: None,
                error: Some("无法识别的命令".into()),
                status_running: None,
            },
        };
        if write_all(pipe.0, message_to_json(&reply).to_string().as_bytes()) == 0 {
            break;
        }
    }
    if let Ok(mut list) = clients.lock() {
        list.retain(|handle| *handle != pipe);
    }
    unsafe { let _ = CloseHandle(pipe.0); };
}

// ---------------------------------------------------------------- 客户端

/// 已验证身份的管道客户端。
pub struct Client {
    handle: HANDLE,
    next_id: u64,
}

impl Client {
    /// 连接本会话的清理服务，并校验对端确实是 `expected_helper`。
    pub fn connect(expected_helper: &Path) -> Result<Self> {
        let name = session_pipe_name(current_session_id());
        let wide = windows::core::HSTRING::from(name.as_str());
        let handle = unsafe {
            let mut handle = HANDLE::default();
            // 管道忙（服务端正处理连接间隙）时稍等重试。
            for _ in 0..20 {
                handle = CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
                .unwrap_or(HANDLE::default());
                if !handle.is_invalid() {
                    break;
                }
                if windows::Win32::Foundation::GetLastError() != ERROR_PIPE_BUSY {
                    return Err(Error::Service("清理服务未运行".into()));
                }
                let _ = windows::Win32::System::Pipes::WaitNamedPipeW(PCWSTR(wide.as_ptr()), 500);
            }
            if handle.is_invalid() {
                return Err(Error::Service("清理服务未运行".into()));
            }
            let mode = PIPE_READMODE_MESSAGE;
            let _ = SetNamedPipeHandleState(handle, Some(&mode as *const _), None, None);
            handle
        };
        verify_server_identity(handle, expected_helper)?;
        Ok(Self { handle, next_id: 1 })
    }

    /// 发送请求，返回分配的请求 ID。
    pub fn send(&mut self, request: &Request) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let text = request_to_json(id, request).to_string();
        if write_all(self.handle, text.as_bytes()) == 0 {
            return Err(Error::Service("管道已断开".into()));
        }
        Ok(id)
    }

    /// 读取下一条服务端消息（响应或事件）；断开返回 `None`。
    pub fn recv(&self) -> Option<Message> {
        let text = read_message(self.handle)?;
        let value: Value = serde_json::from_str(&text).ok()?;
        message_from_json(&value)
    }

    /// 切到非阻塞模式（轮询读用；windowisle 的客户端线程以 ~1 秒周期调用
    /// [`try_recv`]，避免为读消息单开线程）。
    pub fn set_nowait(&self) {
        unsafe {
            let mode = windows::Win32::System::Pipes::PIPE_NOWAIT;
            let _ = SetNamedPipeHandleState(self.handle, Some(&mode as *const _), None, None);
        }
    }

    /// 非阻塞读一条：区分「暂时没有消息」与「连接已断」。
    pub fn try_recv(&self) -> TryRecv {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(self.handle, Some(buffer.as_mut_slice()), Some(&mut read as *mut u32), None).is_ok()
        };
        if ok && read > 0 {
            if let Some(message) = std::str::from_utf8(&buffer[..read as usize])
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|value| message_from_json(&value))
            {
                return TryRecv::Message(message);
            }
            return TryRecv::Empty;
        }
        let error = unsafe { windows::Win32::Foundation::GetLastError() };
        // ERROR_NO_DATA(232)：非阻塞模式下暂无消息；其它视为断开。
        if ok || error.0 == 232 {
            TryRecv::Empty
        } else {
            TryRecv::Closed
        }
    }

    /// 简单请求-响应（内部 send 后等 `id` 匹配的 Reply，透传事件）。
    pub fn roundtrip(&mut self, request: &Request, mut on_event: impl FnMut(Message)) -> Result<Message> {
        let id = self.send(request)?;
        loop {
            match self.recv() {
                Some(message @ Message::Reply { id: reply_id, .. }) if reply_id == id => {
                    return Ok(message)
                }
                Some(event) => on_event(event),
                None => return Err(Error::Service("管道已断开".into())),
            }
        }
    }
}

/// 非阻塞读结果。
pub enum TryRecv {
    Message(Message),
    /// 暂无消息（非阻塞下常见）。
    Empty,
    /// 连接已断开，需要重连。
    Closed,
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.handle); };
    }
}

/// 校验管道对端就是 SCM 里注册并正在运行的 WindowIsleClean 服务（防冒充）。
///
/// 主校验：管道服务端进程号 == SCM 报告的服务进程号。普通用户查得到这两个
/// 数，但改不了服务注册（程序路径只有管理员能改），冒充者的进程号对不上。
/// 旧做法是 OpenProcess 读服务进程映像路径——普通用户对 SYSTEM 进程没有
/// 这个权限（错误 5），窗屿永远连不上。
///
/// 附加校验：能打开进程时（窗屿恰好以管理员运行）顺带核对映像路径；
/// 打不开就以进程号校验为准。
fn verify_server_identity(pipe: HANDLE, expected_helper: &Path) -> Result<()> {
    unsafe {
        let mut pid = 0u32;
        GetNamedPipeServerProcessId(pipe, &mut pid)
            .map_err(|e| Error::win32(e, "GetNamedPipeServerProcessId"))?;
        match crate::service::query_pid() {
            Some(service_pid) if service_pid == pid => {}
            Some(service_pid) => {
                return Err(Error::Service(format!(
                    "管道对端不是清理服务（进程 {pid}，服务进程 {service_pid}）"
                )))
            }
            None => return Err(Error::Service("清理服务未在运行".into())),
        }
        let Ok(process) = windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        ) else {
            return Ok(());
        };
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = windows::Win32::System::Threading::QueryFullProcessImageNameW(
            process,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
        .is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return Ok(());
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        // 整条路径精确比较（不区分大小写）：子串匹配会放过
        // 「C:\evil\C:\Program Files\WindowIsle\...」之类的冒充路径。
        let normalize = |text: &str| text.trim_start_matches(r"\\?\").to_lowercase();
        if normalize(&path) != normalize(&expected_helper.to_string_lossy()) {
            return Err(Error::Service(format!("服务进程身份不符：{path}（需要修复安装）")));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 读写

/// MESSAGE 模式整条读取；断开返回 `None`。
fn read_message(pipe: HANDLE) -> Option<String> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(pipe, Some(buffer.as_mut_slice()), Some(&mut read as *mut u32), None).is_ok()
    };
    if !ok || read == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buffer[..read as usize]).into_owned())
}

fn write_all(pipe: HANDLE, bytes: &[u8]) -> usize {
    let mut written = 0u32;
    let ok = unsafe {
        WriteFile(pipe, Some(bytes), Some(&mut written as *mut u32), None).is_ok()
    };
    if ok { written as usize } else { 0 }
}

/// 判断服务是否已安装并在运行（供 UI 提示；查询失败按未运行处理）。
pub fn service_running() -> bool {
    crate::service::query_running()
}

/// 服务在 SCM 中的状态（区分未安装 / 已安装未运行 / 运行中）。
pub fn service_state() -> crate::service::ServiceState {
    crate::service::query_state()
}

// 服务模块引用（保持 pipe.rs 编译独立性的占位在 lib.rs 统一声明）。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_requests() {
        let requests = [
            Request::Configure { interval_hours: 6, threshold: 15, exclusions: vec!["windowisle".into()] },
            Request::Clean,
            Request::Status,
        ];
        for request in requests {
            let value = request_to_json(7, &request);
            assert_eq!(value.get("id").and_then(Value::as_u64), Some(7));
            assert_eq!(request_from_json(&value), Some(request_clone(&request)));
        }
    }

    fn request_clone(request: &Request) -> Request {
        request.clone()
    }

    #[test]
    fn json_roundtrip_messages() {
        let messages = [
            Message::Reply { id: 3, ok: true, freed_bytes: Some(1_234_567), duration_ms: Some(89), error: None, status_running: Some(true) },
            Message::Reply { id: 4, ok: false, freed_bytes: None, duration_ms: None, error: Some("x".into()), status_running: None },
            Message::Cleaned { duration_ms: 1200 },
            Message::CleanFailed { error: "e".into() },
        ];
        for message in messages {
            assert_eq!(message_from_json(&message_to_json(&message)), Some(message));
        }
    }

    #[test]
    fn sddl_deny_network_and_allow_system_user() {
        let sddl = sddl_for("S-1-5-21-1-2-3-1000");
        assert!(sddl.contains("(D;;GA;;;NU)"), "必须显式拒绝网络登录：{sddl}");
        assert!(sddl.contains("(A;;GA;;;SY)"), "必须允许 SYSTEM：{sddl}");
        assert!(sddl.contains("S-1-5-21-1-2-3-1000"));
        // 默认 DACL 的 Everyone 授权不应出现。
        assert!(!sddl.contains("WD") && !sddl.contains("OW"));
    }

    #[test]
    fn pipe_name_contains_session() {
        assert_eq!(session_pipe_name(3), format!("{PIPE_PREFIX}3"));
    }
}
