//! 从 jimage（JDK 的 `lib/modules`）里读真实 JDK 的类，只解析结构、不执行。
//!
//! 用法：
//! ```text
//! run_jimage [路径] [类名...]
//! run_jimage                      # 用本机 JDK 的 lib/modules
//! run_jimage D:\vmless\patch\img  # 用解包出来的目录树（模块名当一级子目录）
//! ```
//! 默认打印 `java/lang/Object`、`java/lang/String`、`java/util/HashMap`、
//! `jdk/internal/misc/Unsafe` 的结构统计，可直接和 `javap -v` 对照。
fn main() {
    let mut args = std::env::args().skip(1);
    let default_jdk = r"C:\Program Files\Eclipse Adoptium\jdk-25.0.4.101-hotspot\lib\modules";
    // 第一个参数是存在的路径就当来源；否则当成要看的类名（来源用本机 JDK）
    let mut extra: Vec<String> = Vec::new();
    let path = match args.next() {
        Some(p) if std::path::Path::new(&p).exists() => p,
        Some(p) => {
            extra.push(p);
            default_jdk.to_string()
        }
        None => default_jdk.to_string(),
    };
    extra.extend(args);

    let t = std::time::Instant::now();
    let mut cp = classfile::load::ClassPath::new();
    let p = std::path::Path::new(&path);
    let opened = if p.is_dir() {
        cp.push_module_tree(p)
    } else {
        cp.push(p)
    };
    if let Err(e) = opened {
        println!("打开 {path} 失败: {e}");
        std::process::exit(1);
    }
    let sources: Vec<(String, &'static str)> = cp.sources().map(|(p, k)| (p.display().to_string(), k)).collect();
    let shown: Vec<String> = sources
        .iter()
        .take(3)
        .map(|(p, k)| format!("{k}({p})"))
        .collect();
    let more = if sources.len() > 3 { format!(" …共 {} 个来源", sources.len()) } else { String::new() };

    let targets: Vec<String> = if extra.is_empty() {
        ["java/lang/Object", "java/lang/String", "java/util/HashMap", "jdk/internal/misc/Unsafe"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        extra
    };

    println!("来源: {}{more}", shown.join(", "));
    let mut ok = 0;
    for name in &targets {
        if stat(&cp, name) {
            ok += 1;
        }
    }
    let missing = targets.len() - ok;
    println!(
        "---- {ok} 个类取到并解析成功，{missing} 个没取到，耗时 {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    if missing > 0 {
        std::process::exit(1);
    }
}

fn class_at(cf: &classfile::ClassFile, index: u16) -> Option<String> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref())? {
        classfile::CpEntry::Class(n) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// 打印一个类的结构统计；取不到或解不开时返回 false
fn stat(cp: &classfile::load::ClassPath, name: &str) -> bool {
    let bytes = match cp.find(name) {
        Ok(Some(b)) => b,
        Ok(None) => {
            println!("{name:<38} 没取到（classpath 里没有这个类）");
            return false;
        }
        Err(e) => {
            println!("{name:<38} 取到但读不出: {e}");
            return false;
        }
    };
    let cf = match classfile::parse(&bytes) {
        Ok(cf) => cf,
        Err(e) => {
            println!("{name:<38} 字节取到了（{} 字节）但解析失败: {e}", bytes.len());
            return false;
        }
    };
    let this = cf.this_class_name().unwrap_or_default();
    let sup = if cf.super_class == 0 {
        "（无，java/lang/Object）".to_string()
    } else {
        class_at(&cf, cf.super_class).unwrap_or_else(|| format!("#{}", cf.super_class))
    };
    let ifaces: Vec<String> = cf
        .interfaces
        .iter()
        .filter_map(|i| class_at(&cf, *i))
        .collect();
    let src = cp.which(name).map(|p| p.display().to_string()).unwrap_or_default();
    println!(
        "{this}\n  父类 = {sup}\n  接口 = {} 个{}\n  方法 = {} 个，字段 = {} 个，常量池 = {} 项\n  class 版本 = {}.{}（major {}），access_flags = {:#06x}，{} 字节\n  来源 = {src}",
        ifaces.len(),
        if ifaces.is_empty() { String::new() } else { format!("（{}）", ifaces.join(", ")) },
        cf.methods.len(),
        cf.fields.len(),
        cf.constant_pool.len(),
        cf.major_version,
        cf.minor_version,
        cf.major_version,
        cf.access_flags,
        bytes.len()
    );
    true
}
