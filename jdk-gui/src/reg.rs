//! 注册表读写 —— 只服务 `jabswitch`。
//!
//! # 真 `jabswitch` 干的事
//!
//! 它读写的键是
//! `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Accessibility\Configuration`
//! 里的 **`JavaAccessBridge`**（`REG_SZ`，`"1"` = 开、`"0"` = 关）。
//! Windows 的"轻松使用"设置面板读的就是这个值，Java Access Bridge 的
//! `windowsaccessbridge-*.dll` 也据此决定要不要把 AT 接入 JVM。
//!
//! 所以我们不是"模拟"这个开关 —— 改的就是那个真值。

use crate::win::*;

pub const KEY_PATH: &str = "Software\\Microsoft\\Windows NT\\CurrentVersion\\Accessibility\\Configuration";
pub const VALUE_NAME: &str = "JavaAccessBridge";

fn open(sam: u32) -> Result<HKEY, String> {
    let path = wide_z(KEY_PATH);
    let mut key: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, sam, &mut key) };
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "打开注册表项 HKLM\\…\\Accessibility\\Configuration 失败：{}",
            error_text(rc as u32)
        ));
    }
    Ok(key)
}

fn create(sam: u32) -> Result<HKEY, String> {
    let path = wide_z(KEY_PATH);
    let mut key: HKEY = std::ptr::null_mut();
    let mut disp = 0u32;
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null(),
            0,
            sam,
            std::ptr::null_mut(),
            &mut key,
            &mut disp,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "创建注册表项失败（可能需要管理员权限）：{}",
            error_text(rc as u32)
        ));
    }
    Ok(key)
}

/// 查询当前值。`Ok(None)` = 值不存在（不是错误）。
pub fn query() -> Result<Option<String>, String> {
    let key = match open(KEY_READ) {
        Ok(k) => k,
        // 键不存在等价于"值不存在"
        Err(_) => return Ok(None),
    };
    let name = wide_z(VALUE_NAME);
    let mut ty = 0u32;
    let mut len = 0u32;
    let probe = unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut len) };
    if probe == ERROR_FILE_NOT_FOUND {
        unsafe {
            RegCloseKey(key);
        }
        return Ok(None);
    }
    if probe != ERROR_SUCCESS {
        unsafe {
            RegCloseKey(key);
        }
        return Err(format!("查询 JavaAccessBridge 失败：{}", error_text(probe as u32)));
    }
    let mut buf = vec![0u8; len.max(4) as usize];
    let mut got = len;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr(),
            &mut got,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!("读取 JavaAccessBridge 失败：{}", error_text(rc as u32)));
    }
    let bytes = &buf[..got.min(buf.len() as u32) as usize];
    let s = if ty == REG_SZ {
        // UTF-16LE，含结尾 NUL
        let mut u: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        if u.last() == Some(&0) {
            u.pop();
        }
        String::from_utf16_lossy(&u)
    } else {
        // 非 REG_SZ（少见）：按字节转十六进制，别假装能解析
        bytes.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join("")
    };
    Ok(Some(s))
}

/// 写入值（`"1"` / `"0"`）。键不存在时自动创建 —— 真 `jabswitch` 也是这么做的。
pub fn set(value: &str) -> Result<(), String> {
    let key = create(KEY_WRITE).or_else(|_| create(KEY_ALL_ACCESS))?;
    let name = wide_z(VALUE_NAME);
    let mut data: Vec<u16> = value.encode_utf16().collect();
    data.push(0);
    let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
    let rc = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!("写入 JavaAccessBridge 失败：{}", error_text(rc as u32)));
    }
    Ok(())
}
