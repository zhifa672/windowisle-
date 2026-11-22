//! Agent 任务进度（DeepSeek Harness）：只读 DSH 的本地会话缓存，把
//! Agent 用 `todo_write` 维护的任务清单变成岛上的实时进度与完成提醒。
//!
//! 数据源：`%USERPROFILE%\.dsh\storages\session_projcache\sessions\session-*.json`
//! 的 `record.rows.todos.val`（[{content, status}]，status ∈ pending /
//! in_progress / completed——字段结构以 tests/fixtures/dsh 的真实样本为准）。
//! 后台线程每 2 秒检查文件修改时间，变了才读；DSH 写到一半的残缺 JSON
//! 保留上一次结果。对 DSH 零侵入。
//!
//! 设计文档见 docs（dsh-task-progress 插件的系统级版本）；进度公式与
//! 插件一致：(完成 + 0.5 × 进行中) / 总数。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------- 类型

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            // 未知状态跳过这一条（而不是当 Pending），DSH 新增状态时
            // 进度不会被算低。
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub content: String,
    pub status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// 会话 id（文件名去掉 .json）。
    pub id: String,
    /// 会话标题；为空时界面显示「未命名会话」。
    pub title: String,
    pub tasks: Vec<Task>,
    /// 文件最后修改时间（Unix 秒），判断「最近活跃」和排序。
    pub updated: i64,
    /// token 统计：模型 + 推理等级 + 缓存命中率 + 输出速度（tok/s）。
    pub stats: TokenStats,
}

/// 投影文件里的 tokenUsage.totals + sessionStats + modelSelection 摘要。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokenStats {
    /// 模型名（如 "deepseek-flash"）。
    pub model: String,
    /// 推理等级（如 "max"、"high"、"none"）。
    pub reasoning_effort: String,
    /// 缓存命中 token 数（读取的）。
    pub cache_read: u64,
    /// 缓存写入 token 数。
    pub cache_write: u64,
    /// 未命中缓存输入的 token 数。
    pub uncached_input: u64,
    /// 输出 token 总数。
    pub output: u64,
    /// 解码耗时（毫秒）。
    pub decode_ms: u64,
    /// 解码 token 数（与 output 可能一致，用于算速度）。
    pub decode_tokens: u64,
}

impl TokenStats {
    /// 缓存命中率：读缓存量 / 总输入量（含写入和未命中）。
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let total = self.cache_read + self.cache_write + self.uncached_input;
        if total == 0 {
            None
        } else {
            Some(self.cache_read as f64 / total as f64)
        }
    }

    /// 输出速度（tok/s）：解码 token / 解码耗时。
    pub fn output_rate(&self) -> Option<f64> {
        if self.decode_ms == 0 || self.decode_tokens == 0 {
            None
        } else {
            Some(self.decode_tokens as f64 / (self.decode_ms as f64 / 1000.0))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub active: usize,
    pub total: usize,
}

impl Progress {
    /// 与插件一致：(完成 + 0.5 × 进行中) / 总数。
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 + 0.5 * self.active as f32) / self.total as f32
        }
    }

    pub fn all_done(&self) -> bool {
        self.total > 0 && self.done == self.total
    }
}

/// 任务属于哪个 Agent：决定圆环中间、常驻排序预览里画哪个图标。
/// 目前数据源只有 DeepSeek Harness，会话都属于 DeepSeek；以后接入别的 Agent 时
/// 在这里加一项，并在 `Session::agent` 里按数据来源区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    DeepSeek,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::DeepSeek => "DeepSeek",
        }
    }
}

impl Session {
    /// 这个会话属于哪个 Agent（见 `Agent`）。
    pub fn agent(&self) -> Agent {
        Agent::DeepSeek
    }

    pub fn progress(&self) -> Progress {
        let mut progress = Progress { done: 0, active: 0, total: self.tasks.len() };
        for task in &self.tasks {
            match task.status {
                TaskStatus::Completed => progress.done += 1,
                TaskStatus::InProgress => progress.active += 1,
                TaskStatus::Pending => {}
            }
        }
        progress
    }

