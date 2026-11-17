//! `jaccesswalker` —— 走一遍桌面上的窗口。
//!
//! 真 `jaccesswalker` 是 Java Access Bridge 附带的 GUI 工具，用来浏览 Java 无障碍树。
//! 这里做的是它在 **Win32 层的对应物**：
//!
//! - 文本模式（默认）：`EnumWindows` 枚举顶层窗口，**按进程分组**打印
//!   类名 / 标题 / 可见性 / 尺寸；`-pid` 过滤
//! - `-o gui`：树形视图（进程 → 窗口 → 子窗口），选中一行看详情
//!
//! ```text
//! jaccesswalker [-pid <n>] [-title <片段>] [-children] [-o text|gui] [-h] [-v]
//! ```

use std::cell::RefCell;
use std::collections::HashMap;

use vmless_gui::ui::{self, ctl, Fonts};
use vmless_gui::win::*;
use vmless_gui::winenum::{self, ProcessGroup};

const CLASS: &str = "VmlessJAccessWalker";

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Opts {
    pid: Option<u32>,
    title: Option<String>,
    children: bool,
    gui: bool,
}

fn usage() -> ! {
    println!(
        "jaccesswalker {} —— 枚举桌面窗口（按进程分组）\n\
         \n\
         用法: jaccesswalker [选项]\n\
         \n\
         选项:\n\
         \x20 -pid <n>          只看指定进程\n\
         \x20 -title <片段>     只看标题包含该片段的窗口（不区分大小写）\n\
         \x20 -children         连子窗口一起列（默认只列顶层窗口）\n\
         \x20 -o <text|gui>     输出方式：文本（默认）或树形窗口\n\
         \x20 -h, --help        显示本帮助\n\
         \x20 -v, --version     显示版本\n\
         \n\
         退出码: 0 = 正常（含\"没有窗口\"）；1 = 用法错误；3 = 窗口创建失败\n\
         \n\
         注：这是 Win32 层（EnumWindows / EnumChildWindows），不是 MSAA / Java\n\
         无障碍层。要看 MSAA 树请用 jaccessinspector。",
        vmless_gui::VERSION
    );
    std::process::exit(0)
}

