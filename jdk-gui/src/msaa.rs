//! `IAccessible`（MSAA）的最小封装 —— 只用 `jaccessinspector` 需要的 5 个方法。
//!
//! # 为什么手写 COM 而不是引 `windows` crate
//!
//! 需要的只是 `IAccessible` 的 5 个方法，但一个 COM 接口在 Rust 里能用，取决于
//! **三件**事都对：① vtable 的**槽位顺序**（前 7 个是 `IUnknown`(3) + `IDispatch`(4)，
//! `get_accParent` 从 7 开始）；② 每个方法的**签名**（尤其 `VARIANT` 是**按值**传的
//! 24 字节结构体）；③ 引用计数的**归属**（`get_accChild` 给的 `IDispatch` 要 Release，
//! `QueryInterface` 又加了一次引用）。三件里错一件就是访问违例，而不是"返回失败"。
//!
//! # 失败也要能降级
//!
//! `AccessibleObjectFromWindow` 对没有无障碍提供者的窗口（老式自绘窗口、某些控制台）
//! 会返回 `E_FAIL` / `E_NOINTERFACE`。这时 `jaccessinspector` 退回到 Win32 窗口树
//! （`EnumChildWindows`），**并在输出里写明这是 Win32 层，不是 MSAA 层** ——
//! 假装拿到了 MSAA 树比拿不到更糟。

#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::win::*;

/// `IID_IAccessible`
/// = `{618736E0-3C3D-11CF-810C-00AA00389B71}`
const IID_IACCESSIBLE: GUID = GUID {
    data1: 0x6187_36E0,
    data2: 0x3C3D,
    data3: 0x11CF,
    data4: [0x81, 0x0C, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

// vtable 槽位（IUnknown 0..=2, IDispatch 3..=6, IAccessible 7..=27）
const SLOT_QI: usize = 0;
const SLOT_RELEASE: usize = 2;
const SLOT_CHILD_COUNT: usize = 8;
const SLOT_CHILD: usize = 9;
const SLOT_NAME: usize = 10;
const SLOT_VALUE: usize = 11;
const SLOT_ROLE: usize = 13;
const SLOT_STATE: usize = 14;
const SLOT_LOCATION: usize = 22;

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;

type QIFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type ChildCountFn = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
type ChildFn = unsafe extern "system" fn(*mut c_void, VARIANT, *mut *mut c_void) -> HRESULT;
type VariantOutFn = unsafe extern "system" fn(*mut c_void, VARIANT, *mut VARIANT) -> HRESULT;
type BstrFn = unsafe extern "system" fn(*mut c_void, VARIANT, *mut *mut u16) -> HRESULT;
type LocationFn = unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32, *mut i32, VARIANT) -> HRESULT;

fn vtable(this: *mut c_void) -> *mut *mut c_void {
    unsafe { *(this as *mut *mut *mut c_void) }
}

unsafe fn entry(this: *mut c_void, idx: usize) -> *mut c_void {
    *vtable(this).add(idx)
}

fn child_self() -> VARIANT {
    VARIANT { vt: VT_I4, wReserved1: 0, wReserved2: 0, wReserved3: 0, val: [0, 0] }
}

fn child_id(i: i32) -> VARIANT {
    let mut v = child_self();
    v.val[0] = i as u32 as u64; // lVal 是低 32 位
    v
}

/// 持有一次 `IAccessible` 引用。
pub struct Accessible {
    ptr: *mut c_void,
}

// IAccessible 是 COM 接口，跨线程调用由 marshal 决定；这里只在单个线程用。
unsafe impl Send for Accessible {}

impl Accessible {
    /// 从窗口取 `IAccessible`（`OBJID_CLIENT`）。失败返回错误文本。
    pub fn from_window(h: HWND) -> Result<Accessible, String> {
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { AccessibleObjectFromWindow(h, OBJID_CLIENT, &IID_IACCESSIBLE, &mut out) };
        if hr != S_OK || out.is_null() {
            return Err(format!("AccessibleObjectFromWindow 失败 hr=0x{:08X}", hr as u32));
        }
        Ok(Accessible { ptr: out })
    }

    fn call_child_count(&self) -> Option<i32> {
        unsafe {
            let f: ChildCountFn = std::mem::transmute(entry(self.ptr, SLOT_CHILD_COUNT));
            let mut n = 0i32;
            if f(self.ptr, &mut n) == S_OK {
                Some(n)
            } else {
                None
            }
        }
    }

    fn call_child(&self, i: i32) -> Option<(Accessible, bool)> {
        unsafe {
            let f: ChildFn = std::mem::transmute(entry(self.ptr, SLOT_CHILD));
            let mut disp: *mut c_void = std::ptr::null_mut();
            let hr = f(self.ptr, child_id(i), &mut disp);
            if hr != S_OK || disp.is_null() {
                return Some((Accessible { ptr: std::ptr::null_mut() }, false));
            }
            // IDispatch → IAccessible
            let qi: QIFn = std::mem::transmute(entry(disp, SLOT_QI));
            let mut acc: *mut c_void = std::ptr::null_mut();
            let hr = qi(disp, &IID_IACCESSIBLE, &mut acc);
            let release: ReleaseFn = std::mem::transmute(entry(disp, SLOT_RELEASE));
            release(disp);
            if hr == S_OK && !acc.is_null() {
                Some((Accessible { ptr: acc }, true))
            } else {
                Some((Accessible { ptr: std::ptr::null_mut() }, false))
            }
        }
    }

    fn call_bstr(&self, slot: usize, child: VARIANT) -> Option<String> {
        unsafe {
            let f: BstrFn = std::mem::transmute(entry(self.ptr, slot));
            let mut s: *mut u16 = std::ptr::null_mut();
            if f(self.ptr, child, &mut s) != S_OK || s.is_null() {
                return None;
            }
            let len = SysStringLen(s) as usize;
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
            // BSTR 必须还给 COM 内存管理器
            free_bstr(s);
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
    }

    /// `get_accValue`（编辑框/滚动条这类控件会给出有用信息）
    fn call_value(&self, child: VARIANT) -> Option<String> {
        self.call_bstr(SLOT_VALUE, child)
    }

    fn call_i4(&self, slot: usize, child: VARIANT) -> Option<i32> {
        unsafe {
            let f: VariantOutFn = std::mem::transmute(entry(self.ptr, slot));
            let mut v = child_self();
            let hr = f(self.ptr, child, &mut v);
            if hr != S_OK {
                VariantClear(&mut v);
                return None;
            }
            let out = if v.vt == VT_I4 { Some(v.val[0] as u32 as i32) } else { None };
            VariantClear(&mut v);
            out
        }
    }

    fn call_location(&self, child: VARIANT) -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let f: LocationFn = std::mem::transmute(entry(self.ptr, SLOT_LOCATION));
            let (mut x, mut y, mut w, mut h) = (0i32, 0i32, 0i32, 0i32);
            if f(self.ptr, &mut x, &mut y, &mut w, &mut h, child) == S_OK {
                Some((x, y, w, h))
            } else {
                None
            }
        }
    }

    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }
}

