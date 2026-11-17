//! 进程选择对话框 —— `jconsole` 不带 `-p` 时先弹它。
//!
//! 走 **嵌套消息循环**（真模态）：调用方在 `choose_vm` 里阻塞，直到用户选完或取消。
//! 不用 `PostQuitMessage` 结束，因为那会把外层的消息循环一起关掉 —— 这里用
//! "窗口不存在了" 作为结束条件，`WM_QUIT` 则原样 `PostQuitMessage` 回去。

use std::cell::RefCell;

use vmless_attach::client::Client;

use crate::ui::{self, ctl, Fonts};
use crate::win::*;

const CLASS: &str = "VmlessPickVm";
const ID_LIST: i32 = 2001;
const ID_OK: i32 = 2002;
const ID_REFRESH: i32 = 2003;
const ID_CANCEL: i32 = 2004;
const ID_HINT: i32 = 2005;

struct St {
    list: HWND,
    ok: HWND,
    refresh: HWND,
    cancel: HWND,
    hint: HWND,
    fonts: Option<Fonts>,
    dpi: u32,
    rows: Vec<(u32, String)>,
    children: Vec<HWND>,
}

thread_local! {
    static ST: RefCell<Option<Box<St>>> = const { RefCell::new(None) };
    static DONE: RefCell<bool> = const { RefCell::new(false) };
    static RESULT: RefCell<Option<u32>> = const { RefCell::new(None) };
}

