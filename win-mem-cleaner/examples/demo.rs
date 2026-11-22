//! 演示：内存查询 / 手动清理一次 / 自动提权清理 / 逐区域清理 / 自动调度。
//!
//! 用法：
//! - `cargo run --example demo`                        打印当前内存信息
//! - `cargo run --example demo -- clean`               手动清理（需管理员终端）
//! - `cargo run --example demo -- clean --elevate`     清理，未提权时弹 UAC 自动提权
//! - `cargo run --example demo -- areas --elevate`     逐区域清理并显示每步释放量
//! - `cargo run --example demo -- watch 1 15`          每 1 小时定时清理；可用 <15% 触发清理

use std::thread;
use std::time::Duration;

use win_mem_cleaner::{
    optimize, optimize_elevated, optimize_stepwise, query_memory, start_auto_cleaner, CleanEvent,
    CleanOptions, CleanReason, CleanReport, ElevateOptions, MemoryAreas, SchedulerConfig,
};

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

/// 打印逐区域清理明细（含每区域前后可用内存净变化）。
fn print_report_details(report: &CleanReport) {
    let mut total_freed: i64 = 0;
    for area in &report.areas {
        let freed = match area.freed_bytes() {
            Some(bytes) => {
                total_freed += bytes;
                format!("{:+.1} MB", bytes as f64 / 1024.0 / 1024.0)
            }
            None => "未采样".to_string(),
        };
        match &area.error {
            None => println!(
                "  [OK]   {:<28} {:>7.2}s  {}",
                area.area.name_cn(),
                area.duration.as_secs_f64(),
                freed
            ),
            Some(e) => println!("  [失败] {:<26} {e}", area.area.name_cn()),
        }
    }
    println!(
        "总耗时 {:?}，逐区域累计净变化 {:+.0} MB（含区域间自然波动）",
        report.total_duration,
        total_freed as f64 / 1024.0 / 1024.0
    );
}

/// 逐区域清理（观察每个区域的独立效果）。
fn clean_stepwise(elevate: bool) {
    let report = if elevate {
        println!("逐区域清理（未提权时将弹出 UAC 自动提权）……");
        let options = ElevateOptions { stepwise: true, ..ElevateOptions::default() };
        match optimize_elevated(CleanReason::Manual, MemoryAreas::ALL, &CleanOptions::default(), &options) {
            Ok(report) => report,
            Err(e) => {
                eprintln!("提权清理失败: {e}");
                return;
            }
        }
    } else {
        println!("逐区域清理（需管理员权限）……");
        optimize_stepwise(CleanReason::Manual, MemoryAreas::ALL, &CleanOptions::default())
    };
    print_report_details(&report);
}

fn print_memory() {
    match query_memory() {
        Ok(m) => {
            let p = m.physical;
            println!(
                "物理内存: 总 {} | 可用 {} ({:.0}% 可用) | 已用 {} ({:.0}%)",
                gb(p.total_bytes),
                gb(p.free_bytes),
                p.free_percent,
                gb(p.used_bytes),
                p.used_percent
            );
            let v = m.page_file;
            println!(
                "页面文件: 总 {} | 可用 {}",
                gb(v.total_bytes),
                gb(v.free_bytes)
            );
        }
        Err(e) => eprintln!("查询内存失败: {e}"),
    }
}

fn clean_once(elevate: bool) {
    let report = if elevate {
        println!("开始清理（未提权时将弹出 UAC 自动提权）……");
        match optimize_elevated(
            CleanReason::Manual,
            MemoryAreas::ALL,
            &CleanOptions::default(),
            &ElevateOptions::default(),
        ) {
            Ok(report) => report,
            Err(e) => {
                eprintln!("提权清理失败: {e}");
                return;
            }
        }
    } else {
        println!("开始清理（需管理员权限）……");
        optimize(CleanReason::Manual, MemoryAreas::ALL, &CleanOptions::default())
    };
    for area in &report.areas {
        match &area.error {
            None => println!("  [OK] {:<28} {:?}", area.area.name_cn(), area.duration),
            Some(e) => println!("  [失败] {:<26} {e}", area.area.name_cn()),
        }
    }
    println!("总耗时 {:?}", report.total_duration);
}

fn watch(interval_hours: u32, threshold: u8) {
    let config = SchedulerConfig {
        interval_hours,
        free_threshold_percent: threshold,
        ..SchedulerConfig::default()
    };
    println!(
        "调度器已启动：每 {interval_hours} 小时定时清理；可用内存 <{threshold}% 触发清理（Ctrl+C 退出）"
    );
    let mut handle = start_auto_cleaner(config, |event| match event {
        CleanEvent::Tick { memory } => {
            println!("[tick] 物理内存可用 {:.0}%", memory.physical.free_percent);
        }
        CleanEvent::Cleaned { reason, report } => {
            println!(
                "[cleaned] 原因={:?} 成功={} 耗时={:?}",
                reason,
                report.is_success(),
                report.total_duration
            );
        }
        CleanEvent::CleanFailed { reason, error } => {
            println!("[clean-failed] 原因={reason:?} 错误={error}");
        }
        CleanEvent::Stopped => println!("[stopped]"),
    });
    // 演示用：跑 10 分钟后自动退出。
    thread::sleep(Duration::from_secs(600));
    handle.stop();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let elevate = args.iter().any(|a| a == "--elevate");
    match args.get(1).map(String::as_str) {
        None => print_memory(),
        Some("clean") => clean_once(elevate),
        Some("areas") => clean_stepwise(elevate),
        Some("watch") => {
            let hours: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            let threshold: u8 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(15);
            watch(hours.max(1), threshold);
        }
        Some(other) => {
            eprintln!("未知参数: {other}");
            eprintln!("用法: demo [clean [--elevate] | areas [--elevate] | watch <小时> <阈值%>]");
        }
    }
}