    /// 第一条进行中的任务（收起态悬停提示、清单自动对准用）。
    pub fn current(&self) -> Option<&Task> {
        self.tasks.iter().find(|t| t.status == TaskStatus::InProgress)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// 设置里关掉了。
    Disabled,
    /// 目录不存在（多半是没装 DSH）。
    NotFound(PathBuf),
    /// 目录里有文件，但一个都认不出来（DSH 改了格式）。
    Unrecognized(String),
    /// 正常；`sessions` 是认出来的会话文件数（含没有任务清单的）。
    Ready { sessions: usize },
}

pub enum Event {
    Sessions(Vec<Session>),
    Status(Status),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    /// 自定义数据目录；None = 自动检测。
    pub dir: Option<PathBuf>,
}

// ---------------------------------------------------------------- 目录检测

pub fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        PathBuf::from(home)
            .join(".dsh")
            .join("storages")
            .join("session_projcache")
            .join("sessions"),
    )
}

/// 当前 Unix 时间（秒）。会话的 `updated` 是文件修改时间，算「多久没更新」
/// 只能和它比；`App::t` 是窗屿启动以来的秒数，两者不能直接相减。
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn resolve_dir(config: &Config) -> Option<PathBuf> {
    config
        .dir
        .clone()
        .filter(|d| !d.as_os_str().is_empty())
        .or_else(default_dir)
}

// ---------------------------------------------------------------- 解析（纯函数，跨平台可测）

