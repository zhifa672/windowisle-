//! Agent 任务页与收起态槽位的绘制（设计文档第 6、7 节）。
//! 面板局部坐标 419×140，文字 y 是基线；照 AI 额度页排：左圆环右清单。

use crate::app::{App, DSH_PAGE};
use crate::dsh_tasks::{Agent, Session, Status, Task, TaskStatus};
use crate::paint::{self, Card, H};
use crate::theme::{self, W};
use crate::anim::smoothstep;
use eframe::egui::{self, Color32};
use std::f32::consts::{FRAC_PI_2, TAU};

/// 进行中部分的透明度：0.35–1.0 之间呼吸，周期 2 秒；关闭动效时固定 0.7。
fn breathe(t: f64) -> f32 {
    if crate::bar_layout::animations_enabled() {
        0.675 + 0.325 * (t as f32 * std::f32::consts::PI).sin()
    } else {
        0.7
    }
}

/// 距会话最后一次更新过了多少秒。`updated` 是文件修改时间（Unix 秒），
/// 只能和墙上时间比，不能用 `App::t`（窗屿启动以来的秒数）。
fn age_secs(session: &Session) -> i64 {
    (crate::dsh_tasks::unix_now() - session.updated).max(0)
}

/// 超过 10 分钟没更新：会话多半已经停了，界面变灰。
fn is_stale(session: &Session) -> bool {
    age_secs(session) > 600
}

/// 展开页：会话标签 + 左侧进度圆环 + 右侧任务清单。
pub(crate) fn page_dsh(app: &mut App, c: &Card) {
    // 空状态（未装 DSH / 关闭 / 没有任务清单）。
    if app.dsh.sessions.is_empty() {
        empty_state(app, c);
        return;
    }
    let session = selected_session(app).cloned();

    // 会话标签（与额度页账号标签同款布局），最多 3 个 + 翻页箭头。
    session_tabs(app, c);

    let Some(session) = session else { return };
    let progress = session.progress();
    let all_done = progress.all_done();
    let stale = is_stale(&session);

    // 左侧圆环：中心 (103, 92)，r=28，线宽 6。
    let center = (103.0, 92.0);
    let accent = if stale { Color32::from_gray(110) } else { theme::SOFT_GREEN };
    progress_ring(app, c, &session, center.0, center.1, 28.0, 6.0, accent, "page");

    // 圆环中间：上面是 DeepSeek 图标（完成时变成对勾），下面是进度数字（完成时换成「完成」）。
    let morph = done_morph(app, &session);
    center_mark(c, session.agent(), center.0, 79.0, 16.0, 17.0, 2.2, accent, morph, if stale { 0.45 } else { 1.0 });
    let numbers = 1.0 - smoothstep(0.0, 0.5, morph);
    if numbers > 0.01 {
        let faded = c.faded(numbers);
        faded.text(center.0, 101.0, &format!("{}/{}", progress.done, progress.total), 13.0, W::Bold, theme::WHITE, H::Center);
        let percent = (progress.fraction() * 100.0).round() as i32;
        faded.text(center.0, 112.0, &format!("{percent}%"), 9.0, W::Regular, Color32::from_gray(140), H::Center);
    }
    if all_done && numbers < 0.99 {
        c.faded(1.0 - numbers).text(center.0, 104.0, "完成", 10.0, W::Semibold, theme::WHITE, H::Center);
    }
    // 点圆环把 DSH 桌面版切到前台（dsh://open 只能显示窗口，不能指定会话）。
    let ring_hit = c.hit(center.0 - 32.0, 60.0, 64.0, 64.0, "dsh-ring").on_hover_text(format!(
        "{}\n最后更新 {} 分钟前\n共 {} 项：完成 {}，进行中 {}\n点击打开 DeepSeek Harness",
        session_title(&session),
        age_secs(&session) / 60,
        progress.total,
        progress.done,
        progress.active
    ));
    if ring_hit.clicked() {
        app.pending.push(crate::app::Cmd::OpenDsh);
    }

    // 右侧任务清单：x 157–376，4 行。
    task_list(app, c, &session);

    // 右栏顶部、任务清单上方：模型 · 推理等级 · 缓存命中率 · 输出速度。
    token_stats_line(c, &session);
}

