//! 清理服务宿主入口：让任意宿主程序（windowisle.exe 或独立的
//! win-mem-cleaner-helper.exe）兼任清理服务，宿主 `main` 开头调一次
//! [`service_cli`] 即可：
//!
//! - `--service-install`：（需管理员）把**当前程序**复制到
//!   [`crate::service_exe_path`] 并注册为 SYSTEM 服务、启动；
//! - `--service-uninstall`：（需管理员）停止并删除服务与复制的程序；
//! - `--service-run`：SCM 拉起的服务主循环（调度器 + 每会话命名管道）。
//!
//! 参数不匹配时返回 `None`，宿主照常启动自己的界面。

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::{
    optimize, query_memory, start_auto_cleaner, CleanEvent, CleanOptions, CleanReason, MemoryAreas,
    Message, Request, SchedulerConfig, PROTECTED_PROCESSES,
};

/// 没有任何客户端连着超过这么久，服务自行停止（按需启动：windowisle 启动时
/// 再拉起）。留 60 秒是为了 windowisle 重启时不必来回起停服务。
const IDLE_EXIT: std::time::Duration = std::time::Duration::from_secs(60);

/// 服务实际清理的区域：推荐组合（不含会让整机卡死的完整备用列表、
/// 已修改页与卷缓存刷新，见 [`MemoryAreas::RECOMMENDED`]）。
const SERVICE_AREAS: MemoryAreas = MemoryAreas::RECOMMENDED;

/// 用户排除列表 + 系统关键进程。非空即走逐进程清工作集，关键进程
/// （dwm、explorer 等）永远不被清空。
fn exclusions_with_protected(user: &[String]) -> Vec<String> {
    let mut all: Vec<String> = PROTECTED_PROCESSES.iter().map(|name| name.to_string()).collect();
    for name in user {
        if !all.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
            all.push(name.clone());
        }
    }
    all
}

fn scheduler_config(interval_hours: u32, threshold: u8, user_exclusions: &[String]) -> SchedulerConfig {
    SchedulerConfig {
        interval_hours,
        free_threshold_percent: threshold,
        process_exclusions: exclusions_with_protected(user_exclusions),
        areas: SERVICE_AREAS,
        ..SchedulerConfig::default()
    }
}

/// 处理服务相关命令行；返回 `Some(退出码)` 时宿主应直接退出。
pub fn service_cli() -> Option<i32> {
    let arg = std::env::args().nth(1)?;
    match arg.as_str() {
        "--service-install" => Some(install()),
        "--service-uninstall" => Some(match crate::uninstall_service() {
            Ok(()) => 0,
            Err(e) => {
                append_log("install.log", &format!("卸载失败：{e}"));
                1
            }
        }),
        "--service-run" => Some(match service_main() {
            Ok(()) => 0,
            Err(e) => {
                append_log("service.log", &format!("服务运行失败：{e}"));
                1
            }
        }),
        _ => None,
    }
}

fn install() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            append_log("install.log", &format!("安装失败：定位自身路径失败：{e}"));
            return 1;
        }
    };
    // 复制到 Program Files 并注册（见 service::installed_exe_path）。
    match crate::install_service(&exe) {
        Ok(()) => {
            append_log("install.log", &format!("安装成功：{}", crate::service_exe_path().display()));
            // 顺手清理旧的自启计划任务：架构反转前的过渡方案，以及改名前「岛仔」的
            // 最高权限自启任务（普通权限删不掉，这里正好有管理员权限）。任务名都是 Daozai。
            {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("schtasks")
                    .args(["/DELETE", "/F", "/TN", "Daozai"])
                    .creation_flags(0x0800_0000)
                    .status();
            }
            0
        }
        Err(e) => {
            // 以 runas 隐藏窗口运行，stderr 没人看得到：落盘给宿主/用户排查。
            append_log("install.log", &format!("安装失败：{e}"));
            1
        }
    }
}

