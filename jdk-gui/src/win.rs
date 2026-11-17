//! Win32 裸 FFI —— 只用 `extern "system"` 手写声明，**不引入任何 windows crate**。
//!
//! # 为什么手写
//!
//! `windows` / `winapi` 这两个 crate 加起来编译时间和依赖树都比本 crate 本体大一个量级，
//! 而这里只用到 user32/gdi32/comctl32/advapi32 的几十个函数。手写的代价是**类型必须和
//! C 完全一致**（尤其 `MSG` / `LVITEMW` 这类结构体的对齐），收益是：零依赖、体积可控、
//! 每个声明都能对着 MSDN 一眼核对。
//!
//! # 约定
//!
//! 1. 一律用 **W（UTF-16）** 版本，`A` 版本不声明 —— 混用会出编码事故。
//! 2. 字符串进出用 [`wide`] / [`wide_z`] / [`from_wide`] 三个函数，别处不出现裸指针。
//! 3. `Win10 1607+` 才有的两个 DPI 函数（`SetProcessDpiAwarenessContext` /
//!    `GetDpiForWindow`）走 **LoadLibrary + GetProcAddress**：静态链接的话在 Win7/Win8
//!    上进程会直接加载失败（"找不到入口点"），那是启动即死，比重试一次便宜不了。

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type HWND = *mut c_void;
pub type HINSTANCE = *mut c_void;
pub type HICON = *mut c_void;
pub type HCURSOR = *mut c_void;
pub type HBRUSH = *mut c_void;
pub type HFONT = *mut c_void;
pub type HDC = *mut c_void;
pub type HBITMAP = *mut c_void;
pub type HPEN = *mut c_void;
pub type HGDIOBJ = *mut c_void;
pub type HMENU = *mut c_void;
pub type HKEY = *mut c_void;
pub type HANDLE = *mut c_void;
pub type HIMAGELIST = *mut c_void;
pub type LRESULT = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type ATOM = u16;
pub type UINT = u32;
pub type DWORD = u32;
pub type BOOL = i32;
pub type HRESULT = i32;
pub type COLORREF = u32;

pub const TRUE: BOOL = 1;
pub const FALSE: BOOL = 0;
pub const NULL_HWND: HWND = std::ptr::null_mut();

// ---------------------------------------------------------------------------
// 结构体（布局必须与 C 一致；注释里写清 Win64 下的偏移，便于核对）
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SIZE {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RECT {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }
    pub fn w(&self) -> i32 {
        self.right - self.left
    }
    pub fn h(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
    /// 内缩（负值即外扩）。
    pub fn inset(&self, dx: i32, dy: i32) -> RECT {
        RECT::new(self.left + dx, self.top + dy, self.right - dx, self.bottom - dy)
    }
}

#[repr(C)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: u32,
    pub pt: POINT,
    pub lPrivate: u32,
}

#[repr(C)]
pub struct PAINTSTRUCT {
    pub hdc: HDC,
    pub fErase: BOOL,
    pub rcPaint: RECT,
    pub fRestore: BOOL,
    pub fIncUpdate: BOOL,
    pub rgbReserved: [u8; 32],
}

pub type WNDPROC = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: u32,
    pub style: u32,
    pub lpfnWndProc: Option<WNDPROC>,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HINSTANCE,
    pub hIcon: HICON,
    pub hCursor: HCURSOR,
    pub hbrBackground: HBRUSH,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
    pub hIconSm: HICON,
}

#[repr(C)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

impl FILETIME {
    pub fn as_u64(&self) -> u64 {
        ((self.dwHighDateTime as u64) << 32) | self.dwLowDateTime as u64
    }
}

#[repr(C)]
pub struct INITCOMMONCONTROLSEX {
    pub dwSize: u32,
    pub dwICC: u32,
}

#[repr(C)]
pub struct NMHDR {
    pub hwndFrom: HWND,
    pub idFrom: usize,
    pub code: u32,
}

#[repr(C)]
pub struct LVCOLUMNW {
    pub mask: u32,
    pub fmt: i32,
    pub cx: i32,
    pub pszText: *mut u16,
    pub cchText: i32,
    pub iSubItem: i32,
    pub iImage: i32,
    pub iOrder: i32,
    pub cxMin: i32,
    pub cxDefault: i32,
    pub cxIdeal: i32,
}

