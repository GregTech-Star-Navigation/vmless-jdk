//! `jaccessinspector` —— 无障碍树检查器。
//!
//! 真 `jaccessinspector` 是 Java Access Bridge 附带的工具，用 Java 的 `AccessibleContext`
//! 遍历 Swing 组件的无障碍树。这里做的是它的 **Windows 侧对应物**：
//!
//! - 优先走 **MSAA**（`oleacc!AccessibleObjectFromWindow` + `IAccessible`），
//!   拿到的是 `role` / `name` / `state` / `rect` 这套真正的无障碍属性
//! - MSAA 拿不到（窗口没有无障碍提供者、或子节点树是空的）时，**退回 Win32 窗口树**
//!   （`EnumChildWindows` + `GetClassNameW` + `GetWindowTextW`），并在输出里
//!   **写明这一层不是 MSAA** —— 假装拿到了 MSAA 树比拿不到更糟
//!
//! ```text
//! jaccessinspector [-pid <n> | -title <片段> | -hwnd <0x...>] [-o tree|gui] [-d <深度>]
//! ```

use std::cell::RefCell;

use vmless_gui::msaa;
use vmless_gui::ui::{self, ctl, Fonts};
use vmless_gui::win::*;
use vmless_gui::winenum;

const CLASS: &str = "VmlessJAccessInspector";

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

struct Opts {
    pid: Option<u32>,
    title: Option<String>,
    hwnd: Option<usize>,
    gui: bool,
    depth: usize,
    /// 强制走 Win32 层（不调用 MSAA）：用于对比两层拿到的信息差异
    force_win32: bool,
}

fn usage() -> ! {
    println!(
        "jaccessinspector {} —— 无障碍（MSAA / Win32）树检查器\n\
         \n\
         用法: jaccessinspector [-pid <n> | -title <片段> | -hwnd <0x...>] [-o tree|gui] [-d <深度>]\n\
         \n\
         选项:\n\
         \x20 -pid <n>        目标进程\n\
         \x20 -title <片段>   目标窗口标题包含该片段（不区分大小写）\n\
         \x20 -hwnd <0x...>   直接给窗口句柄\n\
         \x20 -o <tree|gui>   输出方式：文本树（默认）或树形窗口\n\
         \x20 -d <n>          文本模式的最大展开深度（默认 8）\n\
         \x20 -h, --help      显示本帮助\n\
         \x20 -v, --version   显示版本\n\
         \n\
         不给目标时，自动选当前前台窗口。\n\
         \n\
         数据来源：先试 MSAA（oleacc!AccessibleObjectFromWindow + IAccessible）；\n\
         拿不到就退回 Win32 层（EnumChildWindows + GetClassNameW + GetWindowTextW），\n\
         并在输出里注明是 Win32 层。\n\
         \n\
         退出码: 0 = 正常；1 = 用法错误；2 = 找不到目标窗口；3 = 窗口创建失败",
        vmless_gui::VERSION
    );
    std::process::exit(0)
}