fn with_st<R>(f: impl FnOnce(&mut St) -> R) -> Option<R> {
    ST.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(|b| f(b))))
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = if h.is_null() { 96 } else { dpi_of(h) };
    match msg {
        WM_CREATE => {
            let fonts = Fonts::new(dpi);
            let f = fonts.ui.raw();
            let d = dpi;
            let list = ctl::listview(h, ID_LIST, RECT::new(0, 0, 10, 10));
            ctl::lv_column(list, 0, "PID", scale(d, 70), false);
            ctl::lv_column(list, 1, "主类 / 命令行", scale(d, 330), false);
            ctl::lv_column(list, 2, "运行时长", scale(d, 90), true);
            let ok = ctl::button(h, ID_OK, "连接(&O)", RECT::new(0, 0, 10, 10));
            let refresh = ctl::button(h, ID_REFRESH, "刷新(&R)", RECT::new(0, 0, 10, 10));
            let cancel = ctl::button(h, ID_CANCEL, "取消", RECT::new(0, 0, 10, 10));
            let hint = ctl::label(h, ID_HINT, "选中一个 VMless 虚拟机，双击或点「连接」", RECT::new(0, 0, 10, 10));
            for c in [list, ok, refresh, cancel, hint] {
                ui::set_font(c, f);
            }
            ST.with(|s| {
                if let Ok(mut g) = s.try_borrow_mut() {
                    *g = Some(Box::new(St {
                        list,
                        ok,
                        refresh,
                        cancel,
                        hint,
                        fonts: Some(fonts),
                        dpi: d,
                        rows: Vec::new(),
                        children: vec![list, ok, refresh, cancel, hint],
                    }));
                }
            });
            layout(h);
            reload(h);
            0
        }
        WM_SIZE => {
            with_st(|s| s.dpi = dpi);
            layout(h);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u16;
            if code == 0 {
                match id {
                    ID_OK => accept(h),
                    ID_REFRESH => reload(h),
                    ID_CANCEL => {
                        unsafe {
                            DestroyWindow(h);
                        }
                    }
                    _ => {}
                }
            }
            0
        }
        WM_NOTIFY => {
            let code = ctl::notify_code(lp);
            if code == -3 {
                // NM_DBLCLK
                accept(h);
            }
            0
        }
        WM_DPICHANGED => {
            with_st(|s| {
                s.fonts = Some(Fonts::new(dpi));
                s.dpi = dpi;
            });
            with_st(|s| {
                if let Some(f) = s.fonts.as_ref() {
                    for c in s.children.clone() {
                        ui::set_font(c, f.ui.raw());
                    }
                }
            });
            layout(h);
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = wp as HDC;
            unsafe {
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, ui::C_TEXT);
            }
            // 用类背景刷，避免每帧新建（系统会还给我们的刷子）
            unsafe { GetClassLongPtrW(h, -10 /* GCLP_HBRBACKGROUND */) as LRESULT }
        }
        WM_ERASEBKGND => {
            let hdc = wp as HDC;
            let rc = client_rect(h);
            let b = unsafe { CreateSolidBrush(ui::C_BG) };
            if !b.is_null() {
                unsafe {
                    FillRect(hdc, &rc, b);
                    DeleteObject(b);
                }
            }
            1
        }
        WM_GETMINMAXINFO => {
            let mmi = lp as *mut MINMAXINFO;
            if !mmi.is_null() {
                unsafe {
                    (*mmi).ptMinTrackSize.x = scale(dpi, 520);
                    (*mmi).ptMinTrackSize.y = scale(dpi, 300);
                }
            }
            0
        }
        WM_DESTROY => {
            let kids = with_st(|s| std::mem::take(&mut s.children)).unwrap_or_default();
            ui::destroy_children(&kids);
            ST.with(|s| {
                if let Ok(mut g) = s.try_borrow_mut() {
                    *g = None;
                }
            });
            DONE.with(|d| *d.borrow_mut() = true);
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn layout(h: HWND) {
    let Some((d, kids)) = with_st(|s| (s.dpi, (s.list, s.ok, s.refresh, s.cancel, s.hint))) else {
        return;
    };
    let cr = client_rect(h);
    let (w, hh) = (cr.w(), cr.h());
    let pad = scale(d, 10);
    let bh = scale(d, 30);
    let hint_h = scale(d, 22);
    ui::move_to(kids.0, RECT::new(pad, pad, w - pad, hh - pad - bh - pad - hint_h));
    ui::move_to(kids.4, RECT::new(pad, hh - pad - bh - hint_h, w - pad, hh - pad - bh - pad / 2));
    let bw = scale(d, 92);
    let x0 = w - pad - bw * 3 - pad * 2;
    ui::move_to(kids.1, RECT::new(x0, hh - pad - bh, x0 + bw, hh - pad));
    ui::move_to(kids.2, RECT::new(x0 + bw + pad, hh - pad - bh, x0 + bw * 2 + pad, hh - pad));
    ui::move_to(kids.3, RECT::new(x0 + bw * 2 + pad * 2, hh - pad - bh, x0 + bw * 3 + pad * 2, hh - pad));
}

fn reload(h: HWND) {
    let vms = crate::vm::list_vms();
    let Some(rows) = with_st(|s| {
        s.rows = vms.iter().map(|(p, c, _)| (*p, c.clone())).collect();
        ctl::lv_clear(s.list);
        let mut first = true;
        for (pid, cmd, up) in &vms {
            ctl::lv_row(s.list, &[format!("{}", pid), cmd.clone(), crate::vm::format_duration(*up)]);
            if first {
                ctl::lv_select(s.list, 0);
                first = false;
            }
        }
        vms.len()
    }) else {
        return;
    };
    let text = if rows == 0 {
        "没有可用的 VMless 虚拟机（先启动一个，例如 vmsim.exe）".to_string()
    } else {
        format!("共 {} 个 VMless 虚拟机 —— 双击或点「连接」", rows)
    };
    with_st(|s| ui::set_text(s.hint, &text));
    let _ = h;
}

fn accept(h: HWND) {
    let Some((pid, list)) = with_st(|s| (s.rows.get(ctl::lv_selected(s.list) as usize).map(|r| r.0), s.list)) else {
        return;
    };
    let _ = list;
    let Some(pid) = pid else {
        with_st(|s| ui::set_text(s.hint, "先选中一行"));
        return;
    };
    // 选完再连一次：列表可能是几秒前刷新的，目标可能已经退出
    match Client::attach(pid, 2000) {
        Ok(_) => {
            RESULT.with(|r| *r.borrow_mut() = Some(pid));
            unsafe {
                DestroyWindow(h);
            }
        }
        Err(e) => {
            with_st(|s| ui::set_text(s.hint, &format!("连不上 {}：{}", pid, e)));
        }
    }
}

/// 弹选择框，返回选中的 pid（取消 = `None`）。
pub fn choose_vm() -> Option<u32> {
    DONE.with(|d| *d.borrow_mut() = false);
    RESULT.with(|r| *r.borrow_mut() = None);
    let spec = ui::WindowSpec {
        class: CLASS,
        title: "选择要监控的虚拟机",
        client_w: 620,
        client_h: 380,
        style: WS_OVERLAPPEDWINDOW & !WS_MAXIMIZEBOX & !WS_THICKFRAME,
        ex_style: 0,
        wndproc,
        bg: ui::C_BG,
    };
    let h = match ui::create_main(&spec) {
        Some(h) => h,
        None => {
            eprintln!("错误：无法创建进程选择窗口：{}", last_error_text());
            return None;
        }
    };
    ui::show(h, true);
    // 嵌套循环：窗口没了就结束
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r == 0 {
            // 收到 WM_QUIT：还回去，让外层循环也能退出
            unsafe {
                PostQuitMessage(msg.wParam as i32);
            }
            break;
        }
        if r < 0 {
            break;
        }
        let alive = unsafe { IsWindow(h) } != 0;
        if !alive {
            break;
        }
        unsafe {
            if IsDialogMessageW(h, &mut msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if unsafe { IsWindow(h) } != 0 {
        unsafe {
            DestroyWindow(h);
        }
    }
    RESULT.with(|r| *r.borrow())
}

// `GetClassLongPtrW` 只在这里用来取类背景刷
#[link(name = "user32")]
extern "system" {
    fn GetClassLongPtrW(h: HWND, idx: i32) -> isize;
}