#[repr(C)]
pub struct LVITEMW {
    pub mask: u32,
    pub iItem: i32,
    pub iSubItem: i32,
    pub state: u32,
    pub stateMask: u32,
    pub pszText: *mut u16,
    pub cchTextMax: i32,
    pub iImage: i32,
    pub lParam: LPARAM,
    pub iIndent: i32,
    pub iGroupId: i32,
    pub cColumns: u32,
    pub puColumns: *mut u32,
    pub piColFmt: *mut i32,
    pub iGroup: i32,
}

#[repr(C)]
pub struct NMLISTVIEW {
    pub hdr: NMHDR,
    pub iItem: i32,
    pub iSubItem: i32,
    pub uNewState: u32,
    pub uOldState: u32,
    pub uChanged: u32,
    pub ptAction: POINT,
    pub lParam: LPARAM,
}

#[repr(C)]
pub struct NMTREEVIEWW {
    pub hdr: NMHDR,
    pub action: u32,
    pub itemOld: TVITEMW,
    pub itemNew: TVITEMW,
    pub ptDrag: POINT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TVITEMW {
    pub mask: u32,
    pub hItem: *mut c_void,
    pub state: u32,
    pub stateMask: u32,
    pub pszText: *mut u16,
    pub cchTextMax: i32,
    pub iImage: i32,
    pub iSelectedImage: i32,
    pub cChildren: i32,
    pub lParam: LPARAM,
}

#[repr(C)]
pub struct TVINSERTSTRUCTW {
    pub hParent: *mut c_void,
    pub hInsertAfter: *mut c_void,
    pub item: TVITEMW,
}

#[repr(C)]
pub struct TCITEMW {
    pub mask: u32,
    pub dwState: u32,
    pub dwStateMask: u32,
    pub pszText: *mut u16,
    pub cchText: i32,
    pub iImage: i32,
    pub lParam: LPARAM,
}

#[repr(C)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `VARIANT`（Win64 下 24 字节：8 字节头 + 16 字节联合体）。
///
/// 联合体里我们只关心 `VT_BSTR`（指针）与 `VT_I4`（整数）两种。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VARIANT {
    pub vt: u16,
    pub wReserved1: u16,
    pub wReserved2: u16,
    pub wReserved3: u16,
    pub val: [u64; 2],
}

pub const VT_EMPTY: u16 = 0;
pub const VT_I4: u16 = 3;
pub const VT_BSTR: u16 = 8;
pub const VT_DISPATCH: u16 = 9;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

// 窗口消息
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_KILLFOCUS: u32 = 0x0008;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SHOWWINDOW: u32 = 0x0018;
pub const WM_CTLCOLORSTATIC: u32 = 0x0138;
pub const WM_CTLCOLORBTN: u32 = 0x0135;
pub const WM_GETMINMAXINFO: u32 = 0x0024;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_SETFONT: u32 = 0x0030;
pub const WM_NOTIFY: u32 = 0x004E;
pub const WM_DPICHANGED: u32 = 0x02E0;
pub const WM_APP: u32 = 0x8000;

// 窗口样式
pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const WS_HSCROLL: u32 = 0x0010_0000;
pub const WS_TABSTOP: u32 = 0x0001_0000;
pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
pub const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_THICKFRAME: u32 = 0x0004_0000;
pub const WS_MINIMIZEBOX: u32 = 0x0002_0000;
pub const WS_MAXIMIZEBOX: u32 = 0x0001_0000;
pub const WS_SYSMENU: u32 = 0x0008_0000;
pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;

