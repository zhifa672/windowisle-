//! 邮件（v0.5.5）：六类邮箱的账号模型、IMAP 收取、正文解析。
//!
//! 轻量化约束：只用 Rust 原生 crate（`imap` + `native-tls` + `mailparse`），
//! 不引 Node/Go/Electron。凭据一律进 Windows 凭据管理器（`keyring`），
//! 账号清单只存「邮箱 + 类型 + 是否监控」，不存密码。
//!
//! 收信优先 `Session::idle()` 长连接；服务器不支持 IDLE 时退回轮询（60/120/300 秒）。
//! 正文只在内存里解析，不落盘、不缓存。

use std::collections::HashMap;

pub const SERVICE: &str = "WindowIsle.Mail";
/// 改名前（岛仔）的凭据服务名：读取时迁移到 SERVICE 名下。
pub const LEGACY_SERVICE: &str = "Daozai.Mail";

// ---------------------------------------------------------------- 邮箱类型

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Qq,
    Mail163,
    Mail126,
    Exmail,
    ICloud,
    Gmail,
    /// 自定义邮箱：自建 IMAP 服务器（host/端口/用户名/证书策略在 Account 上）。
    Custom,
    /// Outlook（本机）：COM 读取正在运行的 Outlook 收件箱，无需密码。
    Outlook,
}

impl Default for Provider {
    fn default() -> Self {
        Provider::Qq
    }
}

impl Provider {
    pub const ALL: [Provider; 8] = [
        Provider::Qq,
        Provider::Mail163,
        Provider::Mail126,
        Provider::Exmail,
        Provider::ICloud,
        Provider::Gmail,
        Provider::Custom,
        Provider::Outlook,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Provider::Qq => "qq",
            Provider::Mail163 => "163",
            Provider::Mail126 => "126",
            Provider::Exmail => "exmail",
            Provider::ICloud => "icloud",
            Provider::Gmail => "gmail",
            Provider::Custom => "custom",
            Provider::Outlook => "outlook",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Qq => "QQ 邮箱",
            Provider::Mail163 => "163 邮箱",
            Provider::Mail126 => "126 邮箱",
            Provider::Exmail => "腾讯企业邮",
            Provider::ICloud => "iCloud 邮箱",
            Provider::Gmail => "Gmail",
            Provider::Custom => "自定义邮箱（IMAP）",
            Provider::Outlook => "Outlook（本机）",
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Provider::Qq => "imap.qq.com",
            Provider::Mail163 => "imap.163.com",
            Provider::Mail126 => "imap.126.com",
            Provider::Exmail => "imap.exmail.qq.com",
            Provider::ICloud => "imap.mail.me.com",
            Provider::Gmail => "imap.gmail.com",
            Provider::Custom => "",
            Provider::Outlook => "",
        }
    }

    pub fn port(self) -> u16 {
        993
    }

    /// 163 / 126 在 LOGIN 之前必须发 RFC 2971 的 `ID`，否则会被判 Unsafe Login。
    pub fn needs_id(self) -> bool {
        matches!(self, Provider::Mail163 | Provider::Mail126)
    }

    /// 设置界面的操作指引。
    pub fn hint(self) -> &'static str {
        match self {
            Provider::Qq => "1. 登录 QQ 邮箱网页版，点右上角「设置」→ 左侧「账号与安全」。\n2. 在新页面选「安全设置」，向下找到「POP3/IMAP/SMTP/Exchange/CardDAV 服务」；未开启时先按提示开启。\n3. 点「生成授权码」，完成验证后复制到下方。这里填写授权码，不是 QQ 密码或邮箱独立密码。",
            Provider::Mail163 | Provider::Mail126 => "1. 登录对应的网易邮箱网页版，点顶部「设置」→「POP3/SMTP/IMAP」。\n2. 确认「IMAP/SMTP 服务」已开启；首次开启时按提示完成验证。\n3. 在「授权密码管理」中点「新增授权密码」，完成验证后复制到下方。填写客户端授权码，不是邮箱登录密码。",
            Provider::Exmail => "1. 登录腾讯企业邮网页版，在「设置」→「收发信设置」检查 IMAP/SMTP 服务。\n2. 常见网页版在「设置」→「邮箱绑定」→「客户端专用密码」→「生成新密码」获取密码；启用安全登录后须使用专用密码。\n3. 企业版本和管理员策略可能不同。找不到入口或服务被禁用时，请让企业管理员确认客户端接入方式，不要照搬 QQ 邮箱的路径。",
            Provider::ICloud => "1. 登录 Apple 账户页面，进入「登录和安全」→「App 专用密码」。\n2. 选择「生成 App 专用密码」，按提示填写名称并完成验证；账户需已开启双重认证。\n3. 邮箱栏填写完整的 iCloud 邮箱地址，下方填写刚生成的 App 专用密码，不是 Apple 账户密码。",
            Provider::Gmail => "1. 登录 Google 账户，进入「安全性与登录」→「您的 Google 账号登录选项」→「开启两步验证」，按提示完成设置。\n2. 打开下方「应用专用密码」页面，输入应用名称（例如：窗屿）并创建，复制生成的 16 位应用专用密码。\n3. 邮箱栏填完整 Gmail 地址，下方填应用专用密码，不是 Google 登录密码。若没有创建入口，请检查组织账号限制、高级保护或仅使用安全密钥的验证设置。",
            Provider::Outlook => "无需密码：窗屿直接读取本机已登录的 Outlook 收件箱。
请先打开 Outlook，邮箱栏填写 Outlook 里该账号的邮箱地址。
Outlook 关闭时暂停收信；邮件正文只在内存中读取，不会标记为已读、不会落盘。",
            Provider::Custom => "填写邮件服务器的 IMAP 地址（域名或 IP，如 mail.company.com）与端口（默认 993 SSL）。\n登录用户名默认用完整邮箱地址；服务器要求单独的登录名时在「用户名」里填写。\n服务器使用自签证书时勾选「跳过证书校验」，否则 TLS 握手会失败。\n密码栏填写邮箱的登录密码（或管理员分配的客户端密码）。",
        }
    }

    pub fn setup_link(self) -> (&'static str, &'static str) {
        match self {
            Provider::Qq => ("打开 QQ 邮箱网页版", "https://mail.qq.com/"),
            Provider::Mail163 => ("打开 163 邮箱网页版", "https://mail.163.com/"),
            Provider::Mail126 => ("打开 126 邮箱网页版", "https://mail.126.com/"),
            Provider::Exmail => ("打开腾讯企业邮网页版", "https://exmail.qq.com/"),
            Provider::ICloud => ("打开 Apple 账户", "https://account.apple.com/"),
            Provider::Gmail => ("打开应用专用密码", "https://myaccount.google.com/apppasswords"),
            Provider::Custom => ("", ""),
            Provider::Outlook => ("", ""),
        }
    }

    /// 是否需要密码/授权码（Outlook 走本机 COM，无需凭据）。
    pub fn uses_password(self) -> bool {
        !matches!(self, Provider::Outlook)
    }

    pub fn parse(key: &str) -> Self {
        match key {
            "163" => Provider::Mail163,
            "126" => Provider::Mail126,
            "exmail" => Provider::Exmail,
            "icloud" => Provider::ICloud,
            "gmail" => Provider::Gmail,
            "custom" => Provider::Custom,
            "outlook" => Provider::Outlook,
            _ => Provider::Qq,
        }
    }
}

// ---------------------------------------------------------------- 账号与状态

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub email: String,
    pub provider: Provider,
    /// 是否监控这个账号（设置里可逐个开关）。
    pub enabled: bool,
    /// 自定义邮箱（Custom）：IMAP 服务器地址；预设邮箱为 None。
    pub imap_host: Option<String>,
    /// 自定义邮箱：IMAP 端口（默认 993 SSL）。
    pub imap_port: Option<u16>,
    /// 自定义邮箱：登录用户名（默认用完整邮箱地址）。
    pub imap_user: Option<String>,
    /// 自定义邮箱：跳过证书校验（自签证书的服务器需要）。
    pub imap_insecure: Option<bool>,
    /// 自定义邮箱：STARTTLS（明文连接后在问候阶段升级 TLS，默认 143）。
    pub imap_starttls: Option<bool>,
}

impl Account {
    /// 连接用的 host / 端口 / 用户名（自定义优先，预设兜底）。
    pub fn host(&self) -> &str {
        self.imap_host.as_deref().filter(|s| !s.is_empty()).unwrap_or_else(|| self.provider.host())
    }

    pub fn port(&self) -> u16 {
        self.imap_port.unwrap_or(if self.starttls() { 143 } else { 993 })
    }

    /// 自定义邮箱使用 STARTTLS（143 明文连接后升级）。
    pub fn starttls(&self) -> bool {
        self.imap_starttls.unwrap_or(false)
    }

    pub fn user(&self) -> &str {
        self.imap_user.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.email)
    }
}

pub fn key(account: &Account) -> String {
    account.email.trim().to_ascii_lowercase()
}

fn accounts_path() -> std::path::PathBuf {
    crate::model::config_dir().join("mail-accounts.json")
}

pub fn load_accounts() -> Vec<Account> {
    match std::fs::read(accounts_path()) {
        Ok(raw) => decode_accounts(&raw),
        Err(_) => Vec::new(),
    }
}

/// 纯解析：账号清单里只有邮箱、类型、开关，没有任何密码字段。
pub fn decode_accounts(raw: &[u8]) -> Vec<Account> {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    json.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let email = a.get("email")?.as_str()?.trim().to_string();
            if email.is_empty() {
                return None;
            }
            Some(Account {
                provider: Provider::parse(a.get("provider").and_then(|p| p.as_str()).unwrap_or("qq")),
                enabled: a.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
                email,
                imap_host: a.get("imap_host").and_then(|v| v.as_str()).map(str::to_string),
                imap_port: a.get("imap_port").and_then(|v| v.as_u64()).and_then(|v| u16::try_from(v).ok()).filter(|p| *p != 0),
                imap_user: a.get("imap_user").and_then(|v| v.as_str()).map(str::to_string),
                imap_insecure: a.get("imap_insecure").and_then(|v| v.as_bool()),
                imap_starttls: a.get("imap_starttls").and_then(|v| v.as_bool()),
            })
        })
        .collect()
}

pub fn encode_accounts(items: &[Account]) -> Vec<u8> {
    let list: Vec<_> = items
        .iter()
        .map(|a| {
            let mut value = serde_json::json!({"email": a.email, "provider": a.provider.key(), "enabled": a.enabled});
            if a.provider == Provider::Custom {
                value["imap_host"] = serde_json::json!(a.imap_host);
                value["imap_port"] = serde_json::json!(a.imap_port);
                value["imap_user"] = serde_json::json!(a.imap_user);
                value["imap_insecure"] = serde_json::json!(a.imap_insecure);
                value["imap_starttls"] = serde_json::json!(a.imap_starttls);
            }
            value
        })
        .collect();
    serde_json::to_vec(&list).unwrap_or_default()
}

