//! `jabswitch` —— Java Access Bridge 的开关。
//!
//! 和真 `jabswitch` 改的是**同一个注册表值**：
//!
//! ```text
//! HKCU\Software\Microsoft\Windows NT\CurrentVersion\Accessibility\Configuration
//!   JavaAccessBridge = REG_SZ "1" / "0"
//! ```
//!
//! Windows 的"轻松使用"设置面板读的就是它，`windowsaccessbridge-*.dll` 也据此决定要不要
//! 把辅助技术接入 JVM。所以这个工具没有任何自创的配置 —— 它只是一个可靠的读写入口，
//! 顺带把"值不存在"和"读不到"两种情况区分开（前者等于关闭，不是错误）。
//!
//! ```text
//! jabswitch [-enable | -disable | -q | -h]
//! ```
//!
//! 退出码：0 = 正常；1 = 用法错误；3 = 注册表访问失败

use vmless_gui::reg;

fn usage() -> ! {
    println!(
        "jabswitch {} —— Java Access Bridge 开关\n\
         \n\
         用法: jabswitch [-enable | -disable | -q]\n\
         \n\
         选项:\n\
         \x20 -enable    打开（写 JavaAccessBridge = \"1\"）\n\
         \x20 -disable   关闭（写 JavaAccessBridge = \"0\"）\n\
         \x20 -q         查询当前状态\n\
         \x20 -h, --help 显示本帮助\n\
         \n\
         注册表位置:\n\
         \x20 HKCU\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Accessibility\\Configuration\n\
         \x20   JavaAccessBridge (REG_SZ) = \"1\" / \"0\"\n\
         \n\
         退出码: 0 = 正常；1 = 用法错误；3 = 注册表访问失败",
        vmless_gui::VERSION
    );
    std::process::exit(0)
}

/// 把注册表里的原始值翻译成"开 / 关 / 未设置 / 意料之外"。
fn describe(v: &Option<String>) -> String {
    match v.as_deref() {
        Some("1") => "已启用".to_string(),
        Some("0") => "已禁用".to_string(),
        Some("") => "已禁用（值为空串）".to_string(),
        Some(other) => format!("状态不明（值为 {:?}，非 \"0\"/\"1\"）", other),
        None => "已禁用（值不存在）".to_string(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match args.first().map(|s| s.as_str()) {
        Some("-h") | Some("--help") | None => {
            usage();
        }
        Some("-v") | Some("--version") => {
            println!("jabswitch {}", vmless_gui::VERSION);
            std::process::exit(0)
        }
        Some("-q") | Some("--query") => "q",
        Some("-enable") | Some("--enable") | Some("-e") => "enable",
        Some("-disable") | Some("--disable") | Some("-d") => "disable",
        Some(other) => {
            eprintln!("错误：未知参数 {:?}（用 -h 看用法）", other);
            std::process::exit(1)
        }
    };
    if args.len() > 1 {
        eprintln!("错误：多余的参数 {:?}（一次只做一个动作）", &args[1..]);
        std::process::exit(1)
    }

    // 先读一次：写之前的状态也要能查到，并且让"没权限读"在写之前就暴露出来
    let before = match reg::query() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误：{}", e);
            std::process::exit(3)
        }
    };

    match mode {
        "q" => {
            println!("Java Access Bridge {}", describe(&before));
            println!(
                "  注册表: HKCU\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Accessibility\\Configuration\\JavaAccessBridge = {}",
                match before.as_deref() {
                    Some(v) => format!("{:?}", v),
                    None => "（不存在）".to_string(),
                }
            );
        }
        "enable" | "disable" => {
            let want = if mode == "enable" { "1" } else { "0" };
            if let Err(e) = reg::set(want) {
                eprintln!("错误：{}", e);
                std::process::exit(3)
            }
            // 写完再读一次，确认真的落盘了（而不是"调用返回成功"）
            let after = match reg::query() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("错误：写入后回读失败：{}", e);
                    std::process::exit(3)
                }
            };
            if after.as_deref() != Some(want) {
                eprintln!(
                    "错误：写入后回读不一致（期望 {:?}，实际 {:?}）",
                    want,
                    after.as_deref().unwrap_or("（不存在）")
                );
                std::process::exit(3)
            }
            println!("Java Access Bridge {}", describe(&after));
        }
        _ => std::process::exit(1), // 上面已经穷举过，走不到这里；写成退出而不是 panic
    }
}