// 控件样式
pub const SS_LEFT: u32 = 0x0000_0000;
pub const SS_CENTERIMAGE: u32 = 0x0000_0200;
pub const SS_NOPREFIX: u32 = 0x0000_0080;
pub const ES_MULTILINE: u32 = 0x0000_0004;
pub const ES_AUTOVSCROLL: u32 = 0x0000_0040;
pub const ES_AUTOHSCROLL: u32 = 0x0000_0080;
pub const ES_READONLY: u32 = 0x0000_0800;
pub const BS_PUSHBUTTON: u32 = 0x0000_0000;
pub const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
pub const LVS_REPORT: u32 = 0x0000_0001;
pub const LVS_SINGLESEL: u32 = 0x0000_0004;
pub const LVS_SHOWSELALWAYS: u32 = 0x0000_0008;
pub const LVS_NOSORTHEADER: u32 = 0x0000_8000;
pub const TVS_HASLINES: u32 = 0x0000_0002;
pub const TVS_LINESATROOT: u32 = 0x0000_0004;
pub const TVS_HASBUTTONS: u32 = 0x0000_0001;
pub const TVS_SHOWSELALWAYS: u32 = 0x0000_0020;

// DrawText
pub const DT_LEFT: u32 = 0x0000;
pub const DT_CENTER: u32 = 0x0001;
pub const DT_RIGHT: u32 = 0x0002;
pub const DT_VCENTER: u32 = 0x0004;
pub const DT_WORDBREAK: u32 = 0x0010;
pub const DT_SINGLELINE: u32 = 0x0020;
pub const DT_NOCLIP: u32 = 0x0100;
pub const DT_END_ELLIPSIS: u32 = 0x0000_8000;
pub const DT_EXPANDTABS: u32 = 0x0040;

// 模式
pub const TRANSPARENT: i32 = 1;
pub const OPAQUE: i32 = 2;
pub const SRCCOPY: u32 = 0x00CC_0020;
pub const PS_SOLID: i32 = 0;
pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
pub const DEFAULT_CHARSET: u32 = 1;
pub const CLEARTYPE_QUALITY: u32 = 5;
pub const ANTIALIASED_QUALITY: u32 = 4;
pub const OUT_DEFAULT_PRECIS: u32 = 0;
pub const CLIP_DEFAULT_PRECIS: u32 = 0;
pub const DEFAULT_PITCH: u32 = 0;

// ShowWindow
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNORMAL: i32 = 1;
pub const SW_SHOW: i32 = 5;
pub const SW_MINIMIZE: i32 = 6;

// SetWindowPos
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_SHOWWINDOW: u32 = 0x0040;
pub const SWP_HIDEWINDOW: u32 = 0x0080;

// 系统色
pub const COLOR_SCROLLBAR: i32 = 0;
pub const COLOR_BTNFACE: i32 = 15;
pub const COLOR_WINDOW: i32 = 5;
pub const COLOR_WINDOWTEXT: i32 = 8;
pub const COLOR_GRAYTEXT: i32 = 17;
pub const COLOR_HIGHLIGHT: i32 = 13;
pub const COLOR_BTNTEXT: i32 = 18;

pub const GWLP_USERDATA: i32 = -21;
pub const GWL_STYLE: i32 = -16;

pub const SM_CXSCREEN: i32 = 0;
pub const SM_CYSCREEN: i32 = 1;
pub const SPI_GETWORKAREA: u32 = 0x0030;

/// `IDC_ARROW` 是 `MAKEINTRESOURCE(32512)`
pub const IDC_ARROW: *const u16 = 32512 as *const u16;

// 列表视图 / 标签 / 树 消息
pub const LVM_FIRST: u32 = 0x1000;
pub const LVM_DELETEALLITEMS: u32 = LVM_FIRST + 9;
pub const LVM_GETNEXTITEM: u32 = LVM_FIRST + 12;
pub const LVM_INSERTITEMW: u32 = LVM_FIRST + 77;
pub const LVM_INSERTCOLUMNW: u32 = LVM_FIRST + 97;
pub const LVM_SETITEMTEXTW: u32 = LVM_FIRST + 116;
pub const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = LVM_FIRST + 54;
pub const LVM_GETITEMCOUNT: u32 = LVM_FIRST + 4;
pub const LVM_ENSUREVISIBLE: u32 = LVM_FIRST + 19;
pub const LVM_SETITEMSTATE: u32 = LVM_FIRST + 43;

