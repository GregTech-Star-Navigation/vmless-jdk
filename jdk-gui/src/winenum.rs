//! 窗口枚举 —— `EnumWindows` / `EnumChildWindows` 的薄封装。
//!
//! `jaccesswalker`（按进程列出顶层窗口）与 `jaccessinspector`（窗口控件树）共用这里。
//!
//! # 为什么先"收集句柄"再处理
//!
//! `EnumWindows` 的回调是在枚举过程中**逐个**调用的，回调里再发 `EnumChildWindows`
//! 会重入同一个 `thread_local` 缓冲。所以这里一律两步走：先把句柄抄到 `Vec`（回调只做
//! `push`），枚举结束后再逐个取属性。

use std::cell::RefCell;

use crate::win::*;

/// 单次枚举的句柄上限。桌面级工具不需要更多，超了会拖慢界面。
const MAX_WINDOWS: usize = 4000;

thread_local! {
    static BUF: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn collect_cb(h: HWND, _lp: LPARAM) -> BOOL {
    BUF.with(|b| {
        if let Ok(mut v) = b.try_borrow_mut() {
            if v.len() < MAX_WINDOWS {
                v.push(h);
            }
        }
    });
    TRUE
}

fn drain() -> Vec<HWND> {
    BUF.with(|b| {
        let mut v = b.borrow_mut();
        let out = v.clone();
        v.clear();
        out
    })
}

/// 所有顶层窗口（含不可见、含工具窗口）。
pub fn top_level() -> Vec<HWND> {
    unsafe {
        EnumWindows(Some(collect_cb), 0);
    }
    drain()
}

/// 某个窗口的**全部后代**（`EnumChildWindows` 会递归，不是只看一层）。
pub fn descendants(root: HWND) -> Vec<HWND> {
    if root.is_null() {
        return Vec::new();
    }
    unsafe {
        EnumChildWindows(root, Some(collect_cb), 0);
    }
    drain()
}

/// 一个窗口的可读属性。
#[derive(Clone, Debug)]
pub struct WinInfo {
    pub hwnd: HWND,
    pub pid: u32,
    pub class: String,
    pub title: String,
    pub visible: bool,
    pub rect: RECT,
    /// 相对顶层窗口的层级（根 = 0）
    pub depth: i32,
    /// 父窗口（顶层窗口为 null）
    pub parent: HWND,
}

impl WinInfo {
    pub fn probe(h: HWND) -> WinInfo {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(h, &mut pid);
        }
        let rect = window_rect(h);
        WinInfo {
            hwnd: h,
            pid,
            class: window_class(h),
            title: window_text(h),
            visible: unsafe { IsWindowVisible(h) } != 0,
            rect,
            depth: 0,
            parent: std::ptr::null_mut(),
        }
    }

    /// `0x000A1B2C`
    pub fn hwnd_hex(&self) -> String {
        format!("0x{:08X}", self.hwnd as usize)
    }

    pub fn size_text(&self) -> String {
        format!("{}x{} @ ({},{})", self.rect.w(), self.rect.h(), self.rect.left, self.rect.top)
    }

    /// 一行摘要：`class="标题" 可见 800x600 @ (0,0)`
    pub fn line(&self) -> String {
        let mut s = format!("{}  class={}", self.hwnd_hex(), self.class);
        if !self.title.is_empty() {
            s.push_str(&format!("  title={:?}", self.title));
        }
        s.push_str(if self.visible { "  visible" } else { "  hidden" });
        s.push_str(&format!("  {}", self.size_text()));
        s
    }
}

/// 窗口层级深度：沿 `GetParent` 往上数到顶层。
fn depth_of(h: HWND) -> i32 {
    let mut d = 0;
    let mut cur = h;
    // 防御：坏句柄时 GetParent 可能返回自身，限次退出
    for _ in 0..64 {
        let p = unsafe { GetParent(cur) };
        if p.is_null() || p == cur {
            break;
        }
        cur = p;
        d += 1;
    }
    d
}

/// 一个顶层窗口 + 它的全部后代（已按枚举顺序，`depth` 已填好）。
pub fn window_tree(root: HWND) -> Vec<WinInfo> {
    let mut v = Vec::new();
    let mut top = WinInfo::probe(root);
    top.depth = 0;
    v.push(top);
    for c in descendants(root) {
        let mut w = WinInfo::probe(c);
        w.depth = depth_of(c);
        w.parent = unsafe { GetParent(c) };
        v.push(w);
    }
    v
}

/// 一个进程的窗口集合。
pub struct ProcessGroup {
    pub pid: u32,
    pub image: String,
    pub windows: Vec<WinInfo>,
}

/// 按进程分组枚举顶层窗口。
///
/// `only_pid` 给了就只留那个进程；`with_children` 为真时每个顶层窗口带上后代。
pub fn by_process(only_pid: Option<u32>, with_children: bool) -> Vec<ProcessGroup> {
    let mut groups: Vec<ProcessGroup> = Vec::new();
    for h in top_level() {
        let info = WinInfo::probe(h);
        if info.class == "Progman" || info.class == "Shell_TrayWnd" {
            // 桌面与任务栏：不是"应用的窗口"，默认跳过，避免输出被噪音淹掉
            // （真 jaccesswalker 也会把它们单独归类，这里保持简单）
        }
        if let Some(p) = only_pid {
            if info.pid != p {
                continue;
            }
        }
        let mut list = Vec::new();
        if with_children {
            list = window_tree(h);
        } else {
            list.push(info.clone());
        }
        match groups.iter_mut().find(|g| g.pid == info.pid) {
            Some(g) => g.windows.extend(list),
            None => groups.push(ProcessGroup {
                pid: info.pid,
                image: process_image_name(info.pid),
                windows: list,
            }),
        }
    }
    groups.sort_by(|a, b| a.pid.cmp(&b.pid));
    groups
}

/// 给定 pid / 标题片段 / 句柄，找一个目标窗口（jaccessinspector 用）。
pub fn find_window(pid: Option<u32>, title: Option<&str>, hwnd: Option<usize>) -> Option<HWND> {
    if let Some(h) = hwnd {
        let p = h as HWND;
        if unsafe { IsWindow(p) } != 0 {
            return Some(p);
        }
        return None;
    }
    for h in top_level() {
        let info = WinInfo::probe(h);
        if let Some(p) = pid {
            if info.pid != p {
                continue;
            }
        }
        if let Some(t) = title {
            if !info.title.to_lowercase().contains(&t.to_lowercase()) {
                continue;
            }
        }
        return Some(h);
    }
    None
}
