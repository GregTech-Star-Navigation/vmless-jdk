//! `jconsole`（VMless 版）—— 四个标签页的实时监控窗口。
//!
//! ```text
//! jconsole [-p <pid>] [-i <采样间隔ms>] [-h] [-v]
//! ```
//!
//! # 数据从哪来
//!
//! | 页面 | 数据源 | 频率 |
//! | --- | --- | --- |
//! | 概览 | perfdata 计数器 + OS 进程 CPU 时间 | 1s |
//! | 内存 | perfdata 的 19 个 GC 计数器（与 `jstat -gc` 同源同口径） | 1s |
//! | 线程 | attach 命令 `Thread.print` | 2s |
//! | 类 | attach 通道 `HeapHisto`（等价 `jmap -histo`） | 5s |
//!
//! 采样在**独立线程**里（见 `vm::spawn_worker`），UI 线程只读快照并重绘 ——
//! 目标 VM 卡死时窗口依然能拖动、能关。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

use vmless_attach::client::Client;
use vmless_gui::ui::{self, ctl, Canvas, Fonts};
use vmless_gui::vm::{self, Snapshot, Shared, GC_COLS};
use vmless_gui::win::*;

const CLS_MAIN: &str = "VmlessJConsole";
const CLS_OVERVIEW: &str = "VmlessJConsoleOverview";
const CLS_MEMORY: &str = "VmlessJConsoleMemory";

const WM_REFRESH: u32 = WM_APP + 1;
const TIMER_UI: usize = 1;

const ID_TAB: i32 = 100;
const ID_OVERVIEW: i32 = 101;
const ID_MEMORY: i32 = 102;
const ID_THREADS: i32 = 110;
const ID_STACK: i32 = 111;
const ID_THREAD_HINT: i32 = 112;
const ID_CLASSES: i32 = 120;
const ID_CLASS_HINT: i32 = 121;
const ID_STATUS: i32 = 130;

const TCN_SELCHANGE: i32 = -551;

/// 内存页保留的采样点数（每秒一个 → 一分钟）。
const HIST_LEN: usize = 60;

#[derive(Clone, Copy, Default)]
struct MemPoint {
    eu: f64,
    ou: f64,
    mu: f64,
}

struct App {
    tab: HWND,
    overview: HWND,
    memory: HWND,
    threads: HWND,
    stack: HWND,
    thread_hint: HWND,
    classes: HWND,
    class_hint: HWND,
    status: HWND,
    fonts: Option<Fonts>,
    dpi: u32,
    pid: u32,
    interval_ms: u64,
    shared: Arc<Shared>,
    hist: VecDeque<MemPoint>,
    last_seq: u64,
    sort_col: i32,
    sel_thread: String,
    last_status: String,
    children: Vec<HWND>,
}

thread_local! {
    static APP: RefCell<Option<Box<App>>> = const { RefCell::new(None) };
}

thread_local! {
    static START_TAB: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| {
        let mut b = c.try_borrow_mut().ok()?;
        let a = b.as_mut()?;
        Some(f(a))
    })
}

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

fn usage() -> ! {
    println!(
        "jconsole {} —— VMless 图形化监控（Win32 原生）\n\
         \n\
         用法: jconsole [-p <pid>] [-i <ms>]\n\
         \n\
         选项:\n\
         \x20 -p <pid>        直接连指定进程；省略则弹出进程列表\n\
         \x20 -i <ms>         采样间隔（默认 1000 毫秒）\n\
         \x20 -h, --help      显示本帮助\n\
         \x20 -v, --version   显示版本\n\
         \n\
         页面: 概览 / 内存（jstat -gc 的 19 个计数器）/ 线程 / 类（jmap -histo）\n\
         \n\
         退出码: 0 = 正常；1 = 用法错误；2 = 目标 VM 不存在或不是 VMless VM；\n\
         \x20       3 = 协议错误；4 = 超时；5 = 连接失败；6 = 目标拒绝；7 = 窗口创建失败",
        vmless_gui::VERSION
    );
    std::process::exit(0)
}

struct Opts {
    pid: Option<u32>,
    interval_ms: u64,
    tab: i32,
}