pub const LVIF_TEXT: u32 = 0x0001;
pub const LVIF_PARAM: u32 = 0x0004;
pub const LVCF_FMT: u32 = 0x0001;
pub const LVCF_WIDTH: u32 = 0x0002;
pub const LVCF_TEXT: u32 = 0x0004;
pub const LVCFMT_LEFT: i32 = 0x0000;
pub const LVCFMT_RIGHT: i32 = 0x0001;
pub const LVNI_SELECTED: i32 = 0x0002;
pub const LVIS_SELECTED: u32 = 0x0002;
pub const LVIS_FOCUSED: u32 = 0x0001;
pub const LVS_EX_FULLROWSELECT: u32 = 0x0000_0020;
pub const LVS_EX_DOUBLEBUFFER: u32 = 0x0001_0000;
pub const LVS_EX_GRIDLINES: u32 = 0x0000_0001;

// 通知码（UINT 比较时按 i32 转）
pub const LVN_FIRST: i32 = -100;
pub const LVN_ITEMCHANGED: i32 = LVN_FIRST - 1;
pub const LVN_COLUMNCLICK: i32 = LVN_FIRST - 8;
pub const LVN_ITEMACTIVATE: i32 = LVN_FIRST - 14;
pub const TVN_FIRST: i32 = -1100;
pub const TVN_SELCHANGEDW: i32 = TVN_FIRST - 51;
pub const TVN_SELCHANGEDA: i32 = TVN_FIRST - 2;

pub const TCM_FIRST: u32 = 0x1300;
pub const TCM_GETCURSEL: u32 = TCM_FIRST + 11;
pub const TCM_GETITEMCOUNT: u32 = TCM_FIRST + 4;
pub const TCM_INSERTITEMW: u32 = TCM_FIRST + 62;
pub const TCM_GETITEMRECT: u32 = TCM_FIRST + 10;
pub const TCM_ADJUSTRECT: u32 = TCM_FIRST + 40;
pub const TCM_SETCURSEL: u32 = TCM_FIRST + 12;
pub const TCIF_TEXT: u32 = 0x0001;
pub const TCI_SRCCHARSET: i32 = 1;
pub const TCI_SRCCODEPAGE: i32 = 2;

pub const TVIF_TEXT: u32 = 0x0001;
pub const TVIF_PARAM: u32 = 0x0004;
pub const TVIF_CHILDREN: u32 = 0x0040;
pub const TVI_ROOT: *mut c_void = std::ptr::null_mut();
pub const TVI_LAST: *mut c_void = (-0x1000i64) as *mut c_void;
pub const TVM_INSERTITEMW: u32 = 0x1100 + 50;
pub const TVM_DELETEITEM: u32 = 0x1100 + 1;
pub const TVM_EXPAND: u32 = 0x1100 + 2;
pub const TVM_GETNEXTITEM: u32 = 0x1100 + 10;
pub const TVM_SELECTITEM: u32 = 0x1100 + 11;
pub const TVE_EXPAND: u32 = 0x0002;
pub const TVGN_CARET: u32 = 9;
pub const TVGN_ROOT: u32 = 0;

// 注册表
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u64 as usize as HKEY;
pub const KEY_READ: u32 = 0x0002_0019;
pub const KEY_WRITE: u32 = 0x0002_0006;
pub const KEY_ALL_ACCESS: u32 = 0x000F_003F;
pub const REG_SZ: u32 = 1;
pub const REG_BINARY: u32 = 3;
pub const ERROR_SUCCESS: i32 = 0;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

// 进程访问权
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;

// 公共控件
pub const ICC_WIN95_CLASSES: u32 = 0x0000_00FF;
pub const ICC_LISTVIEW_CLASSES: u32 = 0x0000_0001;
pub const ICC_TREEVIEW_CLASSES: u32 = 0x0000_0002;
pub const ICC_TAB_CLASSES: u32 = 0x0000_0008;
pub const ICC_STANDARD_CLASSES: u32 = 0x0000_4000;

// MSAA
pub const OBJID_CLIENT: u32 = 0xFFFF_FFFC;
pub const OBJID_WINDOW: u32 = 0x0000_0000;
pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
pub const RPC_E_CHANGED_MODE: i32 = -2147417850; // 0x80010106