/// 服务主循环：调度器 + 管道服务端（每会话一个监听实例，DACL 收紧）。
fn service_main() -> crate::Result<()> {
    let server_slot: Arc<OnceLock<Arc<crate::PipeServer>>> = Arc::new(OnceLock::new());

    // 调度器：自动清理结果广播给所有连接的客户端。
    let slot = server_slot.clone();
    let scheduler = Arc::new(start_auto_cleaner(scheduler_config(0, 0, &[]), move |event| {
        let Some(server) = slot.get() else { return };
        match event {
            CleanEvent::Cleaned { report, .. } => {
                server.broadcast(&Message::Cleaned { duration_ms: report.total_duration.as_millis() as u64 });
            }
            CleanEvent::CleanFailed { error, .. } => {
                server.broadcast(&Message::CleanFailed { error: error.to_string() });
            }
            CleanEvent::Tick { .. } | CleanEvent::Stopped => {}
        }
    }));

    // 管道服务端：configure / clean / status。
    let scheduler_for_handler = scheduler.clone();
    // 最近一次下发的用户排除列表（手动清理也要遵守）。
    let user_exclusions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // 同一时刻只做一次清理：连点或多个客户端同时请求时，后来的直接排队等前一次。
    let clean_lock = Arc::new(Mutex::new(()));
    // 客户端下发过配置（自动清理可能已开启）；全部断开后据此关掉自动清理。
    let armed = Arc::new(AtomicBool::new(false));
    let armed_for_handler = armed.clone();
    let server = crate::PipeServer::spawn(move |id, request| match request {
        Request::Configure { interval_hours, threshold, exclusions } => {
            armed_for_handler.store(true, Ordering::Relaxed);
            scheduler_for_handler.update_config(scheduler_config(interval_hours, threshold, &exclusions));
            if let Ok(mut slot) = user_exclusions.lock() {
                *slot = exclusions;
            }
            Message::Reply { id, ok: true, freed_bytes: None, duration_ms: None, error: None, status_running: None }
        }
        Request::Clean => {
            let _serial = clean_lock.lock();
            let options = CleanOptions {
                process_exclusions: exclusions_with_protected(
                    &user_exclusions.lock().map(|list| list.clone()).unwrap_or_default(),
                ),
            };
            let before = query_memory().ok().map(|m| m.physical.free_bytes);
            let started = Instant::now();
            let report = optimize(CleanReason::Manual, SERVICE_AREAS, &options);
            let freed = before
                .zip(query_memory().ok().map(|m| m.physical.free_bytes))
                .map(|(b, a)| a as i64 - b as i64);
            let ok = report.is_success();
            let error = if ok { None } else { Some("部分内存区域清理失败".to_string()) };
            Message::Reply {
                id,
                ok,
                freed_bytes: freed,
                duration_ms: Some(started.elapsed().as_millis() as u64),
                error,
                status_running: None,
            }
        }
        Request::Status => Message::Reply {
            id,
            ok: true,
            freed_bytes: None,
            duration_ms: None,
            error: None,
            status_running: Some(true),
        },
    });
    let _ = server_slot.set(server);

    // serve 闭包：服务标记为运行中后保持线程活着即可，真正的工作在
    // 调度器与管道线程里。每 5 秒：
    // - 自动清理随窗屿同生共死：所有客户端都断开（窗屿退出或崩溃）后，
    //   立即关掉定时/到限清理；持续空闲 IDLE_EXIT 后服务自行停止。
    // - 把服务端跟踪日志落到日志目录，排查服务端问题（如管道没建立）。
    crate::run_service(move || {
        // 从启动起算：被拉起后迟迟没有客户端连上，也按空闲退出。
        let mut idle_since = Some(Instant::now());
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let has_clients = server_slot.get().is_some_and(|server| server.client_count() > 0);
            // 用「下发过配置」而不是「上一轮有连接」判断：窗屿在 5 秒内连上又断开
            // 也不会漏掉。
            if !has_clients && armed.swap(false, Ordering::Relaxed) {
                scheduler.update_config(scheduler_config(0, 0, &[]));
                append_log("service.log", "窗屿已全部断开：停止自动清理");
            }
            for line in crate::pipe_trace() {
                append_log("service.log", &line);
            }
            if has_clients {
                idle_since = None;
            } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_EXIT {
                append_log("service.log", "空闲超过 60 秒：服务退出（窗屿启动时会再拉起）");
                crate::service::request_stop();
                return;
            }
        }
    })
}

/// 追加一行日志到 `%ProgramFiles%\WindowIsle\logs`。日志放在受保护目录：
/// 旧版写 `C:\ProgramData\DaozaiClean`，普通用户可预先在那里放目录链接，
/// 诱导 SYSTEM 往任意位置写文件。超过 1 MB 时清空重写。
fn append_log(name: &str, line: &str) {
    let dir = crate::service_log_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