/// 统计行：画在右栏顶部（x 157 起，基线 61），夹在会话标签（y 30–51）和任务清单之间。
/// 标题栏那一行的中间是「日期 + 时钟」组，面板底部基线 133 一带是页码豆，都不能放。
/// 缺失的部分跳过。
fn token_stats_line(c: &Card, session: &Session) {
    let stats = &session.stats;
    let mut parts: Vec<String> = Vec::new();
    if !stats.model.is_empty() {
        let model = if stats.reasoning_effort.is_empty() {
            stats.model.clone()
        } else {
            format!("{} · {}", stats.model, stats.reasoning_effort)
        };
        parts.push(model);
    }
    if let Some(rate) = stats.cache_hit_rate() {
        parts.push(format!("缓存 {:.0}%", rate * 100.0));
    }
    if let Some(speed) = stats.output_rate() {
        parts.push(format!("{:.0} tok/s", speed));
    }
    if parts.is_empty() {
        return;
    }
    let text = parts.join(" · ");
    // 和任务清单同宽：x 157–376。
    let fitted = c.fit(&text, 9.0, W::Regular, 219.0);
    let width = c.text_w(&fitted, 9.0, W::Regular);
    c.text(157.0, 61.0, &fitted, 9.0, W::Regular, Color32::from_gray(125), H::Left);
    c.hit(157.0, 52.0, width, 11.0, "dsh-stats").on_hover_text(format!(
        "{}
总输入：{} 缓存读 + {} 缓存写 + {} 未命中
输出 {} tokens，解码 {:.1}s",
        text,
        stats.cache_read,
        stats.cache_write,
        stats.uncached_input,
        stats.output,
        stats.decode_ms as f64 / 1000.0
    ));
}

/// 当前展示的会话：选中的或最近更新的。
fn selected_session(app: &mut App) -> Option<&Session> {
    let selected = app.dsh.selected.clone();
    match selected {
        Some(id) => app.dsh.sessions.iter().find(|s| s.id == id),
        None => app.dsh.sessions.first(),
    }
}

/// 空状态文案（设计文档 6.5）。
fn empty_state(app: &mut App, c: &Card) {
    let (title, second, show_button) = match &app.dsh.status {
        Some(Status::NotFound(_)) => ("未检测到 DeepSeek Harness", "装好 DSH 后会自动识别，也可以手动指定目录", true),
        Some(Status::Unrecognized(_)) => ("读不懂 DSH 的数据", "DSH 可能升级了格式，请把会话文件发给开发者", true),
        Some(Status::Disabled) => ("Agent 任务已关闭", "可以在设置里重新开启", true),
        _ => ("还没有会话写过任务清单", "Agent 调用 todo_write 后，这里会显示进度", false),
    };
    c.text(43.0, 61.0, title, 13.0, W::Bold, theme::WHITE, H::Left);
    c.text(43.0, 80.0, second, 10.5, W::Regular, Color32::from_gray(150), H::Left);
    if show_button {
        let (rect, card) = paint::capsule(c, 43.0, 92.0, 94.0, 24.0, "dsh-goto-settings", theme::BTN, theme::BTN_HOVER);
        card.text(90.0, 108.0, "去设置", 10.0, W::Semibold, theme::WHITE, H::Center);
        if rect.clicked() {
            app.settings_tab = 11;
            app.open_settings();
        }
    }
}

/// 会话标签行（y 30–51）：x = 59 + 列×99，宽 94，最多 3 个 + 翻页箭头。
fn session_tabs(app: &mut App, c: &Card) {
    let sessions = app.dsh.sessions.clone();
    if sessions.is_empty() {
        return;
    }
    let selected_id = app
        .dsh
        .selected
        .clone()
        .or_else(|| sessions.first().map(|s| s.id.clone()));
    let page_base = app.dsh_sessions_tab_base();
    let shown: Vec<&Session> = sessions.iter().skip(page_base).take(3).collect();

    if page_base > 0 {
        if c.hit(42.0, 33.0, 14.0, 16.0, "dsh-tabs-prev").on_hover_text("上一组会话").clicked() {
            app.dsh.sessions_tab_base = page_base.saturating_sub(3);
        }
        c.text(47.0, 45.0, "‹", 12.0, W::Bold, theme::GRAY, H::Center);
    }
    if page_base + 3 < sessions.len() {
        if c.hit(357.0, 33.0, 14.0, 16.0, "dsh-tabs-next").on_hover_text("下一组会话").clicked() {
            app.dsh.sessions_tab_base = (page_base + 3).min(sessions.len().saturating_sub(1));
        }
        c.text(363.0, 45.0, "›", 12.0, W::Bold, theme::GRAY, H::Center);
    }

    for (column, session) in shown.iter().enumerate() {
        let x = 59.0 + column as f32 * 99.0;
        let is_selected = Some(&session.id) == selected_id.as_ref();
        let progress = session.progress();
        let stale = is_stale(session);
        let alpha = if stale { 0.55 } else { 1.0 };
        let card = c.faded(alpha);
        if is_selected {
            let bg = crate::paint::mix(theme::SOFT_GREEN, Color32::BLACK, 0.8);
            card.fill(x, 30.0, 94.0, 21.0, 8.0, bg);
        }
        let label = if progress.all_done() {
            format!("{} ✓", fit_title(c, &session_title(session)))
        } else {
            format!(
                "{} {}",
                fit_title(c, &session_title(session)),
                format!("{}/{}", progress.done, progress.total)
            )
        };
        let color = if is_selected { theme::SOFT_GREEN } else { theme::GRAY };
        card.text(x + 47.0, 44.5, &label, 10.5, if is_selected { W::Semibold } else { W::Regular }, color, H::Center);
        let id = session.id.clone();
        let tip = if stale {
            format!("{}\n最后更新 {} 分钟前", session_title(session), age_secs(session) / 60)
        } else {
            session_title(session)
        };
        if card.hit(x, 30.0, 94.0, 21.0, ("dsh-tab", id.as_str())).on_hover_text(tip).clicked() {
            app.dsh.selected = Some(id);
            app.dsh.first = 0;
            app.dsh.followed = None;
        }
    }
}