// ---------------------------------------------------------------------------
// 函数声明
// ---------------------------------------------------------------------------

#[link(name = "kernel32")]
extern "system" {
    pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    pub fn GetLastError() -> u32;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    pub fn GetProcessTimes(
        p: HANDLE,
        creation: *mut FILETIME,
        exit: *mut FILETIME,
        kernel: *mut FILETIME,
        user: *mut FILETIME,
    ) -> BOOL;
    pub fn QueryFullProcessImageNameW(p: HANDLE, flags: u32, buf: *mut u16, size: *mut u32) -> BOOL;
    pub fn GetCurrentProcessId() -> u32;
    pub fn Sleep(ms: u32);
    pub fn GetTickCount64() -> u64;
    pub fn LoadLibraryW(name: *const u16) -> HINSTANCE;
    pub fn GetProcAddress(module: HINSTANCE, name: *const u8) -> *mut c_void;
    pub fn IsWow64Process(h: HANDLE, out: *mut BOOL) -> BOOL;
}

#[link(name = "user32")]
extern "system" {
    pub fn RegisterClassExW(c: *const WNDCLASSEXW) -> ATOM;
    pub fn CreateWindowExW(
        exstyle: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HMENU,
        inst: HINSTANCE,
        param: *mut c_void,
    ) -> HWND;
    pub fn DestroyWindow(h: HWND) -> BOOL;
    pub fn DefWindowProcW(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT;
    pub fn GetMessageW(msg: *mut MSG, h: HWND, min: u32, max: u32) -> i32;
    pub fn PeekMessageW(msg: *mut MSG, h: HWND, min: u32, max: u32, remove: u32) -> BOOL;
    pub fn TranslateMessage(msg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(code: i32);
    pub fn PostMessageW(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> BOOL;
    pub fn SetWindowTextW(h: HWND, text: *const u16) -> BOOL;
    pub fn GetWindowTextW(h: HWND, buf: *mut u16, max: i32) -> i32;
    pub fn GetWindowTextLengthW(h: HWND) -> i32;
    pub fn GetClassNameW(h: HWND, buf: *mut u16, max: i32) -> i32;
    pub fn SendMessageW(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT;
    pub fn InvalidateRect(h: HWND, r: *const RECT, erase: BOOL) -> BOOL;
    pub fn UpdateWindow(h: HWND) -> BOOL;
    pub fn BeginPaint(h: HWND, ps: *mut PAINTSTRUCT) -> HDC;
    pub fn EndPaint(h: HWND, ps: *const PAINTSTRUCT) -> BOOL;
    pub fn SetTimer(h: HWND, id: usize, ms: u32, proc: *const c_void) -> usize;
    pub fn KillTimer(h: HWND, id: usize) -> BOOL;
    pub fn LoadCursorW(inst: HINSTANCE, name: *const u16) -> HCURSOR;
    pub fn ShowWindow(h: HWND, cmd: i32) -> BOOL;
    pub fn IsWindow(h: HWND) -> BOOL;
    pub fn IsWindowVisible(h: HWND) -> BOOL;
    pub fn IsWindowEnabled(h: HWND) -> BOOL;
    pub fn EnableWindow(h: HWND, enable: BOOL) -> BOOL;
    pub fn GetClientRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn GetWindowRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn ScreenToClient(h: HWND, p: *mut POINT) -> BOOL;
    pub fn ClientToScreen(h: HWND, p: *mut POINT) -> BOOL;
    pub fn MoveWindow(h: HWND, x: i32, y: i32, w: i32, h: i32, repaint: BOOL) -> BOOL;
    pub fn SetWindowPos(h: HWND, after: HWND, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> BOOL;
    pub fn SetFocus(h: HWND) -> HWND;
    pub fn GetFocus() -> HWND;
    pub fn GetParent(h: HWND) -> HWND;
    pub fn GetDlgCtrlID(h: HWND) -> i32;
    pub fn EnumWindows(proc: Option<unsafe extern "system" fn(HWND, LPARAM) -> BOOL>, lp: LPARAM) -> BOOL;
    pub fn EnumChildWindows(h: HWND, proc: Option<unsafe extern "system" fn(HWND, LPARAM) -> BOOL>, lp: LPARAM) -> BOOL;
    pub fn GetWindowThreadProcessId(h: HWND, pid: *mut u32) -> u32;
    pub fn DrawTextW(hdc: HDC, s: *const u16, n: i32, r: *mut RECT, flags: u32) -> i32;
    pub fn FillRect(hdc: HDC, r: *const RECT, brush: HBRUSH) -> i32;
    pub fn GetSystemMetrics(idx: i32) -> i32;
    pub fn SystemParametersInfoW(action: u32, param: u32, out: *mut c_void, winini: u32) -> BOOL;
    pub fn IsDialogMessageW(h: HWND, msg: *mut MSG) -> BOOL;
    pub fn GetSysColor(idx: i32) -> u32;
    pub fn MessageBeep(kind: u32) -> BOOL;
    pub fn SetProcessDPIAware() -> BOOL;
    pub fn AdjustWindowRectEx(r: *mut RECT, style: u32, menu: BOOL, exstyle: u32) -> BOOL;
}

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateFontW(
        height: i32,
        width: i32,
        esc: i32,
        orient: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike: u32,
        charset: u32,
        out_prec: u32,
        clip_prec: u32,
        quality: u32,
        pitch: u32,
        face: *const u16,
    ) -> HFONT;
    pub fn CreateSolidBrush(color: COLORREF) -> HBRUSH;
    pub fn CreatePen(style: i32, width: i32, color: COLORREF) -> HPEN;
    pub fn SelectObject(hdc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(obj: HGDIOBJ) -> BOOL;
    pub fn TextOutW(hdc: HDC, x: i32, y: i32, s: *const u16, n: i32) -> BOOL;
    pub fn Rectangle(hdc: HDC, l: i32, t: i32, r: i32, b: i32) -> BOOL;
    pub fn RoundRect(hdc: HDC, l: i32, t: i32, r: i32, b: i32, ew: i32, eh: i32) -> BOOL;
    pub fn MoveToEx(hdc: HDC, x: i32, y: i32, old: *mut POINT) -> BOOL;
    pub fn LineTo(hdc: HDC, x: i32, y: i32) -> BOOL;
    pub fn Polyline(hdc: HDC, pts: *const POINT, n: i32) -> BOOL;
    pub fn SetBkMode(hdc: HDC, mode: i32) -> i32;
    pub fn SetTextColor(hdc: HDC, c: COLORREF) -> COLORREF;
    pub fn SetBkColor(hdc: HDC, c: COLORREF) -> COLORREF;
    pub fn GetTextExtentPoint32W(hdc: HDC, s: *const u16, n: i32, out: *mut SIZE) -> BOOL;
    pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub fn CreateCompatibleBitmap(hdc: HDC, w: i32, h: i32) -> HBITMAP;
    pub fn BitBlt(dst: HDC, x: i32, y: i32, w: i32, h: i32, src: HDC, sx: i32, sy: i32, rop: u32) -> BOOL;
    pub fn DeleteDC(hdc: HDC) -> BOOL;
    pub fn SaveDC(hdc: HDC) -> i32;
    pub fn RestoreDC(hdc: HDC, saved: i32) -> BOOL;
    pub fn SetTextAlign(hdc: HDC, align: u32) -> u32;
}

#[link(name = "comctl32")]
extern "system" {
    pub fn InitCommonControlsEx(c: *const INITCOMMONCONTROLSEX) -> BOOL;
}

#[link(name = "advapi32")]
extern "system" {
    pub fn RegOpenKeyExW(key: HKEY, sub: *const u16, opts: u32, sam: u32, out: *mut HKEY) -> i32;
    pub fn RegCreateKeyExW(
        key: HKEY,
        sub: *const u16,
        reserved: u32,
        class: *const u16,
        opts: u32,
        sam: u32,
        sa: *mut c_void,
        out: *mut HKEY,
        disposition: *mut u32,
    ) -> i32;
    pub fn RegSetValueExW(key: HKEY, name: *const u16, reserved: u32, ty: u32, data: *const u8, len: u32) -> i32;
    pub fn RegQueryValueExW(
        key: HKEY,
        name: *const u16,
        reserved: *mut u32,
        ty: *mut u32,
        data: *mut u8,
        len: *mut u32,
    ) -> i32;
    pub fn RegDeleteValueW(key: HKEY, name: *const u16) -> i32;
    pub fn RegCloseKey(key: HKEY) -> i32;
}

#[link(name = "oleacc")]
extern "system" {
    pub fn AccessibleObjectFromWindow(hwnd: HWND, id: u32, riid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    pub fn AccessibleChildren(pacc: *mut c_void, start: i32, got: *mut i32, out: *mut VARIANT) -> HRESULT;
}

#[link(name = "ole32")]
extern "system" {
    pub fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> HRESULT;
    pub fn CoUninitialize();
}

#[link(name = "oleaut32")]
extern "system" {
    pub fn VariantClear(v: *mut VARIANT) -> HRESULT;
    pub fn SysStringLen(s: *const u16) -> u32;
}

// ---------------------------------------------------------------------------
// 字符串辅助 —— 全 crate 只有这三处能把 &str 变成 Win32 指针
// ---------------------------------------------------------------------------

/// `&str` → UTF-16（**不含**结尾 NUL）。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `&str` → UTF-16 + 结尾 NUL（给 Win32 API 用）。
pub fn wide_z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// UTF-16 缓冲 → `String`（截到第一个 NUL）。
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 读窗口标题（不受长度限制，内部先问长度）。
pub fn window_text(h: HWND) -> String {
    let n = unsafe { GetWindowTextLengthW(h) };
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 2];
    let got = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
    if got <= 0 {
        return String::new();
    }
    from_wide(&buf[..got as usize])
}

/// 读窗口类名。
pub fn window_class(h: HWND) -> String {
    let mut buf = vec![0u16; 256];
    let got = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    if got <= 0 {
        return String::new();
    }
    from_wide(&buf[..got as usize])
}

/// 窗口客户区尺寸。
pub fn client_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe { GetClientRect(h, &mut r) };
    r
}

/// 桌面工作区（不含任务栏）。拿不到就退回整屏。
pub fn work_area() -> RECT {
    let mut r = RECT::default();
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut c_void, 0) };
    if ok == 0 || r.is_empty() {
        RECT::new(0, 0, unsafe { GetSystemMetrics(SM_CXSCREEN) }, unsafe { GetSystemMetrics(SM_CYSCREEN) })
    } else {
        r
    }
}

/// 窗口在屏幕上的矩形。
pub fn window_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) };
    r
}

