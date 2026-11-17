//! 平台相关的最小垫片。
//!
//! 只做标准库做不到的两件事：判断 pid 是否存活、把目录标成隐藏。
//! **全部 `unsafe` 都关在这里**，其余模块没有 unsafe。

/// pid 存活判定（Windows）。
///
/// 为什么不用 `tasklist`：`jps` 要枚举上百个 pid，起进程太慢；而且 `OpenProcess` 是
/// 唯一没有副作用的做法（不会像 `kill(pid, 0)` 那样在 unix 上碰到 pid 复用问题之外的麻烦）。
#[cfg(windows)]
pub fn pid_alive_windows(pid: u32) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
    }

    // 不用 unsafe 的替代方案都不满足"零开销 + 无副作用"，所以这里是唯一一处 unsafe。
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

/// 把目录/文件标成隐藏（失败无所谓，纯装饰）。
#[cfg(windows)]
pub fn set_hidden(p: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetFileAttributesW(name: *const u16, attrs: u32) -> i32;
        fn GetFileAttributesW(name: *const u16) -> u32;
    }

    let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    wide.push(0);
    unsafe {
        let cur = GetFileAttributesW(wide.as_ptr());
        if cur == INVALID_FILE_ATTRIBUTES {
            return Err(std::io::Error::last_os_error());
        }
        if cur & FILE_ATTRIBUTE_HIDDEN != 0 {
            return Ok(());
        }
        if SetFileAttributesW(wide.as_ptr(), cur | FILE_ATTRIBUTE_HIDDEN) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 本进程当前打开的句柄数（Windows）。用于"1000 次 attach 不泄漏"的自检。
#[cfg(windows)]
pub fn handle_count() -> Option<u32> {
    use core::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessHandleCount(h: *mut c_void, count: *mut u32) -> i32;
    }
    unsafe {
        let mut n = 0u32;
        if GetProcessHandleCount(GetCurrentProcess(), &mut n) != 0 {
            Some(n)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
pub fn handle_count() -> Option<u32> {
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count();
    Some(n as u32)
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_is_alive() {
        assert!(super::pid_alive_windows(std::process::id()));
    }

    #[test]
    fn bogus_pid_is_dead() {
        // 取一个必然不存在的 pid（Windows 的 pid 上限是 0x3FFFFF）
        assert!(!super::pid_alive_windows(u32::MAX - 1));
    }
}

/// 本地时间（用于 `jinfo` / `VM.system_properties` 里那行 `#<日期>`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub dow: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

#[cfg(windows)]
pub fn local_time() -> Option<LocalTime> {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(st: *mut SystemTime);
    }
    let mut st = SystemTime { year: 0, month: 0, dow: 0, day: 0, hour: 0, minute: 0, second: 0, millis: 0 };
    unsafe { GetLocalTime(&mut st) };
    if st.year == 0 {
        return None;
    }
    Some(LocalTime {
        year: st.year,
        month: st.month as u8,
        day: st.day as u8,
        dow: st.dow as u8,
        hour: st.hour as u8,
        min: st.minute as u8,
        sec: st.second as u8,
    })
}

#[cfg(not(windows))]
pub fn local_time() -> Option<LocalTime> {
    // 没有 libc 就走 UTC（够用了，只是注释行）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    // 1970-01-01 是周四
    let dow = ((days + 4) % 7) as u8;
    let (mut y, mut m, mut d) = (1970i32, 1u8, 1u8);
    let mut left = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let yd = if leap { 366 } else { 365 };
        if left < yd {
            break;
        }
        left -= yd;
        y += 1;
    }
    let ml = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for (i, days_in) in ml.iter().enumerate() {
        let dim = if i == 1 && ((y % 4 == 0 && y % 100 != 0) || y % 400 == 0) { 29 } else { *days_in };
        if left < dim as i64 {
            m = i as u8 + 1;
            d = left as u8 + 1;
            break;
        }
        left -= dim as i64;
    }
    Some(LocalTime {
        year: y as u16,
        month: m,
        day: d,
        dow,
        hour: (rem / 3600) as u8,
        min: ((rem % 3600) / 60) as u8,
        sec: (rem % 60) as u8,
    })
}

/// 时区缩写（`CST`）。取系统时区名的首字母 —— `China Standard Time` → `CST`。
#[cfg(windows)]
pub fn tz_abbrev() -> String {
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 16],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 16],
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(tz: *mut TimeZoneInformation) -> u32;
    }
    let mut tz: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let r = unsafe { GetTimeZoneInformation(&mut tz) };
    let name = if r == 2 { &tz.daylight_name } else { &tz.standard_name };
    let s: String = name.iter().take_while(|c| **c != 0).filter_map(|c| char::from_u32(*c as u32)).collect();
    // 优先用"每个词的首字母"（`China Standard Time` → `CST`，与 Java 显示的一致）
    let initials: String = s
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if initials.len() >= 2 {
        return initials;
    }
    // 本地化时区名（例如「中国标准时间」）取不出缩写 —— 退成 UTC 偏移，形如 `GMT+8`。
    // 这比把整个中文名字塞进那行日期里强得多（Java 那行是 `#Tue Nov 17 ... CST 2026`）。
    let total_min = tz.bias + if r == 2 { tz.daylight_bias } else { tz.standard_bias };
    let east = -total_min; // Windows 的 bias 是"UTC = 本地 + bias"
    let sign = if east < 0 { '-' } else { '+' };
    let hh = east.abs() / 60;
    let mm = east.abs() % 60;
    if mm == 0 {
        format!("GMT{}{}", sign, hh)
    } else {
        format!("GMT{}{}:{:02}", sign, hh, mm)
    }
}

