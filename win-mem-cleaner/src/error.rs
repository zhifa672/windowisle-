//! 错误类型。

use std::fmt;

/// 库级错误。
#[derive(Debug, Clone)]
pub enum Error {
    /// Win32 API 调用失败（code 为 GetLastError()，context 为调用点说明）。
    Win32 { code: i32, context: &'static str },
    /// ntdll NtSetSystemInformation 返回非零 NTSTATUS。
    NtStatus(i32),
    /// 需要的提升权限不可用（通常是未以管理员身份运行）。
    PrivilegeRequired(&'static str),
    /// 自动提权（UAC/runas helper）链路失败：用户取消、helper 缺失、
    /// 超时、报告损坏等。
    Elevation(String),
    /// 清理服务（Windows 服务 / 管道）链路问题：未安装、未运行、断开、
    /// 身份校验失败等。
    Service(String),
    /// 当前系统不支持该内存区域。
    Unsupported(&'static str),
    /// 部分目标清理失败（例如个别进程拒绝访问以外的错误）。
    Partial(Vec<String>),
}

impl Error {
    pub(crate) fn win32(e: windows::core::Error, context: &'static str) -> Self {
        // windows crate 把 Win32 错误包装成 FACILITY_WIN32 的 HRESULT
        // （0x8007XXXX），低 16 位才是真实错误码。
        let hresult = e.code().0 as u32;
        let code = if (hresult & 0xFFFF_0000) == 0x8007_0000 {
            (hresult & 0xFFFF) as i32
        } else {
            e.code().0
        };
        Error::Win32 { code, context }
    }

    /// 错误码（Win32 / NTSTATUS）。
    pub fn code(&self) -> Option<i32> {
        match self {
            Error::Win32 { code, .. } => Some(*code),
            Error::NtStatus(code) => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Win32 { code, context } => write!(f, "Win32 错误 {code} ({context})"),
            Error::NtStatus(code) => write!(f, "NTSTATUS 错误 {code:#010x}"),
            Error::PrivilegeRequired(name) => {
                write!(f, "需要提升权限 {name}（请以管理员身份运行）")
            }
            Error::Elevation(msg) => write!(f, "自动提权失败: {msg}"),
            Error::Service(msg) => write!(f, "清理服务不可用: {msg}"),
            Error::Unsupported(area) => write!(f, "当前系统不支持清理 {area}"),
            Error::Partial(errors) => write!(f, "部分目标清理失败: {}", errors.join(" | ")),
        }
    }
}

impl std::error::Error for Error {}

/// 库级 Result。
pub type Result<T> = std::result::Result<T, Error>;