pub fn save_accounts(items: &[Account]) {
    let data = encode_accounts(items);
    if data.is_empty() {
        return;
    }
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::model::atomic_write(&path, &data);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Connecting,
    Ready,
    /// 授权码缺失或失效。
    Auth(String),
    Failed(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Idle => "未连接".into(),
            Status::Connecting => "连接中…".into(),
            Status::Ready => "已连接".into(),
            Status::Auth(msg) => format!("登录被拒绝：{msg}"),
            Status::Failed(msg) => format!("连接失败：{msg}"),
        }
    }
    pub fn ok(&self) -> bool {
        matches!(self, Status::Ready)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub account: String,
    pub from: String,
    pub subject: String,
    /// 本地时间 `MM-DD HH:MM`
    pub time: String,
    pub code: Option<String>,
    /// 完整纯文字正文，保留分段，仅保存在内存中。
    pub summary: String,
    pub links: Vec<Link>,
    /// 会议邀请（来自 .ics / text/calendar），没有就是 `None`。
    pub meeting: Option<Meeting>,
    pub uid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// 仅把有效网页链接交给默认浏览器。
pub fn web_url(raw:&str)->Option<String> {
    let parsed=url::Url::parse(raw.trim()).ok()?;
    if !matches!(parsed.scheme(),"https"|"http") || parsed.host_str().is_none(){return None;}
    Some(parsed.into())
}

pub fn links_of(parsed:&Parsed)->Vec<Link> {
    use std::sync::OnceLock;
    static ANCHOR:OnceLock<regex::Regex>=OnceLock::new();
    static URL:OnceLock<regex::Regex>=OnceLock::new();
    let anchor=ANCHOR.get_or_init(||regex::Regex::new(r#"(?is)<a\b[^>]*?\s+href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a\s*>"#).unwrap());
    let url=URL.get_or_init(||regex::Regex::new(r#"(?i)https?://[^\s<>"'，。；！？、]+"#).unwrap());
    let mut links=Vec::new();let mut seen=std::collections::HashSet::new();
    let mut add=|raw:&str,label:String| {
        let decoded=otp::html_to_text(raw);
        if let Some(url)=web_url(decoded.trim()) {
            if seen.insert(url.clone()) {links.push(Link{label:if label.trim().is_empty(){url.clone()}else{label.trim().into()},url});}
        }
    };
    for cap in anchor.captures_iter(&parsed.html) {
        if let Some(href)=cap.get(1).or_else(||cap.get(2)).or_else(||cap.get(3)) {
            add(href.as_str(),otp::html_to_text(&cap[4]));
        }
    }
    for text in [&parsed.text,&otp::html_to_text(&parsed.html)] {
        for hit in url.find_iter(text) {
            let mut target=hit.as_str().trim_end_matches(['.',',',';',':','!','，','。','；','！','？','）','】','》']);
            for (open,close) in [('(',')'),('[',']'),('{','}')] {
                while target.ends_with(close) && target.chars().filter(|c|*c==close).count()>target.chars().filter(|c|*c==open).count(){target=&target[..target.len()-1];}
            }
            add(target,target.into());
        }
    }
    links
}

/// 会议邀请里界面和提醒要用的字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meeting {
    pub all_day: bool,
    pub title: String,
    /// 开始时间（Unix 秒，本地时区解释）。
    pub start: i64,
    /// 来源提供的结束时间；缺失或无效时不推测时长。
    pub end: Option<i64>,
    /// 本地时间 `MM-DD HH:MM`，直接显示。
    pub at: String,
    /// iCalendar 的 `UID`；邮件和日历两条来源靠它 + 开始时间去重。
    pub uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Status { account: String, status: Status },
    Mail { item: Item, new: bool },
    Test { account: String, ok: bool, note: String },
}

// ---------------------------------------------------------------- Outlook（本机）纯逻辑

/// Outlook 新邮件发送计划（顺序 = 从旧到新）：首轮全部 `false`
/// （启动时不把旧邮件弹成提醒），之后最新的 5 封（列表末尾）为 `true`。
pub(crate) fn plan_emit(total: usize, first: bool) -> Vec<bool> {
    let mut flags = vec![false; total];
    if !first {
        for flag in flags.iter_mut().rev().take(5) {
            *flag = true;
        }
    }
    flags
}

/// Outlook 一次读取的原始字段（COM 层读出后交给纯函数组装）。
#[derive(Clone, Debug)]
pub(crate) struct OutlookMail {
    pub entry_id: String,
    pub subject: String,
    pub sender: String,
    pub body: String,
    pub html: String,
    /// OLE 自动化日期：自 1899-12-30 起的天数（本地时间）。
    pub received: f64,
    pub meeting: Option<Meeting>,
}

/// OLE 自动化日期 → 本地时间。
pub(crate) fn ole_date(days: f64) -> Option<chrono::DateTime<chrono::Local>> {
    if !days.is_finite() || days < 1.0 {
        return None;
    }
    let base = chrono::NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let at = base + chrono::Duration::milliseconds((days * 86_400_000.0).round() as i64);
    at.and_local_timezone(chrono::Local).single()
}

/// EntryID → Item.uid：FNV-1a 32 位哈希，避开 0。
pub(crate) fn uid_of(entry_id: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in entry_id.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

/// Outlook 原始字段 → 界面 Item（验证码 / 摘要 / 链接复用邮件解析）。
pub(crate) fn outlook_item(account: &Account, m: OutlookMail) -> Item {
    let mut parsed = Parsed {
        from: m.sender.clone(),
        subject: m.subject.clone(),
        text: m.body.clone(),
        html: m.html.clone(),
        calendar: String::new(),
        emphasized: Vec::new(),
    };
    if parsed.text.is_empty() && !parsed.html.is_empty() {
        parsed.emphasized = crate::otp::emphasized(&parsed.html);
    }
    let subject = if parsed.subject.is_empty() { "（无主题）".to_string() } else { parsed.subject.clone() };
    Item {
        account: account.email.clone(),
        from: if parsed.from.is_empty() { account.email.clone() } else { parsed.from.clone() },
        subject,
        time: ole_date(m.received).map(|d| d.format("%m-%d %H:%M").to_string()).unwrap_or_default(),
        code: code_of(&parsed),
        summary: cap_summary(body_of(&parsed)),
        links: links_of(&parsed),
        meeting: m.meeting,
        uid: uid_of(&m.entry_id),
    }
}

// ---------------------------------------------------------------- 正文解析

pub struct Parsed {
    pub from: String,
    pub subject: String,
    /// text/plain 正文（没有则空）。
    pub text: String,
    /// text/html 正文转成的纯文本（没有则空）。
    pub html: String,
    /// text/calendar 正文（.ics 原文，没有则空）。
    pub calendar: String,
    pub emphasized: Vec<String>,
}

fn header(mail: &mailparse::ParsedMail, name: &str) -> String {
    mail.headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case(name))
        .map(|h| h.get_value().trim().to_string())
        .unwrap_or_default()
}

fn display_addr(raw: &str) -> String {
    mailparse::addrparse(raw)
        .ok()
        .and_then(|list| {
            list.iter().find_map(|a| match a {
                mailparse::MailAddr::Single(s) => Some(s.display_name.clone().unwrap_or_else(|| s.addr.clone())),
                mailparse::MailAddr::Group(g) => g.addrs.first().map(|s| s.display_name.clone().unwrap_or_else(|| s.addr.clone())),
            })
        })
        .unwrap_or_else(|| raw.trim().trim_matches('"').to_string())
}

fn collect(mail: &mailparse::ParsedMail, plain: &mut String, html: &mut String, calendar: &mut String) {
    if mail.subparts.is_empty() {
        let ct = mail.ctype.mimetype.to_ascii_lowercase();
        if ct == "text/plain" {
            if let Ok(body) = mail.get_body() {
                plain.push_str(&body);
                plain.push('\n');
            }
        } else if ct == "text/html" {
            if let Ok(body) = mail.get_body() {
                html.push_str(&body);
                html.push('\n');
            }
        } else if ct == "text/calendar" || ct == "application/ics" {
            if let Ok(body) = mail.get_body() {
                calendar.push_str(&body);
                calendar.push('\n');
            }
        }
    } else {
        for part in &mail.subparts {
            collect(part, plain, html, calendar);
        }
    }
}

/// 解析一封原始邮件（头部 + 正文），只保留界面上要用的字段。
pub fn parse_message(raw: &[u8]) -> Parsed {
    let Ok(mail) = mailparse::parse_mail(raw) else {
        return Parsed {
            from: String::new(),
            subject: String::new(),
            text: String::from_utf8_lossy(raw).into_owned(),
            html: String::new(),
            calendar: String::new(),
            emphasized: Vec::new(),
        };
    };
    let subject = header(&mail, "Subject");
    let from = display_addr(&header(&mail, "From"));
    let (mut plain, mut html, mut calendar) = (String::new(), String::new(), String::new());
    collect(&mail, &mut plain, &mut html, &mut calendar);
    let emphasized = if plain.trim().is_empty() {
        otp::emphasized(&html)
    } else {
        Vec::new()
    };
    Parsed { from, subject, text: plain, html, calendar, emphasized }
}

/// 优先纯文本，HTML 邮件转成文字；保留完整正文与段落。
/// 单封邮件正文进入内存前的硬上限，避免超大邮件把「最近 60 封」撑爆内存。
pub const MAX_SUMMARY_BYTES: usize = 256 * 1024;
pub const SUMMARY_TRUNCATED: &str = "\n…（邮件过长，已截断）";

pub fn cap_summary(mut text: String) -> String {
    if text.len() <= MAX_SUMMARY_BYTES {
        return text;
    }
    let mut end = MAX_SUMMARY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(SUMMARY_TRUNCATED);
    text
}

pub fn body_of(parsed:&Parsed)->String {
    let body = if !parsed.text.trim().is_empty() {
        parsed.text.clone()
    } else if !parsed.html.trim().is_empty() {
        otp::html_to_text(&parsed.html)
    } else {
        String::new()
    };
    body.replace("\r\n","\n").trim().to_string()
}

/// 紧凑场景使用的短摘要。
pub fn summary_of(parsed: &Parsed) -> String {
    let body=body_of(parsed);
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(200).collect();
    if flat.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// 从解析结果里提验证码：先纯文本，纯文本没有再看 HTML 文本。
pub fn code_of(parsed: &Parsed) -> Option<String> {
    if let Some(c) = otp::extract(&parsed.subject, &parsed.text, &parsed.emphasized) {
        return Some(c.text);
    }
    if !parsed.html.trim().is_empty() {
        let text = otp::html_to_text(&parsed.html);
        if let Some(c) = otp::extract(&parsed.subject, &text, &parsed.emphasized) {
            return Some(c.text);
        }
    }
    None
}

/// 从 .ics 文本里取第一个 VEVENT 的标题与开始时间（邮件正文用）。
pub fn meeting_of(calendar: &str) -> Option<Meeting> {
    meetings_of(calendar).into_iter().next()
}

/// 从一段 iCalendar 里取出所有带时刻的 VEVENT。
///
/// CalDAV 的 `calendar-query` 一次会返回多个事件，邮件里的 `.ics` 通常只有一个；
/// 两者共用这一套解析；支持带时刻和全天日期，没有有效 DTSTART 就跳过。
pub fn meetings_of(calendar: &str) -> Vec<Meeting> {
    // RFC 5545 折行：以空格/制表符开头的行是上一行的续行。
    let mut lines: Vec<String> = Vec::new();
    for raw in calendar.lines() {
        let line = raw.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            if let Some(last) = lines.last_mut() {
                last.push_str(&line[1..]);
            }
        } else {
            lines.push(line.to_string());
        }
    }

    let mut out = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut in_event = false;
    let mut cancelled = false;
    let mut nested = 0usize;
    for line in &lines {
        let up = line.to_ascii_uppercase();
        if up=="BEGIN:VCALENDAR" {cancelled=false;}
        if !in_event && up=="METHOD:CANCEL" {cancelled=true;}
        if up=="BEGIN:VEVENT" {
            in_event = true;
            nested=0;
            fields.clear();
            continue;
        }
        if up=="END:VEVENT" {
            if in_event && !cancelled {
                if let Some(m) = event_of(&fields) {
                    out.push(m);
                }
            }
            in_event = false;
            continue;
        }
        if !in_event {
            continue;
        }
        if up.starts_with("BEGIN:"){nested+=1;continue;}
        if up.starts_with("END:"){nested=nested.saturating_sub(1);continue;}
        if nested>0 {continue;}
        let Some((head, value)) = line.split_once(':') else { continue };
        let name = head.split(';').next().unwrap_or("").to_ascii_uppercase();
        if name == "DTSTART" || name == "DTEND" {
            if let Some(tz) = head.split(';').skip(1).find_map(|p| p.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case("TZID")).map(|(_, v)| v.trim_matches('"'))) {
                fields.push((format!("{name}-TZID"), tz.to_string()));
            }
        }
        fields.push((name, value.trim().to_string()));
    }
    out
}

fn event_of(fields: &[(String, String)]) -> Option<Meeting> {
    use chrono::TimeZone;
    let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    if get("STATUS").is_some_and(|v|v.eq_ignore_ascii_case("CANCELLED")){return None;}
    let parse_time = |name: &str| -> Option<chrono::DateTime<chrono::Local>> {
        let value = get(name)?;
        Some(if let Some(s) = value.strip_suffix('Z') {
            chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").ok()?.and_utc().with_timezone(&chrono::Local)
        } else {
            if !value.contains('T') {
                return chrono::NaiveDate::parse_from_str(&value,"%Y%m%d").ok()?.and_hms_opt(0,0,0)?.and_local_timezone(chrono::Local).earliest();
            }
            let n = chrono::NaiveDateTime::parse_from_str(&value, "%Y%m%dT%H%M%S").ok()?;
            if let Some(tz) = get(&format!("{name}-TZID")) {
                let zone: chrono_tz::Tz = tz.parse().ok()?;
                zone.from_local_datetime(&n).single()?.with_timezone(&chrono::Local)
            } else {
                chrono::Local.from_local_datetime(&n).single()?
            }
        })
    };
    let naive = parse_time("DTSTART")?;
    let all_day=get("DTSTART").is_some_and(|value|!value.contains('T'));
    let end = parse_time("DTEND").map(|at|at.timestamp()).filter(|end|*end>naive.timestamp())
        .or_else(||all_day.then(||naive.date_naive().succ_opt()?.and_hms_opt(0,0,0)?.and_local_timezone(chrono::Local).earliest().map(|at|at.timestamp())).flatten());
    let title = get("SUMMARY").map(|v| ics_text(&v)).filter(|t| !t.is_empty()).unwrap_or_else(|| "会议".to_string());
    Some(Meeting {
        all_day,
        title,
        start: naive.timestamp(),
        end,
        at: naive.format(if all_day {"%m-%d 全天"}else{"%m-%d %H:%M"}).to_string(),
        uid: get("UID").unwrap_or_default(),
    })
}

/// iCalendar 文本转义：`\n` / `\,` / `\;` / `\\`。
fn ics_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

use crate::otp;

// ---------------------------------------------------------------- 凭据

#[cfg(windows)]
pub mod secrets {
    /// 凭据只进 Windows 凭据管理器；键是邮箱地址，值是授权码。
    pub fn save(email: &str, password: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(super::SERVICE, &email.to_ascii_lowercase())
            .map_err(|e| format!("凭据管理器不可用：{e}"))?;
        entry.set_password(password).map_err(|e| format!("保存授权码失败：{e}"))
    }

    pub fn load(email: &str) -> Result<String, String> {
        crate::legacy::adopt_credential(super::SERVICE, super::LEGACY_SERVICE, &email.to_ascii_lowercase());
        let entry = keyring::Entry::new(super::SERVICE, &email.to_ascii_lowercase())
            .map_err(|e| format!("凭据管理器不可用：{e}"))?;
        entry.get_password().map_err(|_| "没有保存授权码".to_string())
    }

    pub fn delete(email: &str) {
        if let Ok(entry) = keyring::Entry::new(super::SERVICE, &email.to_ascii_lowercase()) {
            let _ = entry.delete_credential();
        }
        crate::legacy::delete_legacy_credential(super::LEGACY_SERVICE, &email.to_ascii_lowercase());
    }
}

#[cfg(not(windows))]
pub mod secrets {
    pub fn save(_email: &str, _password: &str) -> Result<(), String> {
        Err("仅 Windows 支持凭据管理器".into())
    }
    pub fn load(_email: &str) -> Result<String, String> {
        Err("仅 Windows 支持凭据管理器".into())
    }
    pub fn delete(_email: &str) {}
}

// ---------------------------------------------------------------- 收信服务

#[cfg(windows)]
mod net {
    //! TLS 连接 + 163/126 的 pre-login `ID` 握手。
    //!
    //! `imap` 2.4 只在已登录的 `Session` 上暴露 `run_command_and_check_ok`，
    //! 而网易要求 greeting → ID → LOGIN 的顺序，所以这里手工读问候、发 ID，
    //! 再把「已经读掉的开头字节」原样交回给 imap。

    use super::{Account, Provider};
    use imap::extensions::idle::SetReadTimeout;
    use native_tls::{TlsConnector, TlsStream};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    pub type Stream = TlsStream<TcpStream>;

    /// 先吐出预先读到的问候字节，再透传给底层流。
    pub struct Prefixed<S> {
        head: Vec<u8>,
        pos: usize,
        inner: S,
    }

    impl<S> Prefixed<S> {
        fn new(head: Vec<u8>, inner: S) -> Self {
            Self { head, pos: 0, inner }
        }
    }

    impl<S: Read> Read for Prefixed<S> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.head.len() {
                let n = (self.head.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            } else {
                self.inner.read(buf)
            }
        }
    }

    impl<S: Write> Write for Prefixed<S> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    impl SetReadTimeout for Prefixed<Stream> {
        fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::Result<()> {
            self.inner
                .get_ref()
                // imap 结束 IDLE 时传入 None；恢复普通命令的超时，避免无限阻塞。
                .set_read_timeout(timeout.or(Some(Duration::from_secs(20))))
                .map_err(imap::Error::Io)
        }
    }

    fn read_line<R: std::io::Read>(stream: &mut R, buf: &mut Vec<u8>) -> Result<(), String> {
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return Err("服务器关闭了连接".into()),
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n") {
                        return Ok(());
                    }
                    if buf.len() > 8192 {
                        return Err("服务器响应过长".into());
                    }
                }
                Err(e) => return Err(format!("读取失败：{e}")),
            }
        }
    }

    /// 读服务器问候直到 `* OK` / `* PREAUTH`，返回原始字节（STARTTLS
    /// 需要读明文问候，SSL 读 TLS 后的问候，逻辑相同）。
    fn read_greeting<R: std::io::Read>(stream: &mut R) -> Result<Vec<u8>, ConnectError> {
        let mut greeting = Vec::new();
        loop {
            let mut line = Vec::new();
            read_line(stream, &mut line).map_err(ConnectError::Net)?;
            let upper = String::from_utf8_lossy(&line).to_ascii_uppercase();
            greeting.extend_from_slice(&line);
            if upper.starts_with("* BYE") {
                return Err(ConnectError::Net("服务器拒绝连接".into()));
            }
            if upper.starts_with("* OK") || upper.starts_with("* PREAUTH") {
                return Ok(greeting);
            }
            if greeting.len() > 8192 {
                return Err(ConnectError::Net("服务器问候异常".into()));
            }
        }
    }

    /// STARTTLS 协商：发 `S0 STARTTLS`，必须读到 `S0 OK` 才算成功；
    /// 服务器拒绝时绝不降级成明文登录。
    pub(super) fn starttls_handshake<S: std::io::Read + std::io::Write>(stream: &mut S) -> Result<(), ConnectError> {
        stream
            .write_all(b"S0 STARTTLS\r\n")
            .map_err(|e| ConnectError::Net(format!("发送 STARTTLS 失败：{e}")))?;
        let _ = stream.flush();
        for _ in 0..10 {
            let mut line = Vec::new();
            read_line(stream, &mut line).map_err(ConnectError::Net)?;
            let text = String::from_utf8_lossy(&line);
            if text.starts_with("S0 ") {
                if text.to_ascii_uppercase().contains(" OK") {
                    return Ok(());
                }
                return Err(ConnectError::Net(format!("服务器不支持 STARTTLS：{}", text.trim())));
            }
        }
        Err(ConnectError::Net("服务器不支持 STARTTLS（无响应）".into()))
    }

    /// 连接/登录错误：Auth 是服务器明确拒绝凭据（不应反复重试，域账号
    /// 连续输错会被 AD 锁定）；Net 是网络层问题（可退避重试）。
    #[derive(Debug)]
    pub enum ConnectError {
        Auth(String),
        Net(String),
    }

    pub fn connect(account: &Account, password: &str) -> Result<imap::Session<Prefixed<Stream>>, ConnectError> {
        // 自定义邮箱可跳过证书校验（自签证书服务器）。
        let mut builder = TlsConnector::builder();
        if account.imap_insecure.unwrap_or(false) {
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        let tls = builder
            .build()
            .map_err(|e| ConnectError::Net(format!("TLS 初始化失败：{e}")))?;
        let host = account.host();
        let tcp = TcpStream::connect((host, account.port()))
            .map_err(|e| ConnectError::Net(format!("无法连接 {host}：{e}")))?;
        let timeout = Some(Duration::from_secs(20));
        let _ = tcp.set_read_timeout(timeout);
        let _ = tcp.set_write_timeout(timeout);
        // STARTTLS：先读明文问候，协商升级后再做 TLS 握手；SSL 则直接握手。
        let (mut stream, greeting) = if account.starttls() {
            let mut tcp = tcp;
            let greeting = read_greeting(&mut tcp)?;
            starttls_handshake(&mut tcp)?;
            let tls_stream = tls.connect(host, tcp).map_err(|e| ConnectError::Net(format!("TLS 握手失败：{e}")))?;
            (tls_stream, greeting)
        } else {
            let mut tls_stream = tls.connect(host, tcp).map_err(|e| ConnectError::Net(format!("TLS 握手失败：{e}")))?;
            let greeting = read_greeting(&mut tls_stream)?;
            (tls_stream, greeting)
        };

        // 2) 网易系：LOGIN 之前先发 ID（RFC 2971）
        if account.provider.needs_id() {
            let id = format!(
                "A0001 ID (\"name\" \"windowisle\" \"version\" \"{}\" \"vendor\" \"windowisle\")\r\n",
                env!("CARGO_PKG_VERSION")
            );
            stream
                .write_all(id.as_bytes())
                .map_err(|e| ConnectError::Net(format!("发送 ID 失败：{e}")))?;
            let _ = stream.flush();
            let mut ok = false;
            for _ in 0..20 {
                let mut line = Vec::new();
                read_line(&mut stream, &mut line).map_err(ConnectError::Net)?;
                let text = String::from_utf8_lossy(&line).to_string();
                if text.starts_with("A0001 ") {
                    ok = text.to_ascii_uppercase().contains(" OK");
                    break;
                }
            }
            if !ok {
                return Err(ConnectError::Net("服务器不接受 ID 命令".into()));
            }
        }

        // 3) 把问候交回 imap，再正常登录
        let mut client = imap::Client::new(Prefixed::new(greeting, stream));
        client.read_greeting().map_err(|e| ConnectError::Net(format!("服务器问候异常：{e}")))?;
        let session = client
            .login(account.user().to_string(), password)
            .map_err(|(e, _)| login_error(e, account.provider))?;
        Ok(session)
    }

    /// 登录错误分类：凭据被拒（Auth，不重试）与网络/服务器问题（Net，退避重试）。
    pub fn login_error(e: imap::Error, provider: Provider) -> ConnectError {
        match e {
            imap::Error::No(s) => {
                let text = s.trim().to_string();
                let lower = text.to_ascii_lowercase();
                if lower.contains("[unavailable]") {
                    // RFC 5530：服务器暂时不可用，属网络侧。
                    ConnectError::Net(format!("{text}（服务器暂时不可用）"))
                } else if lower.contains("unsafe login") {
                    ConnectError::Net(format!("{text}（网易要求先发 ID 命令）"))
                } else {
                    ConnectError::Auth(auth_hint(&text, provider))
                }
            }
            imap::Error::Bad(s) => ConnectError::Auth(auth_hint(s.trim(), provider)),
            imap::Error::Io(io) => ConnectError::Net(format!("网络错误：{io}")),
            other => ConnectError::Net(format!("登录失败：{other}")),
        }
    }

    fn auth_hint(text: &str, provider: Provider) -> String {
        if provider == Provider::Custom {
            format!("{text}（请检查用户名和密码）")
        } else {
            format!("{text}（请检查授权码是否失效）")
        }
    }

    /// 测试连接：连上、登录、选 INBOX，然后退出。
    pub fn probe(account: &Account, password: &str) -> Result<(), String> {
        let mut session = connect(account, password).map_err(|e| match e {
            ConnectError::Auth(msg) | ConnectError::Net(msg) => msg,
        })?;
        session.select("INBOX").map_err(|e| format!("无法打开收件箱：{e}"))?;
        let _ = session.logout();
        Ok(())
    }
}

