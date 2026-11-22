//! 自动清理调度器：定时清理 + 可用内存低于阈值自动清理
//! （对应 C# WinService.OnTimer 的逻辑）。
//!
//! - 每 `tick_interval`（默认 60 秒，与 C# 一致）检查一次；
//! - 定时清理：距上次计划清理超过 `interval_hours` 小时则触发（0 = 关闭）；
//! - 阈值清理：物理内存可用百分比低于 `free_threshold_percent` 且距上次
//!   低内存清理超过 `low_memory_cooldown`（默认 5 分钟，与 C# 常量一致）才触发；
//! - 两者同轮到点时定时清理优先（与 C# 的 return 语义一致）。

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cleaner::{self, CleanReason, CleanReport, CleanOptions, MemoryAreas};
use crate::memory::{self, MemoryInfo};

/// 调度器配置（可在运行中通过 [`AutoCleanHandle::update_config`] 更新）。
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// 检查周期，默认 60 秒。
    pub tick_interval: Duration,
    /// 定时清理间隔（小时）；0 表示关闭。
    pub interval_hours: u32,
    /// 可用物理内存百分比低于该值时触发清理；0 表示关闭。
    pub free_threshold_percent: u8,
    /// 两次阈值清理之间的最小间隔，默认 5 分钟。
    pub low_memory_cooldown: Duration,
    /// 清理哪些内存区域，默认 [`MemoryAreas::ALL`]。
    pub areas: MemoryAreas,
    /// 工作集清理的进程排除列表。
    pub process_exclusions: Vec<String>,
    /// 未提权时是否自动提权清理（runas 启动 helper，**每次清理会弹一次
    /// UAC 确认框**）。默认关闭：关闭时未提权的清理仅报
    /// `PrivilegeRequired` 错误。
    pub auto_elevate: bool,
    /// 提权 helper 可执行文件路径；`None` 时使用当前 exe 同目录下的
    /// `win-mem-cleaner-helper.exe`。
    pub helper_path: Option<std::path::PathBuf>,
    /// 等待提权 helper 完成的超时，默认 5 分钟。
    pub elevate_timeout: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(60),
            interval_hours: 0,
            free_threshold_percent: 0,
            low_memory_cooldown: Duration::from_secs(5 * 60),
            areas: MemoryAreas::ALL,
            process_exclusions: Vec::new(),
            auto_elevate: false,
            helper_path: None,
            elevate_timeout: Duration::from_secs(5 * 60),
        }
    }
}

impl SchedulerConfig {
    /// 定时清理间隔换算为 Duration。
    fn interval_duration(&self) -> Option<Duration> {
        if self.interval_hours == 0 {
            None
        } else {
            Some(Duration::from_secs(u64::from(self.interval_hours) * 3600))
        }
    }
}

/// 调度器对外事件。
#[derive(Debug, Clone)]
pub enum CleanEvent {
    /// 每次心跳检查（携带当前内存快照，宿主可用来刷新 UI）。
    Tick { memory: MemoryInfo },
    /// 完成了一次自动清理。
    Cleaned { reason: CleanReason, report: CleanReport },
    /// 清理整体失败（如自动提权被取消/超时）。
    CleanFailed { reason: CleanReason, error: crate::Error },
    /// 调度线程异常退出（例如内存查询持续失败不会退出，仅内部记录）。
    Stopped,
}

enum Command {
    Update(Box<SchedulerConfig>),
    Stop,
}

/// 运行中的调度器句柄。drop 不会停止线程，需显式调用 [`AutoCleanHandle::stop`]。
pub struct AutoCleanHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl AutoCleanHandle {
    /// 运行中更新配置（下一轮心跳生效，冷却/计时起点保持不变）。
    pub fn update_config(&self, config: SchedulerConfig) {
        let _ = self.tx.send(Command::Update(Box::new(config)));
    }

    /// 停止调度线程并等待退出。
    pub fn stop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// 线程是否已退出。
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
    }
}

/// 调度器内部计时状态。
#[derive(Debug, Clone, Copy)]
struct SchedulerState {
    last_by_interval: Instant,
    last_by_usage: Instant,
}

impl SchedulerState {
    fn now() -> Self {
        let now = Instant::now();
        Self { last_by_interval: now, last_by_usage: now }
    }

    /// 与 C# WinService.OnTimer 相同的判定：先定时、后阈值，命中即返回。
    fn decide(
        &self,
        config: &SchedulerConfig,
        free_percent: u8,
        now: Instant,
    ) -> Option<CleanReason> {
        // 定时清理
        if let Some(interval) = config.interval_duration() {
            if now.duration_since(self.last_by_interval) >= interval {
                return Some(CleanReason::Schedule);
            }
        }

        // 阈值清理
        if config.free_threshold_percent > 0
            && free_percent < config.free_threshold_percent
            && now.duration_since(self.last_by_usage) >= config.low_memory_cooldown
        {
            return Some(CleanReason::LowMemory);
        }

        None
    }