// ---------------------------------------------------------------------------
// DPI（Win10 1607+ 走动态解析，老系统降级）
// ---------------------------------------------------------------------------

type SetDpiCtxFn = unsafe extern "system" fn(i32) -> BOOL;
type GetDpiFn = unsafe extern "system" fn(HWND) -> u32;

fn proc_addr(module: &str, name: &str) -> *mut c_void {
    let dll = wide_z(module);
    let h = unsafe { LoadLibraryW(dll.as_ptr()) };
    if h.is_null() {
        return std::ptr::null_mut();
    }
    let mut n: Vec<u8> = name.bytes().collect();
    n.push(0);
    unsafe { GetProcAddress(h, n.as_ptr()) }
}

/// 进程级 DPI 感知：per-monitor v2 → per-monitor v1 → system aware，逐级降级。
///
/// 必须在**创建任何窗口之前**调用（Windows 只在进程启动/首个窗口时读一次）。
pub fn enable_dpi_awareness() {
    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = (HANDLE)-4
    let f = proc_addr("user32.dll", "SetProcessDpiAwarenessContext");
    if !f.is_null() {
        let f: SetDpiCtxFn = unsafe { std::mem::transmute(f) };
        if unsafe { f(-4) } != 0 {
            return;
        }
        if unsafe { f(-3) } != 0 {
            return; // PER_MONITOR_AWARE_V1
        }
    }
    unsafe {
        let _ = SetProcessDPIAware();
    }
}