impl Drop for Accessible {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                let f: ReleaseFn = std::mem::transmute(entry(self.ptr, SLOT_RELEASE));
                f(self.ptr);
            }
        }
    }
}

unsafe fn free_bstr(s: *mut u16) {
    // oleaut32 的 SysFreeString 需要声明；这里用一个等价的外部声明避免再引一个模块。
    #[link(name = "oleaut32")]
    extern "system" {
        fn SysFreeString(s: *mut u16);
    }
    SysFreeString(s);
}

/// 树的节点。`source` 说明这条信息是 MSAA 给的还是 Win32 兜底给的。
#[derive(Clone, Debug)]
pub struct Node {
    pub role: i32,
    pub name: String,
    pub value: String,
    pub state: i32,
    pub rect: RECT,
    pub children: Vec<Node>,
    /// 这条节点的子节点数是否被截断（超上限）
    pub truncated: bool,
}

impl Node {
    pub fn role_name(&self) -> &'static str {
        role_name(self.role)
    }

    pub fn state_text(&self) -> String {
        state_names(self.state)
    }

    /// 缩进后的文本树。
    pub fn render(&self, out: &mut String, depth: usize, max_depth: usize, seq: &mut usize) {
        let indent = "  ".repeat(depth);
        *seq += 1;
        out.push_str(&format!(
            "{}{}. role={}(0x{:X}) name={:?} state=[{}] rect=({},{},{},{}){}\n",
            indent,
            seq,
            self.role_name(),
            self.role,
            self.name,
            self.state_text(),
            self.rect.left,
            self.rect.top,
            self.rect.w(),
            self.rect.h(),
            if self.value.is_empty() { String::new() } else { format!(" value={:?}", self.value) }
        ));
        if depth >= max_depth {
            if !self.children.is_empty() {
                out.push_str(&format!("{}  …（已达 -d 深度上限，{} 个子节点未展开）\n", indent, self.children.len()));
            }
            return;
        }
        for c in &self.children {
            c.render(out, depth + 1, max_depth, seq);
        }
        if self.truncated {
            out.push_str(&format!("{}  …（子节点数超上限，已截断）\n", indent));
        }
    }
}