#[cfg(not(windows))]
pub fn tz_abbrev() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if let Some(last) = tz.rsplit('/').next() {
            return last.to_string();
        }
    }
    "UTC".to_string()
}

// ---------------------------------------------------------------------------
// perfdata 的具名共享内存（Windows 特有，但**必须做**）
// ---------------------------------------------------------------------------
//
// 反过来的过程：真 jps/jstat 在 Windows 上不是 mmap 那个文件，而是
// `OpenFileMapping(FILE_MAP_READ, false, "hsperfdata_<user>_<pid>")`
// （实测：这个名字能打开真实 HotSpot 进程的映射，别的候选都是 ERROR_FILE_NOT_FOUND）。
// 所以我们的 VM 必须**用这个具名对象**把 perfdata 暴露出去，否则真工具一律
// 报 `Could not attach to <pid>`。文件本身仍要写 —— 那是给 jps 枚举 pid 用的面包屑。

/// `hsperfdata_<user>_<pid>` —— 与 HotSpot 的命名规则一致。
pub fn perf_shm_name(user: &str, pid: u32) -> String {
    format!("hsperfdata_{}_{}", user, pid)
}

/// 活着的具名共享内存。drop 时关句柄（名字随之消失）。
#[cfg(windows)]
pub struct NamedMapping {
    handle: *mut core::ffi::c_void,
    size: usize,
}

#[cfg(windows)]
impl NamedMapping {
    pub fn size(&self) -> usize {
        self.size
    }
}

#[cfg(windows)]
unsafe impl Send for NamedMapping {}

#[cfg(windows)]
impl Drop for NamedMapping {
    fn drop(&mut self) {
        unsafe {
            CloseHandleInternal(self.handle);
        }
    }
}

#[cfg(windows)]
fn CloseHandleInternal(h: *mut core::ffi::c_void) {
    #[link(name = "kernel32")]
    extern "system" {
        fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
    }
    unsafe {
        CloseHandle(h);
    }
}

/// 用 `CreateFileMapping` 把 `file` 背后的数据暴露成具名共享内存。
///
/// `name` 必须是 [`perf_shm_name`] 的结果，否则真工具找不到。
#[cfg(windows)]
pub fn create_named_mapping(file: &std::fs::File, name: &str, size: usize) -> std::io::Result<NamedMapping> {
    use core::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;

    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingW(
            h: *mut c_void,
            attrs: *mut c_void,
            protect: u32,
            max_high: u32,
            max_low: u32,
            name: *const u16,
        ) -> *mut c_void;
    }

    let mut wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().collect();
    wide.push(0);
    let hi = (size as u64 >> 32) as u32;
    let lo = (size as u64 & 0xffff_ffff) as u32;
    let h = unsafe {
        CreateFileMappingW(file.as_raw_handle() as *mut c_void, std::ptr::null_mut(), PAGE_READWRITE, hi, lo, wide.as_ptr())
    };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(NamedMapping { handle: h, size })
}

/// 非 Windows：unix 上 HotSpot 直接 mmap 文件，不需要具名对象。
#[cfg(not(windows))]
pub struct NamedMapping {
    size: usize,
}

#[cfg(not(windows))]
impl NamedMapping {
    pub fn size(&self) -> usize {
        self.size
    }
}

#[cfg(not(windows))]
pub fn create_named_mapping(_file: &std::fs::File, _name: &str, size: usize) -> std::io::Result<NamedMapping> {
    Ok(NamedMapping { size })
}