/// Outlook（本机）：COM 读取正在运行的 Outlook 收件箱，无需密码。
/// 每次读取重新获取 COM 对象、读完全部释放；新邮件用 EntryID 去重。
#[cfg(windows)]
mod outlook {
    use super::service::{emit, sleep_stop};
    use super::{Account, Event, Meeting, OutlookMail, Status, ole_date, outlook_item, plan_emit};

    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    use windows::core::{BSTR, GUID, HSTRING, Interface, PCWSTR, VARIANT};
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, IDispatch, CLSIDFromProgID, COINIT_MULTITHREADED,
        DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPPARAMS,
    };
    use windows::Win32::System::Ole::GetActiveObject;

    const LCID: u32 = 0x0400; // LOCALE_USER_DEFAULT

    /// COM 错误分类（决定界面提示与重试节奏）。
    enum Fail {
        /// 没有安装 Outlook。
        NotInstalled,
        /// Outlook 未运行（或已中途退出）。
        NotRunning,
        /// Outlook 正忙（模态对话框等），下一轮再试。
        Busy,
        /// 多账号里找不到该邮箱地址。
        AccountNotFound,
        /// 安全提示被拒绝（或杀软拦截）。
        Denied,
        Other(String),
    }

    impl From<windows::core::Error> for Fail {
        fn from(e: windows::core::Error) -> Self {
            match e.code().0 as u32 {
                0x8004_01E3 | 0x8001_0108 | 0x8007_06BA | 0x8004_01FD => Fail::NotRunning,
                0x8001_0001 | 0x8001_010A => Fail::Busy,
                0x8000_4004 | 0x8007_0005 => Fail::Denied,
                _ => Fail::Other(format!("Outlook 错误 0x{:08X}", e.code().0 as u32)),
            }
        }
    }

    impl Fail {
        fn message(&self, email: &str) -> Status {
            match self {
                Fail::NotInstalled => Status::Failed("本机没有安装 Outlook".into()),
                Fail::NotRunning => Status::Failed(
                    "Outlook 未运行。请先打开 Outlook（窗屿不能以管理员身份运行，否则读取不到）".into(),
                ),
                Fail::AccountNotFound => Status::Failed(format!("Outlook 里没有 {email} 这个账号")),
                Fail::Denied => Status::Auth(
                    "Outlook 拒绝了访问。请检查杀毒软件状态，或在 Outlook「文件 → 选项 → 信任中心 → 信任中心设置 → 编程访问」中调整".into(),
                ),
                Fail::Busy => Status::Connecting,
                Fail::Other(msg) => Status::Failed(msg.clone()),
            }
        }
    }

    /// IDispatch 极简封装：Invoke / 属性 / 方法 / 取对象。
    struct Disp(IDispatch);

    impl Disp {
        fn invoke(&self, name: &str, flags: DISPATCH_FLAGS, args: &[VARIANT]) -> Result<VARIANT, Fail> {
            let wide = HSTRING::from(name);
            let mut id = 0i32;
            unsafe {
                self.0
                    .GetIDsOfNames(&GUID::zeroed(), &PCWSTR(wide.as_ptr()), 1, LCID, &mut id)
                    .map_err(Fail::from)?;
            }
            // DISPPARAMS 的参数是倒序的。
            let mut rev: Vec<VARIANT> = args.iter().rev().cloned().collect();
            let params = DISPPARAMS {
                rgvarg: rev.as_mut_ptr(),
                cArgs: rev.len() as u32,
                ..Default::default()
            };
            let mut out = VARIANT::default();
            unsafe {
                self.0
                    .Invoke(id, &GUID::zeroed(), LCID, flags, &params, Some(&mut out), None, None)
                    .map_err(Fail::from)?;
            }
            Ok(out)
        }

        fn get(&self, name: &str) -> Result<VARIANT, Fail> {
            self.invoke(name, DISPATCH_PROPERTYGET, &[])
        }

        fn call(&self, name: &str, args: &[VARIANT]) -> Result<VARIANT, Fail> {
            self.invoke(name, DISPATCH_METHOD, args)
        }

        fn obj(&self, name: &str, args: &[VARIANT]) -> Result<Disp, Fail> {
            let value = self.invoke(name, DISPATCH_METHOD | DISPATCH_PROPERTYGET, args)?;
            Ok(Disp(IDispatch::try_from(&value).map_err(Fail::from)?))
        }

        fn text(&self, name: &str) -> Result<String, Fail> {
            Ok(BSTR::try_from(&self.get(name)?).map(|b| b.to_string()).unwrap_or_default())
        }

        fn int(&self, name: &str) -> Result<i32, Fail> {
            i32::try_from(&self.get(name)?).map_err(Fail::from)
        }

        fn double(&self, name: &str) -> Result<f64, Fail> {
            let value = self.get(name)?;
            f64::try_from(&value).map_err(|_| Fail::Other(format!("{name} 不是日期值")))
        }

        fn item(&self, index: i32) -> Result<Disp, Fail> {
            self.obj("Item", &[VARIANT::from(index)])
        }
    }

    fn active_outlook() -> Result<Disp, Fail> {
        unsafe {
            let clsid = CLSIDFromProgID(windows::core::w!("Outlook.Application")).map_err(|_| Fail::NotInstalled)?;
            let mut unknown = None;
            GetActiveObject(&clsid, None, &mut unknown).map_err(|_| Fail::NotRunning)?;
            let unknown = unknown.ok_or(Fail::NotRunning)?;
            Ok(Disp(unknown.cast::<IDispatch>().map_err(Fail::from)?))
        }
    }

    /// 找到该邮箱地址对应的收件箱；只有一个账号时直接用默认收件箱。
    fn inbox_of(namespace: &Disp, email: &str) -> Result<Disp, Fail> {
        let accounts = namespace.obj("Accounts", &[])?;
        let count = accounts.int("Count")?;
        for index in 1..=count {
            let account = accounts.item(index)?;
            if account.text("SmtpAddress").unwrap_or_default().eq_ignore_ascii_case(email.trim()) {
                let store = account.obj("DeliveryStore", &[])?;
                if let Ok(inbox) = store.obj("GetDefaultFolder", &[VARIANT::from(6i32)]) {
                    return Ok(inbox);
                }
                // IMAP 数据文件的 GetDefaultFolder 可能报错：按名称找收件箱。
                let root = store.obj("GetRootFolder", &[])?;
                let folders = root.obj("Folders", &[])?;
                for i in 1..=folders.int("Count")? {
                    let folder = folders.item(i)?;
                    let name = folder.text("Name")?;
                    if name.eq_ignore_ascii_case("收件箱") || name.eq_ignore_ascii_case("Inbox") {
                        return Ok(folder);
                    }
                }
            }
        }
        if count == 1 {
            return namespace.obj("GetDefaultFolder", &[VARIANT::from(6i32)]);
        }
        Err(Fail::AccountNotFound)
    }

    /// 会议邀请的日历项 → Meeting；任何一步失败都返回 None。
    fn read_meeting(item: &Disp) -> Option<Meeting> {
        let appointment = item.obj("GetAssociatedAppointment", &[VARIANT::from(false)]).ok()?;
        let start = appointment.double("Start").ok()?;
        let start_at = ole_date(start)?; // 日期无效则整条日程无效，不生成 1970 年
        let end = appointment.double("End").ok();
        let all_day = appointment.get("AllDayEvent").map(|v| i32::try_from(&v).unwrap_or(0)).unwrap_or(0) != 0;
        Some(Meeting {
            all_day,
            title: appointment.text("Subject").ok()?,
            start: start_at.timestamp(),
            end: end.and_then(|e| ole_date(e).map(|d| d.timestamp())),
            at: if all_day {
                start_at.format("%m-%d 全天").to_string()
            } else {
                start_at.format("%m-%d %H:%M").to_string()
            },
            uid: appointment.text("GlobalAppointmentID").ok()?,
        })
    }

    /// 读取单封：只读属性，不会标记已读（不调用 Display 等会写入的方法）。
    fn read_item(item: &Disp, class: i32) -> Result<OutlookMail, Fail> {
        let mut html = item.text("HTMLBody")?;
        const MAX_HTML: usize = 512 * 1024;
        if html.len() > MAX_HTML {
            let mut end = MAX_HTML;
            while !html.is_char_boundary(end) {
                end -= 1;
            }
            html.truncate(end);
        }
        // 会议邀请：读关联的日历项。日程已被删除等情况下读取会失败，
        // 只把 meeting 置空，不影响这封邮件本身。
        let meeting = if class == 53 { read_meeting(item) } else { None };
        // 54（会议取消）当普通邮件显示，meeting 为空。
        Ok(OutlookMail {
            entry_id: item.text("EntryID")?,
            subject: item.text("Subject")?,
            sender: item.text("SenderName")?,
            body: item.text("Body")?,
            html,
            received: item.double("ReceivedTime")?,
            meeting,
        })
    }

    /// 一次读取：按 ReceivedTime 倒序取前 30 封，用 EntryID 判断新邮件。
    /// 不用 Items.Restrict 按日期过滤——日期串格式跟随系统区域设置，
    /// 中英文系统写法不同容易出错。
    fn poll(account: &Account, seen: &mut std::collections::HashSet<String>, first: bool, emit_new: impl Fn(Event)) -> Result<(), Fail> {
        let app = active_outlook()?;
        let namespace = app.obj("GetNamespace", &[VARIANT::from("MAPI")])?;
        let inbox = inbox_of(&namespace, &account.email)?;
        let items = inbox.obj("Items", &[])?;
        items.call("Sort", &[VARIANT::from("[ReceivedTime]"), VARIANT::from(true)])?;
        let count = items.int("Count")?.min(30);
        let mut fresh: Vec<(String, OutlookMail)> = Vec::new();
        let mut this_round: Vec<String> = Vec::new();
        for index in 1..=count {
            let item = items.item(index)?;
            let class = item.int("Class")?;
            // 43 普通邮件，53 会议邀请，54 会议取消；退信回执等跳过。
            if !matches!(class, 43 | 53 | 54) {
                continue;
            }
            let entry_id = item.text("EntryID")?;
            this_round.push(entry_id.clone());
            if seen.contains(&entry_id) {
                continue;
            }
            // 单封读取失败只跳过这一封（记入 seen，之后不再重试它）；
            // 未运行 / 正忙 / 被拒绝属于整体状态，照常向上返回。
            match read_item(&item, class) {
                Ok(raw) => fresh.push((entry_id, raw)),
                Err(fail @ (Fail::NotRunning | Fail::Busy | Fail::Denied)) => return Err(fail),
                Err(_) => {}
            }
        }
        // 输入按新到旧排序：反转成从旧到新发送，最新的最后插入、
        // 排在界面最上面；new 标志由 plan_emit 纯函数给出。
        fresh.reverse();
        for (is_new, (_, raw)) in plan_emit(fresh.len(), first).into_iter().zip(fresh.into_iter()) {
            emit_new(Event::Mail { item: outlook_item(account, raw), new: is_new });
        }
        for id in &this_round {
            seen.insert(id.clone());
        }
        // seen 超过 2000 条时收缩到本轮看到的条目。
        if seen.len() > 2000 {
            *seen = this_round.into_iter().collect();
        }
        Ok(())
    }


    pub(super) fn worker(account: Account, stop: Arc<std::sync::atomic::AtomicBool>, ev: Sender<Event>) {
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
        let mut seen = std::collections::HashSet::new();
        let mut first = true;
        let mut last_status: Option<Status> = None;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match poll(&account, &mut seen, first, |event| emit(&stop, &ev, event)) {
                Ok(()) => {
                    // 第一轮成功后，后续新出现的才是真正的新邮件。
                    first = false;
                    if last_status.as_ref() != Some(&Status::Ready) {
                        last_status = Some(Status::Ready);
                        emit(&stop, &ev, Event::Status { account: account.email.clone(), status: Status::Ready });
                    }
                }
                Err(Fail::Busy) => {}
                Err(Fail::Denied) => {
                    // 用户拒绝过安全提示后，每 30 秒请求会反复弹窗：5 分钟后再试。
                    let status = Fail::Denied.message(&account.email);
                    last_status = Some(status.clone());
                    emit(&stop, &ev, Event::Status { account: account.email.clone(), status });
                    if sleep_stop(&stop, 300) { break; }
                    continue;
                }
                Err(fail) => {
                    let status = fail.message(&account.email);
                    last_status = Some(status.clone());
                    emit(&stop, &ev, Event::Status { account: account.email.clone(), status });
                }
            }
            if sleep_stop(&stop, 30) {
                break;
            }
        }
        unsafe { CoUninitialize(); }
    }

    /// 测试连接：确认能挂上运行中的 Outlook 并读到收件箱。
    pub(super) fn probe(account: &Account) -> Result<String, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com = hr.is_ok();
            let result = (|| -> Result<String, Fail> {
                let app = active_outlook()?;
                let namespace = app.obj("GetNamespace", &[VARIANT::from("MAPI")])?;
                let inbox = inbox_of(&namespace, &account.email)?;
                let items = inbox.obj("Items", &[])?;
                Ok(format!("已连接 Outlook，收件箱 {} 封", items.int("Count")?))
            })();
            if com {
                CoUninitialize();
            }
            result.map_err(|fail| match fail.message(account.email.trim()) {
                Status::Failed(msg) | Status::Auth(msg) => msg,
                _ => "连接 Outlook 失败".into(),
            })
        }
    }
}

