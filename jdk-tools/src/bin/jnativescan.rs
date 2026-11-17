//! `jnativescan`（JDK 25 新工具）—— 找出会触碰 native 能力的代码。
//!
//! 两件事：
//! 1. `native` 方法声明（`ACC_NATIVE`），按 来源 → 类 → 方法 聚合；
//! 2. `--print-native-access` 用的「可能做 native 访问的模块」清单。
//!
//! 受限方法（FOREIGN 的 `Linker.downcallHandle` 之类）我们没有运行时的 restricted
//! 方法注册表，所以那一栏统一打 `<no restricted methods>` —— 与真 JDK 在「找不到」时的
//! 输出一致；差别写在 `--help` 里。

use vmless_tools::cli::Args;
use vmless_tools::scan;
use vmless_tools::{die, split_list, usage_err, EXIT_USAGE};

const TOOL: &str = "jnativescan";

fn help() {
    println!("The jnativescan tool can be used to find methods that may access native functionality when");
    println!("run. This includes restricted method calls and 'native' method declarations.");
    println!();
    println!("Option                  Description                                            ");
    println!("------                  -----------                                            ");
    println!("-?, -h, --help          help                                                   ");
    println!("--add-modules <String>  List of root modules to scan                           ");
    println!("--class-path <Path>     The class path as used at runtime                     ");
    println!("--module-path <Path>    The module path as used at runtime                     ");
    println!("--print-native-access   print a comma separated list of modules that may       ");
    println!("                          perform native access operations. ALL-UNNAMED is used ");
    println!("                          to indicate unnamed modules.                         ");
    println!("--release <Version>     The runtime version that will run the application      ");
    println!("--version               Print version information and exit                    ");
    println!();
    println!("本实现的边界：受限方法（java.lang.foreign 的 Linker 等）需要运行时的 restricted");
    println!("方法注册表，我们没有，因此这一栏固定输出 `<no restricted methods>`；native 方法");
    println!("声明的扫描是按字节码 `ACC_NATIVE` 标志真扫的。");
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() || args.is_empty() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &["--class-path", "--module-path", "--add-modules", "--release"];
    let known: &[&str] = &[
        "--class-path",
        "--module-path",
        "--add-modules",
        "--print-native-access",
        "--release",
        "--version",
        "-?",
        "-h",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    if let Ok(pos) = args.positionals(takes) {
        if !pos.is_empty() {
            eprintln!("{TOOL}: 不接受位置参数（用 --class-path 或 --module-path 指定要扫的路径）");
            std::process::exit(EXIT_USAGE);
        }
    }
    let print_access = args.flag("--print-native-access");
    let mut inputs: Vec<(String, String)> = Vec::new(); // (来源描述, 路径)
    if let Ok(Some(v)) = args.value("--class-path") {
        for p in vmless_tools::split_paths(&v) {
            // 盘符要拼回去
            inputs.push((format!("{p} (ALL-UNNAMED)"), p.trim().to_string()));
        }
    }
    if let Ok(Some(v)) = args.value("--module-path") {
        let add = args.value("--add-modules").ok().flatten().unwrap_or_default();
        let roots = split_list(&add);
        for p in v.split([';']).filter(|s| !s.trim().is_empty()) {
            inputs.push((format!("{p} (module-path)"), p.trim().to_string()));
        }
        if !roots.is_empty() {
            eprintln!("{TOOL}: 注意：--add-modules {} 只影响报告口径，扫描仍按路径全量进行", roots.join(","));
        }
    }
    if inputs.is_empty() {
        usage_err(TOOL, "没给 --class-path / --module-path");
    }

    let mut modules_seen: Vec<String> = Vec::new();
    let mut total_native = 0usize;
    for (label, path) in &inputs {
        let (kind, classes) = match scan::collect_classes(std::path::Path::new(path), &[]) {
            Ok(v) => v,
            Err(e) => die(TOOL, &e),
        };
        // 按 (模块, 类) 聚合
        let mut per_class: Vec<(String, String, Vec<(String, String, bool)>)> = Vec::new();
        for e in &classes {
            let cf = match scan::parse_entry(e) {
                Ok(c) => c,
                Err(err) => {
                    // 单个类坏掉不该让整次扫描失败：报到 stderr，继续
                    eprintln!("{TOOL}: {err}");
                    continue;
                }
            };
            let natives = match scan::native_methods(&cf) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("{TOOL}: {}: {err}", e.entry);
                    continue;
                }
            };
            if natives.is_empty() {
                continue;
            }
            total_native += natives.len();
            let modname = if e.module.is_empty() {
                "ALL-UNNAMED".to_string()
            } else {
                e.module.clone()
            };
            if !modules_seen.contains(&modname) {
                modules_seen.push(modname.clone());
            }
            per_class.push((modname, e.binary_name(), natives));
        }
        if print_access {
            // 汇总口径：每行一个模块
            let mut mods: Vec<String> = modules_seen.clone();
            mods.sort();
            mods.dedup();
            for m in &mods {
                println!("{m}");
            }
            return;
        }
        let _ = kind;
        println!("{label}:");
        if per_class.is_empty() {
            println!("  <no restricted methods>");
            continue;
        }
        // 模块 → 类 的顺序稳定
        per_class.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut last_mod = String::new();
        let mut last_class = String::new();
        for (modname, class, natives) in &per_class {
            if *modname != last_mod {
                // 模块切换时打一行模块标题（只有模块树/jimage 才有多模块）
                if modname != "ALL-UNNAMED" {
                    println!("{modname}:");
                }
                last_mod = modname.clone();
                last_class.clear();
            }
            if *class != last_class {
                println!("  {class}:");
                last_class = class.clone();
            }
            let mut sorted = natives.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for (name, desc, _is_static) in sorted {
                println!(
                    "    {}::{} is a native method declaration",
                    short_name(class),
                    render_method(&name, &desc)
                );
            }
        }
    }
    eprintln!("{TOOL}: 共 {total_native} 个 native 方法声明");
}

