//! 窗口框架、自绘画布、控件薄封装。
//!
//! 这里只做三件事：
//! 1. 注册窗口类 + 消息循环（[`create_main`] / [`run_loop`]）
//! 2. 双缓冲自绘（[`paint`] + [`Canvas`]）—— 所有"进度条 / 折线 / 表格"都走它，
//!    这样四个工具的观感一致，也不需要在每个窗口里重复处理 `WM_ERASEBKGND` 闪烁
//! 3. 控件封装（[`ctl`]）—— 列表视图 / 标签 / 树的 `SendMessageW` 细节全部收在这里

use std::ffi::c_void;

use crate::win::*;

// ---------------------------------------------------------------------------
// 主题
// ---------------------------------------------------------------------------

/// `COLORREF` 是 `0x00BBGGRR`，**不是** `0x00RRGGBB`。手写十六进制一定会颠倒红蓝，
/// 所以全 crate 只用这个函数产生颜色。
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
}

pub const C_BG: u32 = rgb(0xFA, 0xFA, 0xFA);
pub const C_PANEL: u32 = rgb(0xFF, 0xFF, 0xFF);
pub const C_TEXT: u32 = rgb(0x1E, 0x1E, 0x1E);
pub const C_MUTED: u32 = rgb(0x6B, 0x6B, 0x6B);
pub const C_FAINT: u32 = rgb(0x9A, 0x9A, 0x9A);
pub const C_GRID: u32 = rgb(0xDD, 0xDD, 0xDD);
pub const C_RULE: u32 = rgb(0xE6, 0xE6, 0xE6);
pub const C_TRACK: u32 = rgb(0xEC, 0xEC, 0xEC);
pub const C_HEAP: u32 = rgb(0x3F, 0x7F, 0xC4);
pub const C_HEAP_DARK: u32 = rgb(0x2C, 0x5F, 0x9C);
pub const C_NONHEAP: u32 = rgb(0x5C, 0xA1, 0x4E);
pub const C_META: u32 = rgb(0xD2, 0x92, 0x3E);
pub const C_CCS: u32 = rgb(0x8E, 0x7C, 0xC3);
pub const C_WARN: u32 = rgb(0xC0, 0x39, 0x2B);
pub const C_OK: u32 = rgb(0x2E, 0x8B, 0x57);
pub const C_EDEN: u32 = rgb(0x3F, 0x7F, 0xC4);
pub const C_OLD: u32 = rgb(0xB0, 0x4A, 0x8E);
pub const C_SERIES3: u32 = rgb(0xD2, 0x92, 0x3E);

// ---------------------------------------------------------------------------
// GDI 对象的 RAII 包装 —— 句柄必须 DeleteObject，否则 GDI 对象计数会涨
// ---------------------------------------------------------------------------

pub struct Font(pub HFONT);

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DeleteObject(self.0);
            }
        }
    }
}

impl Font {
    pub fn raw(&self) -> HFONT {
        self.0
    }
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

/// 造字体。`pt` 是磅值（UI 常规字 9pt，标题 11pt，等宽 9pt）。
pub fn make_font(dpi: u32, pt: i32, weight: i32, mono: bool) -> Font {
    let height = -((pt as i64 * dpi as i64) / 72) as i32;
    let face = if mono { wide_z("Consolas") } else { wide_z("Segoe UI") };
    let h = unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH,
            face.as_ptr(),
        )
    };
    Font(h)
}

/// 一组字体（尺寸变了就整组重建）。
pub struct Fonts {
    pub ui: Font,
    pub ui_small: Font,
    pub ui_bold: Font,
    pub ui_title: Font,
    pub mono: Font,
}

impl Fonts {
    pub fn new(dpi: u32) -> Fonts {
        Fonts {
            ui: make_font(dpi, 9, FW_NORMAL, false),
            ui_small: make_font(dpi, 8, FW_NORMAL, false),
            ui_bold: make_font(dpi, 9, FW_SEMIBOLD, false),
            ui_title: make_font(dpi, 12, FW_SEMIBOLD, false),
            mono: make_font(dpi, 9, FW_NORMAL, true),
        }
    }
}