#[cfg(windows)]
mod service {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Arc;
    use std::time::Duration;

    enum Cmd {
        Sync(Vec<Account>, u32),
        Test(Account, String),
    }

    pub struct Service {
        cmd: Sender<Cmd>,
        ev: Receiver<Event>,
    }

    impl Service {
        pub fn start() -> Self {
            let (ev_tx, ev_rx) = channel();
            let (cmd_tx, cmd_rx) = channel();
            let _ = std::thread::Builder::new()
                .name("windowisle-mail".into())
                .spawn(move || supervisor(cmd_rx, ev_tx));
            Self { cmd: cmd_tx, ev: ev_rx }
        }

        pub fn sync(&self, accounts: &[Account], interval_secs: u32) {
            let _ = self.cmd.send(Cmd::Sync(accounts.to_vec(), interval_secs));
        }

        pub fn test(&self, account: Account, password: String) {
            let _ = self.cmd.send(Cmd::Test(account, password));
        }

        pub fn try_recv(&self) -> Option<Event> {
            self.ev.try_recv().ok()
        }
    }

    /// 计算这轮同步要停哪些、启哪些（配置变了的账号两边都有）。
    /// 旧实现只用邮箱地址区分线程：自定义邮箱改了服务器或端口后，
    /// 旧线程会一直拿旧配置连。
    fn plan_sync(
        current: &HashMap<String, Account>,
        wanted: &HashMap<String, Account>,
    ) -> (Vec<String>, Vec<Account>) {
        let stop: Vec<String> = current
            .iter()
            .filter(|(k, running)| match wanted.get(*k) {
                None => true,
                Some(want) => want != *running,
            })
            .map(|(k, _)| k.clone())
            .collect();
        let start: Vec<Account> = wanted
            .iter()
            .filter(|(k, want)| match current.get(*k) {
                None => true,
                Some(running) => running != *want,
            })
            .map(|(_, want)| want.clone())
            .collect();
        (stop, start)
    }

