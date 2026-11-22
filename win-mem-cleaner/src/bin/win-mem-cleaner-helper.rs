//! 独立的提权 helper 进程（不随 windowisle 分发；windowisle.exe 自身兼任清理服务），
//! 两种形态：
//!
//! 形态一 Windows 服务：`--service-install` / `--service-uninstall` /
//! `--service-run`，见 [`win_mem_cleaner::service_cli`]。
//! 形态二（兼容旧路径）一次性 UAC helper：被 `optimize_elevated` 通过
//! runas 启动，执行清理后写报告退出。

fn main() {
    if let Some(code) = win_mem_cleaner::service_cli() {
        std::process::exit(code);
    }
    std::process::exit(win_mem_cleaner::helper_main());
}
