//! 提升当前进程令牌特权（对应 C# 的 SetIncreasePrivilege）。

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_PRIVILEGES_ATTRIBUTES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::{Error, Result};

/// 清理工作集（按进程枚举模式）需要 SeDebugPrivilege。
pub(crate) const SE_DEBUG_PRIVILEGE: &str = "SeDebugPrivilege";
/// 清理系统文件缓存需要 SeIncreaseQuotaPrivilege。
pub(crate) const SE_INCREASE_QUOTA_PRIVILEGE: &str = "SeIncreaseQuotaPrivilege";
/// 清理备用列表/已修改页列表/合并页列表需要 SeProfileSingleProcessPrivilege。
pub(crate) const SE_PROFILE_SINGLE_PROCESS_PRIVILEGE: &str = "SeProfileSingleProcessPrivilege";

/// 在当前进程访问令牌上启用指定特权。
///
/// 注意 AdjustTokenPrivileges 可能返回“成功”但未真正赋权
/// （GetLastError 为 ERROR_NOT_ALL_ASSIGNED），此函数将其视为失败。
pub(crate) fn enable_privilege(name: &str) -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| Error::win32(e, "OpenProcessToken"))?;
        if token.is_invalid() {
            return Err(Error::PrivilegeRequired(
                "无法打开进程令牌（请以管理员身份运行）",
            ));
        }

        let result = (|| -> Result<()> {
            let mut luid = Default::default();
            LookupPrivilegeValueW(None, &HSTRING::from(name), &mut luid)
                .map_err(|e| Error::win32(e, "LookupPrivilegeValueW"))?;

            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(2), // SE_PRIVILEGE_ENABLED
                }],
            };

            AdjustTokenPrivileges(
                token,
                false,
                Some(&privileges as *const TOKEN_PRIVILEGES),
                0,
                None,
                None,
            )
            .map_err(|e| Error::win32(e, "AdjustTokenPrivileges"))?;

            // 成功返回后仍需检查 ERROR_NOT_ALL_ASSIGNED。
            if crate::native::last_win32_error().0 == crate::native::ERROR_NOT_ALL_ASSIGNED as u32
            {
                return Err(Error::PrivilegeRequired(name_static(name)));
            }
            Ok(())
        })();

        let _ = CloseHandle(token);
        result
    }
}

/// 把已知特权名转为 &'static str，用于错误信息。
fn name_static(name: &str) -> &'static str {
    match name {
        SE_DEBUG_PRIVILEGE => SE_DEBUG_PRIVILEGE,
        SE_INCREASE_QUOTA_PRIVILEGE => SE_INCREASE_QUOTA_PRIVILEGE,
        SE_PROFILE_SINGLE_PROCESS_PRIVILEGE => SE_PROFILE_SINGLE_PROCESS_PRIVILEGE,
        _ => "未知特权",
    }
}