/// 解析一份会话投影文件。`Ok(None)` = 认得出但没有任务清单；
/// `Err` = 认不出（含读到写了一半的残缺 JSON——服务层保留旧结果）。
pub fn parse_session(raw: &[u8], file_id: &str, updated: i64) -> Result<Option<Session>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| format!("JSON 解析失败：{e}"))?;

    // 主路径：record.rows.todos.val / record.rows.title.val。
    let rows = value
        .pointer("/record/rows")
        .ok_or_else(|| "缺少 record.rows".to_string())?;
    let title = rows
        .pointer("/title/val")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let todos = rows.pointer("/todos/val");
    let items = match todos.and_then(|v| v.as_array()) {
        Some(items) => items,
        // 兜底：主路径没有 todos 时，在有限深度内找名为 todos 的键，
        // 值是数组、或带 val 数组的对象，且元素有 content 和 status。
        // DSH 小幅调整结构时不至于整个功能失效。
        None => match find_todos(&value, 0) {
            Some(items) => items,
            None => return Ok(None),
        },
    };

    let mut tasks = Vec::new();
    for item in items {
        let Some(content) = item.get("content").and_then(|v| v.as_str()) else { continue };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let Some(status) = item.get("status").and_then(|v| v.as_str()).and_then(TaskStatus::parse)
        else { continue };
        tasks.push(Task { content: content.to_string(), status });
    }
    if tasks.is_empty() {
        return Ok(None);
    }

    // token / 模型统计（字段缺失时对应值为零/空，界面不显示）。
    let mut stats = TokenStats::default();
    if let Some(usage) = rows.pointer("/tokenUsage/val/totals") {
        stats.cache_read = usage.get("cacheReadTokens").and_then(|v| v.as_u64()).unwrap_or(0);
        stats.cache_write = usage.get("cacheWriteTokens").and_then(|v| v.as_u64()).unwrap_or(0);
        stats.uncached_input = usage.get("uncachedInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
        stats.output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    }
    if let Some(session_stats) = rows.pointer("/sessionStats/val") {
        stats.decode_ms = session_stats.get("decodeMs").and_then(|v| v.as_u64()).unwrap_or(0);
        stats.decode_tokens = session_stats.get("decodeTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    }
    if let Some(last_used) = rows.pointer("/modelSelection/val/lastUsed") {
        stats.model = last_used.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        stats.reasoning_effort = last_used
            .get("reasoningEffort")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    Ok(Some(Session { id: file_id.to_string(), title, tasks, updated, stats }))
}

/// 深度受限地找 todos 键（兜底路径）。
fn find_todos(value: &serde_json::Value, depth: usize) -> Option<&Vec<serde_json::Value>> {
    if depth > 6 {
        return None;
    }
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get("todos") {
                if let Some(items) = found.as_array() {
                    if items.iter().any(looks_like_task) {
                        return Some(items);
                    }
                }
                if let Some(items) = found.pointer("/val").and_then(|v| v.as_array()) {
                    if items.iter().any(looks_like_task) {
                        return Some(items);
                    }
                }
            }
            for child in map.values() {
                if let Some(found) = find_todos(child, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(|child| find_todos(child, depth + 1)),
        _ => None,
    }
}

fn looks_like_task(value: &serde_json::Value) -> bool {
    value.get("content").is_some_and(|v| v.as_str().is_some())
        && value.get("status").is_some_and(|v| v.as_str().is_some())
}

// ---------------------------------------------------------------- 提醒判定（纯逻辑）

/// 一次要发的提醒。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// 单条完成。
    TaskDone { session: String, title: String, content: String },
    /// 5 秒内完成了多条。
    Batch { session: String, title: String, count: usize },
    /// 全部完成。
    /// `last` 是这次刚完成的那条（多条时取第一条），「提醒里显示任务内容」打开时附在提醒里。
    AllDone { session: String, title: String, total: usize, last: String },
}

/// 与上一次的会话快照比较，决定发哪些提醒。首次看到的会话只记录
/// 不提醒（窗屿启动 / 新读到会话都不补报）。
#[derive(Default)]
pub struct Watch {
    /// 每个会话上一次的 (content, status)，按出现顺序。
    last: HashMap<String, Vec<(String, TaskStatus)>>,
    /// 防重复：键 → 上次提醒的时刻（App::t，秒）。
    reminded: HashMap<String, f64>,
}

impl Watch {
    pub fn observe(&mut self, sessions: &[Session], now: f64) -> Vec<Notice> {
        let mut notices = Vec::new();
        let live: std::collections::HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        self.last.retain(|id, _| live.contains(id.as_str()));

        for session in sessions {
            let fresh: Vec<(String, TaskStatus)> =
                session.tasks.iter().map(|t| (t.content.clone(), t.status)).collect();
            let previous = match self.last.get(&session.id) {
                // 首次看到：只记录，不提醒。
                None => {
                    self.last.insert(session.id.clone(), fresh);
                    continue;
                }
                Some(previous) => previous,
            };

            // 刚完成的任务：同一 content，旧状态存在且不是 Completed、
            // 新状态是 Completed。重复内容按出现次数配对（修正插件以
            // content 为键互相覆盖的问题）。
            let mut just_done = 0usize;
            let mut done_content: Option<String> = None;
            for (index, (content, status)) in fresh.iter().enumerate() {
                if *status != TaskStatus::Completed {
                    continue;
                }
                let occurrences_before = previous
                    .iter()
                    .take(index + 1)
                    .filter(|(c, _)| c == content)
                    .count();
                let was = previous
                    .iter()
                    .filter(|(c, _)| c == content)
                    .nth(occurrences_before.saturating_sub(1))
                    .map(|(_, s)| *s);
                match was {
                    Some(old) if old != TaskStatus::Completed => {
                        just_done += 1;
                        if done_content.is_none() {
                            done_content = Some(content.clone());
                        }
                    }
                    _ => {}
                }
            }

            if just_done > 0 {
                let progress = session.progress();
                let notice = if progress.all_done() {
                    // 全部完成优先于单条；最后一条完成时只报全部完成。
                    self.gate(format!("{}|*", session.id), now).then(|| Notice::AllDone {
                        session: session.id.clone(),
                        title: display_title(&session.title),
                        total: progress.total,
                        last: done_content.clone().unwrap_or_default(),
                    })
                } else if just_done > 1 {
                    self.gate(format!("{}|batch", session.id), now).then(|| Notice::Batch {
                        session: session.id.clone(),
                        title: display_title(&session.title),
                        count: just_done,
                    })
                } else {
                    self.gate(format!("{}|{}", session.id, done_content.clone().unwrap_or_default()), now)
                        .then(|| Notice::TaskDone {
                            session: session.id.clone(),
                            title: display_title(&session.title),
                            content: done_content.unwrap_or_default(),
                        })
                };
                if let Some(notice) = notice {
                    notices.push(notice);
                }
            }

            self.last.insert(session.id.clone(), fresh);
        }

        // reminded 收缩：超过 64 条时清掉 20 秒前的。
        if self.reminded.len() > 64 {
            self.reminded.retain(|_, at| now - *at < 20.0);
        }
        notices
    }

    /// 同一个键 5 秒内只提醒一次；通过返回 false 表示被拦下。
    fn gate(&mut self, key: String, now: f64) -> bool {
        match self.reminded.get(&key) {
            Some(at) if now - *at < 5.0 => false,
            _ => {
                self.reminded.insert(key, now);
                true
            }
        }
    }
}

pub fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "未命名会话".to_string()
    } else {
        title.trim().to_string()
    }
}

// ---------------------------------------------------------------- 后台服务

enum Command {
    Sync(Config),
    Shutdown,
}

/// 只在有带任务的会话之间排序时用的窗口。
const FRESH_WINDOW: Duration = Duration::from_secs(24 * 3600);

pub struct Service {
    cmd: Option<Sender<Command>>,
    ev: Receiver<Event>,
}

impl Service {
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (ev_tx, ev_rx) = channel::<Event>();
        let _ = std::thread::Builder::new()
            .name("windowisle-dsh".into())
            .spawn(move || supervise(cmd_rx, ev_tx));
        Self { cmd: Some(cmd_tx), ev: ev_rx }
    }

    pub fn sync(&self, config: Config) {
        if let Some(tx) = &self.cmd {
            let _ = tx.send(Command::Sync(config));
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.ev.try_recv().ok()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Command::Shutdown);
        }
    }
}