/// 右侧任务清单（4 行，基线 76/91/106/121），自动对准进行中的任务。
/// 上方留给统计行；第 4 行底部约在 124，离页码豆（约 131）还有距离。
fn task_list(app: &mut App, c: &Card, session: &Session) {
    let total = session.tasks.len();
    if total == 0 {
        return;
    }
    // 自动对准：进行中的任务排在第二行；用户手动滚动后（进行中未变）不拉回。
    if let Some(index) = session.tasks.iter().position(|t| t.status == TaskStatus::InProgress) {
        if app.dsh.followed.as_ref() != Some(&(session.id.clone(), index)) {
            app.dsh.first = index.saturating_sub(1).min(total.saturating_sub(4));
            app.dsh.followed = Some((session.id.clone(), index));
        }
    }
    let first = app.dsh.first.min(total.saturating_sub(1));
    for row in 0..4 {
        let index = first + row;
        if index >= total {
            break;
        }
        let task = &session.tasks[index];
        let baseline = 76.0 + row as f32 * 15.0;
        let (icon_color, text, weight, text_color) = match task.status {
            TaskStatus::Completed => (theme::SOFT_GREEN, task.content.clone(), W::Regular, Color32::from_gray(120)),
            TaskStatus::InProgress => (theme::SOFT_GREEN, task.content.clone(), W::Semibold, theme::WHITE),
            TaskStatus::Pending => (Color32::from_gray(110), task.content.clone(), W::Regular, Color32::from_gray(185)),
        };
        match task.status {
            TaskStatus::Completed => {
                paint::check(c, 163.0, baseline - 4.0, 6.0, 1.0, 1.2, icon_color);
                let fitted = fit_task(c, &text);
                c.text(173.0, baseline, &fitted, 11.0, weight, text_color, H::Left);
                // 删除线：长度按截断后的文字宽度算。
                let line_w = c.text_w(&fitted, 11.0, weight).min(196.0);
                c.fill(173.0, baseline - 4.0, line_w, 1.0, 0.5, text_color);
            }
            TaskStatus::InProgress => {
                c.circle(163.0, baseline - 4.0, 3.0, icon_color);
                c.ring(163.0, baseline - 4.0, 5.5, 1.2, paint::alpha(icon_color, breathe(app.t)));
                let fitted = fit_task(c, &text);
                c.text(173.0, baseline, &fitted, 11.0, weight, text_color, H::Left);
            }
            TaskStatus::Pending => {
                c.ring(163.0, baseline - 4.0, 3.0, 1.0, icon_color);
                let fitted = fit_task(c, &text);
                c.text(173.0, baseline, &fitted, 11.0, weight, text_color, H::Left);
            }
        }
        let hit = c.hit(157.0, baseline - 11.0, 219.0, 15.0, ("dsh-task", index));
        hit.on_hover_text(task.content.clone());
    }
}