    fn supervisor(cmd_rx: Receiver<Cmd>, ev: Sender<Event>) {
        let mut workers: HashMap<String, (Account, Arc<AtomicBool>)> = HashMap::new();
        let interval = Arc::new(AtomicU64::new(120));
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                Cmd::Sync(accounts, secs) => {
                    interval.store(secs.clamp(30, 900) as u64, Ordering::Relaxed);
                    let wanted: HashMap<String, Account> = accounts
                        .into_iter()
                        .filter(|a| a.enabled)
                        .map(|a| (key(&a), a))
                        .collect();
                    let running: HashMap<String, Account> = workers.iter().map(|(k, (a, _))| (k.clone(), a.clone())).collect();
                    let (stop_keys, start_accounts) = plan_sync(&running, &wanted);
                    for k in stop_keys {
                        if let Some((_, flag)) = workers.remove(&k) {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                    for account in start_accounts {
                        let stored = account.clone();
                        let k = key(&account);
                        let stop = Arc::new(AtomicBool::new(false));
                        let flag = stop.clone();
                        let ev = ev.clone();
                        let name = format!("windowisle-mail-{}", account.email);
                        // Outlook（本机）：COM 轮询线程，不跟随 IMAP 间隔。
                        let spawned = if account.provider == Provider::Outlook {
                            std::thread::Builder::new()
                                .name(name)
                                .spawn(move || outlook::worker(account, flag, ev))
                                .is_ok()
                        } else {
                            let iv = interval.clone();
                            std::thread::Builder::new()
                                .name(name)
                                .spawn(move || worker(account, flag, iv, ev))
                                .is_ok()
                        };
                        if spawned {
                            workers.insert(k, (stored, stop));
                        }
                    }
                }
                Cmd::Test(account, password) => {
                    let ev = ev.clone();
                    let _ = std::thread::Builder::new()
                        .name("windowisle-mail-test".into())
                        .spawn(move || {
                            let result: Result<(), String> = if account.provider == Provider::Outlook {
                                outlook::probe(&account).map(|_| ())
                            } else {
                                net::probe(&account, &password)
                            };
                            let _ = ev.send(Event::Test {
                                account: account.email,
                                ok: result.is_ok(),
                                note: result.err().unwrap_or_else(|| "连接成功".into()),
                            });
                        });
                }
            }
        }
        for (_, (_, flag)) in workers {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// 旧线程不能覆盖新线程的状态：线程被要求停止后不再发事件
    /// （它可能还卡在 20 秒的连接超时里，晚到的 Ready/Failed 会盖掉
    /// 新线程的状态）。
    pub(super) fn emit(stop: &AtomicBool, ev: &Sender<Event>, event: Event) {
        if !stop.load(Ordering::Relaxed) {
            let _ = ev.send(event);
        }
    }

    pub(super) fn sleep_stop(stop: &AtomicBool, secs: u64) -> bool {
        for _ in 0..secs * 2 {
            if stop.load(Ordering::Relaxed) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    fn wait_for_change<T: std::io::Read + std::io::Write + imap::extensions::idle::SetReadTimeout>(session: &mut imap::Session<T>, check: Duration) -> imap::Result<()> {
        // 优先响应服务器推送；定期补查，避免服务端漏推送时长时间没有提醒。
        session.idle()?.wait_with_timeout(check).map(|_| ())
    }

    #[test]
    fn starttls_handshake_ok_no_and_disconnect() {
        use net::starttls_handshake;
        use net::ConnectError;
        use std::io::{BufRead, Write};
        // 模拟服务器：OK / NO / 直接断开三种情况。
        for (scenario, expect_ok) in [(0, true), (1, false), (2, false)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                write!(socket, "* OK ready\r\n").unwrap();
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                assert!(line.contains("STARTTLS"), "应收到 STARTTLS 命令");
                match scenario {
                    0 => write!(socket, "S0 OK Begin TLS negotiation\r\n").unwrap(),
                    1 => write!(socket, "S0 NO STARTTLS not supported\r\n").unwrap(),
                    _ => {} // 直接关闭连接
                }
            });
            let mut tcp = std::net::TcpStream::connect(address).unwrap();
            let result = starttls_handshake(&mut tcp);
            server.join().unwrap();
            match (expect_ok, result) {
                (true, Ok(())) => {}
                (false, Err(ConnectError::Net(msg))) => assert!(msg.contains("STARTTLS") || msg.contains("关闭") || msg.contains("读取"), "{msg}"),
                other => panic!("场景 {scenario} 结果不符：{other:?}"),
            }
        }
    }

    #[test]
    fn login_error_classifies_auth_vs_net() {
        use net::ConnectError;
        // 凭据被拒 → Auth，且自定义邮箱的提示是检查用户名密码。
        match net::login_error(imap::Error::No("[AUTHENTICATIONFAILED] Invalid credentials".into()), Provider::Custom) {
            ConnectError::Auth(msg) => assert!(msg.contains("用户名和密码"), "{msg}"),
            ConnectError::Net(_) => panic!("凭据被拒应归为 Auth"),
        }
        // 授权码体系仍是「检查授权码」。
        match net::login_error(imap::Error::No("AUTHENTICATIONFAILED".into()), Provider::Qq) {
            ConnectError::Auth(msg) => assert!(msg.contains("授权码"), "{msg}"),
            ConnectError::Net(_) => panic!("凭据被拒应归为 Auth"),
        }
        // 服务器暂时不可用（RFC 5530）→ Net。
        assert!(matches!(
            net::login_error(imap::Error::No("[UNAVAILABLE] Server busy".into()), Provider::Custom),
            ConnectError::Net(_)
        ));
        // 网易 unsafe login → Net。
        assert!(matches!(
            net::login_error(imap::Error::No("Unsafe Login".into()), Provider::Mail163),
            ConnectError::Net(_)
        ));
        // IO → Net；Bad → Auth。
        assert!(matches!(
            net::login_error(imap::Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "t")), Provider::Custom),
            ConnectError::Net(_)
        ));
        assert!(matches!(
            net::login_error(imap::Error::Bad("Invalid command".into()), Provider::Custom),
            ConnectError::Auth(_)
        ));
    }

