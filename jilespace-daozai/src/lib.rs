//! 窗屿（WindowIsle）：Windows 桌面顶部的信息小岛。基于开源项目「岛仔」（MIT）修改。
//!
//! - `app` / `pages`：与平台无关的界面与动效（外壳与录制器共用）
//! - `anim` / `paint` / `shape` / `theme`：弹簧、矢量绘制、面板轮廓、配色字体
//! - `model` / `lyrics` / `smtc` / `icons`：数据、歌词、系统媒体、文件图标
//! - `native` / `tray`：仅 Windows 的窗口区域、命中穿透与托盘

pub mod poll_clock;
pub mod processes;
pub mod region_capture;
pub mod screenshot;
pub mod scroll_capture;
pub mod anim;
pub mod annotate;
pub mod app;
pub mod audio;
pub mod brightness;
pub mod brightness_hud;
pub mod bt;
pub mod calendar;
pub mod clipboard;
pub mod clipboard_ui;
pub mod capture_ui;
pub mod command_palette;
mod calendar_ui;
mod local_calendar;
mod calendar_editor;
pub mod glass;
pub mod icons;
pub mod input;
mod clipboard_hotkey;
pub mod lyrics;
pub mod mail;
/// 改名「岛仔 → 窗屿」后的一次性迁移（数据目录、凭据）。
pub mod legacy;
pub mod media_hud;
mod mail_reader;
mod task_picker;
pub mod model;
pub mod notifications;
pub mod notifications_ui;
pub mod ocr;
pub mod otp;
pub mod quota;
mod quota_ui;
mod dsh_tasks_ui;
mod pages;
mod extras;
mod pet;
mod bar_layout;
mod resident_settings;
mod focus_bar;
#[cfg(windows)]
pub mod system;
pub mod dsh_tasks;
pub mod mem_clean;
pub mod paint;
pub mod perf;
pub mod shape;
pub mod smtc;
pub mod theme;
pub mod settings_theme;
mod panel;
pub mod pin;
pub mod pin_store;
mod updates;
pub mod volume;
pub mod weather;
pub mod world_clock;

#[cfg(windows)]
pub mod native;
#[cfg(windows)]
pub mod tray;

#[cfg(test)]
mod ui_tests;
#[cfg(test)]
#[path = "bin/windowisle-record/raster.rs"]
mod preview_raster;