/// 收起态任务槽位的图形（设计文档 7.3）：圆环 + Agent 图标 + 进度文字。
/// 岛上和「常驻排列」的拖动预览共用，两边看到的是同一个组件。
/// 槽位宽 46：左边圆环（中心 x-14，r=8，线宽 2），中间是该会话 Agent 的图标，
/// 一眼认出是哪个 Agent；右边从 x-2 起写进度，「10/12」也放得下。
fn paint_task_slot(app: &App, c: &Card, x: f32, session: &Session, tag: &str) {
    let progress = session.progress();
    let accent = theme::SOFT_GREEN;
    let ring_x = x - 14.0;
    progress_ring(app, c, session, ring_x, 15.0, 8.0, 2.0, accent, tag);
    center_mark(c, session.agent(), ring_x, 15.0, 10.0, 8.0, 1.5, accent, done_morph(app, session), 1.0);
    // 右边：进度数字；全部完成时写「完成」。
    if progress.all_done() {
        c.text(x - 2.0, 18.5, "完成", 9.0, W::Bold, accent, H::Left);
    } else {
        c.text(x - 2.0, 18.5, &format!("{}/{}", progress.done, progress.total), 9.0, W::Bold, theme::WHITE, H::Left);
    }
}

/// 「常驻排列」拖动预览里的任务槽位：有正在进行的会话就画它（图标是它所属的 Agent），
/// 没有就画一个示例进度，让用户认得出这个位置放的是什么。
pub(crate) fn draw_task_preview(app: &App, c: &Card, x: f32) {
    match app.dsh_resident() {
        Some(session) => paint_task_slot(app, c, x, session, "preview"),
        None => {
            let status = |n: usize| match n {
                0..=2 => TaskStatus::Completed,
                3 => TaskStatus::InProgress,
                _ => TaskStatus::Pending,
            };
            let sample = Session {
                id: "resident-preview".into(),
                title: String::new(),
                tasks: (0..5).map(|n| Task { content: format!("示例 {n}"), status: status(n) }).collect(),
                updated: 0,
                stats: Default::default(),
            };
            paint_task_slot(app, c, x, &sample, "preview");
        }
    }
}

/// 收起态任务槽位：图形 + 悬停提示 + 点击展开任务页。
pub(crate) fn draw_task_bar(app: &mut App, c: &Card, local_cx: f32, layout: &crate::bar_layout::Layout) {
    let Some(x_offset) = layout.x[crate::bar_layout::TASK] else { return };
    let x = local_cx + x_offset;
    let Some(session) = app.dsh_resident() else { return };
    let progress = session.progress();
    let all_done = progress.all_done();
    paint_task_slot(app, c, x, session, "bar");

    let summary = if all_done {
        format!("{}\n全部任务完成\n点击查看", session_title(session))
    } else {
        match session.current() {
            Some(task) => format!("{}\n正在：{}\n点击查看", session_title(&session), task.content),
            None => format!("{}\n{}/{}\n点击查看", session_title(&session), progress.done, progress.total),
        }
    };
    if c.hit(x - 23.0, 3.0, 46.0, 24.0, "dsh-task-bar").on_hover_text(summary).clicked() {
        app.show_page(DSH_PAGE);
    }
}

// ---------------------------------------------------------------- 圆环与中心图标

/// 完成动画的时长（秒）：图标缩小淡出，对勾按笔画描出来。
const MORPH_SECS: f64 = 0.6;

/// 完成动画进度：0 = 显示图标，1 = 显示对勾。
/// 看着它完成的会话从完成时刻开始播放；窗屿启动前就已经完成的直接显示对勾。
pub(crate) fn done_morph(app: &App, session: &Session) -> f32 {
    if !session.progress().all_done() {
        return 0.0;
    }
    if !crate::bar_layout::animations_enabled() {
        return 1.0;
    }
    match app.dsh.done_at.get(&session.id) {
        Some(at) => ((app.t - at) / MORPH_SECS).clamp(0.0, 1.0) as f32,
        None => 1.0,
    }
}