    #[test]
    fn plan_sync_handles_unchanged_changed_and_removed() {
        let account = |port: Option<u16>| Account {
            email: "a@b.c".into(),
            provider: Provider::Custom,
            enabled: true,
            imap_host: Some("b.c".into()),
            imap_port: port,
            imap_user: None,
            imap_insecure: None, imap_starttls: None,
        };
        let map = |accounts: Vec<Account>| accounts.into_iter().map(|a| (key(&a), a)).collect::<HashMap<_, _>>();
        // 完全一致：既不停也不启。
        let (stop, start) = plan_sync(&map(vec![account(Some(993))]), &map(vec![account(Some(993))]));
        assert!(stop.is_empty() && start.is_empty());
        // 端口变了：先停后启。
        let (stop, start) = plan_sync(&map(vec![account(Some(993))]), &map(vec![account(Some(143))]));
        assert_eq!(stop.len(), 1);
        assert_eq!(start.len(), 1);
        assert_eq!(start[0].port(), 143);
        // 账号被删除：只停不启。
        let (stop, start) = plan_sync(&map(vec![account(None)]), &map(vec![]));
        assert_eq!(stop.len(), 1);
        assert!(start.is_empty());
        // 新增账号：只启不停。
        let (stop, start) = plan_sync(&map(vec![]), &map(vec![account(None)]));
        assert!(stop.is_empty());
        assert_eq!(start.len(), 1);
    }