fn parse_args() -> Opts {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts { pid: None, interval_ms: vm::INTERVAL_MS, tab: 0 };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => usage(),
            "-v" | "--version" => {
                println!("jconsole {}", vmless_gui::VERSION);
                std::process::exit(0)
            }
            "-p" | "-pid" => {
                i += 1;
                match argv.get(i).and_then(|s| s.parse::<u32>().ok()) {
                    Some(p) if p > 0 => o.pid = Some(p),
                    _ => {
                        eprintln!("错误：-p 需要一个正整数 pid");
                        std::process::exit(1)
                    }
                }
            }
            "-tab" => {
                i += 1;
                match argv.get(i).and_then(|s| s.parse::<i32>().ok()) {
                    Some(v) if (0..=3).contains(&v) => o.tab = v,
                    _ => {
                        eprintln!("错误：-tab 只接受 0..3（概览/内存/线程/类）");
                        std::process::exit(1)
                    }
                }
            }
            "-i" | "-interval" => {
                i += 1;
                match argv.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(v) if (100..=600_000).contains(&v) => o.interval_ms = v,
                    _ => {
                        eprintln!("错误：-i 需要 100..600000 之间的毫秒数");
                        std::process::exit(1)
                    }
                }
            }
            other => {
                eprintln!("错误：未知参数 {:?}（用 -h 看用法）", other);
                std::process::exit(1)
            }
        }
        i += 1;
    }
    o
}

// ---------------------------------------------------------------------------
// 主窗口
// ---------------------------------------------------------------------------