// ---------------------------------------------------------------------------
// 画布（内存 DC 上的双缓冲）
// ---------------------------------------------------------------------------

pub struct Canvas {
    hdc: HDC,
    pub rc: RECT,
    font: HFONT,
}

impl Canvas {
    pub fn hdc(&self) -> HDC {
        self.hdc
    }

    fn use_font(&mut self, f: HFONT) -> HFONT {
        if f.is_null() || f == self.font {
            return self.font;
        }
        let _old = unsafe { SelectObject(self.hdc, f) };
        let prev = self.font;
        self.font = f;
        prev
    }

    fn set_font(&mut self, f: HFONT) -> HFONT {
        let prev = self.use_font(f);
        prev
    }

    pub fn fill(&self, r: RECT, color: u32) {
        let b = unsafe { CreateSolidBrush(color) };
        if b.is_null() {
            return;
        }
        unsafe {
            FillRect(self.hdc, &r, b);
            DeleteObject(b);
        }
    }

    /// 1px 描边（向内画，落在矩形内部，不会溢出到相邻区域）。
    pub fn frame(&self, r: RECT, color: u32) {
        if r.is_empty() {
            return;
        }
        let b = unsafe { CreateSolidBrush(color) };
        if b.is_null() {
            return;
        }
        unsafe {
            let one = RECT::new(r.left, r.top, r.right, r.top + 1);
            FillRect(self.hdc, &one, b);
            let one = RECT::new(r.left, r.bottom - 1, r.right, r.bottom);
            FillRect(self.hdc, &one, b);
            let one = RECT::new(r.left, r.top, r.left + 1, r.bottom);
            FillRect(self.hdc, &one, b);
            let one = RECT::new(r.right - 1, r.top, r.right, r.bottom);
            FillRect(self.hdc, &one, b);
            DeleteObject(b);
        }
    }

    pub fn hline(&self, x1: i32, x2: i32, y: i32, color: u32) {
        self.fill(RECT::new(x1, y, x2, y + 1), color);
    }

    pub fn vline(&self, x: i32, y1: i32, y2: i32, color: u32) {
        self.fill(RECT::new(x, y1, x + 1, y2), color);
    }

