//! `serialver` —— 打印类的 `serialVersionUID`（按规范的 SHA-1 算法真算）。
//!
//! 与真 JDK 的差异：JDK 用反射 + 自己类加载器，我们用字节码直接算（不加载类）；
//! 因此 `Serializable` 判定走 classpath 上的继承链（父类/接口逐个查），不看运行中的 VM。

use vmless_tools::cli::Args;
use vmless_tools::svuid;
use vmless_tools::{die, usage_err, EXIT_USAGE};

const TOOL: &str = "serialver";

fn help() {
    println!("Usage: serialver [-classpath <classpath>] [-show] [classname...]");
    println!();
    println!("  -classpath <classpath>   class path where to look for the classes");
    println!("                           (default: '.', 另外始终会把 $JAVA_HOME/lib/modules 当兜底)");
    println!("  -show                    prints the serialVersionUID of the classes");
    println!("  -?, -h, --help           Print this help message");
    println!("  --version                Print version information");
    println!();
    println!("每个类打印一行：<类名>:    private static final long serialVersionUID = <值>L;");
    println!("不可序列化的类报 `Class <name> is not Serializable.` 到 stderr（退出码 1）。");
}

/// 默认 classpath：当前目录 +（若有）`$JAVA_HOME/lib/modules`
fn default_spec() -> String {
    let mut v = vec![".".to_string()];
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let m = format!("{}/lib/modules", jh.trim_end_matches(['/', '\\']));
        if std::path::Path::new(&m).is_file() {
            v.push(m);
        }
    }
    v.join(";")
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
    let takes: &[&str] = &["-classpath", "--classpath"];
    let known: &[&str] = &["-classpath", "--classpath", "-show", "-?", "-h", "--help", "--version"];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let spec = match args.value("-classpath") {
        Ok(Some(v)) => v,
        Ok(None) => default_spec(),
        Err(e) => usage_err(TOOL, &e),
    };
    let mut cp = match classfile::load::ClassPath::from_spec(&spec) {
        Ok(c) => c,
        Err(e) => die(TOOL, &e),
    };
    if cp.is_empty() {
        die(TOOL, &format!("classpath 为空：{spec}"));
    }
    let classes = match args.positionals(takes) {
        Ok(v) => v,
        Err(e) => usage_err(TOOL, &e),
    };
    if classes.is_empty() {
        usage_err(TOOL, "没有给类名");
    }

    let mut bad = 0;
    for name in classes {
        match one(&mut cp, &name) {
            Ok(Some(v)) => println!(
                "{name}:    private static final long serialVersionUID = {v}L;"
            ),
            Ok(None) => {
                eprintln!("Class {name} is not Serializable.");
                bad += 1;
            }
            Err(e) => {
                eprintln!("{TOOL}: {e}");
                bad += 1;
            }
        }
    }
    if bad > 0 {
        std::process::exit(1);
    }
}

fn one(cp: &mut classfile::load::ClassPath, name: &str) -> Result<Option<i64>, String> {
    let internal = name.replace('.', "/");
    let bytes = cp
        .find(&internal)?
        .ok_or_else(|| format!("找不到类 {name}（classpath: {:?}）", cp.sources().map(|(p, _)| p.to_path_buf()).collect::<Vec<_>>()))?;
    let cf = classfile::parse(&bytes).map_err(|e| format!("解析 {name} 失败: {e}"))?;
    // 类名以字节码里的为准（`serialver -classpath ... Foo` 与真实类名可能不同）
    let real = svuid::binary_name(&cf)?;
    if !is_serializable(cp, &real, 0)? {
        return Ok(None);
    }
    // 声明值优先（与 ObjectStreamClass.lookup 一致），没有才按规范算
    if let Some(v) = svuid::declared(&cf)? {
        return Ok(Some(v));
    }
    Ok(Some(svuid::compute_default(&cf)?))
}

/// 该类（含父类链与接口链）是否实现 `java.io.Serializable`
fn is_serializable(
    cp: &mut classfile::load::ClassPath,
    binary: &str,
    depth: usize,
) -> Result<bool, String> {
    if depth > 64 {
        return Ok(false);
    }
    if binary == "java.io.Serializable" {
        return Ok(true);
    }
    let internal = binary.replace('.', "/");
    let bytes = match cp.find(&internal)? {
        Some(b) => b,
        None => return Ok(false), // 不在 classpath 上：当作不可知 → 不可序列化
    };
    let cf = classfile::parse(&bytes).map_err(|e| format!("解析 {binary} 失败: {e}"))?;
    for i in svuid::interface_names(&cf)? {
        if is_serializable(cp, &i, depth + 1)? {
            return Ok(true);
        }
    }
    if let Some(s) = svuid::super_binary_name(&cf)? {
        if is_serializable(cp, &s, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}