    #[test]
    fn idle_without_push_returns_for_a_fallback_check() {
        use std::io::{BufRead,Write};
        for push in [true,false] {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move||{
            let (mut socket,_)=listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(14))).unwrap();
            let mut reader=std::io::BufReader::new(socket.try_clone().unwrap());
            write!(socket,"* OK ready\r\n").unwrap();
            let mut line=String::new();reader.read_line(&mut line).unwrap();
            write!(socket,"{} OK logged in\r\n",line.split_whitespace().next().unwrap()).unwrap();
            line.clear();reader.read_line(&mut line).unwrap();
            let tag=line.split_whitespace().next().unwrap().to_string();
            assert!(line.contains("IDLE"));write!(socket,"+ idling\r\n").unwrap();
            if push {write!(socket,"* 1 EXISTS\r\n").unwrap();}
            line.clear();
            if reader.read_line(&mut line).is_err(){return false;}
            assert_eq!(line,"DONE\r\n");
            write!(socket,"{tag} OK idle ended\r\n").unwrap();true
        });
        let mut client=imap::Client::new(std::net::TcpStream::connect(address).unwrap());
        client.read_greeting().unwrap();
        let mut session=client.login("test","test").unwrap();
        let start=std::time::Instant::now();
        let result=wait_for_change(&mut session,Duration::from_secs(10));
        assert!(server.join().unwrap(),"服务端未推送时，必须在 14 秒内主动结束等待并补查");
        result.unwrap();
        if push {assert!(start.elapsed()<Duration::from_secs(2),"有推送时必须立即处理，不能等待检查间隔");}
        }
    }

    fn worker(account: Account, stop: Arc<AtomicBool>, interval: Arc<AtomicU64>, ev: Sender<Event>) {
        let mut first = true;
        let mut last_uid = 0u32;
        let mut validity: Option<u32> = None;
        // 被服务器拒绝过的密码：30 分钟内不再发起登录。域账号连续输错
        // 五次左右会被 AD 锁定（连电脑登录和 VPN 一起锁），所以拒绝后
        // 必须冷却；用户在设置里换了密码后 password != bad，下一轮最多
        // 60 秒就会重试。
        let mut rejected: Option<(String, std::time::Instant)> = None;
        let mut backoff = 30u64;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            emit(&stop, &ev, Event::Status {
                account: account.email.clone(),
                status: Status::Connecting,
            });
            let password = match secrets::load(&account.email) {
                Ok(p) => p,
                Err(e) => {
                    // 本机没存凭据不属于「被拒绝」，用 Failed 避免误导。
                    // 「没有保存授权码」不再重复附上；凭据管理器本身
                    // 不可用（加载库/系统问题）才把细节带给用户。
                    emit(&stop, &ev, Event::Status {
                        account: account.email.clone(),
                        status: Status::Failed(if e.starts_with("凭据管理器不可用") {
                            format!("{e}，请在设置里重新输入")
                        } else {
                            "未保存密码/授权码，请在设置里重新输入".into()
                        }),
                    });
                    if sleep_stop(&stop, 60) {
                        return;
                    }
                    continue;
                }
            };
            if let Some((bad, at)) = &rejected {
                if *bad == password && at.elapsed() < Duration::from_secs(1800) {
                    emit(&stop, &ev, Event::Status {
                        account: account.email.clone(),
                        status: Status::Auth("请在设置里重新输入".into()),
                    });
                    if sleep_stop(&stop, 60) {
                        return;
                    }
                    continue;
                }
            }
            let mut session = match net::connect(&account, &password) {
                Ok(s) => s,
                Err(net::ConnectError::Auth(msg)) => {
                    rejected = Some((password, std::time::Instant::now()));
                    emit(&stop, &ev, Event::Status {
                        account: account.email.clone(),
                        status: Status::Auth(msg),
                    });
                    if sleep_stop(&stop, 60) {
                        return;
                    }
                    continue;
                }
                Err(net::ConnectError::Net(msg)) => {
                    emit(&stop, &ev, Event::Status {
                        account: account.email.clone(),
                        status: Status::Failed(msg),
                    });
                    if sleep_stop(&stop, backoff) {
                        return;
                    }
                    backoff = (backoff * 2).min(300);
                    continue;
                }
            };
            backoff = 30;
            rejected = None;
            let mailbox = match session.select("INBOX") {
                Ok(mb) => mb,
                Err(_) => {
                    emit(&stop, &ev, Event::Status {
                        account: account.email.clone(),
                        status: Status::Failed("无法打开收件箱".into()),
                    });
                    if sleep_stop(&stop, 30) {
                        return;
                    }
                    continue;
                }
            };
            // 状态在 select 成功后才报「已连接」。
            emit(&stop, &ev, Event::Status {
                account: account.email.clone(),
                status: Status::Ready,
            });
            // UIDVALIDITY 变化说明服务器重建了 UID（迁移/重建邮箱），
            // 旧 last_uid 作废：按首次处理重建基线，但不弹新邮件提醒。
            if validity.is_some() && validity != mailbox.uid_validity {
                last_uid = recent(&mut session, &mailbox, &account, &stop, &ev).unwrap_or(0);
            } else if first {
                last_uid = recent(&mut session, &mailbox, &account, &stop, &ev).unwrap_or(0);
                first = false;
            } else if fetch_new(&mut session, &account, &mut last_uid, &stop, &ev).is_err() {
                // 断线期间到达的邮件：重连后立即补收；连接已出错，
                // 直接退避走外层重连，不在坏连接上重试。
                if sleep_stop(&stop, 10) {
                    return;
                }
                continue;
            }
            validity = mailbox.uid_validity;

            let idle_supported=match session.capabilities() {
                Ok(capabilities)=>capabilities.has_str("IDLE"),
                Err(_)=>{if sleep_stop(&stop,10){return;}continue;},
            };
            loop {
                if stop.load(Ordering::Relaxed) {
                    let _ = session.logout();
                    return;
                }
                if idle_supported {
                    // 推送连接出错后重连，不能永久降级为轮询。
                    if wait_for_change(&mut session,Duration::from_secs(interval.load(Ordering::Relaxed))).is_err() { break; }
                } else {
                    if sleep_stop(&stop, interval.load(Ordering::Relaxed)) {
                        let _ = session.logout();
                        return;
                    }
                    if session.noop().is_err() {
                        break;
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    let _ = session.logout();
                    return;
                }
                if fetch_new(&mut session, &account, &mut last_uid, &stop, &ev).is_err() {
                    break;
                }
            }
            if sleep_stop(&stop, 10) {
                return;
            }
        }
    }

    fn raw_of(fetch: &imap::types::Fetch) -> Option<Vec<u8>> {
        let header = fetch.header()?;
        let mut raw = header.to_vec();
        if let Some(text) = fetch.text() {
            raw.extend_from_slice(b"\r\n");
            raw.extend_from_slice(text);
        }
        Some(raw)
    }

    fn item_of(fetch: &imap::types::Fetch, account: &Account) -> Option<Item> {
        let parsed = parse_message(&raw_of(fetch)?);
        let subject = if parsed.subject.is_empty() { "（无主题）".to_string() } else { parsed.subject.clone() };
        Some(Item {
            account: account.email.clone(),
            from: if parsed.from.is_empty() { account.email.clone() } else { parsed.from.clone() },
            subject,
            time: fetch
                .internal_date()
                .map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            code: code_of(&parsed),
            summary: cap_summary(body_of(&parsed)),
            links: links_of(&parsed),
            meeting: meeting_of(&parsed.calendar),
            uid: fetch.uid.unwrap_or(0),
        })
    }

    /// 收取字段：正文只取前 2 MB——带附件的邮件不再整封下载，
    /// 正文和 html 一般排在附件前面，足够解析。
    const FETCH_QUERY: &str = "(UID INTERNALDATE BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.2097152>)";

    /// 首次连接：把最近 20 封读进面板（一条命令批量取），不当作新邮件提示。
    fn recent(
        session: &mut imap::Session<net::Prefixed<net::Stream>>,
        mailbox: &imap::types::Mailbox,
        account: &Account,
        stop: &AtomicBool,
        ev: &Sender<Event>,
    ) -> Result<u32, ()> {
        // 批量拉取失败时用 uid_next-1 兜底：返回 0 会让下一轮
        // UID 1:* 把旧邮件当新邮件弹提醒。
        let fallback = mailbox.uid_next.map(|n| n.saturating_sub(1)).unwrap_or(0);
        let mut max = 0u32;
        if mailbox.exists > 0 {
            let from = mailbox.exists.saturating_sub(19).max(1);
            let Ok(fetched) = session.fetch(format!("{from}:{}", mailbox.exists), FETCH_QUERY) else {
                return Ok(fallback);
            };
            for f in fetched.iter() {
                if let Some(uid) = f.uid {
                    max = max.max(uid);
                }
                if let Some(item) = item_of(f, account) {
                    emit(stop, ev, Event::Mail { item, new: false });
                }
            }
        }
        Ok(max.max(fallback))
    }

    #[test]
    #[ignore = "需要真实 QQ 邮箱及新测试邮件，验证后台监听"]
    fn qq_live_notification_worker() {
        let accounts=load_accounts().into_iter().filter(|a|a.provider==Provider::Qq&&a.enabled).collect::<Vec<_>>();
        assert!(!accounts.is_empty());
        let service=Service::start();service.sync(&accounts,120);
        let start=std::time::Instant::now();
        while start.elapsed()<Duration::from_secs(160) {
            if let Ok(event)=service.ev.recv_timeout(Duration::from_secs(1)) {
                match event {
                    Event::Status{status,..}=>println!("worker_status={status:?}"),
                    Event::Mail{new,item}=>{
                        println!("worker_mail new={new} test_message={}",item.subject.starts_with("窗屿"));
                        if new && item.subject=="窗屿后台监听实测" {
                            service.sync(&[],120);return;
                        }
                    },
                    _=>{}
                }
            }
        }
        service.sync(&[],120);panic!("160 秒内未收到新邮件事件");
    }

    #[test]
    #[ignore="需要本轮已发送的日历邀请和普通日期邮件"]
    fn qq_live_calendar_mail_probe() {
        let account=load_accounts().into_iter().find(|a|a.provider==Provider::Qq&&a.enabled).unwrap();
        let password=secrets::load(&account.email).unwrap();
        let mut session=net::connect(&account,&password).unwrap();let mailbox=session.examine("INBOX").unwrap();
        let (tx,rx)=std::sync::mpsc::channel();recent(&mut session,&mailbox,&account,&AtomicBool::new(false),&tx).unwrap();
        let items=rx.try_iter().filter_map(|ev|if let Event::Mail{item,..}=ev{Some(item)}else{None}).collect::<Vec<_>>();
        let invite=items.iter().find(|m|m.subject=="【窗屿测试】邮件日程邀请：收件箱合并验证").expect("测试邀请未到达");
        assert_eq!(invite.meeting.as_ref().unwrap().uid,"windowisle-mail-test-20261103-1500");
        let plain=items.iter().find(|m|m.subject=="WINDOWISLE-DATE-TEST-20261103").expect("普通日期邮件未到达");
        assert!(plain.meeting.is_none());session.logout().unwrap();
        println!("invitation_recognized=true plain_date_rejected=true");
    }

    /// 显式运行的真实邮箱只读检查；不输出地址、凭据或邮件内容。
    #[test]
    #[ignore = "需要本机已连接的 QQ 邮箱，显式执行只读实测"]
    fn qq_live_read_only_probe() {
        let accounts=load_accounts();
        let account=accounts.iter().find(|a|a.provider==Provider::Qq&&a.enabled).expect("未找到已启用 QQ 邮箱");
        let password=secrets::load(&account.email).expect("读取邮箱凭据失败");
        let mut session=net::connect(account,&password).expect("真实登录失败");
        let mailbox = session.examine("INBOX").expect("只读打开收件箱失败");
        println!("idle_supported={}",session.capabilities().expect("读取能力失败").has_str("IDLE"));
        let before=session.uid_search("UNSEEN").expect("读取未读状态失败");
        let (tx,rx)=std::sync::mpsc::channel();
        let mut last=recent(&mut session,&mailbox,account,&AtomicBool::new(false),&tx).expect("最近邮件读取失败");
        let initial=rx.try_iter().collect::<Vec<_>>();
        assert!(initial.iter().all(|e|matches!(e,Event::Mail{new:false,..})));
        println!("login=ok readonly_inbox=ok recent_parsed={}",initial.len());
        for event in &initial {if let Event::Mail{item,..}=event{
            println!("subject_chars={} sender_chars={} body_bytes={} links={} max_link_chars={}",item.subject.chars().count(),item.from.chars().count(),item.summary.len(),item.links.len(),item.links.iter().map(|l|l.label.chars().count()).max().unwrap_or(0));
        }}
        let received=initial.iter().any(|e|matches!(e,Event::Mail{item,..} if item.subject=="窗屿收信测试"&&item.code.as_deref()==Some("482731")&&!item.links.is_empty()));
        println!("test_message_with_code_and_link={received}");
        fetch_new(&mut session,account,&mut last,&AtomicBool::new(false),&tx).expect("增量收取失败");
        let added=rx.try_iter().count();
        let after=session.uid_search("UNSEEN").expect("检查未读状态失败");
        println!("incremental_fetch=ok new_messages={} unseen_unchanged={}",added,before==after);
        session.logout().expect("退出失败");
    }

    fn fetch_new(
        session: &mut imap::Session<net::Prefixed<net::Stream>>,
        account: &Account,
        last: &mut u32,
        stop: &AtomicBool,
        ev: &Sender<Event>,
    ) -> Result<(), ()> {
        let set = session
            .uid_search(format!("UID {}:*", last.saturating_add(1)))
            .map_err(|_| ())?;
        let mut uids: Vec<u32> = set.into_iter().filter(|u| *u > *last).collect();
        uids.sort_unstable();
        if uids.len() > 5 {
            uids = uids.split_off(uids.len() - 5);
        }
        for uid in uids {
            match session.uid_fetch(uid.to_string(), FETCH_QUERY) {
                Ok(fetched) => {
                    for f in fetched.iter() {
                        if let Some(item) = item_of(f, account) {
                            emit(stop, ev, Event::Mail { item, new: true });
                        }
                    }
                }
                // 服务器明确拒绝这一封（No/Bad）：跳过它，不中断整轮收取。
                Err(imap::Error::No(_) | imap::Error::Bad(_)) => {}
                // 网络/连接问题：交上层重连。
                Err(_) => return Err(()),
            }
            *last = (*last).max(uid);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod service {
    use super::*;

    pub struct Service;

    impl Service {
        pub fn start() -> Self {
            Self
        }
        pub fn sync(&self, _accounts: &[Account], _interval_secs: u32) {}
        pub fn test(&self, _account: Account, _password: String) {}
        pub fn try_recv(&self) -> Option<Event> {
            None
        }
    }
}

pub use service::Service;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_multipart_still_yields_text() {
        // 模拟 BODY.PEEK[TEXT]<0.N> 截断：附件带 base64 正文，
        // 截断点落在 base64 中间（贴近真实），multipart 缺结束边界。
        let text_part = "verification code 552014, please confirm.";
        let attachment = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODk=".repeat(8);
        let raw = format!(
            "From: a@b.c\r\nSubject: trunc\r\nContent-Type: multipart/mixed; boundary=BB\r\n\r\n--BB\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text_part}\r\n--BB\r\nContent-Type: application/octet-stream\r\n\r\n{attachment}"
        );
        let raw = raw.as_bytes();
        let cut = raw.len() - attachment.len() / 2;
        let truncated = &raw[..cut];
        let parsed = parse_message(truncated);
        let body = body_of(&parsed);
        assert!(body.contains("552014"), "截断的 multipart 应仍能取出纯文本：{body:?}");
    }

    #[test]
    fn plan_emit_marks_latest_five_and_none_on_first_round() {
        // 顺序 = 从旧到新（末尾是最新的）。
        assert_eq!(plan_emit(7, true), vec![false, false, false, false, false, false, false]);
        assert_eq!(plan_emit(7, false), vec![false, false, true, true, true, true, true]);
        assert_eq!(plan_emit(3, false), vec![true, true, true]);
        assert!(plan_emit(0, false).is_empty());
    }

    #[test]
    fn ole_date_converts_automation_dates() {
        // 2026-01-01 12:00（本地时间）= 46023.5 天。
        let at = ole_date(46023.5).expect("合法日期");
        assert_eq!(at.format("%Y-%m-%d %H:%M").to_string(), "2026-01-01 12:00");
        assert!(ole_date(0.0).is_none());
        assert!(ole_date(-1.0).is_none());
        assert!(ole_date(f64::NAN).is_none());
    }

    #[test]
    fn uid_of_is_stable_and_nonzero() {
        let a = uid_of("AAAA1234");
        let b = uid_of("AAAA1234");
        let c = uid_of("AAAA1235");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a != 0);
    }

    #[test]
    fn outlook_item_extracts_code_and_links_from_html_only_mail() {
        let account = Account {
            email: "me@corp.example".into(),
            provider: Provider::Outlook,
            enabled: true,
            imap_host: None,
            imap_port: None,
            imap_user: None,
            imap_insecure: None,
            imap_starttls: None,
        };
        let item = outlook_item(&account, OutlookMail {
            entry_id: "entry-1".into(),
            subject: "登录验证".into(),
            sender: "系统".into(),
            body: String::new(),
            html: "<html><body>验证码 942613，<a href='https://sso.example.com/login?t=1'>点击登录</a></body></html>".into(),
            received: 46023.5,
            meeting: None,
        });
        assert_eq!(item.account, "me@corp.example");
        assert_eq!(item.from, "系统");
        assert_eq!(item.subject, "登录验证");
        assert_eq!(item.code.as_deref(), Some("942613"), "纯 HTML 邮件应能提出验证码");
        assert_eq!(item.links.len(), 1, "纯 HTML 邮件应能提出链接");
        assert!(item.uid != 0);
        assert_eq!(item.time, "01-01 12:00");
        assert!(item.meeting.is_none());
    }

    #[test]
    fn outlook_account_json_roundtrip_has_no_imap_fields() {
        let account = Account {
            email: "me@corp.example".into(),
            provider: Provider::Outlook,
            enabled: true,
            imap_host: None,
            imap_port: None,
            imap_user: None,
            imap_insecure: None,
            imap_starttls: None,
        };
        let encoded = String::from_utf8(decode_and_check(&account)).expect("utf8");
        assert!(!encoded.contains("imap_"), "Outlook 账号不应写 imap_* 字段：{encoded}");
        assert_eq!(decode_accounts(encoded.as_bytes()), vec![account]);
    }

    fn decode_and_check(account: &Account) -> Vec<u8> {
        encode_accounts(std::slice::from_ref(account))
    }


    #[test]
    fn long_mail_summaries_are_capped_without_splitting_utf8() {
        let huge = "邮件正文".repeat(MAX_SUMMARY_BYTES / 6);
        let capped = cap_summary(huge);
        assert!(capped.len() <= MAX_SUMMARY_BYTES + SUMMARY_TRUNCATED.len());
        assert!(capped.ends_with("已截断）"));
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        let short = "你好，世界".to_string();
        assert_eq!(cap_summary(short.clone()), short);
    }
    #[test]
    fn quality_cancelled_meetings_are_not_active_events() {
        let event="BEGIN:VEVENT\nUID:cancelled\nDTSTART:20261101T060000Z\nSTATUS:CANCELLED\nEND:VEVENT";
        assert!(super::meetings_of(event).is_empty());
        let message=format!("BEGIN:VCALENDAR\nMETHOD:CANCEL\n{}\nEND:VCALENDAR",event.replace("STATUS:CANCELLED\n",""));
        assert!(super::meetings_of(&message).is_empty());
    }

    #[test]
    fn quality_alarm_fields_do_not_replace_meeting_fields() {
        let event="BEGIN:VEVENT\nDTSTART:20261101T060000Z\nBEGIN:VALARM\nSUMMARY:闹钟文案\nEND:VALARM\nSUMMARY:正式会议\nEND:VEVENT";
        assert_eq!(super::meetings_of(event)[0].title,"正式会议");
    }

    #[test]
    fn quality_ics_unfolding_preserves_content_spaces() {
        let event="BEGIN:VEVENT\nDTSTART:20261101T060000Z\nSUMMARY:项目\n  评审\nEND:VEVENT";
        assert_eq!(super::meetings_of(event)[0].title,"项目 评审");
    }
    #[test]
    fn email_links_preserve_targets_labels_and_query_parameters() {
        let parsed=super::parse_message(b"Content-Type: text/html; charset=utf-8\r\n\r\n<a href=\"https://example.com/review?a=1&amp;b=2\"><b>Review</b></a><a href='https://example.com/review?a=1&amp;b=2'>Again</a><a href=javascript:alert(1)>Unsafe</a>");
        assert_eq!(super::links_of(&parsed),vec![super::Link{label:"Review".into(),url:"https://example.com/review?a=1&b=2".into()}]);
        let plain=super::parse_message(b"Content-Type: text/plain\r\n\r\nRead https://example.com/docs. Then https://example.com/help");
        let links=super::links_of(&plain);
        assert_eq!(links.len(),2);assert_eq!(links[0].url,"https://example.com/docs");
        assert!(super::web_url("file:///C:/Windows/notepad.exe").is_none());
        assert!(super::web_url("https://").is_none());
        let parens=super::parse_message(b"Content-Type: text/plain\r\n\r\nhttps://example.com/Function_(mathematics). (https://example.com/help)");
        let targets=super::links_of(&parens);
        assert_eq!(targets[0].url,"https://example.com/Function_(mathematics)");
        assert_eq!(targets[1].url,"https://example.com/help");
    }
    use chrono::TimeZone;

    fn raw(subject: &str, body: &str) -> Vec<u8> {
        format!("From: =?UTF-8?B?5bCP54mb?= <hi@example.com>\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn parses_subject_sender_and_code() {
        let parsed = parse_message(&raw("验证码", "您的验证码是：834921，10分钟内有效。"));
        assert_eq!(parsed.subject, "验证码");
        assert_eq!(parsed.from, "小牛");
        assert_eq!(code_of(&parsed).as_deref(), Some("834921"));
    }

    #[test]
    fn html_only_mail_still_finds_code() {
        let html = "<html><body><p>您正在登录</p><p>验证码：<b>906213</b></p></body></html>";
        let raw = format!("From: a@b.c\r\nSubject: 登录\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}");
        let parsed = parse_message(raw.as_bytes());
        assert_eq!(code_of(&parsed).as_deref(), Some("906213"));
    }

    #[test]
    fn accounts_roundtrip_without_secrets() {
        let items = vec![
            Account { email: "a@qq.com".into(), provider: Provider::Qq, enabled: true, imap_host: None, imap_port: None, imap_user: None, imap_insecure: None, imap_starttls: None },
            Account { email: "b@163.com".into(), provider: Provider::Mail163, enabled: false, imap_host: None, imap_port: None, imap_user: None, imap_insecure: None, imap_starttls: None },
        ];
        let bytes = encode_accounts(&items);
        assert_eq!(decode_accounts(&bytes), items);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("password") && !text.contains("auth"));
        assert_eq!(decode_accounts(b"not json"), Vec::new());
    }

    #[test]
    fn provider_presets_cover_six_kinds() {
        assert_eq!(Provider::ALL.len(), 8);
        assert_eq!(Provider::parse("gmail"), Provider::Gmail);
        assert_eq!(Provider::Gmail.host(), "imap.gmail.com");
        assert_eq!(Provider::Gmail.port(), 993);
        let gmail=Account{email:"demo@gmail.com".into(),provider:Provider::Gmail,enabled:true, imap_host: None, imap_port: None, imap_user: None, imap_insecure: None, imap_starttls: None};
        assert_eq!(decode_accounts(&encode_accounts(&[gmail.clone()])),vec![gmail]);
        assert!(Provider::Mail163.needs_id() && Provider::Mail126.needs_id());
        assert!(!Provider::Qq.needs_id());
        assert_eq!(Provider::parse("exmail").host(), "imap.exmail.qq.com");
        // 自定义邮箱：自定义字段随 JSON 往返，host/port/user 走自定义值。
        assert!(!Provider::Custom.needs_id());
        let custom=Account{
            email:"me@corp.example".into(),provider:Provider::Custom,enabled:true,
            imap_host:Some("mail.corp.example".into()),imap_port:Some(993),
            imap_user:Some("me-login".into()),imap_insecure:Some(true),imap_starttls:None,
        };
        assert_eq!(decode_accounts(&encode_accounts(&[custom.clone()])),vec![custom.clone()]);
        assert_eq!(custom.host(),"mail.corp.example");
        assert_eq!(custom.port(),993);
        assert_eq!(custom.user(),"me-login");
        let no_user=Account{email:"a@b.c".into(),provider:Provider::Custom,enabled:true,
            imap_host:Some("b.c".into()),imap_port:None,imap_user:None,imap_insecure:None,imap_starttls:None};
        assert_eq!(no_user.user(),"a@b.c");
    }

    #[test]
    fn summary_flattens_body_and_caps_length() {
        let parsed = parse_message(&raw("周会", "第一行\n\n  第二行   有空格\r\n第三行"));
        assert_eq!(summary_of(&parsed), "第一行 第二行 有空格 第三行");
        let long = "字".repeat(400);
        let parsed = parse_message(&raw("长", &long));
        let s = summary_of(&parsed);
        assert_eq!(s.chars().count(), 201, "200 字 + 省略号");
        assert!(s.ends_with('…'));
        // 没有正文时摘要为空，界面只显示主题
        assert_eq!(summary_of(&parse_message(&raw("空", ""))), "");
    }

    #[test]
    fn full_body_keeps_paragraphs_and_content_beyond_preview_and_old_fetch_limit() {
        let text=format!("第一段\n\n{}\n\n最后一段：完整正文结束", "正文内容".repeat(10000));
        let parsed=parse_message(&raw("长邮件",&text));
        assert_eq!(body_of(&parsed),text);
        let html=parse_message(b"Content-Type: text/html; charset=utf-8\r\n\r\n<p>First paragraph</p><p>Last paragraph</p>");
        assert_eq!(body_of(&html),"First paragraph\nLast paragraph");
    }

    const ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:demo-1@example.com\r\n\
SUMMARY:产品评审会\r\n\
DTSTART;TZID=Asia/Shanghai:20261030T143000\r\n\
DTEND;TZID=Asia/Shanghai:20261030T153000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn ics_meeting_title_and_start() {
        let m = meeting_of(ICS).expect("应当识别出会议");
        assert_eq!(m.title, "产品评审会");
        let start = chrono::Utc
            .with_ymd_and_hms(2026, 10, 30, 6, 30, 0)
            .earliest()
            .unwrap();
        assert_eq!(m.at, start.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string());
        let expect = start.timestamp();
        assert_eq!(m.start, expect);
        assert_eq!(m.end,Some(expect+3600));
    }

    #[test]
    fn ics_end_time_keeps_its_timezone_and_never_invents_a_duration() {
        let ics="BEGIN:VEVENT\nDTSTART;TZID=Asia/Shanghai:20261030T143000\nDTEND:20261030T073000Z\nEND:VEVENT";
        let meeting=meeting_of(ics).unwrap();
        assert_eq!(meeting.end,Some(meeting.start+3600));
        let ics="BEGIN:VEVENT\nDTSTART:20261030T063000Z\nDTEND;TZID=Asia/Shanghai:20261030T153000\nEND:VEVENT";
        let meeting=meeting_of(ics).unwrap();
        assert_eq!(meeting.end,Some(meeting.start+3600));
        for end in ["","DTEND:20261030T060000Z\n","DTEND:invalid\n","DTEND;TZID=Unknown/Zone:20261030T153000\n"] {
            let meeting=meeting_of(&format!("BEGIN:VEVENT\nDTSTART:20261030T063000Z\n{end}END:VEVENT")).unwrap();
            assert!(meeting.end.is_none(),"缺失或无效的结束时间应保留为空");
        }
    }

    #[test]
    fn ics_utc_and_folded_lines() {
        // 折行：CRLF + 一个空格被删掉，"Design s" + "ync" = "Design sync"
        let utc = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Design s\n ync\nDTSTART:20261030T063000Z\nEND:VEVENT\nEND:VCALENDAR\n";
        let m = meeting_of(utc).expect("Z 时间应能解析");
        assert_eq!(m.title, "Design sync");
        let local = chrono::DateTime::from_timestamp(m.start, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert_eq!(m.at, local);
    }

    #[test]
    fn ics_named_time_zone_uses_its_own_offset() {
        let ics = "BEGIN:VEVENT\nSUMMARY:跨时区会议\nDTSTART;TZID=America/New_York:20261030T090000\nEND:VEVENT\n";
        let m = meeting_of(ics).unwrap();
        assert_eq!(m.start, chrono::DateTime::parse_from_rfc3339("2026-10-30T13:00:00Z").unwrap().timestamp());
        let unknown = "BEGIN:VEVENT\nDTSTART;TZID=Unknown/Zone:20261030T090000\nEND:VEVENT\n";
        assert!(meeting_of(unknown).is_none());
    }

    #[test]
    fn all_day_calendar_event_has_exclusive_end() {
        let event=meeting_of("BEGIN:VEVENT\nUID:all-day\nSUMMARY:全天\nDTSTART;VALUE=DATE:20261103\nDTEND;VALUE=DATE:20261105\nEND:VEVENT").expect("全天日程应可展示");
        assert_eq!(event.end.unwrap()-event.start,2*86400);
    }

    #[test]
    fn no_ics_means_no_meeting() {
        // 普通邮件没有日历部分，不从正文猜测日期。
        let all_day = "BEGIN:VEVENT\nSUMMARY:放假\nDTSTART;VALUE=DATE:20261030\nEND:VEVENT\n";
        assert!(meeting_of(all_day).unwrap().all_day);
        assert!(meeting_of("").is_none());
        assert!(meeting_of("会议时间：10月30日 14:30").is_none());
    }

    #[test]
    fn calendar_part_is_collected_from_mime() {
        let raw = format!(
            "From: a@b.c\r\nSubject: 邀请\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XYZ\r\n\r\n--XYZ\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n会议邀请见附件\r\n--XYZ\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n{ICS}\r\n--XYZ--\r\n"
        );
        let parsed = parse_message(raw.as_bytes());
        assert!(parsed.calendar.contains("产品评审会"));
        let m = meeting_of(&parsed.calendar).unwrap();
        assert_eq!(m.title, "产品评审会");
        assert_eq!(summary_of(&parsed), "会议邀请见附件");
    }
}