struct Cached {
    /// 修改时间（纳秒），只用来判断文件变没变；秒级精度会漏掉同一秒内的两次写入。
    stamp: u128,
    len: u64,
    parsed: Option<Session>,
    /// 认得出的文件（有 record.rows）。
    recognized: bool,
}

fn supervise(cmd_rx: Receiver<Command>, ev: Sender<Event>) {
    let mut config = Config { enabled: true, dir: None };
    let mut cache: HashMap<PathBuf, Cached> = HashMap::new();
    let mut last_status: Option<Status> = None;
    let mut last_emitted: Option<Vec<Session>> = None;

    loop {
        // 命令优先；2 秒内分段睡，退出不用等满一轮。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match cmd_rx.recv_timeout(remaining) {
                Ok(Command::Sync(next)) => config = next,
                Ok(Command::Shutdown) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            }
        }

        // 关闭、找不到目录：清掉缓存，并让界面也清掉旧会话。
        let unavailable = if !config.enabled {
            Some(Status::Disabled)
        } else {
            match resolve_dir(&config) {
                None => Some(Status::NotFound(PathBuf::new())),
                Some(dir) if !dir.is_dir() => Some(Status::NotFound(dir)),
                Some(_) => None,
            }
        };
        if let Some(status) = unavailable {
            emit_status(&ev, &mut last_status, status);
            cache.clear();
            if last_emitted.as_ref().map_or(true, |list| !list.is_empty()) {
                last_emitted = Some(Vec::new());
                let _ = ev.send(Event::Sessions(Vec::new()));
            }
            continue;
        }
        let Some(dir) = resolve_dir(&config) else { continue };

        let mut recognized = 0usize;
        let mut live: Vec<PathBuf> = Vec::new();
        let entries = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > 8 * 1024 * 1024 {
                continue;
            }
            live.push(path.clone());
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .unwrap_or_default();
            let (mtime, stamp) = (modified.as_secs() as i64, modified.as_nanos());
            let changed = match cache.get(&path) {
                Some(cached) => cached.stamp != stamp || cached.len != meta.len(),
                None => true,
            };
            if !changed {
                if cache.get(&path).is_some_and(|c| c.recognized) {
                    recognized += 1;
                }
                continue;
            }
            match std::fs::read(&path) {
                Ok(raw) => {
                    let file_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();
                    match parse_session(&raw, &file_id, mtime) {
                        Ok(parsed) => {
                            recognized += 1;
                            cache.insert(path, Cached { stamp, len: meta.len(), parsed, recognized: true });
                        }
                        // JSON 完整但结构认不出（不是会话文件）：按当前修改时间缓存，
                        // 文件不变就不再重复读。
                        Err(_) if serde_json::from_slice::<serde::de::IgnoredAny>(&raw).is_ok() => {
                            cache.insert(path, Cached { stamp, len: meta.len(), parsed: None, recognized: false });
                        }
                        Err(_) => {
                            // 残缺 JSON（DSH 正在写）：保留上一次结果，并且**不更新**
                            // 修改时间和长度——写完后即使这两个值不再变，下一轮也会重读。
                            if let Some(cached) = cache.get(&path) {
                                if cached.recognized {
                                    recognized += 1;
                                }
                            } else {
                                cache.insert(path, Cached { stamp: 0, len: u64::MAX, parsed: None, recognized: false });
                            }
                        }
                    }
                }
                Err(_) => {
                    // 读不了（多半被 DSH 占用）：同样下一轮重试，已有结果保留。
                    if !cache.contains_key(&path) {
                        cache.insert(path, Cached { stamp: 0, len: u64::MAX, parsed: None, recognized: false });
                    }
                }
            }
        }
        cache.retain(|path, _| live.contains(path));

        let status = if recognized > 0 || live.is_empty() {
            Status::Ready { sessions: recognized }
        } else {
            Status::Unrecognized("所有会话文件都认不出".into())
        };
        emit_status(&ev, &mut last_status, status);

        // 只保留有任务、且 24 小时内更新过的会话，按更新时间从新到旧。
        let now = unix_now();
        let mut sessions: Vec<Session> = cache
            .values()
            .filter_map(|c| c.parsed.clone())
            .filter(|s| now - s.updated <= FRESH_WINDOW.as_secs() as i64)
            .collect();
        sessions.sort_by(|a, b| b.updated.cmp(&a.updated));

        if last_emitted.as_ref() != Some(&sessions) {
            last_emitted = Some(sessions.clone());
            let _ = ev.send(Event::Sessions(sessions));
        }
    }
}