/// `java.lang.String` → `String`
fn short_name(binary: &str) -> &str {
    binary.rsplit('.').next().unwrap_or(binary)
}

/// JVM 描述符 → Java 风格：`(long)void`、`()int`、`(java.lang.String,int)boolean`
fn render_method(name: &str, desc: &str) -> String {
    let bytes = desc.as_bytes();
    let close = bytes.iter().position(|b| *b == b')').unwrap_or(0);
    let params = &desc[1..close];
    let ret = &desc[close + 1..];
    let mut out = String::new();
    out.push_str(name);
    out.push('(');
    let mut i = 0;
    let mut first = true;
    while i < params.len() {
        let (ty, next) = parse_type(params, i);
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&ty);
        i = next;
    }
    out.push(')');
    out.push_str(&parse_type(ret, 0).0);
    out
}

fn parse_type(s: &str, i: usize) -> (String, usize) {
    let b = s.as_bytes();
    if i >= b.len() {
        return (String::new(), i);
    }
    match b[i] {
        b'[' => {
            let (inner, n) = parse_type(s, i + 1);
            (format!("{inner}[]"), n)
        }
        b'L' => {
            let end = s[i..].find(';').map(|p| i + p).unwrap_or(s.len() - 1);
            let path = &s[i + 1..end];
            let simple = path.rsplit('/').next().unwrap_or(path).replace('$', ".");
            (simple, (end + 1).min(b.len()))
        }
        c => (
            match c {
                b'B' => "byte",
                b'C' => "char",
                b'D' => "double",
                b'F' => "float",
                b'I' => "int",
                b'J' => "long",
                b'S' => "short",
                b'Z' => "boolean",
                b'V' => "void",
                _ => "?",
            }
            .to_string(),
            i + 1,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::render_method;

    #[test]
    fn sigs() {
        assert_eq!(render_method("foo", "()I"), "foo()int");
        assert_eq!(render_method("bar", "(J)V"), "bar(long)void");
        assert_eq!(
            render_method("x", "(Ljava/lang/String;[I)Ljava/util/List;"),
            "x(String,int[])List"
        );
    }
}
