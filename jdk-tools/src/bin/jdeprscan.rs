//! `jdeprscan` —— 扫出对**已废弃 / 已标记移除**的 API 的使用。
//!
//! 真 JDK 的清单来自 `lib/jdeprscan`（由 ct.sym 生成，含每个版本的完整废弃 API）。
//! 我们不链那份数据，改用两条真实来源：
//! 1. **内置表**：常见 JDK 废弃 API（含 `forRemoval` 与 since 版本），可 `--list` 打印；
//! 2. **被扫描输入自身**：类里声明为 `@Deprecated` / 带 `Deprecated` 属性的成员，
//!    只要其它被扫描类引用了它就报出来（这部分对任意 jar 都有效，不依赖 JDK 版本）。
//!
//! 输出格式与真 JDK 对齐：
//! ```text
//! Directory C:/tmp/x:
//! class D uses deprecated method java/lang/Thread::stop()V (forRemoval=true)
//! ```

use std::path::Path;

use vmless_tools::cli::Args;
use vmless_tools::scan;
use vmless_tools::{die, usage_err, EXIT_USAGE};

const TOOL: &str = "jdeprscan";

/// (owner, name, desc, 是否方法, forRemoval, since)
struct Dep {
    owner: &'static str,
    name: &'static str,
    desc: &'static str,
    is_method: bool,
    for_removal: bool,
    since: u32,
}