/// 递归上限：再深的树对人也没用，且 MSAA 的某些实现会给出环状/爆炸式结构。
const MAX_NODES: usize = 3000;
const MAX_DEPTH: usize = 24;

struct Counter {
    n: usize,
}

/// 取一个窗口的 MSAA 树。
pub fn tree(hwnd: HWND) -> Result<Node, String> {
    let root = Accessible::from_window(hwnd)?;
    let mut c = Counter { n: 0 };
    Ok(expand(&root, &mut c, 0))
}

/// 只读一个 `IAccessible` 自身（childID = CHILDID_SELF）的属性，不带子节点。
fn probe_self(acc: &Accessible) -> Node {
    let self_var = child_self();
    let name = acc.call_bstr(SLOT_NAME, self_var).unwrap_or_default();
    let value = acc.call_value(self_var).unwrap_or_default();
    let role = acc.call_i4(SLOT_ROLE, self_var).unwrap_or(0);
    let state = acc.call_i4(SLOT_STATE, self_var).unwrap_or(0);
    let rect = acc
        .call_location(self_var)
        .map(|(x, y, w, h)| RECT::new(x, y, x + w, y + h))
        .unwrap_or_default();
    Node { role, name, value, state, rect, children: Vec::new(), truncated: false }
}

/// 单个窗口的 MSAA 属性（**不含**子节点）。
///
/// 用途：很多老式 Win32 窗口的 MSAA 提供者是 OLEACC 的通用代理，它对
/// `get_accChildCount` 返回了数字、但 `get_accRole(parent, childID=i)` 全部失败
/// （实测：jconsole 的 9 个子节点全是 role=0/name=""）。这时逐个**子窗口**调
/// `AccessibleObjectFromWindow` 反而拿得到真实属性（statictext / pushbutton / pagetab…）。
pub fn window_node(h: HWND) -> Option<Node> {
    let acc = Accessible::from_window(h).ok()?;
    Some(probe_self(&acc))
}

fn expand(acc: &Accessible, counter: &mut Counter, depth: usize) -> Node {
    let mut node = probe_self(acc);
    counter.n += 1;
    if depth >= MAX_DEPTH || counter.n >= MAX_NODES {
        return node;
    }
    let count = acc.call_child_count().unwrap_or(0).clamp(0, 10_000);
    for i in 1..=count {
        if counter.n >= MAX_NODES {
            node.truncated = true;
            break;
        }
        match acc.call_child(i) {
            // 完整子对象：递归
            Some((child, true)) => {
                let sub = expand(&child, counter, depth + 1);
                node.children.push(sub);
            }
            // 简单元素：属性仍从父对象带 childID 查
            _ => {
                let var = child_id(i);
                let name = acc.call_bstr(SLOT_NAME, var).unwrap_or_default();
                let value = acc.call_value(var).unwrap_or_default();
                let role = acc.call_i4(SLOT_ROLE, var).unwrap_or(0);
                let state = acc.call_i4(SLOT_STATE, var).unwrap_or(0);
                let rect = acc
                    .call_location(var)
                    .map(|(x, y, w, h)| RECT::new(x, y, x + w, y + h))
                    .unwrap_or_default();
                counter.n += 1;
                node.children.push(Node {
                    role,
                    name,
                    value,
                    state,
                    rect,
                    children: Vec::new(),
                    truncated: false,
                });
            }
        }
    }
    node
}