    /// 圆角填充（进度条的槽与填充都用它）。
    pub fn round_fill(&self, r: RECT, color: u32, radius: i32) {
        if r.is_empty() {
            return;
        }
        let pen = unsafe { CreatePen(PS_SOLID, 1, color) };
        let brush = unsafe { CreateSolidBrush(color) };
        if pen.is_null() || brush.is_null() {
            unsafe {
                if !pen.is_null() {
                    DeleteObject(pen);
                }
                if !brush.is_null() {
                    DeleteObject(brush);
                }
            }
            return;
        }
        unsafe {
            let op = SelectObject(self.hdc, pen);
            let ob = SelectObject(self.hdc, brush);
            RoundRect(self.hdc, r.left, r.top, r.right, r.bottom, radius * 2, radius * 2);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, op);
            DeleteObject(pen);
            DeleteObject(brush);
        }
    }

    /// 折线。`pts` 少于 2 个点直接返回。
    pub fn polyline(&self, pts: &[POINT], color: u32, width: i32) {
        if pts.len() < 2 {
            return;
        }
        let pen = unsafe { CreatePen(PS_SOLID, width.max(1), color) };
        if pen.is_null() {
            return;
        }
        unsafe {
            let old = SelectObject(self.hdc, pen);
            Polyline(self.hdc, pts.as_ptr(), pts.len() as i32);
            SelectObject(self.hdc, old);
            DeleteObject(pen);
        }
    }

    /// 文本（左上角定位，`DT_*` 由调用方给）。`r` 会被 `DrawTextW` 就地修改，
    /// 所以传值进来。
    pub fn text(&mut self, r: RECT, s: &str, color: u32, font: HFONT, flags: u32) -> i32 {
        let prev = self.set_font(font);
        let buf = wide_z(s);
        let mut rr = r;
        unsafe {
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, color);
            let h = DrawTextW(self.hdc, buf.as_ptr(), -1, &mut rr, flags);
            if !prev.is_null() {
                SelectObject(self.hdc, prev);
                self.font = prev;
            }
            h
        }
    }

    /// 单行文本（竖直居中于 `r`）。
    pub fn text_line(&mut self, r: RECT, s: &str, color: u32, font: HFONT) -> i32 {
        self.text(r, s, color, font, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX_ | DT_END_ELLIPSIS)
    }

    /// 单行文本，右对齐（数值列）。
    pub fn text_right(&mut self, r: RECT, s: &str, color: u32, font: HFONT) -> i32 {
        self.text(r, s, color, font, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS)
    }

    /// 单行文本，居中。
    pub fn text_center(&mut self, r: RECT, s: &str, color: u32, font: HFONT) -> i32 {
        self.text(r, s, color, font, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS)
    }

    /// 文本宽度（像素）。
    pub fn measure(&mut self, s: &str, font: HFONT) -> i32 {
        let prev = self.set_font(font);
        let buf = wide(s);
        let mut sz = SIZE::default();
        let w = unsafe {
            if GetTextExtentPoint32W(self.hdc, buf.as_ptr(), buf.len() as i32, &mut sz) != 0 {
                sz.cx
            } else {
                0
            }
        };
        if !prev.is_null() {
            unsafe {
                SelectObject(self.hdc, prev);
            }
            self.font = prev;
        }
        w
    }

    /// 进度条：`frac` 夹到 0..=1。
    pub fn bar(&self, r: RECT, frac: f64, color: u32, radius: i32) {
        self.round_fill(r, C_TRACK, radius);
        let f = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
        let w = ((r.w() as f64) * f).round() as i32;
        if w > 2 {
            self.round_fill(RECT::new(r.left, r.top, r.left + w, r.bottom), color, radius);
        }
    }
}

/// `DT_NOPREFIX`：我们显示的是数据（路径、类名），`&` 不该被吃掉。
const DT_NOPREFIX_: u32 = 0x0000_0800;

/// 自绘：`f` 在内存 DC 上画，画完一次 `BitBlt` 上屏（不会闪烁）。
pub fn paint<F: FnOnce(&mut Canvas)>(h: HWND, bg: u32, f: F) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(h, &mut ps) };
    if hdc.is_null() {
        return;
    }
    let rc = client_rect(h);
    let (w, hh) = (rc.w().max(1), rc.h().max(1));
    let mem = unsafe { CreateCompatibleDC(hdc) };
    let bmp = if mem.is_null() { std::ptr::null_mut() } else { unsafe { CreateCompatibleBitmap(hdc, w, hh) } };
    if !mem.is_null() && !bmp.is_null() {
        let old = unsafe { SelectObject(mem, bmp) };
        let mut c = Canvas { hdc: mem, rc, font: std::ptr::null_mut() };
        c.fill(rc, bg);
        f(&mut c);
        unsafe {
            BitBlt(hdc, 0, 0, w, hh, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
        }
    } else {
        // 双缓冲失败（内存紧张）时退化成直接画 —— 画面可能闪，但不至于空白
        let mut c = Canvas { hdc, rc, font: std::ptr::null_mut() };
        c.fill(rc, bg);
        f(&mut c);
        unsafe {
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
        }
    }
    unsafe {
        EndPaint(h, &ps);
    }
}

// ---------------------------------------------------------------------------
// 窗口类 / 创建 / 消息循环
// ---------------------------------------------------------------------------

/// 注册窗口类。重复注册（错误 1410）视为成功。
pub fn register_class(name: &str, wndproc: WNDPROC, bg: u32) -> bool {
    let class = wide_z(name);
    let brush = unsafe { CreateSolidBrush(bg) };
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0x0002 | 0x0001, // CS_HREDRAW | CS_VREDRAW
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        hIcon: std::ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: brush,
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    unsafe {
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            let e = GetLastError();
            return e == 1410; // ERROR_CLASS_ALREADY_EXISTS
        }
    }
    true
}