const DEPS: &[Dep] = &[
    Dep { owner: "java/lang/Thread", name: "stop", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Thread", name: "stop", desc: "(Ljava/lang/Throwable;)V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Thread", name: "suspend", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Thread", name: "resume", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Thread", name: "countStackFrames", desc: "()I", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Thread", name: "destroy", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/ThreadGroup", name: "suspend", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/ThreadGroup", name: "resume", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/ThreadGroup", name: "stop", desc: "()V", is_method: true, for_removal: true, since: 20 },
    Dep { owner: "java/lang/Runtime", name: "runFinalizersOnExit", desc: "(Z)V", is_method: true, for_removal: true, since: 1 },
    Dep { owner: "java/lang/System", name: "runFinalizersOnExit", desc: "(Z)V", is_method: true, for_removal: true, since: 1 },
    Dep { owner: "java/lang/System", name: "getSecurityManager", desc: "()Ljava/lang/SecurityManager;", is_method: true, for_removal: true, since: 17 },
    Dep { owner: "java/lang/System", name: "setSecurityManager", desc: "(Ljava/lang/SecurityManager;)V", is_method: true, for_removal: true, since: 17 },
    Dep { owner: "java/lang/Object", name: "finalize", desc: "()V", is_method: true, for_removal: true, since: 18 },
    Dep { owner: "java/lang/SecurityManager", name: "", desc: "", is_method: false, for_removal: true, since: 17 },
    Dep { owner: "java/util/Date", name: "getYear", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getMonth", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getDay", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getHours", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getMinutes", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getSeconds", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "setYear", desc: "(I)V", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "toGMTString", desc: "()Ljava/lang/String;", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "getTimezoneOffset", desc: "()I", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "<init>", desc: "(III)V", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "<init>", desc: "(IIIII)V", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "<init>", desc: "(IIIIII)V", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/util/Date", name: "<init>", desc: "(Ljava/lang/String;)V", is_method: true, for_removal: false, since: 1 },
    Dep { owner: "java/lang/Integer", name: "<init>", desc: "(I)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Integer", name: "intValue", desc: "()I", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Long", name: "<init>", desc: "(J)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Boolean", name: "<init>", desc: "(Z)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Character", name: "<init>", desc: "(C)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Double", name: "<init>", desc: "(D)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Float", name: "<init>", desc: "(F)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Short", name: "<init>", desc: "(S)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Byte", name: "<init>", desc: "(B)V", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Class", name: "newInstance", desc: "()Ljava/lang/Object;", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Class", name: "getDeclaredField", desc: "(Ljava/lang/String;)Ljava/lang/reflect/Field;", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/util/Collections", name: "unmodifiableList", desc: "(Ljava/util/List;)Ljava/util/List;", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/lang/Thread", name: "getId", desc: "()J", is_method: true, for_removal: false, since: 19 },
    Dep { owner: "java/lang/Thread", name: "threadId", desc: "()J", is_method: true, for_removal: false, since: 19 },
    Dep { owner: "javax/net/ssl/SSLSession", name: "getPeerCertificateChain", desc: "()[Ljavax/security/cert/X509Certificate;", is_method: true, for_removal: false, since: 9 },
    Dep { owner: "java/io/File", name: "toURL", desc: "()Ljava/net/URL;", is_method: true, for_removal: false, since: 20 },
    Dep { owner: "java/net/URL", name: "<init>", desc: "(Ljava/lang/String;)V", is_method: true, for_removal: false, since: 20 },
    Dep { owner: "java/lang/System", name: "getenv", desc: "(Ljava/lang/String;)Ljava/lang/String;", is_method: true, for_removal: false, since: 20 },
    Dep { owner: "javax/script/ScriptEngineManager", name: "", desc: "", is_method: false, for_removal: false, since: 6 },
    Dep { owner: "java/awt/Font", name: "getFont", desc: "(Ljava/lang/String;)Ljava/awt/Font;", is_method: true, for_removal: false, since: 1 },
];

fn help() {
    println!("Usage: jdeprscan [options] {{dir|jar|class}} ...");
    println!();
    println!("Options:");
    println!("        --class-path PATH");
    println!("        --for-removal");
    println!("        --full-version");
    println!("  -? -h --help");
    println!("  -l    --list");
    println!("        --release 8|9|10|11|12|13|14|15|16|17|18|19|20|21|22|23|24|25");
    println!("  -v    --verbose");
    println!("        --version");
    println!();
    println!("--class-path 选项提供用于解析类引用的类路径。");
    println!("--for-removal 选项仅扫描正在报废（已标记删除）的 API。");
    println!();
    println!("本实现的清单来源见 crate 文档：内置表 + 输入自身的 @Deprecated 交叉引用；");
    println!("--list 打印内置表，`--verbose` 额外打印扫描进度。");
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
    let takes: &[&str] = &["--class-path", "--release"];
    let known: &[&str] = &[
        "-l",
        "--list",
        "--class-path",
        "--for-removal",
        "--full-version",
        "--release",
        "-v",
        "--verbose",
        "--version",
        "-?",
        "-h",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    if args.flag("--list") || args.flag("-l") {
        println!("Deprecated APIs known to this implementation:");
        let mut rows: Vec<&Dep> = DEPS.iter().collect();
        rows.sort_by(|a, b| {
            a.owner
                .cmp(b.owner)
                .then_with(|| a.name.cmp(b.name))
                .then_with(|| a.desc.cmp(b.desc))
        });
        for d in rows {
            if d.is_method {
                println!("  method {}::{}{} since {}", d.owner, d.name, d.desc, d.since);
            } else {
                println!("  class {} since {}", d.owner, d.since);
            }
        }
        println!("(共 {} 条；真 JDK 的完整清单在 lib/jdeprscan 资源里)", DEPS.len());
        return;
    }
    let release: u32 = match args.value("--release") {
        Ok(Some(v)) => match v.parse() {
            Ok(n) if (8..=99).contains(&n) => n,
            _ => usage_err(TOOL, &format!("--release 的值不合法：{v}（要在 8..25 之间）")),
        },
        Ok(None) => 25,
        Err(e) => usage_err(TOOL, &e),
    };
    let for_removal_only = args.flag("--for-removal");
    let verbose = args.flag("-v") || args.flag("--verbose");
    let pos = match args.positionals(takes) {
        Ok(p) => p,
        Err(e) => usage_err(TOOL, &e),
    };
    if pos.is_empty() {
        usage_err(TOOL, "没有要扫描的 dir|jar|class");
    }

    let mut total = 0usize;
    for p in &pos {
        let path = Path::new(p);
        if !path.exists() {
            die(TOOL, &format!("路径不存在: {p}"));
        }
        match scan_one(path, release, for_removal_only, verbose) {
            Ok(n) => total += n,
            Err(e) => die(TOOL, &e),
        }
    }
    if verbose {
        eprintln!("{TOOL}: 共 {total} 处废弃 API 使用");
    }
}

fn header_for(path: &Path) -> String {
    if path.is_dir() {
        format!("Directory {}:", path.display())
    } else {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        match ext {
            "jar" | "jmod" | "zip" => format!("Jar {}:", path.display()),
            _ => format!("Class {}:", path.display()),
        }
    }
}

fn scan_one(
    path: &Path,
    release: u32,
    for_removal_only: bool,
    verbose: bool,
) -> Result<usize, String> {
    println!("{}", header_for(path));
    let (kind, classes) = scan::collect_classes(path, &[])?;
    if verbose {
        eprintln!("{TOOL}: {} 里 {} 个类（{:?}）", path.display(), classes.len(), kind);
    }
    // 输入自身声明的废弃成员：内部名 → 废弃的 (name, desc)
    let mut self_dep: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut self_dep_class: Vec<String> = Vec::new();
    let mut parsed = Vec::new();
    for e in &classes {
        let cf = match scan::parse_entry(e) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("{TOOL}: {err}");
                continue;
            }
        };
        let internal = e.internal_name();
        if vmless_tools::svuid::is_deprecated(&cf) {
            self_dep_class.push(internal.clone());
        }
        let mut deps = Vec::new();
        for m in cf.fields.iter().chain(cf.methods.iter()) {
            if vmless_tools::svuid::member_deprecated(&cf, m) {
                let name = cf
                    .utf8(m.name_index)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let desc = cf
                    .utf8(m.descriptor_index)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                deps.push((name, desc));
            }
        }
        if !deps.is_empty() {
            self_dep.insert(internal.clone(), deps);
        }
        parsed.push((internal, e.binary_name(), cf));
    }

    let mut hits = 0usize;
    for (internal, binary, cf) in &parsed {
        for (owner, name, desc, is_method) in scan::member_refs(cf) {
            // 1) 内置表
            let mut reported = false;
            for d in DEPS {
                if !d.is_method || !is_method {
                    continue;
                }
                if d.owner != owner {
                    continue;
                }
                if d.name != name || d.desc != desc {
                    continue;
                }
                if d.since > release {
                    continue;
                }
                if for_removal_only && !d.for_removal {
                    continue;
                }
                println!(
                    "class {binary} uses deprecated method {owner}::{name}{desc} (forRemoval={})",
                    d.for_removal
                );
                hits += 1;
                reported = true;
            }
            if reported {
                continue;
            }
            // 2) 输入自身的 @Deprecated
            if let Some(list) = self_dep.get(&owner) {
                if list.iter().any(|(n, ds)| *n == name && *ds == desc) {
                    println!(
                        "class {binary} uses deprecated {} {owner}::{name}{desc} (forRemoval=false)",
                        if is_method { "method" } else { "field" }
                    );
                    hits += 1;
                }
            }
        }
        // 3) 类型级：引用了输入里被标废弃的类
        for c in scan::class_refs(cf) {
            if self_dep_class.contains(&c) && c != *internal {
                if for_removal_only {
                    continue;
                }
                println!("class {binary} uses deprecated class {c} (forRemoval=false)");
                hits += 1;
            }
        }
    }
    let _ = release;
    Ok(hits)
}