    fn mark(&mut self, reason: CleanReason, now: Instant) {
        match reason {
            CleanReason::Schedule => self.last_by_interval = now,
            CleanReason::LowMemory => self.last_by_usage = now,
            CleanReason::Manual => {}
        }
    }
}

/// 启动自动清理调度器（后台线程）。
///
/// `on_event` 在调度线程上回调（不要在其中做长时间阻塞的工作）。
pub fn start_auto_cleaner<F>(config: SchedulerConfig, on_event: F) -> AutoCleanHandle
where
    F: Fn(CleanEvent) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<Command>();
    let join = thread::Builder::new()
        .name("win-mem-cleaner".into())
        .spawn(move || {
            run_scheduler(config, rx, &on_event);
            on_event(CleanEvent::Stopped);
        })
        .expect("启动 win-mem-cleaner 调度线程失败");

    AutoCleanHandle { tx, join: Some(join) }
}

fn run_scheduler<F>(mut config: SchedulerConfig, rx: Receiver<Command>, on_event: &F)
where
    F: Fn(CleanEvent),
{
    let mut state = SchedulerState::now();

    loop {
        match rx.recv_timeout(config.tick_interval) {
            Ok(Command::Update(new_config)) => {
                config = *new_config;
                continue; // 配置更新不算心跳
            }
            Ok(Command::Stop) => break,
            Err(RecvTimeoutError::Timeout) => {} // 到点，执行心跳
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // 心跳：查询内存并广播（查询失败则本轮跳过判定）。
        let memory = match memory::query() {
            Ok(m) => m,
            Err(_) => continue,
        };
        on_event(CleanEvent::Tick { memory });

        if let Some(reason) = state.decide(&config, memory.physical.free_percent, Instant::now()) {
            let options = CleanOptions { process_exclusions: config.process_exclusions.clone() };

            // 未提权且开启自动提权时走 runas helper；其余情况进程内直接清理。
            let outcome = if config.auto_elevate && !crate::elevation::is_elevated() {
                let elevate = crate::elevation::ElevateOptions {
                    helper_path: config.helper_path.clone(),
                    timeout: config.elevate_timeout,
                    stepwise: false,
                };
                crate::elevation::optimize_elevated(reason, config.areas, &options, &elevate)
            } else {
                Ok(cleaner::optimize(reason, config.areas, &options))
            };

            state.mark(reason, Instant::now());
            match outcome {
                Ok(report) => on_event(CleanEvent::Cleaned { reason, report }),
                Err(error) => on_event(CleanEvent::CleanFailed { reason, error }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(interval_hours: u32, threshold: u8) -> SchedulerConfig {
        SchedulerConfig {
            interval_hours,
            free_threshold_percent: threshold,
            ..SchedulerConfig::default()
        }
    }

    #[test]
    fn schedule_fires_after_interval() {
        let cfg = config(1, 0);
        let st = SchedulerState::now();
        assert_eq!(st.decide(&cfg, 90, Instant::now()), None);
        let later = Instant::now() + Duration::from_secs(3600);
        assert_eq!(st.decide(&cfg, 90, later), Some(CleanReason::Schedule));
    }

    #[test]
    fn low_memory_fires_below_threshold_after_cooldown() {
        let cfg = config(0, 20);
        let st = SchedulerState::now();
        // 内存紧张但冷却期内不触发。
        assert_eq!(st.decide(&cfg, 10, Instant::now()), None);
        let later = Instant::now() + Duration::from_secs(6 * 60);
        assert_eq!(st.decide(&cfg, 10, later), Some(CleanReason::LowMemory));
        // 内存充足不触发。
        assert_eq!(st.decide(&cfg, 50, later), None);
    }

    #[test]
    fn schedule_takes_priority_over_low_memory() {
        let cfg = config(1, 20);
        let st = SchedulerState::now();
        let later = Instant::now() + Duration::from_secs(3600 + 3600);
        assert_eq!(st.decide(&cfg, 5, later), Some(CleanReason::Schedule));
    }

    #[test]
    fn mark_resets_only_matching_timer() {
        let cfg = config(1, 20);
        let start = Instant::now();
        let mut st = SchedulerState { last_by_interval: start, last_by_usage: start };

        let t1 = start + Duration::from_secs(3600);
        assert_eq!(st.decide(&cfg, 10, t1), Some(CleanReason::Schedule));
        st.mark(CleanReason::Schedule, t1);

        // 定时刚清理过，但内存仍紧张且低内存计时早已超冷却 → 触发 LowMemory。
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(st.decide(&cfg, 10, t2), Some(CleanReason::LowMemory));
        st.mark(CleanReason::LowMemory, t2);

        // 双双冷却中 → 不触发。
        let t3 = t2 + Duration::from_secs(60);
        assert_eq!(st.decide(&cfg, 10, t3), None);
    }

    #[test]
    fn disabled_features_never_fire() {
        let cfg = config(0, 0);
        let st = SchedulerState::now();
        let later = Instant::now() + Duration::from_secs(100 * 3600);
        assert_eq!(st.decide(&cfg, 0, later), None);
    }
}