/// `ROLE_SYSTEM_*` → 名字（只列常用的，其余给 `role_0xNN`）。
pub fn role_name(r: i32) -> &'static str {
    match r {
        0x1 => "titlebar",
        0x2 => "menubar",
        0x3 => "scrollbar",
        0x4 => "grip",
        0x5 => "sound",
        0x6 => "cursor",
        0x7 => "caret",
        0x8 => "alert",
        0x9 => "window",
        0xA => "client",
        0xB => "menupopup",
        0xC => "menuitem",
        0xD => "tooltip",
        0xE => "application",
        0xF => "document",
        0x10 => "pane",
        0x11 => "chart",
        0x12 => "dialog",
        0x13 => "border",
        0x14 => "grouping",
        0x15 => "separator",
        0x16 => "toolbar",
        0x17 => "statusbar",
        0x18 => "table",
        0x19 => "columnheader",
        0x1A => "rowheader",
        0x1B => "column",
        0x1C => "row",
        0x1D => "cell",
        0x1E => "link",
        0x1F => "helpballoon",
        0x20 => "character",
        0x21 => "list",
        0x22 => "listitem",
        0x23 => "outline",
        0x24 => "outlineitem",
        0x25 => "pagetab",
        0x26 => "propertypage",
        0x27 => "indicator",
        0x28 => "graphic",
        0x29 => "statictext",
        0x2A => "text",
        0x2B => "pushbutton",
        0x2C => "checkbox",
        0x2D => "radiobutton",
        0x2E => "combobox",
        0x2F => "droplist",
        0x30 => "progressbar",
        0x31 => "dial",
        0x32 => "hotkeyfield",
        0x33 => "slider",
        0x34 => "spinbutton",
        0x35 => "diagram",
        0x36 => "animation",
        0x37 => "equation",
        0x38 => "buttondropdown",
        0x39 => "buttonmenu",
        0x3A => "buttondropdowngrid",
        0x3B => "whitespace",
        0x3C => "pagetablist",
        0x3D => "clock",
        0x3E => "splitbutton",
        0x3F => "ipaddress",
        0x40 => "outlinebutton",
        0 => "none",
        _ => "unknown",
    }
}

const STATE_FLAGS: &[(i32, &str)] = &[
    (0x0000_0001, "unavailable"),
    (0x0000_0002, "selected"),
    (0x0000_0004, "focused"),
    (0x0000_0008, "pressed"),
    (0x0000_0010, "checked"),
    (0x0000_0020, "mixed"),
    (0x0000_0040, "readonly"),
    (0x0000_0080, "hottracked"),
    (0x0000_0100, "default"),
    (0x0000_0200, "expanded"),
    (0x0000_0400, "collapsed"),
    (0x0000_0800, "busy"),
    (0x0000_1000, "floating"),
    (0x0000_2000, "marqueed"),
    (0x0000_4000, "animated"),
    (0x0000_8000, "invisible"),
    (0x0001_0000, "offscreen"),
    (0x0002_0000, "sizeable"),
    (0x0004_0000, "moveable"),
    (0x0008_0000, "selfvoicing"),
    (0x0010_0000, "focusable"),
    (0x0020_0000, "selectable"),
    (0x0040_0000, "linked"),
    (0x0080_0000, "traversed"),
    (0x0100_0000, "multiselectable"),
    (0x0200_0000, "extselectable"),
    (0x0400_0000, "alert-low"),
    (0x0800_0000, "alert-medium"),
    (0x1000_0000, "alert-high"),
    (0x2000_0000, "protected"),
    (0x4000_0000, "haspopup"),
];

/// 状态位 → `focused|focusable|…`
pub fn state_names(state: i32) -> String {
    let mut v: Vec<&str> = Vec::new();
    for (bit, name) in STATE_FLAGS {
        if state & *bit != 0 {
            v.push(name);
        }
    }
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join("|")
    }
}

/// 在当前线程初始化 COM（重复调用无害；`RPC_E_CHANGED_MODE` 也当成功）。
pub fn com_init() {
    unsafe {
        let hr = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        let _ = hr;
    }
}

pub fn com_uninit() {
    unsafe { CoUninitialize() };
}

/// 把 MSAA 树和 Win32 列表对照：MSAA 只给了根节点、没有子节点，而 Win32 明明有子窗口
/// 时，说明这个窗口的 MSAA 提供者没实现子节点 —— 此时兜底到 Win32 更有意义。
pub fn looks_thin(n: &Node, win32_children: usize) -> bool {
    n.children.is_empty() && win32_children > 0
}

/// S_FALSE：`IAccessible::get_accChild` 用它表示"这是一个简单元素，不是子对象"。
pub const S_FALSE_HR: HRESULT = S_FALSE;