pub struct WindowSpec<'a> {
    pub class: &'a str,
    pub title: &'a str,
    /// 客户区尺寸（不是窗口外框）
    pub client_w: i32,
    pub client_h: i32,
    pub style: u32,
    pub ex_style: u32,
    pub wndproc: WNDPROC,
    pub bg: u32,
}

/// 建主窗口并在屏幕上居中。失败返回 `None`（调用方给明确错误，不要 `unwrap`）。
pub fn create_main(spec: &WindowSpec) -> Option<HWND> {
    if !register_class(spec.class, spec.wndproc, spec.bg) {
        return None;
    }
    let class = wide_z(spec.class);
    let title = wide_z(spec.title);
    // 客户区 → 外框
    let mut r = RECT::new(0, 0, spec.client_w, spec.client_h);
    unsafe {
        AdjustWindowRectEx(&mut r, spec.style, 0, spec.ex_style);
    }
    let (ww, wh) = (r.w(), r.h());
    let wa0 = work_area();
    let x = wa0.left + ((wa0.w() - ww) / 2).max(0);
    let y = wa0.top + ((wa0.h() - wh) / 2).max(0);
    let h = unsafe {
        CreateWindowExW(
            spec.ex_style,
            class.as_ptr(),
            title.as_ptr(),
            spec.style,
            x,
            y,
            ww.max(spec.client_w),
            wh.max(spec.client_h),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        )
    };
    if h.is_null() {
        return None;
    }
    // 建好之后再按真实 DPI 校正一次客户区：`client_w/client_h` 是 96-DPI 设计值，
    // 150% 下窗口必须跟着放大，否则界面挤在半个窗口里（字体已经放大了）。
    let dpi = dpi_of(h);
    let (cw, ch) = (scale(dpi, spec.client_w), scale(dpi, spec.client_h));
    let mut r = RECT::new(0, 0, cw, ch);
    unsafe {
        AdjustWindowRectEx(&mut r, spec.style, 0, spec.ex_style);
    }
    let (ww, wh) = (r.w().max(cw), r.h().max(ch));
    // 在**工作区**里居中，并夹到工作区尺寸：150% 下窗口可能比屏幕还高，
    // 用整屏坐标居中会让标题/状态栏被任务栏盖住。
    let wa = work_area();
    let ww = ww.min(wa.w()).max(scale(96, 240));
    let wh = wh.min(wa.h()).max(scale(96, 160));
    let x = wa.left + ((wa.w() - ww) / 2).max(0);
    let y = wa.top + ((wa.h() - wh) / 2).max(0);
    unsafe {
        SetWindowPos(h, std::ptr::null_mut(), x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    Some(h)
}

/// 标准消息循环。返回进程退出码。
pub fn run_loop() -> i32 {
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r == 0 {
            return msg.wParam as i32;
        }
        if r < 0 {
            return 1;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 建一个子控件。
pub fn child(
    class: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    r: RECT,
    parent: HWND,
    id: i32,
) -> HWND {
    let c = wide_z(class);
    let t = wide_z(text);
    unsafe {
        CreateWindowExW(
            ex_style,
            c.as_ptr(),
            t.as_ptr(),
            style | WS_CHILD,
            r.left,
            r.top,
            r.w().max(1),
            r.h().max(1),
            parent,
            id as usize as HMENU,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        )
    }
}

pub fn set_text(h: HWND, s: &str) {
    if h.is_null() {
        return;
    }
    let b = wide_z(s);
    unsafe {
        SetWindowTextW(h, b.as_ptr());
    }
}

pub fn set_font(h: HWND, f: HFONT) {
    if h.is_null() {
        return;
    }
    unsafe {
        SendMessageW(h, WM_SETFONT, f as usize, 1);
    }
}

pub fn show(h: HWND, visible: bool) {
    if h.is_null() {
        return;
    }
    unsafe {
        ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE });
    }
}

pub fn move_to(h: HWND, r: RECT) {
    if h.is_null() {
        return;
    }
    unsafe {
        MoveWindow(h, r.left, r.top, r.w().max(1), r.h().max(1), 1);
    }
}

pub fn enable(h: HWND, on: bool) {
    if h.is_null() {
        return;
    }
    unsafe {
        EnableWindow(h, if on { TRUE } else { FALSE });
    }
}

/// 初始化公共控件（列表视图/标签/树都必须先做，否则类名不存在）。
pub fn init_common_controls() {
    let c = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_WIN95_CLASSES | ICC_LISTVIEW_CLASSES | ICC_TREEVIEW_CLASSES | ICC_TAB_CLASSES | ICC_STANDARD_CLASSES,
    };
    unsafe {
        InitCommonControlsEx(&c);
    }
}

// ---------------------------------------------------------------------------
// 控件封装
// ---------------------------------------------------------------------------

pub mod ctl {
    use super::*;


    pub fn listview(parent: HWND, id: i32, r: RECT) -> HWND {
        let h = child(
            "SysListView32",
            "",
            WS_VISIBLE | WS_BORDER | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            r,
            parent,
            id,
        );
        if !h.is_null() {
            unsafe {
                SendMessageW(
                    h,
                    LVM_SETEXTENDEDLISTVIEWSTYLE,
                    (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize,
                    (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM,
                );
            }
        }
        h
    }

    pub fn treeview(parent: HWND, id: i32, r: RECT) -> HWND {
        child(
            "SysTreeView32",
            "",
            WS_VISIBLE | WS_BORDER | TVS_HASLINES | TVS_LINESATROOT | TVS_HASBUTTONS | TVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            r,
            parent,
            id,
        )
    }

    pub fn tab(parent: HWND, id: i32, r: RECT) -> HWND {
        child("SysTabControl32", "", WS_VISIBLE | WS_CLIPSIBLINGS, 0, r, parent, id)
    }

    pub fn edit_multiline(parent: HWND, id: i32, r: RECT) -> HWND {
        child(
            "Edit",
            "",
            WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | ES_MULTILINE | ES_AUTOVSCROLL | ES_AUTOHSCROLL | ES_READONLY,
            WS_EX_CLIENTEDGE,
            r,
            parent,
            id,
        )
    }

    pub fn button(parent: HWND, id: i32, text: &str, r: RECT) -> HWND {
        child("Button", text, WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON, 0, r, parent, id)
    }

    pub fn label(parent: HWND, id: i32, text: &str, r: RECT) -> HWND {
        child("Static", text, WS_VISIBLE | SS_LEFT | SS_NOPREFIX, 0, r, parent, id)
    }

    /// 加一列。`width` 是像素。
    pub fn lv_column(h: HWND, idx: i32, title: &str, width: i32, right_align: bool) {
        // 必须是 wide_z：LVCOLUMNW.pszText 由 comctl32 按 NUL 结尾读取，
        // `wide("")` 给出的是空 Vec，as_mut_ptr() 是「非空但悬挂」的指针 → 访问违例。
        let mut buf = wide_z(title);
        let mut c = LVCOLUMNW {
            mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
            fmt: if right_align { LVCFMT_RIGHT } else { LVCFMT_LEFT },
            cx: width,
            pszText: buf.as_mut_ptr(),
            cchText: 0,
            iSubItem: 0,
            iImage: 0,
            iOrder: 0,
            cxMin: 0,
            cxDefault: 0,
            cxIdeal: 0,
        };
        unsafe {
            SendMessageW(h, LVM_INSERTCOLUMNW, idx as usize, &mut c as *mut _ as LPARAM);
        }
    }

    pub fn lv_clear(h: HWND) {
        if h.is_null() {
            return;
        }
        unsafe {
            SendMessageW(h, LVM_DELETEALLITEMS, 0, 0);
        }
    }

    pub fn lv_count(h: HWND) -> i32 {
        unsafe { SendMessageW(h, LVM_GETITEMCOUNT, 0, 0) as i32 }
    }

    /// 追加一行。返回行号（-1 = 失败）。
    pub fn lv_row(h: HWND, cells: &[String]) -> i32 {
        if h.is_null() || cells.is_empty() {
            return -1;
        }
        let mut first = wide_z(&cells[0]);
        let mut item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: lv_count(h),
            iSubItem: 0,
            state: 0,
            stateMask: 0,
            pszText: first.as_mut_ptr(),
            cchTextMax: first.len() as i32,
            iImage: 0,
            lParam: 0,
            iIndent: 0,
            iGroupId: 0,
            cColumns: 0,
            puColumns: std::ptr::null_mut(),
            piColFmt: std::ptr::null_mut(),
            iGroup: 0,
        };
        let idx = unsafe { SendMessageW(h, LVM_INSERTITEMW, 0, &mut item as *mut _ as LPARAM) as i32 };
        if idx < 0 {
            return -1;
        }
        for (i, c) in cells.iter().enumerate().skip(1) {
            let mut buf = wide_z(c);
            let mut sub = LVITEMW {
                mask: LVIF_TEXT,
                iItem: idx,
                iSubItem: i as i32,
                state: 0,
                stateMask: 0,
                pszText: buf.as_mut_ptr(),
                cchTextMax: buf.len() as i32,
                iImage: 0,
                lParam: 0,
                iIndent: 0,
                iGroupId: 0,
                cColumns: 0,
                puColumns: std::ptr::null_mut(),
                piColFmt: std::ptr::null_mut(),
                iGroup: 0,
            };
            unsafe {
                SendMessageW(h, LVM_SETITEMTEXTW, idx as usize, &mut sub as *mut _ as LPARAM);
            }
        }
        idx
    }

    /// 选中的行号（-1 = 没选中）。
    pub fn lv_selected(h: HWND) -> i32 {
        if h.is_null() {
            return -1;
        }
        unsafe { SendMessageW(h, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32 }
    }

    /// 选中第 `idx` 行（`idx < 0` = 全不选）。
    pub fn lv_select(h: HWND, idx: i32) {
        if h.is_null() {
            return;
        }
        let mut item = LVITEMW {
            mask: 0,
            iItem: idx.max(0),
            iSubItem: 0,
            state: if idx >= 0 { LVIS_SELECTED | LVIS_FOCUSED } else { 0 },
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            pszText: std::ptr::null_mut(),
            cchTextMax: 0,
            iImage: 0,
            lParam: 0,
            iIndent: 0,
            iGroupId: 0,
            cColumns: 0,
            puColumns: std::ptr::null_mut(),
            piColFmt: std::ptr::null_mut(),
            iGroup: 0,
        };
        unsafe {
            SendMessageW(h, LVM_SETITEMSTATE, idx.max(0) as usize, &mut item as *mut _ as LPARAM);
        }
    }

    pub fn tab_add(h: HWND, title: &str) -> i32 {
        let mut buf = wide_z(title);
        let mut item = TCITEMW {
            mask: TCIF_TEXT,
            dwState: 0,
            dwStateMask: 0,
            pszText: buf.as_mut_ptr(),
            cchText: 0,
            iImage: 0,
            lParam: 0,
        };
        let n = unsafe { SendMessageW(h, TCM_GETITEMCOUNT, 0, 0) as i32 };
        unsafe { SendMessageW(h, TCM_INSERTITEMW, n as usize, &mut item as *mut _ as LPARAM) as i32 }
    }

    pub fn tab_sel(h: HWND) -> i32 {
        unsafe { SendMessageW(h, TCM_GETCURSEL, 0, 0) as i32 }
    }

    pub fn tab_set_sel(h: HWND, idx: i32) {
        unsafe {
            SendMessageW(h, TCM_SETCURSEL, idx.max(0) as usize, 0);
        }
    }

    /// 标签控件的"页面区"矩形（客户端坐标）。`advance` = 内缩量。
    pub fn tab_display_rect(h: HWND) -> RECT {
        let mut r = client_rect(h);
        unsafe {
            SendMessageW(h, TCM_ADJUSTRECT, 0, &mut r as *mut _ as LPARAM);
        }
        r
    }

    /// 树的插入：`parent` 为 `TVI_ROOT`/`TVI_LAST` 常量。
    pub fn tv_add(h: HWND, parent: *mut c_void, text: &str) -> *mut c_void {
        let mut buf = wide_z(text);
        let item = TVITEMW {
            mask: TVIF_TEXT,
            hItem: std::ptr::null_mut(),
            state: 0,
            stateMask: 0,
            pszText: buf.as_mut_ptr(),
            cchTextMax: 0,
            iImage: 0,
            iSelectedImage: 0,
            cChildren: 0,
            lParam: 0,
        };
        let mut ins = TVINSERTSTRUCTW { hParent: parent, hInsertAfter: TVI_LAST, item };
        unsafe { SendMessageW(h, TVM_INSERTITEMW, 0, &mut ins as *mut _ as LPARAM) as *mut c_void }
    }

    pub fn tv_expand_all(h: HWND, item: *mut c_void) {
        unsafe {
            SendMessageW(h, TVM_EXPAND, TVE_EXPAND as usize, item as LPARAM);
        }
    }

    /// 通知码（`WM_NOTIFY` 的 `lParam` → `NMHDR.code`）。
    pub fn notify_code(lp: LPARAM) -> i32 {
        unsafe {
            let hdr = lp as *const NMHDR;
            if hdr.is_null() {
                return 0;
            }
            (*hdr).code as i32
        }
    }
}

/// 把一个子控件的字体批量设好。
pub fn set_font_all(hs: &[HWND], f: HFONT) {
    for &h in hs {
        set_font(h, f);
    }
}

// ---------------------------------------------------------------------------
// 子树清空（树的"清空全部"只有这一个入口：hItem = TVI_ROOT）
// ---------------------------------------------------------------------------

pub fn tv_clear_all(h: HWND) {
    if h.is_null() {
        return;
    }
    unsafe {
        SendMessageW(h, TVM_DELETEITEM, 0, TVI_ROOT as LPARAM);
    }
}

/// 把窗口提到同级 Z 序的最顶端。
///
/// 标签页模式必须用：`SysTabControl32` 在收到 `TCM_INSERTITEM` 后会把自己提到同级最前，
/// 于是**后创建的页面反而被标签控件盖住** —— 现象是"页面明明 CreateWindow 成功、WM_PAINT 也
/// 跑了、BitBlt 也返回 1，屏幕上却只有标签控件的灰底"。
pub fn bring_to_top(h: HWND) {
    if h.is_null() {
        return;
    }
    unsafe {
        SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// 关闭窗口前先把子控件拆掉 —— 这样 `state` 里那些 GDI `Font` 在本窗体
/// 真正消失之后才析构，不会出现"控件还在用已被 DeleteObject 的字体"。
pub fn destroy_children(hs: &[HWND]) {
    for &h in hs {
        if !h.is_null() && unsafe { IsWindow(h) } != 0 {
            unsafe {
                DestroyWindow(h);
            }
        }
    }
}

/// 树节点文本（`TVM_GETITEMW`）。
pub fn tv_text(h: HWND, item: *mut std::ffi::c_void) -> String {
    if h.is_null() || item.is_null() {
        return String::new();
    }
    let mut buf = vec![0u16; 512];
    let mut it = TVITEMW {
        mask: TVIF_TEXT,
        hItem: item,
        state: 0,
        stateMask: 0,
        pszText: buf.as_mut_ptr(),
        cchTextMax: buf.len() as i32,
        iImage: 0,
        iSelectedImage: 0,
        cChildren: 0,
        lParam: 0,
    };
    unsafe {
        SendMessageW(h, TVM_GETITEMW, 0, &mut it as *mut _ as LPARAM);
    }
    from_wide(&buf)
}