/// 窗口所在显示器的 DPI（100% = 96）。拿不到就按 96 走。
pub fn dpi_of(h: HWND) -> u32 {
    if let Some(v) = std::env::var_os("GUI_DPI").and_then(|s| s.to_str().and_then(|t| t.trim().parse::<u32>().ok())) {
        if (72..=480).contains(&v) {
            return v;
        }
    }
    let f = proc_addr("user32.dll", "GetDpiForWindow");
    if !f.is_null() {
        let f: GetDpiFn = unsafe { std::mem::transmute(f) };
        let d = unsafe { f(h) };
        if d >= 72 {
            return d;
        }
    }
    96
}

/// 按 DPI 缩放一个 96-DPI 设计尺寸。
pub fn scale(dpi: u32, v: i32) -> i32 {
    ((v as i64 * dpi as i64) / 96) as i32
}

/// 取系统错误号对应的可读文本（`FormatMessage` 的简版：常见几个就够了）。
pub fn last_error_text() -> String {
    let e = unsafe { GetLastError() };
    error_text(e)
}

pub fn error_text(code: u32) -> String {
    let s = match code {
        0 => "成功",
        2 => "找不到文件",
        3 => "找不到路径",
        5 => "拒绝访问",
        6 => "句柄无效",
        8 => "内存不足",
        13 => "数据无效",
        14 => "存储不足",
        87 => "参数无效",
        1060 => "指定的服务不存在",
        1400 => "句柄无效",
        _ => "",
    };
    if s.is_empty() {
        format!("Win32 错误 {}", code)
    } else {
        format!("{}（Win32 错误 {}）", s, code)
    }
}

