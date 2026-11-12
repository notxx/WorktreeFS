//! 固定安全描述符（spec M0：FS 不支持 ACL，全卷共用一份 SD）。
//!
//! SDDL：`O:<当前用户>G:<当前用户>D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)`
//! —— 受保护 DACL，SYSTEM/管理员/Everyone 完全访问（继承到子对象）。
//! 所有者必须显式为**运行 FS 的用户**：
//!
//! * 缺省（无 O:/G:）→ SD 无 owner，内核创建路径报 "Function not
//!   implemented"（实测）；
//! * `O:SY`（SYSTEM）→ git 的 safe.directory 判定 dubious ownership（实测）。
//! 当前用户 SID 从进程令牌一次性读取；挂载点只服务本机 Tower/git/构建
//! 工具，M0 不做 per-file ACL。

use std::sync::OnceLock;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetSecurityDescriptorLength, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID,
    SID_AND_ATTRIBUTES, TOKEN_QUERY, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 进程内缓存的（自相对）安全描述符字节。
static FIXED_SD: OnceLock<Option<Vec<u8>>> = OnceLock::new();

/// 当前进程令牌的用户 SID 字符串（如 `S-1-5-21-...-1001`）。
fn current_user_sid() -> Option<String> {
    // SAFETY: GetCurrentProcess 伪句柄；输出参数均为合法指针，缓冲手动释放。
    unsafe {
        let mut token = Default::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len)
            .ok()?;
        let user = *(buf.as_ptr() as *const SID_AND_ATTRIBUTES);
        let mut str_sid = PWSTR::null();
        ConvertSidToStringSidW(PSID(user.Sid.0.cast()), &mut str_sid).ok()?;
        let s = str_sid.to_string().ok();
        LocalFree(Some(HLOCAL(str_sid.as_ptr().cast())));
        CloseHandle(token).ok();
        s
    }
}

fn build_sddl() -> Option<String> {
    let sid = current_user_sid()?;
    Some(format!(
        "O:{sid}G:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)"
    ))
}

/// 返回固定安全描述符字节（自相对格式）。转换失败（极少见）时返回 None，
/// 调用方按 `STATUS_INVALID_PARAMETER` 处理。
pub fn fixed_security_descriptor() -> Option<&'static [u8]> {
    FIXED_SD
        .get_or_init(|| {
            let sddl = windows::core::HSTRING::from(build_sddl()?);
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let mut len = 0u32;
            // SAFETY: 参数均为合法输出指针；成功后 sd 指向本地分配的 SD，
            // 取字节副本后 LocalFree。
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut sd,
                    Some(&mut len),
                )
            }
            .ok()?;
            let bytes =
                unsafe { std::slice::from_raw_parts(sd.0 as *const u8, len as usize) }.to_vec();
            // SAFETY: sd 由 SDDL 转换分配（本地句柄），取副本后释放。
            unsafe { LocalFree(Some(HLOCAL(sd.0))) };
            Some(bytes)
        })
        .as_deref()
}

/// SD 字节数（`get_security_by_name` 在无缓冲时也需报告该值）。
pub fn fixed_security_descriptor_len() -> u64 {
    fixed_security_descriptor().map_or(0, |sd| {
        // SAFETY: 指针来自 SDDL 转换，SD 结构长度字段有效。
        let len =
            unsafe { GetSecurityDescriptorLength(PSECURITY_DESCRIPTOR(sd.as_ptr() as *mut _)) };
        len as u64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_sd_converts_once_and_reports_length() {
        let sd = fixed_security_descriptor().expect("SDDL conversion must succeed");
        assert!(!sd.is_empty());
        assert_eq!(sd.len() as u64, fixed_security_descriptor_len());
        // 第二次取同一段缓存
        assert!(std::ptr::eq(sd, fixed_security_descriptor().unwrap()));
        // 当前用户 SID 可解析
        let sid = current_user_sid().expect("current user SID");
        assert!(sid.starts_with("S-1-"));
    }
}