fn parse_args() -> Opts {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts { pid: None, title: None, hwnd: None, gui: false, depth: 8, force_win32: false };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => usage(),
            "-v" | "--version" => {
                println!("jaccessinspector {}", vmless_gui::VERSION);
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
            "-hwnd" => {
                i += 1;
                let v = argv.get(i).map(|s| s.as_str()).unwrap_or("");
                let v = v.trim_start_matches("0x").trim_start_matches("0X");
                match usize::from_str_radix(v, 16) {
                    Ok(h) if h != 0 => o.hwnd = Some(h),
                    _ => {
                        eprintln!("错误：-hwnd 需要一个十六进制句柄，例如 0x000A1B2C");
                        std::process::exit(1)
                    }
                }
            }
            "-o" => {
                i += 1;
                match argv.get(i).map(|s| s.as_str()) {
                    Some("tree") | Some("text") => o.gui = false,
                    Some("gui") => o.gui = true,
                    _ => {
                        eprintln!("错误：-o 只接受 tree 或 gui");
                        std::process::exit(1)
                    }
                }
            }
            "-win32" => o.force_win32 = true,
            "-d" => {
                i += 1;
                match argv.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    Some(v) if v <= 32 => o.depth = v,
                    _ => {
                        eprintln!("错误：-d 需要 0..32 的整数");
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
// 取树
// ---------------------------------------------------------------------------

/// Win32 兜底树：`class=… title=… visible 800x600 @ (x,y)`，缩进表示层级。
#[derive(Clone, Debug)]
struct FlatNode {
    depth: usize,
    text: String,
}

fn win32_tree(h: HWND) -> Vec<FlatNode> {
    let mut v = Vec::new();
    v.push(FlatNode { depth: 0, text: format!("window {} {}", window_class(h), hwnd32(h)) });
    for c in winenum::descendants(h) {
        let info = winenum::WinInfo::probe(c);
        let d = ancestor_depth(c);
        v.push(FlatNode { depth: d, text: format!("{}  {}", info.class, if info.title.is_empty() { "(无标题)".into() } else { format!("{:?}", info.title) }) });
    }
    v
}

fn hwnd32(h: HWND) -> String {
    format!("0x{:08X}", h as usize)
}

fn ancestor_depth(h: HWND) -> usize {
    let mut d = 0;
    let mut cur = h;
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

enum Tree {
    Msaa(msaa::Node),
    Win32(Vec<FlatNode>),
}

/// 子节点"有没有信息"：role=0 且 name 空的子节点等于没给。
fn meaningful(node: &msaa::Node) -> bool {
    node.children.iter().any(|c| c.role != 0 || !c.name.is_empty())
}

/// 混合树：根用整窗 MSAA；子节点**逐个 Win32 子窗口**探 MSAA。
///
/// 老式 Win32 窗口的 MSAA 提供者是 OLEACC 的通用代理：`get_accChildCount` 有值，
/// 但 `get_accRole(parent, childID=i)` 全部失败（子节点 role 全空）。这时按
/// "一个窗口 = 一个可访问对象" 递归探，反而能拿到 statictext / list / pagetab 这些
/// 真实角色 —— 见 `jaccessinspector -title jconsole` 的输出。
fn hybrid_tree(root: msaa::Node, h: HWND, cap: &mut usize) -> msaa::Node {
    let mut root = root;
    if *cap == 0 {
        return root;
    }
    // 根节点原本带的那批 `role=none` 空子节点直接丢掉 —— 它们没有任何可用信息
    root.children.clear();
    for c in direct_children(h) {
        if *cap == 0 {
            break;
        }
        *cap -= 1;
        let mut node = msaa::window_node(c).unwrap_or_else(|| fallback_node(c));
        if depth_below(c) < 12 {
            node = hybrid_tree(node, c, cap);
        }
        root.children.push(node);
    }
    root
}

fn direct_children(h: HWND) -> Vec<HWND> {
    winenum::descendants(h)
        .into_iter()
        .filter(|&c| unsafe { GetParent(c) } == h)
        .collect()
}

fn depth_below(h: HWND) -> usize {
    ancestor_depth(h)
}

fn fallback_node(h: HWND) -> msaa::Node {
    // 连 MSAA 对象都拿不到的子窗口：至少把类名/标题/位置留下，不要凭空丢掉
    let t = window_text(h);
    msaa::Node {
        role: 0,
        name: if t.is_empty() { window_class(h) } else { t },
        value: String::new(),
        state: 0,
        rect: window_rect(h),
        children: Vec::new(),
        truncated: false,
    }
}

fn build_tree(h: HWND, force_win32: bool) -> (Tree, String) {
    if force_win32 {
        return (Tree::Win32(win32_tree(h)), "Win32 层（EnumChildWindows + GetClassNameW，按 -win32 强制）".into());
    }
    msaa::com_init();
    let win32_children = winenum::descendants(h).len();
    match msaa::tree(h) {
        Ok(node) => {
            if meaningful(&node) {
                (Tree::Msaa(node), "MSAA（oleacc!AccessibleObjectFromWindow + IAccessible）".into())
            } else if win32_children > 0 {
                let note = format!(
                    "MSAA 根节点可用，但它的提供者没给子节点属性（子节点 role 全空）—— 子节点改用逐窗口 MSAA 探测（共 {} 个 Win32 子窗口）",
                    win32_children
                );
                let mut cap = 500usize;
                (Tree::Msaa(hybrid_tree(node, h, &mut cap)), note)
            } else {
                (Tree::Msaa(node), "MSAA（oleacc!AccessibleObjectFromWindow + IAccessible）".into())
            }
        }
        Err(e) => {
            let note = format!("{} —— 退回 Win32 层（EnumChildWindows）", e);
            (Tree::Win32(win32_tree(h)), note)
        }
    }
}

fn render_text(h: HWND, t: &Tree, note: &str, depth: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("窗口 {:?} class={} hwnd={}\n", window_text(h), window_class(h), hwnd32(h)));
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h, &mut pid);
    }
    out.push_str(&format!("进程 {} ({})\n", pid, process_image_name(pid)));
    out.push_str(&format!("来源: {}\n", note));
    out.push('\n');
    match t {
        Tree::Msaa(n) => {
            let mut seq = 0usize;
            out.push_str(&format!("MSAA 树（role / name / state / rect，共 {} 个节点）：\n", count_nodes(n)));
            n.render(&mut out, 0, depth, &mut seq);
        }
        Tree::Win32(v) => {
            out.push_str(&format!("Win32 树（class / title，共 {} 个节点）：\n", v.len()));
            for n in v {
                out.push_str(&format!("{}{}\n", "  ".repeat(n.depth.min(32)), n.text));
            }
        }
    }
    out
}

fn count_nodes(n: &msaa::Node) -> usize {
    1 + n.children.iter().map(count_nodes).sum::<usize>()
}

// ---------------------------------------------------------------------------
// GUI
// ---------------------------------------------------------------------------

struct St {
    tree: HWND,
    info: HWND,
    refresh: HWND,
    fonts: Option<Fonts>,
    dpi: u32,
    children: Vec<HWND>,
}

thread_local! {
    static ST: RefCell<Option<Box<St>>> = const { RefCell::new(None) };
    static TARGET: RefCell<Option<HWND>> = const { RefCell::new(None) };
    static FORCE_WIN32: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_st<R>(f: impl FnOnce(&mut St) -> R) -> Option<R> {
    ST.with(|c| {
        let mut b = c.try_borrow_mut().ok()?;
        b.as_mut().map(|s| f(s))
    })
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = if h.is_null() { 96 } else { dpi_of(h) };
    match msg {
        WM_CREATE => {
            ui::init_common_controls();
            let fonts = Fonts::new(dpi);
            let f = fonts.ui.raw();
            let tree = ctl::treeview(h, 3001, RECT::new(0, 0, 10, 10));
            let info = ctl::label(h, 3002, "读取中…", RECT::new(0, 0, 10, 10));
            let refresh = ctl::button(h, 3003, "刷新(&R)", RECT::new(0, 0, 10, 10));
            for c in [tree, info, refresh] {
                ui::set_font(c, f);
            }
            ST.with(|c| {
                if let Ok(mut b) = c.try_borrow_mut() {
                    *b = Some(Box::new(St {
                        tree,
                        info,
                        refresh,
                        fonts: Some(fonts),
                        dpi,
                        children: vec![tree, info, refresh],
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
        WM_PAINT => {
            ui::paint(h, ui::C_BG, |_c| {});
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u16;
            if id == 3003 && code == 0 {
                reload(h);
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
                    (*mmi).ptMinTrackSize.x = scale(dpi, 520);
                    (*mmi).ptMinTrackSize.y = scale(dpi, 360);
                }
            }
            0
        }
        WM_DESTROY => {
            let kids = with_st(|s| std::mem::take(&mut s.children)).unwrap_or_default();
            ui::destroy_children(&kids);
            ST.with(|c| {
                if let Ok(mut b) = c.try_borrow_mut() {
                    *b = None;
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
    let Some((d, kids)) = with_st(|s| (s.dpi, (s.tree, s.refresh, s.info))) else { return };
    let cr = client_rect(h);
    let (w, hh) = (cr.w(), cr.h());
    let pad = scale(d, 8);
    let bh = scale(d, 28);
    let info_h = scale(d, 36);
    ui::move_to(kids.1, RECT::new(pad, pad, pad + scale(d, 96), pad + bh));
    ui::move_to(kids.2, RECT::new(pad * 3 + scale(d, 96), pad, w - pad, pad + info_h));
    ui::move_to(kids.0, RECT::new(pad, pad + info_h + pad, w - pad, hh - pad));
    let _ = kids.0;
    let _ = h;
}

fn reload(h: HWND) {
    let target = TARGET.with(|t| *t.borrow());
    let Some(target) = target else { return };
    let (tree, note) = build_tree(target, FORCE_WIN32.with(|f| f.get()));
    let Some(st) = with_st(|s| (s.tree, s.info)) else { return };
    ui::tv_clear_all(st.0);
    match &tree {
        Tree::Msaa(n) => {
            add_msaa(st.0, TVI_ROOT, n);
        }
        Tree::Win32(v) => {
            let mut items: Vec<*mut std::ffi::c_void> = vec![TVI_ROOT];
            for n in v {
                let parent = *items.get(n.depth).unwrap_or(&TVI_ROOT);
                let item = ctl::tv_add(st.0, parent, &n.text);
                if !n.text.is_empty() {
                    ctl::tv_expand_all(st.0, item);
                }
                if items.len() > n.depth + 1 {
                    items[n.depth + 1] = item;
                    items.truncate(n.depth + 2);
                } else {
                    items.push(item);
                }
            }
        }
    }
    ui::set_text(st.1, &format!("{} | 目标 {:?}", note, window_text(target)));
    let _ = h;
}

fn add_msaa(tree: HWND, parent: *mut std::ffi::c_void, n: &msaa::Node) -> *mut std::ffi::c_void {
    let text = format!(
        "{} {} {} ({},{},{},{})",
        n.role_name(),
        if n.name.is_empty() { "(无名)" } else { n.name.as_str() },
        n.state_text(),
        n.rect.left,
        n.rect.top,
        n.rect.w(),
        n.rect.h()
    );
    let item = ctl::tv_add(tree, parent, &text);
    for c in &n.children {
        add_msaa(tree, item, c);
    }
    // 展开必须放在**子节点插完之后** —— 先展开一个还没有子节点的 item 是空操作，
    // 结果就是界面上只剩根节点一行（踩过）。
    if !n.children.is_empty() {
        ctl::tv_expand_all(tree, item);
    }
    item
}

fn gui_mode(o: &Opts) -> i32 {
    let Some(h) = winenum::find_window(o.pid, o.title.as_deref(), o.hwnd) else {
        eprintln!("错误：找不到目标窗口（-pid/-title/-hwnd 都没命中）");
        return 2;
    };
    TARGET.with(|t| *t.borrow_mut() = Some(h));
    FORCE_WIN32.with(|f| f.set(o.force_win32));
    // CoInitializeEx 必须在**使用 MSAA 的线程**上调用；GUI 模式下那就是主线程
    msaa::com_init();
    enable_dpi_awareness();
    ui::init_common_controls();
    let spec = ui::WindowSpec {
        class: CLASS,
        title: "jaccessinspector —— 无障碍树",
        client_w: 780,
        client_h: 560,
        style: WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        ex_style: 0,
        wndproc,
        bg: ui::C_BG,
    };
    match ui::create_main(&spec) {
        Some(w) => {
            ui::show(w, true);
            ui::run_loop()
        }
        None => {
            let msg = format!("无法创建窗口：{}", last_error_text());
            eprintln!("错误：{}", msg);
            message_box("jaccessinspector", &msg, MB_ICONERROR);
            3
        }
    }
}

fn main() {
    let o = parse_args();
    if o.gui {
        std::process::exit(gui_mode(&o));
    }
    let explicit = o.pid.is_some() || o.title.is_some() || o.hwnd.is_some();
    // 给了目标就必须命中：找不到是错误，**不要**偷偷换成前台窗口
    // （否则 `-pid 999999` 会打印一堆不相干的窗口，退出码还是 0 —— 那是骗人）
    let target = match winenum::find_window(o.pid, o.title.as_deref(), o.hwnd) {
        Some(h) => h,
        None if explicit => {
            eprintln!(
                "错误：找不到目标窗口（-pid {} -title {:?} -hwnd {:?} 都没有命中）",
                o.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                o.title.clone().unwrap_or_default(),
                o.hwnd.map(|h| format!("0x{:08X}", h)).unwrap_or_default()
            );
            std::process::exit(2);
        }
        None => {
            let fg = unsafe { GetForegroundWindow() };
            if fg.is_null() {
                eprintln!("错误：没有指定目标，前台窗口也取不到");
                std::process::exit(2);
            }
            fg
        }
    };
    let (tree, note) = build_tree(target, o.force_win32);
    print!("{}", render_text(target, &tree, &note, o.depth));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    msaa::com_uninit();
    std::process::exit(0);
}