unsafe extern "system" fn main_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = if h.is_null() { 96 } else { dpi_of(h) };
    match msg {
        WM_CREATE => 0,
        WM_SIZE => {
            layout(h);
            0
        }
        WM_PAINT => {
            // 主窗口自身只负责"没被控件盖住的地方"（有 WS_CLIPCHILDREN 保证不越界），
            // 否则窗口边缘会残留别的东西（比如终端窗口的像素）。
            ui::paint(h, ui::C_BG, |_c| {});
            0
        }
        WM_TIMER => {
            if wp == TIMER_UI {
                pull(h);
            }
            0
        }
        WM_REFRESH => {
            pull(h);
            0
        }
        WM_NOTIFY => {
            let code = ctl::notify_code(lp);
            if code == TCN_SELCHANGE {
                show_page(h);
                // 刚切过去的页面立刻要一次最新数据
                with_app(|a| {
                    match ctl::tab_sel(a.tab) {
                        1 => a.shared.request(vm::REQ_HISTO),
                        2 => a.shared.request(vm::REQ_THREADS),
                        3 => a.shared.request(vm::REQ_HISTO),
                        _ => {}
                    }
                });
            } else if code == -108 {
                // LVN_COLUMNCLICK
                let nmlv = lp as *const NMLISTVIEW;
                if !nmlv.is_null() {
                    let sub = unsafe { (*nmlv).iSubItem };
                    with_app(|a| {
                        if a.sort_col != sub.max(1) {
                            a.sort_col = sub.max(1);
                            refill_classes(a);
                        }
                    });
                }
            } else if code == -101 {
                // LVN_ITEMCHANGED → 线程页刷新栈
                let nmlv = lp as *const NMLISTVIEW;
                if !nmlv.is_null() {
                    with_app(|a| {
                        if !a.threads.is_null() && unsafe { (*nmlv).hdr.hwndFrom } == a.threads {
                            fill_stack(a);
                        }
                    });
                }
            }
            0
        }
        WM_DPICHANGED => {
            with_app(|a| {
                a.dpi = dpi;
                a.fonts = Some(Fonts::new(dpi));
                if let Some(f) = a.fonts.as_ref() {
                    for c in a.children.clone() {
                        ui::set_font(c, f.ui.raw());
                    }
                }
            });
            layout(h);
            unsafe {
                InvalidateRect(h, std::ptr::null(), 1);
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = wp as HDC;
            unsafe {
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, ui::C_TEXT);
            }
            let b = unsafe { CreateSolidBrush(ui::C_BG) };
            b as LRESULT
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
                    (*mmi).ptMinTrackSize.x = scale(dpi, 640);
                    (*mmi).ptMinTrackSize.y = scale(dpi, 420);
                }
            }
            0
        }
        WM_CLOSE => {
            unsafe {
                DestroyWindow(h);
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                KillTimer(h, TIMER_UI);
            }
            // 让采样线程收工（它自己不 join，靠 stop 标志 + 自然退出）
            with_app(|a| {
                a.shared.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let kids = with_app(|a| std::mem::take(&mut a.children)).unwrap_or_default();
            ui::destroy_children(&kids);
            APP.with(|c| {
                if let Ok(mut b) = c.try_borrow_mut() {
                    *b = None; // 字体在这里析构
                }
            });
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn layout(h: HWND) {
    let Some((d, tab, ov, mem, threads, stack, th, classes, ch, status, kids)) = with_app(|a| {
        a.dpi = dpi_of(h);
        (
            a.dpi,
            a.tab,
            a.overview,
            a.memory,
            a.threads,
            a.stack,
            a.thread_hint,
            a.classes,
            a.class_hint,
            a.status,
            a.children.clone(),
        )
    }) else {
        return;
    };
    let _ = kids;
    let cr = client_rect(h);
    let (w, hh) = (cr.w(), cr.h());
    let status_h = scale(d, 24);
    let pad = scale(d, 8);
    // 标签控件占满上边，状态栏在底部
    ui::move_to(tab, RECT::new(pad / 2, pad / 2, w - pad / 2, hh - status_h));
    ui::move_to(status, RECT::new(pad + scale(d, 4), hh - status_h + scale(d, 2), w - pad, hh - scale(d, 2)));
    let page = ctl::tab_display_rect(tab);
    // 内缩要够：标签控件的边框会盖住页面顶部那几像素，提示行就看不见了
    let page = page.inset(scale(d, 8), scale(d, 8));
    ui::move_to(ov, page);
    ui::move_to(mem, page);
    // 标签控件会把自己提到 Z 序最前，页面必须再提回来（见 ui::bring_to_top）
    ui::bring_to_top(ov);
    ui::bring_to_top(mem);
    // 线程页：提示行 + 列表 + 栈
    let hint_h = scale(d, 20);
    ui::move_to(th, RECT::new(page.left, page.top, page.right, page.top + hint_h));
    let list_h = ((page.h() as f64) * 0.46).max(scale(d, 120) as f64) as i32;
    ui::move_to(
        threads,
        RECT::new(page.left, page.top + hint_h + scale(d, 4), page.right, page.top + hint_h + list_h),
    );
    ui::move_to(
        stack,
        RECT::new(
            page.left,
            page.top + hint_h + list_h + scale(d, 6),
            page.right,
            page.bottom,
        ),
    );
    ui::move_to(ch, RECT::new(page.left, page.top, page.right, page.top + hint_h));
    ui::move_to(classes, RECT::new(page.left, page.top + hint_h + scale(d, 4), page.right, page.bottom));
    show_page(h);
}

fn show_page(h: HWND) {
    let _ = h;
    with_app(|a| {
        let sel = ctl::tab_sel(a.tab).max(0) as usize;
        ui::show(a.overview, sel == 0);
        ui::show(a.memory, sel == 1);
        let t = sel == 2;
        ui::show(a.thread_hint, t);
        ui::show(a.threads, t);
        ui::show(a.stack, t);
        let c = sel == 3;
        ui::show(a.class_hint, c);
        ui::show(a.classes, c);
    });
    unsafe {
        InvalidateRect(h, std::ptr::null(), 0);
    }
}

// ---------------------------------------------------------------------------
// 数据落地到界面
// ---------------------------------------------------------------------------

fn pull(h: HWND) {
    let _ = h;
    let Some(snap) = with_app(|a| a.shared.read(|s| s.clone())) else {
        return;
    };
    with_app(|a| {
        if snap.seq != a.last_seq {
            a.last_seq = snap.seq;
            if snap.connected {
                a.hist.push_back(MemPoint {
                    eu: Snapshot::ratio(snap.gc[5], snap.gc[4]),
                    ou: Snapshot::ratio(snap.gc[7], snap.gc[6]),
                    mu: Snapshot::ratio(snap.gc[9], snap.gc[8]),
                });
                while a.hist.len() > HIST_LEN {
                    a.hist.pop_front();
                }
            }
            refill_threads(a, &snap);
            refill_classes(a);
        }
    let txt = status_text(&snap, a.interval_ms);
        if txt != a.last_status {
            ui::set_text(a.status, &txt);
            a.last_status = txt;
        }
    });
    // 页面重绘
    with_app(|a| {
        for w in [a.overview, a.memory] {
            if !w.is_null() && unsafe { IsWindowVisible(w) } != 0 {
                unsafe {
                    InvalidateRect(w, std::ptr::null(), 0);
                }
            }
        }
    });
}

fn status_text(s: &Snapshot, interval_ms: u64) -> String {
    let iv = if interval_ms % 1000 == 0 {
        format!("{}s", interval_ms / 1000)
    } else {
        format!("{}ms", interval_ms)
    };
    if !s.connected {
        let e = s.error.clone().unwrap_or_else(|| "目标已退出".into());
        return format!("连接断开：{} · 每 3 秒重试（第 {} 次）", e, s.retry.max(1));
    }
    format!(
        "已连接 {} · 采样间隔 {} · 采样 #{} · 堆 {} / {} · 线程 {} · 类 {}",
        s.pid,
        iv,
        s.seq,
        vm::format_size(s.heap_used),
        vm::format_size(s.heap_max),
        s.threads_live,
        s.classes_loaded
    )
}

fn refill_threads(a: &mut App, snap: &Snapshot) {
    let Some(t) = snap.threads.clone() else { return };
    let prev = a.sel_thread.clone();
    ctl::lv_clear(a.threads);
    for th in t.iter() {
        ctl::lv_row(
            a.threads,
            &[
                th.name.clone(),
                th.state.clone(),
                format!("#{}", th.java_id),
                if th.daemon { "守护".into() } else { String::new() },
            ],
        );
    }
    let idx = t
        .iter()
        .position(|x| x.name == prev)
        .unwrap_or(0)
        .min(t.len().saturating_sub(1));
    if !t.is_empty() {
        ctl::lv_select(a.threads, idx as i32);
        a.sel_thread = t[idx].name.clone();
    }
    ui::set_text(
        a.thread_hint,
        &format!("线程 {} 个（来自 Thread.print，每 2 秒）", t.len()),
    );
    fill_stack(a);
}

fn fill_stack(a: &mut App) {
    let Some(t) = a.shared.read(|s| s.threads.clone()) else {
        ui::set_text(a.stack, "（还没取到线程栈）");
        return;
    };
    let idx = ctl::lv_selected(a.threads);
    if idx < 0 || idx as usize >= t.len() {
        ui::set_text(a.stack, "选中一个线程看栈");
        return;
    }
    let th = &t[idx as usize];
    a.sel_thread = th.name.clone();
    let mut s = String::new();
    s.push_str(&format!("\"{}\" #{} daemon={} 状态 {}\r\n", th.name, th.java_id, th.daemon, th.state));
    if th.stack.is_empty() {
        s.push_str("（无栈帧）\r\n");
    }
    for line in &th.stack {
        s.push_str(line);
        s.push_str("\r\n");
    }
    ui::set_text(a.stack, &s);
}

fn refill_classes(a: &mut App) {
    let Some(rows) = a.shared.read(|s| s.histo.clone()) else {
        ui::set_text(a.class_hint, "正在取类直方图（GC.class_histogram）…");
        return;
    };
    let mut v: Vec<&vm::HistoRow> = rows.iter().collect();
    if a.sort_col == 2 {
        v.sort_by(|x, y| y.bytes.cmp(&x.bytes));
    } else {
        v.sort_by(|x, y| y.instances.cmp(&x.instances));
    }
    let prev = ctl::lv_selected(a.classes);
    ctl::lv_clear(a.classes);
    let total_inst: u64 = rows.iter().map(|r| r.instances).sum();
    let total_bytes: u64 = rows.iter().map(|r| r.bytes).sum();
    for r in &v {
        ctl::lv_row(
            a.classes,
            &[
                r.class_name.clone(),
                format!("{}", r.instances),
                vm::format_size(r.bytes as i64),
            ],
        );
    }
    if prev >= 0 && (prev as usize) < v.len() {
        ctl::lv_select(a.classes, prev);
    } else if !v.is_empty() {
        ctl::lv_select(a.classes, 0);
    }
    ui::set_text(
        a.class_hint,
        &format!(
            "{} 个类 · 实例 {} · 共 {} · 由 GC.class_histogram 每 5 秒刷新 · 点表头按{}排序",
            v.len(),
            total_inst,
            vm::format_size(total_bytes as i64),
            if a.sort_col == 2 { " 字节" } else { "实例数" }
        ),
    );
}

// ---------------------------------------------------------------------------
// 概览页自绘
// ---------------------------------------------------------------------------

unsafe extern "system" fn overview_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            with_app(|a| {
                ui::paint(h, ui::C_BG, |c| draw_overview(c, a));
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_DPICHANGED => 0,
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

unsafe extern "system" fn memory_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            with_app(|a| {
                ui::paint(h, ui::C_BG, |c| draw_memory(c, a));
            });
            0
        }
        WM_ERASEBKGND => 1,
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn bar_row(c: &mut Canvas, a: &App, x: i32, y: i32, w: i32, label: &str, used: i64, cap: i64, max: i64, color: u32) {
    let d = a.dpi;
    let Some(f) = a.fonts.as_ref() else { return };
    let lw = scale(d, 62);
    let vw = scale(d, 310);
    let bh = scale(d, 16);
    let bw = (w - lw - vw).max(scale(d, 40));
    c.text_line(RECT::new(x, y, x + lw, y + bh), label, ui::C_MUTED, f.ui.raw());
    let frac = if cap > 0 { used as f64 / cap as f64 } else { 0.0 };
    c.bar(
        RECT::new(x + lw, y, x + lw + bw, y + bh),
        frac,
        color,
        scale(d, 7),
    );
    let pct = if cap > 0 { used as f64 * 100.0 / cap as f64 } else { 0.0 };
    // 分母用 committed（这是 HotSpot/jconsole 里 "Committed" 的口径，进度条才有意义）；
    // max 单独标出来，免得 "31 KB / 104 MB" 让人以为 VM 快炸了。
    let extra = if max > cap && max > 0 { format!(" · 上限 {}", vm::format_size(max)) } else { String::new() };
    c.text_right(
        RECT::new(x + lw + bw + scale(d, 10), y, x + w, y + bh),
        &format!("{} / {}  ({:.1}%){}", vm::format_size(used), vm::format_size(cap), pct, extra),
        ui::C_TEXT,
        f.ui.raw(),
    );
}

fn stat_row(c: &mut Canvas, a: &App, x: i32, y: i32, w: i32, h: i32, label: &str, value: &str) {
    let Some(f) = a.fonts.as_ref() else { return };
    let lw = scale(a.dpi, 86);
    c.text_line(RECT::new(x, y, x + lw, y + h), label, ui::C_MUTED, f.ui.raw());
    c.text_line(RECT::new(x + lw, y, x + w, y + h), value, ui::C_TEXT, f.ui.raw());
}

fn draw_overview(c: &mut Canvas, a: &mut App) {
    let snap = a.shared.read(|s| s.clone());
    let d = a.dpi;
    let font_ui = a.fonts.as_ref().map(|f| f.ui.raw()).unwrap_or(std::ptr::null_mut());
    let font_title = a.fonts.as_ref().map(|f| f.ui_title.raw()).unwrap_or(font_ui);
    let font_bold = a.fonts.as_ref().map(|f| f.ui_bold.raw()).unwrap_or(font_ui);
    let font_small = a.fonts.as_ref().map(|f| f.ui_small.raw()).unwrap_or(font_ui);

    let p = scale(d, 14);
    let w = c.rc.w() - p * 2;
    let x = p;
    let mut y = p;

    // 标题
    let title = if snap.main_class.is_empty() {
        format!("pid {}", a.pid)
    } else {
        format!("{}  ·  pid {}", snap.main_class, a.pid)
    };
    c.text(RECT::new(x, y, x + w, y + scale(d, 26)), &title, ui::C_TEXT, font_title, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
    y += scale(d, 24);
    let sub = format!(
        "{}{}",
        if snap.vm_name.is_empty() { "VMless Server VM" } else { &snap.vm_name },
        if snap.vm_version.is_empty() { String::new() } else { format!("  ·  {}", snap.vm_version) }
    );
    c.text(RECT::new(x, y, x + w, y + scale(d, 18)), &sub, ui::C_MUTED, font_small, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
    y += scale(d, 26);

    if !snap.connected {
        let msg = format!(
            "连接断开：{}  —— 每 3 秒自动重连（第 {} 次）",
            snap.error.clone().unwrap_or_else(|| "目标已退出".into()),
            snap.retry.max(1)
        );
        let r = RECT::new(x, y, x + w, y + scale(d, 26));
        c.fill(r, ui::rgb(0xFD, 0xEC, 0xEA));
        c.frame(r, ui::rgb(0xE8, 0xB4, 0xAE));
        c.text(
            r.inset(scale(d, 8), 0),
            &msg,
            ui::C_WARN,
            font_bold,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        y += scale(d, 32);
    }

    // 左列：三条内存条；右列：统计
    let left_w = ((w as f64) * 0.58) as i32;
    let right_x = x + left_w + scale(d, 24);
    let right_w = w - left_w - scale(d, 24);
    let mut by = y;
    bar_row(c, a, x, by, left_w, "堆", snap.heap_used, snap.heap_cap.max(snap.heap_used), snap.heap_max, ui::C_HEAP);
    by += scale(d, 30);
    bar_row(c, a, x, by, left_w, "非堆", snap.nonheap_used, snap.nonheap_cap, 0, ui::C_NONHEAP);
    by += scale(d, 30);
    bar_row(c, a, x, by, left_w, "元空间", snap.meta_used, snap.meta_cap, 0, ui::C_META);
    by += scale(d, 40);

    // 右列：统计
    let row_h = scale(d, 22);
    let mut sy = y;
    stat_row(c, a, right_x, sy, right_w, row_h, "线程", &format!(
        "{} 活 / {} 守护 / 峰值 {}",
        snap.threads_live, snap.threads_daemon, snap.threads_peak
    ));
    sy += row_h;
    stat_row(c, a, right_x, sy, right_w, row_h, "已加载类", &format!("{} 个（已卸载 {}）", snap.classes_loaded, snap.classes_unloaded));
    sy += row_h;
    stat_row(c, a, right_x, sy, right_w, row_h, "运行时长", &vm::format_duration(snap.uptime_ms));
    sy += row_h;
    stat_row(
        c,
        a,
        right_x,
        sy,
        right_w,
        row_h,
        "CPU 时间",
        &format!("{:.2}s（当前 {:.1}% 单核）", snap.cpu_ns as f64 / 1e9, snap.cpu_pct),
    );
    sy += row_h;
    stat_row(c, a, right_x, sy, right_w, row_h, "压缩类空间", &format!(
        "{} / {}",
        vm::format_size(snap.ccs_used),
        vm::format_size(snap.ccs_cap)
    ));

    // 底部：GC 收集器
    let gy = by.max(sy + scale(d, 10)) + scale(d, 6);
    c.hline(x, x + w, gy, ui::C_RULE);
    let mut yy = gy + scale(d, 10);
    c.text(RECT::new(x, yy, x + w, yy + scale(d, 20)), "GC 收集器", ui::C_TEXT, font_bold, DT_LEFT | DT_SINGLELINE);
    yy += scale(d, 22);
    {
        // 表头（右边界必须和数值列一致，否则"次数"和数字不在一条竖线上）
        let c1 = x + scale(d, 300);
        let c2 = x + scale(d, 430);
        let c3 = x + scale(d, 620);
        c.text(RECT::new(c1, yy, c2, yy + scale(d, 18)), "次数", ui::C_MUTED, font_small, DT_RIGHT | DT_SINGLELINE);
        c.text(RECT::new(c2, yy, c3, yy + scale(d, 18)), "耗时", ui::C_MUTED, font_small, DT_RIGHT | DT_SINGLELINE);
        yy += scale(d, 18);
    }
    if snap.collectors.is_empty() {
        c.text(RECT::new(x, yy, x + w, yy + scale(d, 20)), "（VM 未提供收集器信息）", ui::C_FAINT, font_ui, DT_LEFT | DT_SINGLELINE);
    } else {
        // 三列分开画：中文字宽不一致，用 format! 补空格对不齐
        let c1 = x + scale(d, 300);
        let c2 = x + scale(d, 430);
        let c3 = x + scale(d, 620);
        for (i, (name, inv, time_ns)) in snap.collectors.iter().enumerate() {
            let row = RECT::new(x, yy, x + w, yy + scale(d, 20));
            c.text(RECT::new(x, yy, c1, row.bottom), name, ui::C_TEXT, font_ui, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
            c.text_right(RECT::new(c1, yy, c2, row.bottom), &format!("{}", inv), ui::C_TEXT, font_ui);
            c.text_right(RECT::new(c2, yy, c3, row.bottom), &format!("{:.3}s", *time_ns as f64 / 1e9), ui::C_TEXT, font_ui);
            yy += scale(d, 20);
            let _ = i;
        }
    }

    // 右下角：采样计数
    let foot = format!("采样 #{}  ·  {} 个采样点", snap.seq, a.hist.len());
    c.text_right(
        RECT::new(x, c.rc.bottom - scale(d, 24), x + w, c.rc.bottom - scale(d, 6)),
        &foot,
        ui::C_FAINT,
        font_small,
    );
}

// ---------------------------------------------------------------------------
// 内存页自绘（19 个计数器 + 折线）
// ---------------------------------------------------------------------------

fn draw_memory(c: &mut Canvas, a: &mut App) {
    let snap = a.shared.read(|s| s.clone());
    let hist: Vec<MemPoint> = a.hist.iter().copied().collect();
    let d = a.dpi;
    let font_ui = a.fonts.as_ref().map(|f| f.ui.raw()).unwrap_or(std::ptr::null_mut());
    let font_mono = a.fonts.as_ref().map(|f| f.mono.raw()).unwrap_or(font_ui);
    let font_bold = a.fonts.as_ref().map(|f| f.ui_bold.raw()).unwrap_or(font_ui);
    let font_small = a.fonts.as_ref().map(|f| f.ui_small.raw()).unwrap_or(font_ui);

    let p = scale(d, 14);
    let w = c.rc.w() - p * 2;
    let x = p;
    let mut y = p;

    c.text(
        RECT::new(x, y, x + w, y + scale(d, 24)),
        "GC 计数器（jstat -gc 口径，实时）",
        ui::C_TEXT,
        font_bold,
        DT_LEFT | DT_SINGLELINE,
    );
    y += scale(d, 26);

    // 左侧表：两列 × 10 行
    let table_w = ((w as f64) * 0.46) as i32;
    let row_h = scale(d, 20);
    let col_w = table_w / 2;
    for i in 0..19usize {
        let col = (i / 10) as i32;
        let row = (i % 10) as i32;
        let rx = x + col * col_w;
        let ry = y + row * row_h;
        let label_w = scale(d, 46);
        c.text_line(RECT::new(rx, ry, rx + label_w, ry + row_h), GC_COLS[i], ui::C_MUTED, font_mono);
        c.text_right(
            RECT::new(rx + label_w + scale(d, 6), ry, rx + col_w - scale(d, 10), ry + row_h),
            &snap.gc_text(i),
            ui::C_TEXT,
            font_mono,
        );
    }
    let table_h = 10 * row_h;

    // 右侧：折线图（占满剩余高度）
    let note_h = scale(d, 30);
    let chart_x = x + table_w + scale(d, 18);
    let chart_w = w - table_w - scale(d, 18);
    let chart_bottom = (c.rc.bottom - p - note_h).max(y + scale(d, 120));
    let chart_r = RECT::new(chart_x, y, chart_x + chart_w, chart_bottom);
    draw_chart(c, &hist, chart_r, a);

    // 底部说明：单位不写清楚，4096.0 会被当成字节
    let note_y = c.rc.bottom - p - note_h + scale(d, 4);
    c.text(
        RECT::new(x, note_y, x + w, note_y + scale(d, 14)),
        "容量类（S0C…CCSU）单位 KB；时间类（YGCT / FGCT / CGCT / GCT）单位秒；YGC / FGC / CGC 为次数。",
        ui::C_MUTED,
        font_small,
        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    c.text(
        RECT::new(x, note_y + scale(d, 14), x + w, note_y + scale(d, 28)),
        "右侧折线：纵轴 = 该区域「已用 / 容量」（0~100%），每秒 1 点，保留最近 60 点。",
        ui::C_MUTED,
        font_small,
        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let _ = (font_ui, table_h);
}

fn draw_chart(c: &mut Canvas, hist: &[MemPoint], r: RECT, a: &mut App) {
    let d = a.dpi;
    let font_small = a.fonts.as_ref().map(|f| f.ui_small.raw()).unwrap_or(std::ptr::null_mut());
    let font_ui = a.fonts.as_ref().map(|f| f.ui.raw()).unwrap_or(std::ptr::null_mut());
    c.fill(r, ui::C_PANEL);
    c.frame(r, ui::C_RULE);

    let pad_l = scale(d, 36);
    let pad_b = scale(d, 18);
    let pad_t = scale(d, 26);
    let plot = RECT::new(r.left + pad_l, r.top + pad_t, r.right - scale(d, 8), r.bottom - pad_b);
    if plot.is_empty() {
        return;
    }

    // 网格 + 百分比刻度
    for i in 0..=4 {
        let fy = plot.bottom - (plot.h() * i / 4);
        c.hline(plot.left, plot.right, fy, if i == 0 { ui::C_GRID } else { ui::C_RULE });
        let label = format!("{}%", i * 25);
        c.text_right(
            RECT::new(r.left, fy - scale(d, 8), plot.left - scale(d, 4), fy + scale(d, 8)),
            &label,
            ui::C_FAINT,
            font_small,
        );
    }

    // 三条折线
    let series: [(&str, u32, fn(&MemPoint) -> f64); 3] = [
        ("年轻代", ui::C_EDEN, |p| p.eu),
        ("老年代", ui::C_OLD, |p| p.ou),
        ("元空间", ui::C_SERIES3, |p| p.mu),
    ];
    let n = hist.len();
    if n >= 2 {
        let step = plot.w() as f64 / (HIST_LEN as f64 - 1.0);
        let x0 = plot.right as f64 - step * (n as f64 - 1.0);
        for (_, color, get) in series {
            let mut pts: Vec<POINT> = Vec::with_capacity(n);
            for (i, hp) in hist.iter().enumerate() {
                let px = x0 + step * i as f64;
                let py = plot.bottom as f64 - (plot.h() as f64) * get(hp).clamp(0.0, 1.0);
                pts.push(POINT { x: px as i32, y: py as i32 });
            }
            c.polyline(&pts, color, ((d / 96).max(1) + 1) as i32);
        }
    } else {
        c.text(
            plot,
            "  等待采样…（至少 2 个点才画线）",
            ui::C_FAINT,
            font_ui,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // 图例（右上）
    let mut lx = plot.left;
    let ly = r.top + scale(d, 4);
    for (name, color, get) in series {
        let cur = hist.last().map(get).unwrap_or(0.0) * 100.0;
        let label = format!("{} {:.1}%", name, cur);
        c.fill(RECT::new(lx, ly + scale(d, 5), lx + scale(d, 10), ly + scale(d, 15)), color);
        let tw = c.measure(&label, font_small) + scale(d, 8);
        c.text(
            RECT::new(lx + scale(d, 14), ly, lx + scale(d, 14) + tw, ly + scale(d, 20)),
            &label,
            ui::C_TEXT,
            font_small,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        lx += scale(d, 14) + tw + scale(d, 14);
    }
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

fn main() {
    let o = parse_args();
    START_TAB.with(|c| c.set(o.tab));
    enable_dpi_awareness();
    ui::init_common_controls();

    // 先拿连接：`-p 999999` 这种必须**当场报错退出**，不能开出一个空的监控窗口
    let (pid, client) = match o.pid {
        Some(pid) => match Client::attach(pid, 2500) {
            Ok(c) => (pid, Some(c)),
            Err(e) => {
                eprintln!("错误: {}", e);
                std::process::exit(e.exit_code());
            }
        },
        None => match vmless_gui::pick::choose_vm() {
            Some(pid) => match Client::attach(pid, 2500) {
                Ok(c) => (pid, Some(c)),
                Err(e) => {
                    eprintln!("错误: {}", e);
                    std::process::exit(e.exit_code());
                }
            },
            None => std::process::exit(0),
        },
    };

    let shared = Shared::new(pid);
    let spec = ui::WindowSpec {
        class: CLS_MAIN,
        title: &format!("jconsole —— VMless 监控  pid {}", pid),
        client_w: 960,
        client_h: 660,
        style: WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        ex_style: 0,
        wndproc: main_proc,
        bg: ui::C_BG,
    };
    let h = match ui::create_main(&spec) {
        Some(h) => h,
        None => {
            let msg = format!("无法创建主窗口：{}", last_error_text());
            eprintln!("错误：{}", msg);
            message_box("jconsole", &msg, MB_ICONERROR);
            std::process::exit(7);
        }
    };

    // 页面窗口类（自绘）
    ui::register_class(CLS_OVERVIEW, overview_proc, ui::C_BG);
    ui::register_class(CLS_MEMORY, memory_proc, ui::C_BG);

    let dpi = dpi_of(h);
    let fonts = Fonts::new(dpi);
    let f = fonts.ui.raw();

    let tab = ctl::tab(h, ID_TAB, RECT::new(0, 0, 10, 10));
    ctl::tab_add(tab, "概览");
    ctl::tab_add(tab, "内存");
    ctl::tab_add(tab, "线程");
    ctl::tab_add(tab, "类");
    ctl::tab_set_sel(tab, START_TAB.with(|c| c.get()));

    let overview = ui::child(CLS_OVERVIEW, "", WS_VISIBLE | WS_CLIPSIBLINGS, 0, RECT::new(0, 0, 10, 10), h, ID_OVERVIEW);
    let memory = ui::child(CLS_MEMORY, "", WS_CLIPSIBLINGS, 0, RECT::new(0, 0, 10, 10), h, ID_MEMORY);

    let thread_hint = ctl::label(h, ID_THREAD_HINT, "线程（Thread.print）", RECT::new(0, 0, 10, 10));
    let threads = ctl::listview(h, ID_THREADS, RECT::new(0, 0, 10, 10));
    ctl::lv_column(threads, 0, "线程名", scale(dpi, 220), false);
    ctl::lv_column(threads, 1, "状态", scale(dpi, 150), false);
    ctl::lv_column(threads, 2, "ID", scale(dpi, 60), false);
    ctl::lv_column(threads, 3, "守护", scale(dpi, 60), false);
    let stack = ctl::edit_multiline(h, ID_STACK, RECT::new(0, 0, 10, 10));

    let class_hint = ctl::label(h, ID_CLASS_HINT, "类直方图（GC.class_histogram）", RECT::new(0, 0, 10, 10));
    let classes = ctl::listview(h, ID_CLASSES, RECT::new(0, 0, 10, 10));
    ctl::lv_column(classes, 0, "类名", scale(dpi, 460), false);
    ctl::lv_column(classes, 1, "实例数", scale(dpi, 110), true);
    ctl::lv_column(classes, 2, "字节数", scale(dpi, 110), true);

    let status = ctl::label(h, ID_STATUS, "正在连接…", RECT::new(0, 0, 10, 10));

    let kids = vec![tab, overview, memory, thread_hint, threads, stack, class_hint, classes, status];
    for c in &kids {
        ui::set_font(*c, f);
    }
    ui::set_font(stack, fonts.mono.raw());

    APP.with(|c| {
        if let Ok(mut b) = c.try_borrow_mut() {
            *b = Some(Box::new(App {
                tab,
                overview,
                memory,
                threads,
                stack,
                thread_hint,
                classes,
                class_hint,
                status,
                fonts: Some(fonts),
                dpi,
                pid,
                interval_ms: o.interval_ms,
                shared: shared.clone(),
                hist: VecDeque::new(),
                last_seq: 0,
                sort_col: 1,
                sel_thread: String::new(),
                last_status: String::new(),
                children: kids,
            }));
        }
    });

    ui::show(h, true);
    layout(h);
    show_page(h);

    // 采样线程 + UI 定时器（定时器只负责兜底刷新，不碰网络）
    vm::spawn_worker(pid, client, shared, h, WM_REFRESH, o.interval_ms);
    unsafe {
        SetTimer(h, TIMER_UI, 500, std::ptr::null());
    }

    let code = ui::run_loop();
    std::process::exit(code);
}