/// 进程可执行文件名（拿不到返回空串）；用于窗口按进程分组显示。
pub fn process_image_name(pid: u32) -> String {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if h.is_null() {
        return String::new();
    }
    let mut buf = vec![0u16; 1024];
    let mut n = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) };
    unsafe { CloseHandle(h) };
    if ok == 0 || n == 0 {
        return String::new();
    }
    let full = from_wide(&buf[..n as usize]);
    full.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

/// 目标进程累计 CPU 时间（内核 + 用户，单位 100ns tick）。
pub fn process_cpu_ticks(pid: u32) -> Option<(u64, u64)> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if h.is_null() {
        return None;
    }
    let mut c = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut e = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut k = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut u = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    Some((u.as_u64(), k.as_u64()))
}

pub fn widestring_ptr(v: &[u16]) -> *const u16 {
    v.as_ptr()
}

// ---------------------------------------------------------------------------
// 少量 Default 实现（指针字段不实现 Default，这里显式给零值）
// ---------------------------------------------------------------------------

impl Default for PAINTSTRUCT {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Default for MSG {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Default for WNDCLASSEXW {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Default for INITCOMMONCONTROLSEX {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// 追加：窗口子类化 / 最小尺寸 / 消息框
// ---------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    pub fn SetWindowLongPtrW(h: HWND, idx: i32, val: isize) -> isize;
    pub fn GetWindowLongPtrW(h: HWND, idx: i32) -> isize;
    pub fn MessageBoxW(parent: HWND, text: *const u16, caption: *const u16, ty: u32) -> i32;
    pub fn GetAncestor(h: HWND, flags: u32) -> HWND;
    pub fn GetForegroundWindow() -> HWND;
}

#[repr(C)]
pub struct MINMAXINFO {
    pub ptReserved: POINT,
    pub ptMaxSize: POINT,
    pub ptMaxPosition: POINT,
    pub ptMinTrackSize: POINT,
    pub ptMaxTrackSize: POINT,
}

pub const MB_OK: u32 = 0x0000_0000;
pub const MB_ICONERROR: u32 = 0x0000_0010;
pub const MB_ICONWARNING: u32 = 0x0000_0030;
pub const MB_ICONINFORMATION: u32 = 0x0000_0040;

/// 弹一个确定的错误框（GUI 模式的错误路径不应该只写 stderr —— 双击启动时没人看得到）。
pub fn message_box(title: &str, text: &str, ty: u32) {
    let t = wide_z(text);
    let c = wide_z(title);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), ty | MB_OK);
    }
}

pub const TVM_GETITEMW: u32 = 0x1100 + 62;