fn emit_status(ev: &Sender<Event>, last: &mut Option<Status>, status: Status) {
    if last.as_ref() != Some(&status) {
        *last = Some(status.clone());
        let _ = ev.send(Event::Status(status));
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh")
            .join(name);
        std::fs::read(path).unwrap_or_else(|e| panic!("读取样本 {name} 失败：{e}"))
    }

    fn session(tasks: &[(&str, TaskStatus)], title: &str) -> Session {
        Session {
            id: "s1".into(),
            title: title.into(),
            tasks: tasks
                .iter()
                .map(|(c, s)| Task { content: c.to_string(), status: *s })
                .collect(),
            updated: 1000,
            stats: TokenStats::default(),
        }
    }

    #[test]
    fn parse_real_samples() {
        let parsed = parse_session(&fixture("session-sample.json"), "session-sample", 1).unwrap().unwrap();
        assert_eq!(parsed.title, "窗屿计时优化");
        assert_eq!(parsed.tasks.len(), 5);
        assert_eq!(parsed.progress(), Progress { done: 3, active: 1, total: 5 });
        assert!((parsed.progress().fraction() - 0.7).abs() < 1e-6);
        assert_eq!(parsed.current().map(|t| t.content.as_str()), Some("设计交互布局"));

        let done = parse_session(&fixture("session-all-done.json"), "session-all-done", 2).unwrap().unwrap();
        assert_eq!(done.title, "邮箱修复");
        assert!(done.progress().all_done());
        assert_eq!(done.progress().total, 3);
    }

    #[test]
    fn token_stats_parse_from_real_shape() {
        let parsed = parse_session(&fixture("session-sample.json"), "s", 1).unwrap().unwrap();
        let stats = &parsed.stats;
        assert_eq!(stats.model, "deepseek-flash");
        assert_eq!(stats.reasoning_effort, "max");
        assert_eq!(stats.cache_read, 28088320);
        assert_eq!(stats.output, 128056);
        assert_eq!(stats.decode_ms, 580204);
        // 命中率 28088320 / (28088320 + 0 + 137346) ≈ 99.5%
        let rate = stats.cache_hit_rate().unwrap();
        assert!((rate - 0.9951).abs() < 0.001, "命中率 {rate}");
        // 速度 128056 / 580.204s ≈ 220.7 tok/s
        let speed = stats.output_rate().unwrap();
        assert!((speed - 220.7).abs() < 1.0, "速度 {speed}");
        // 空数据 → None。
        let empty = TokenStats::default();
        assert!(empty.cache_hit_rate().is_none());
        assert!(empty.output_rate().is_none());
    }

    #[test]
    fn parse_tolerates_missing_and_broken() {
        // 没有 todos → 认得出但没有清单。
        let raw = br#"{"record":{"rows":{"title":{"val":"t"}}}}"#;
        assert_eq!(parse_session(raw, "x", 0), Ok(None));
        // 残缺 JSON → Err（服务层保留旧结果）。
        assert!(parse_session(b"{\"record\":", "x", 0).is_err());
        // 未知 status 跳过、空 content 跳过。
        let raw = br#"{"record":{"rows":{"todos":{"val":[
            {"content":"a","status":"weird"},
            {"content":"","status":"pending"},
            {"content":"b","status":"completed"}]}}}}"#;
        let parsed = parse_session(raw, "x", 0).unwrap().unwrap();
        assert_eq!(parsed.tasks.len(), 1);
        assert_eq!(parsed.tasks[0].content, "b");
        // 重复 content 两条都保留。
        let raw = br#"{"record":{"rows":{"todos":{"val":[
            {"content":"dup","status":"pending"},
            {"content":"dup","status":"completed"}]}}}}"#;
        let parsed = parse_session(raw, "x", 0).unwrap().unwrap();
        assert_eq!(parsed.tasks.len(), 2);
    }

    #[test]
    fn parse_fallback_finds_todos_at_other_depth() {
        let raw = br#"{"record":{"rows":{"wrapped":{"todos":[
            {"content":"a","status":"pending"},{"content":"b","status":"in_progress"}]}}}}"#;
        let parsed = parse_session(raw, "x", 0).unwrap().unwrap();
        assert_eq!(parsed.tasks.len(), 2);
        assert_eq!(parsed.progress().active, 1);
    }

    #[test]
    fn watch_first_observation_records_without_notifying() {
        let mut watch = Watch::default();
        let all = session(
            &[("a", TaskStatus::Completed), ("b", TaskStatus::Completed)],
            "t",
        );
        assert!(watch.observe(&[all], 100.0).is_empty());
    }

    #[test]
    fn watch_reports_completion_but_not_start() {
        let mut watch = Watch::default();
        let pending = session(&[("a", TaskStatus::Pending)], "t");
        watch.observe(&[pending], 100.0);
        // pending → in_progress 不提醒。
        let active = session(&[("a", TaskStatus::InProgress)], "t");
        assert!(watch.observe(&[active], 101.0).is_empty());
        // in_progress → completed：单任务全部完成，报 AllDone（文档规则：
        // 全部完成优先于单条判断）。
        let done = session(&[("a", TaskStatus::Completed)], "t");
        let notices = watch.observe(&[done], 102.0);
        assert_eq!(notices, vec![Notice::AllDone { session: "s1".into(), title: "t".into(), total: 1, last: "a".into() }]);
    }

    #[test]
    fn watch_batch_and_all_done() {
        let mut watch = Watch::default();
        let base = session(&[("a", TaskStatus::Pending), ("b", TaskStatus::Pending)], "t");
        watch.observe(&[base], 100.0);
        // 一次完成一条（还有一条没完）→ TaskDone。
        let one = session(&[("a", TaskStatus::Completed), ("b", TaskStatus::Pending)], "t");
        let notices = watch.observe(&[one], 110.0);
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0], Notice::TaskDone { .. }));
        // 最后一条完成 → 只报 AllDone。
        let all = session(&[("a", TaskStatus::Completed), ("b", TaskStatus::Completed)], "t");
        let notices = watch.observe(&[all], 120.0);
        assert_eq!(notices, vec![Notice::AllDone { session: "s1".into(), title: "t".into(), total: 2, last: "b".into() }]);

        // 多条同时完成（未全完）→ Batch。
        let mut watch = Watch::default();
        let base = session(
            &[("a", TaskStatus::Pending), ("b", TaskStatus::Pending), ("c", TaskStatus::Pending)],
            "t",
        );
        watch.observe(&[base], 200.0);
        let batch = session(
            &[("a", TaskStatus::Completed), ("b", TaskStatus::Completed), ("c", TaskStatus::Pending)],
            "t",
        );
        let notices = watch.observe(&[batch], 210.0);
        assert_eq!(notices, vec![Notice::Batch { session: "s1".into(), title: "t".into(), count: 2 }]);
    }

    #[test]
    fn watch_dedups_within_five_seconds() {
        let mut watch = Watch::default();
        watch.observe(&[session(&[("a", TaskStatus::Pending)], "t")], 100.0);
        let done = session(&[("a", TaskStatus::Completed)], "t");
        assert_eq!(watch.observe(&[done.clone()], 101.0).len(), 1);
        // 5 秒内的状态抖动（比如来回翻转后再次到达 completed）不重复报。
        let flip = session(&[("a", TaskStatus::InProgress)], "t");
        watch.observe(&[flip], 102.0);
        assert!(watch.observe(&[done], 103.0).is_empty());
    }

    #[test]
    fn watch_reports_again_after_tasks_extended() {
        // 全部完成后 Agent 又追加了任务、再全部完成（间隔超过 5 秒）→ 再报一次。
        let mut watch = Watch::default();
        let first = session(&[("a", TaskStatus::Completed)], "t");
        watch.observe(&[first], 100.0);
        let extended = session(
            &[("a", TaskStatus::Completed), ("b", TaskStatus::Pending)],
            "t",
        );
        watch.observe(&[extended], 110.0);
        let again = session(
            &[("a", TaskStatus::Completed), ("b", TaskStatus::Completed)],
            "t",
        );
        let notices = watch.observe(&[again], 120.0);
        assert_eq!(notices, vec![Notice::AllDone { session: "s1".into(), title: "t".into(), total: 2, last: "b".into() }]);
    }

    #[test]
    fn watch_pairs_duplicate_content_by_occurrence() {
        // 两条同名任务：第一条完成时另一条还在进行中 → 只报一条 TaskDone。
        let mut watch = Watch::default();
        let base = session(
            &[("dup", TaskStatus::Pending), ("dup", TaskStatus::InProgress)],
            "t",
        );
        watch.observe(&[base], 100.0);
        let next = session(
            &[("dup", TaskStatus::Completed), ("dup", TaskStatus::InProgress)],
            "t",
        );
        let notices = watch.observe(&[next], 110.0);
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0], Notice::TaskDone { .. }));
    }

    #[test]
    fn display_title_falls_back_for_empty() {
        assert_eq!(display_title("  "), "未命名会话");
        assert_eq!(display_title("  x "), "x");
    }
}
