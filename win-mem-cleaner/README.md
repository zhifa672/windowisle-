# win-mem-cleaner

Windows 内存清理 Rust 库，功能移植自
[WinMemoryCleaner](https://github.com/IgorMundstein/WinMemoryCleaner)（C#，GPL-3.0），
用于后续接入 [jilespace-daozai](../jilespace-daozai)。

## 功能

- **内存查询**：`query_memory()` —— 物理 RAM / 页面文件的总量、可用、使用率
  （`GlobalMemoryStatusEx`）。
- **手动清理**：`optimize()` —— 8 个内存区域可任意组合：
  工作集、系统文件缓存、已修改页列表、备用列表、低优先级备用列表、
  合并页列表、注册表缓存、已修改文件缓存。
- **定时清理**：每 N 小时自动清理一次。
- **阈值自动清理**：物理内存可用百分比低于阈值时自动清理，
  两次触发之间默认有 5 分钟冷却（与原版一致）。
- **自动提权**：`optimize_elevated()` / 调度器 `auto_elevate` —— 未提权时
  通过 UAC (runas) 启动提权 helper 进程执行清理并把报告传回。

系统要求：Windows 8.1+；**清理操作需要管理员权限**（内存查询不需要）。
实现说明：清理通过 `NtSetSystemInformation`（运行时从 ntdll 解析）、
`EmptyWorkingSet`、`SetSystemFileCacheSize`、卷设备 `DeviceIoControl` 等
Win32/原生 API 完成，命令值与结构布局和原 C# 版本一致。

## 关于管理员权限

原版 WinMemoryCleaner **同样需要管理员权限**，只是它把要求内置了：

- 桌面程序模式：`app.manifest` 中声明 `requireAdministrator`，
  每次启动即弹 UAC 强制提权；
- 服务模式：安装为 Windows 服务并以 `LocalSystem`（SYSTEM 账户）运行。

提权是 Windows 内核层的强制要求，不是实现选择：清空备用列表/已修改页
列表（`SystemMemoryListInformation`）、合并页、刷新系统文件缓存需要令牌
持有 `SeProfileSingleProcessPrivilege` / `SeIncreaseQuotaPrivilege`
（默认仅授予 Administrators 组）；逐进程清工作集需要 `SeDebugPrivilege`；
打开 `\\.\C:` 卷句柄同样需要提权。普通权限下这些调用会被
`ACCESS_DENIED` 拒绝（本库会把 `AdjustTokenPrivileges` 的
ERROR_NOT_ALL_ASSIGNED 识别为"特权不可用"并给出明确报错）。

对 windowisle 集成的建议：windowisle 平时以普通权限运行 UI，需要清理时可选——
1. windowisle 自身加 `requireAdministrator` manifest（同原版，最简单）；
2. 或按需通过 `runas` 启动一个提权 helper 进程执行清理
   （windowisle 主程序保持普通权限，体验更好）。

## 自动提权（UAC）

Windows 不允许运行中的进程原地提权，因此"自动提权"通过
`ShellExecuteExW` 的 `runas` 动词启动一个**提权 helper 进程**实现：

- `is_elevated()`：检测当前进程是否已提权（TokenElevation）；
- `optimize_elevated(reason, areas, options, &ElevateOptions)`：
  已提权则进程内直接清理；否则弹 UAC 启动 helper，等待完成并通过
  临时目录中的 JSON 文件回传 `CleanReport`；
- 调度器配置 `SchedulerConfig::auto_elevate = true`：定时/阈值清理
  未提权时自动走 helper（整体失败会发出 `CleanEvent::CleanFailed`）；
- helper 二进制：`win-mem-cleaner-helper.exe`（本 crate 的 bin target，
  一行调用 `win_mem_cleaner::helper_main()`；windowisle 也可自带同逻辑的
  `[[bin]]`，通过 `ElevateOptions::helper_path` / `SchedulerConfig::helper_path`
  指定路径）。

行为细节：

- **每次**未提权清理都会弹一次 UAC 确认框；用户取消返回
  `Error::Elevation("用户取消了 UAC 提权请求")`；
- 等待 helper 的超时默认 5 分钟（含用户盯着 UAC 对话框的时间）；
- helper 以管理员身份运行，为防滥用，只接受位于**系统临时目录**下的
  config/report 路径（其余一律拒绝执行）；
- UAC 弹窗对定时清理可能打扰用户，`auto_elevate` 默认关闭；
  常驻后台清理建议宿主程序直接以管理员运行（已提权时
  `optimize_elevated` 就是进程内清理，不弹任何窗口）。

## 示例

```rust
use win_mem_cleaner::{
    optimize, query_memory, start_auto_cleaner, CleanEvent, CleanOptions,
    CleanReason, MemoryAreas, SchedulerConfig,
};

// 1. 查询内存
let m = query_memory().unwrap();
println!("物理内存可用 {}%", m.physical.free_percent);

// 2. 手动清理一次（需管理员）
let report = optimize(CleanReason::Manual, MemoryAreas::ALL, &CleanOptions::default());

// 3. 自动调度：每 6 小时清理；可用内存 < 15% 时也清理（冷却 5 分钟）
let config = SchedulerConfig {
    interval_hours: 6,
    free_threshold_percent: 15,
    ..SchedulerConfig::default()
};
let mut handle = start_auto_cleaner(config, |event| match event {
    CleanEvent::Tick { memory } => { /* 刷新 UI */ }
    CleanEvent::Cleaned { reason, report } => { /* 日志/通知 */ }
    CleanEvent::Stopped => {}
});
// handle.update_config(new_config);  // 运行中改配置
// handle.stop();                     // 停止
```

运行演示（examples/demo.rs）：

```
cargo run --example demo                        # 打印内存信息
cargo run --example demo -- clean               # 手动清理一次（需管理员终端）
cargo run --example demo -- clean --elevate     # 清理，未提权时弹 UAC 自动提权
cargo run --example demo -- areas --elevate     # 逐区域清理，显示每步净变化
cargo run --example demo -- watch 1 15          # 每 1 小时清理，可用 <15% 也清理
```

`optimize_stepwise` / `ElevateOptions { stepwise: true }` 会在每个区域
清理前后采样物理内存可用量（`AreaResult::free_before/free_after`，
`freed_bytes()` 为净变化，可为负），适合宿主展示各区域的独立效果。

## 接入 jilespace-daozai

1. 在 windowisle 的 `Cargo.toml` 添加 path 依赖：

   ```toml
   [target.'cfg(windows)'.dependencies]
   win-mem-cleaner = { path = "../win-mem-cleaner" }
   ```

   （本 crate 使用与 windowisle 相同的 `windows = "0.58"` 依赖，版本可合并；
   所需 feature —— `Win32_Security`、`Win32_Storage_FileSystem`、
   `Win32_System_IO`、`Win32_System_LibraryLoader`、
   `Win32_System_SystemInformation`、`Win32_System_Threading`、
   `Win32_System_ProcessStatus`、`Win32_System_Diagnostics_ToolHelp`、
   `Win32_Foundation` —— windowisle 已全部启用，无需改动。）

2. 代码中按需调用：

   ```rust
   use win_mem_cleaner::{SchedulerConfig, start_auto_cleaner, CleanEvent};

   let config = SchedulerConfig {
       interval_hours: settings.mem_clean_interval_hours,
       free_threshold_percent: settings.mem_clean_free_threshold,
       process_exclusions: vec!["windowisle".into()],  // 排除自身
       ..SchedulerConfig::default()
   };
   let mut cleaner = start_auto_cleaner(config, move |event| {
       if let CleanEvent::Cleaned { reason, report } = event {
           // 汇报给窗屿 UI / 日志
       }
   });
   ```

3. 权限二选一：
   - windowisle 以管理员身份运行（清理直接在进程内执行，不弹窗）；
   - 或保持普通权限并启用自动提权——随 windowisle 分发
     `win-mem-cleaner-helper.exe`（或在 windowisle 里加一个
     `[[bin]]` 调 `win_mem_cleaner::helper_main()`，并在配置里指定
     `helper_path`），手动清理与定时清理均通过 UAC 提权执行：

     ```rust
     // 手动（如命令面板里的"清理内存"按钮）
     let report = win_mem_cleaner::optimize_elevated(
         CleanReason::Manual, MemoryAreas::ALL, &CleanOptions::default(),
         &ElevateOptions::default(),
     );

     // 定时（SchedulerConfig）
     let config = SchedulerConfig {
         auto_elevate: true,
         ..SchedulerConfig::default()
     };
     ```

## 宿主兼任清理服务（windowisle 的用法）

windowisle 不再分发独立 helper：`windowisle.exe` 的 `main` 开头调用
`win_mem_cleaner::service_cli()`，命中 `--service-install` /
`--service-uninstall` / `--service-run` 时直接作为服务程序运行并退出。

- `--service-install`（runas，需一次 UAC）：把当前程序复制为
  `%ProgramFiles%\WindowIsle\windowisle-clean-service.exe`（目录取自 HKLM），
  注册/更新 `WindowIsleClean` 服务并启动；已安装时会停服务、覆盖、改路径后重启。
- 服务为**按需启动**（SERVICE_DEMAND_START）：安装时服务 DACL 只给交互用户加
  SERVICE_START（`SERVICE_SDDL`），宿主用 `start_service()` 以普通权限拉起；
  所有客户端断开后立即停自动清理，空闲 60 秒服务自行停止。
- 客户端 `Client::connect(&service_exe_path())` 校验管道对端进程号 == SCM 服务进程号。
- 日志：`%ProgramFiles%\WindowIsle\logs\{install,service}.log`。

## 与原版 (C#) 的差异

- 仅移植服务端核心功能（内存查询、清理、调度）；GUI、日志、热键、
  注册表持久化等不在库内，由宿主应用（windowisle）负责。
- C# `WinService.OnTimer` 每 60 秒检查一次，本库保持一致（可配置）。
- 新增原版没有的**自动提权**：原版靠 `requireAdministrator` manifest /
  LocalSystem 服务保证权限，本库可在普通权限宿主中按需 UAC 提权。
- C# 的 .NET GC 释放步骤（`App.ReleaseMemory`）与 Rust 无关，已省略。
- `AdjustTokenPrivileges` 成功但特权未真正赋权（ERROR_NOT_ALL_ASSIGNED）
  的情况，本库视为失败（原版仅记日志）。
- 清理排除进程时，受保护进程的"拒绝访问"与原版一致被忽略；
  其他逐进程错误汇总进 `Error::Partial`。

## 许可

GPL-3.0（衍生自 GPL-3.0 的 WinMemoryCleaner）。
**注意**：若把本库编入 jilespace-daozai（MIT）分发，组合作品需遵循
GPL-3.0；如需保持 MIT，需自行重写而非使用本移植代码，或咨询法律意见。