/// 进度圆环：底环 + 已完成弧 + 进行中弧（呼吸）。两段弧长都平滑过渡，
/// 任务完成时圆环是「长」满的，不是一下跳满。
#[allow(clippy::too_many_arguments)]
fn progress_ring(app: &App, c: &Card, session: &Session, x: f32, y: f32, r: f32, stroke: f32, accent: Color32, tag: &str) {
    let progress = session.progress();
    let total = progress.total.max(1) as f32;
    let target_done = progress.done as f32 / total;
    let target_active = 0.5 * progress.active as f32 / total;
    let (done, active) = if crate::bar_layout::animations_enabled() {
        let ctx = c.ui.ctx();
        (
            ctx.animate_value_with_time(egui::Id::new(("dsh-ring-done", tag, &session.id)), target_done, 0.35),
            ctx.animate_value_with_time(egui::Id::new(("dsh-ring-active", tag, &session.id)), target_active, 0.35),
        )
    } else {
        (target_done, target_active)
    };
    c.ring(x, y, r, stroke, Color32::from_gray(if r > 12.0 { 48 } else { 60 }));
    let start = -FRAC_PI_2;
    let done_end = start + TAU * done.clamp(0.0, 1.0);
    if done >= 0.999 {
        c.ring(x, y, r, stroke, accent);
    } else if done > 0.005 {
        c.arc(x, y, r, start, done_end, stroke, accent);
    }
    if active > 0.005 && done < 0.999 {
        c.arc(x, y, r, done_end, done_end + TAU * active, stroke, paint::alpha(accent, breathe(app.t)));
    }
}

/// 圆环中心的标记：`morph` 从 0 到 1，Agent 图标缩小淡出，对勾按笔画描出来。
#[allow(clippy::too_many_arguments)]
fn center_mark(c: &Card, agent: Agent, x: f32, y: f32, logo_size: f32, check_size: f32, check_width: f32, accent: Color32, morph: f32, alpha: f32) {
    let fade = smoothstep(0.0, 0.5, morph);
    if fade < 0.999 {
        agent_logo(c, agent, x, y, logo_size, 1.0 - 0.35 * fade, alpha * (1.0 - fade));
    }
    let stroke = smoothstep(0.25, 1.0, morph);
    if stroke > 0.0 {
        // 描到最后略微放大一点，像落定。
        let size = check_size * (0.92 + 0.08 * stroke);
        paint::check(c, x, y, size, stroke, check_width, paint::alpha(accent, alpha));
    }
    if morph > 0.0 && morph < 1.0 {
        c.ui.ctx().request_repaint();
    }
}

/// 各 Agent 的图标（assets/ai-icons，与 AI 额度页的图标同一套）。
fn agent_svg(agent: Agent) -> &'static [u8] {
    match agent {
        Agent::DeepSeek => include_bytes!("../assets/ai-icons/deepseek.svg"),
    }
}

/// Agent 图标：按 Agent 和目标像素尺寸栅格化一次并缓存；缩放动画只缩放绘制区域，不重新栅格化。
#[allow(clippy::too_many_arguments)]
fn agent_logo(c: &Card, agent: Agent, x: f32, y: f32, size: f32, scale: f32, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let ctx = c.ui.ctx();
    let ppp = ctx.pixels_per_point();
    let pixels = (size * c.s * ppp).round().max(1.0) as u32;
    let key = egui::Id::new(("agent-logo", agent, pixels));
    let texture = match ctx.data_mut(|data| data.get_temp::<egui::TextureHandle>(key)) {
        Some(texture) => texture,
        None => {
            let Ok(mut image) = egui_extras::image::load_svg_bytes_with_size(agent_svg(agent), Some(egui::load::SizeHint::Size(pixels, pixels))) else { return };
            // 白色标识只取覆盖率，透明边缘不会变暗（与额度页图标一致）。
            for pixel in &mut image.pixels {
                *pixel = Color32::from_white_alpha(pixel.a());
            }
            let texture = ctx.load_texture(format!("agent-logo-{}-{pixels}", agent.name()), image, egui::TextureOptions::LINEAR);
            ctx.data_mut(|data| data.insert_temp(key, texture.clone()));
            texture
        }
    };
    let side = pixels as f32 / ppp * scale;
    let center = c.pos(x, y);
    // 静止时对齐到物理像素，图标边缘清晰；动画中按实际位置画。
    let min = if scale >= 0.999 {
        egui::pos2(((center.x - side / 2.0) * ppp).round() / ppp, ((center.y - side / 2.0) * ppp).round() / ppp)
    } else {
        egui::pos2(center.x - side / 2.0, center.y - side / 2.0)
    };
    let rect = egui::Rect::from_min_size(min, egui::Vec2::splat(side));
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    c.painter.image(texture.id(), rect, uv, Color32::from_white_alpha((alpha.clamp(0.0, 1.0) * 255.0) as u8));
}

fn fit_title(c: &Card, title: &str) -> String {
    c.fit(title, 10.5, W::Regular, 74.0)
}

fn fit_task(c: &Card, text: &str) -> String {
    c.fit(text, 11.0, W::Regular, 196.0)
}

/// 会话标题（空 → 未命名会话）。
fn session_title(session: &Session) -> String {
    crate::dsh_tasks::display_title(&session.title)
}
