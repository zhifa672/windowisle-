//! 调度器端到端探针：模拟 windowisle 的调用路径（默认配置启动 → 运行中
//! update_config），验证阈值触发与事件回传。快速节流参数用于测试。

use std::time::Duration;

use win_mem_cleaner::{start_auto_cleaner, CleanEvent, SchedulerConfig};

fn main() {
    // 与 windowisle Service::start 相同：默认配置（自动清理全关）启动。
    let mut handle = start_auto_cleaner(SchedulerConfig::default(), |event| match event {
        CleanEvent::Tick { memory } => {
            println!("[tick] 可用 {:.0}%（目标：低于 40% 触发）", memory.physical.free_percent);
        }
        CleanEvent::Cleaned { reason, report } => {
            println!("[CLEARED] reason={:?} 成功={} 耗时={:?}", reason, report.is_success(), report.total_duration);
        }
        CleanEvent::CleanFailed { reason, error } => {
            println!("[FAILED] reason={reason:?} error={error}");
        }
        CleanEvent::Stopped => println!("[stopped]"),
    });

    // 与 windowisle reconfigure 相同的参数形状，但用快速节流便于观察：
    // tick 5 秒、冷却 3 秒、阈值 40%（当前机器可用 18%，恒满足）。
    handle.update_config(SchedulerConfig {
        tick_interval: Duration::from_secs(5),
        interval_hours: 0,
        free_threshold_percent: 40,
        low_memory_cooldown: Duration::from_secs(3),
        ..SchedulerConfig::default()
    });
    println!("探针启动：tick=5s 冷却=3s 阈值=40%，跑 20 秒……");

    std::thread::sleep(Duration::from_secs(20));
    handle.stop();
    println!("探针结束");
}