fn parse_args() -> Opts {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts { pid: None, title: None, children: false, gui: false };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => usage(),
            "-v" | "--version" => {
                println!("jaccesswalker {}", vmless_gui::VERSION);
                std::process::exit(0)
            }
            "-pid" | "-p" => {
                i += 1;
                match argv.get(i).and_then(|s| s.parse::<u32>().ok()) {
                    Some(p) if p > 0 => o.pid = Some(p),
                    _ => {
                        eprintln!("错误：-pid 需要一个正整数");
                        std::process::exit(1)
                    }
                }
            }
            "-title" => {
                i += 1;
                match argv.get(i) {
                    Some(s) if !s.is_empty() => o.title = Some(s.clone()),
                    _ => {
                        eprintln!("错误：-title 需要一个非空片段");
                        std::process::exit(1)
                    }
                }
            }
            "-children" | "-c" => o.children = true,
            "-o" => {
                i += 1;
                match argv.get(i).map(|s| s.as_str()) {
                    Some("text") => o.gui = false,
                    Some("gui") => o.gui = true,
                    _ => {
                        eprintln!("错误：-o 只接受 text 或 gui");
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

thread_local! {
    static FILTER: RefCell<Opts> = RefCell::new(Opts { pid: None, title: None, children: false, gui: false });
}

fn collect(o: &Opts) -> Vec<ProcessGroup> {
    let mut g = winenum::by_process(o.pid, o.children);
    if let Some(t) = &o.title {
        let t = t.to_lowercase();
        for grp in g.iter_mut() {
            // 匹配标题的窗口**连同它的后代**一起留下 —— 子控件通常是空标题，
            // 只按自身标题过滤会把刚匹配到的顶层窗口的子控件全部丢掉（-children 就白开了）。
            let hit: Vec<bool> = grp.windows.iter().map(|w| w.title.to_lowercase().contains(&t)).collect();
            let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
            for (i, w) in grp.windows.iter().enumerate() {
                index.insert(w.hwnd as usize, i);
            }
            let mut keep = vec![false; grp.windows.len()];
            for (i, w) in grp.windows.iter().enumerate() {
                if hit[i] {
                    keep[i] = true;
                    continue;
                }
                let mut cur = w.parent as usize;
                for _ in 0..64 {
                    if cur == 0 {
                        break;
                    }
                    match index.get(&cur) {
                        Some(&pi) => {
                            if hit[pi] {
                                keep[i] = true;
                                break;
                            }
                            cur = grp.windows[pi].parent as usize;
                        }
                        None => break,
                    }
                }
            }
            let mut i = 0;
            grp.windows.retain(|_| {
                let k = keep[i];
                i += 1;
                k
            });
        }
        g.retain(|grp| !grp.windows.is_empty());
    }
    g
}

// ---------------------------------------------------------------------------
// 文本模式
// ---------------------------------------------------------------------------

fn text_mode(o: &Opts) -> i32 {
    let groups = collect(o);
    let wins: usize = groups.iter().map(|g| g.windows.len()).sum();
    println!(
        "共 {} 个窗口 / {} 个进程{}",
        wins,
        groups.len(),
        if o.children { "（含子窗口）" } else { "" }
    );
    println!();
    for g in &groups {
        println!("[pid {}] {}", g.pid, if g.image.is_empty() { "(无法取进程名)" } else { &g.image });
        for w in &g.windows {
            let pad = "  ".repeat(w.depth.max(0) as usize);
            println!("    {}{}", pad, w.line());
        }
        println!();
    }
    if wins == 0 {
        println!("（没有匹配的窗口）");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    0
}

// ---------------------------------------------------------------------------
// GUI
// ---------------------------------------------------------------------------

struct St {
    tree: HWND,
    btn: HWND,
    info: HWND,
    status: HWND,
    fonts: Option<Fonts>,
    dpi: u32,
    children: Vec<HWND>,
}

impl St {
    fn take_children(&mut self) -> Vec<HWND> {
        std::mem::take(&mut self.children)
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = if h.is_null() { 96 } else { dpi_of(h) };
    match msg {
        WM_CREATE => {
            ui::init_common_controls();
            let d = dpi;
            let fonts = Fonts::new(d);
            let f = fonts.ui.raw();
            let tree = ctl::treeview(h, 1001, RECT::new(0, 0, 10, 10));
            let btn = ctl::button(h, 1002, "刷新(&R)", RECT::new(0, 0, 10, 10));
            let info = ctl::label(h, 1003, "选中一行看详情", RECT::new(0, 0, 10, 10));
            let status = ctl::label(h, 1004, "", RECT::new(0, 0, 10, 10));
            for c in [tree, btn, info, status] {
                ui::set_font(c, f);
            }
            let st = St {
                tree,
                btn,
                info,
                status,
                fonts: Some(fonts),
                dpi: d,
                children: vec![tree, btn, info, status],
            };
            unsafe {
                SetWindowLongPtrW(h, GWLP_USERDATA, Box::into_raw(Box::new(st)) as isize);
            }
            layout(h);
            refresh(h);
            0
        }
        WM_PAINT => {
            ui::paint(h, ui::C_BG, |_c| {});
            0
        }
        WM_SIZE => {
            if let Some(st) = state(h) {
                st.dpi = dpi;
            }
            layout(h);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u16;
            if id == 1002 && code == 0 {
                refresh(h);
            }
            0
        }
        WM_NOTIFY => {
            let code = ctl::notify_code(lp);
            if code == TVN_SELCHANGEDW || code == TVN_SELCHANGEDA {
                show_selected(h);
            }
            0
        }
        WM_DPICHANGED => {
            rebuild_fonts(h, dpi);
            layout(h);
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = wp as HDC;
            unsafe {
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, ui::C_TEXT);
            }
            unsafe { CreateSolidBrush(ui::C_BG) as LRESULT }
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
                    (*mmi).ptMinTrackSize.x = scale(dpi, 480);
                    (*mmi).ptMinTrackSize.y = scale(dpi, 320);
                }
            }
            0
        }
        WM_DESTROY => {
            let raw = unsafe { SetWindowLongPtrW(h, GWLP_USERDATA, 0) };
            if raw != 0 {
                let mut st = unsafe { Box::from_raw(raw as *mut St) };
                let kids = st.take_children();
                ui::destroy_children(&kids);
                drop(st); // 字体在这里才析构
            }
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn state(h: HWND) -> Option<&'static mut St> {
    let raw = unsafe { GetWindowLongPtrW(h, GWLP_USERDATA) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut St) })
    }
}

fn layout(h: HWND) {
    let Some(st) = state(h) else { return };
    let d = st.dpi;
    let cr = client_rect(h);
    let (w, hh) = (cr.w(), cr.h());
    let pad = scale(d, 8);
    let btn_h = scale(d, 28);
    let status_h = scale(d, 22);
    ui::move_to(st.btn, RECT::new(pad, pad, pad + scale(d, 96), pad + btn_h));
    ui::move_to(st.info, RECT::new(pad * 3 + scale(d, 96), pad, w - pad, pad + btn_h));
    ui::move_to(st.status, RECT::new(pad, hh - status_h - pad / 2, w - pad, hh - pad / 2));
    ui::move_to(st.tree, RECT::new(pad, pad + btn_h + pad, w - pad, hh - status_h - pad));
    if let Err(e) = (|| Ok::<(), ()>(()))() {
        let _ = e;
    }
    let _ = &mut st.children;
}

fn refresh(h: HWND) {
    let o = FILTER.with(|f| f.borrow().clone());
    let groups = collect(&o);
    let total: usize = groups.iter().map(|g| g.windows.len()).sum();
    let Some(st) = state(h) else { return };
    ui::tv_clear_all(st.tree);
    for g in &groups {
        let name = if g.image.is_empty() { "?".to_string() } else { g.image.clone() };
        let root = ctl::tv_add(st.tree, TVI_ROOT, &format!("[pid {}] {}", g.pid, name));
        let mut map: HashMap<usize, *mut std::ffi::c_void> = HashMap::new();
        let mut sorted: Vec<&vmless_gui::winenum::WinInfo> = g.windows.iter().collect();
        sorted.sort_by_key(|w| w.depth);
        for w in sorted {
            let label = if w.title.is_empty() {
                format!("{}  (无标题)", w.class)
            } else {
                format!("{}  {}", w.class, w.title)
            };
            let parent_item = map.get(&(w.parent as usize)).copied().unwrap_or(root);
            let item = ctl::tv_add(st.tree, parent_item, &label);
            map.insert(w.hwnd as usize, item);
        }
        ctl::tv_expand_all(st.tree, root);
    }
    ui::set_text(
        st.status,
        &format!(
            "共 {} 个窗口 / {} 个进程{}（Win32 层枚举）",
            total,
            groups.len(),
            if o.children { "，含子窗口" } else { "" }
        ),
    );
    ui::set_text(st.info, "选中一行看详情");
}

fn show_selected(h: HWND) {
    let Some(st) = state(h) else { return };
    let item = unsafe { SendMessageW(st.tree, TVM_GETNEXTITEM, TVGN_CARET as usize, 0) } as *mut std::ffi::c_void;
    if item.is_null() {
        ui::set_text(st.info, "选中一行看详情");
        return;
    }
    let text = ui::tv_text(st.tree, item);
    ui::set_text(st.info, &text);
}

fn rebuild_fonts(h: HWND, dpi: u32) {
    let Some(st) = state(h) else { return };
    st.fonts = Some(Fonts::new(dpi));
    st.dpi = dpi;
    if let Some(f) = st.fonts.as_ref() {
        for c in [st.tree, st.btn, st.info, st.status] {
            ui::set_font(c, f.ui.raw());
        }
    }
}

fn gui_mode(o: Opts) -> i32 {
    FILTER.with(|f| *f.borrow_mut() = o.clone());
    enable_dpi_awareness();
    ui::init_common_controls();
    let spec = ui::WindowSpec {
        class: CLASS,
        title: "jaccesswalker —— 桌面窗口（Win32 层）",
        client_w: 820,
        client_h: 500,
        style: WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        ex_style: 0,
        wndproc,
        bg: ui::C_BG,
    };
    match ui::create_main(&spec) {
        Some(h) => {
            ui::show(h, true);
            ui::run_loop()
        }
        None => {
            let msg = format!("无法创建窗口：{}", last_error_text());
            eprintln!("错误：{}", msg);
            message_box("jaccesswalker", &msg, MB_ICONERROR);
            3
        }
    }
}

fn main() {
    let o = parse_args();
    if o.gui {
        std::process::exit(gui_mode(o));
    } else {
        std::process::exit(text_mode(&o));
    }
}
