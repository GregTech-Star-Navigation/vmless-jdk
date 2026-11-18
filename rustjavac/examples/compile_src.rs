//! 源码 → 字节码：读 .java，写出 .class。
//!
//! 用法：
//! ```text
//! compile_src <源文件> <输出目录> [classpath 目录或 jar ...]
//! ```
//! 输出目录**总是**参与查找（自己的中间产物在里面），额外的 classpath 用来解析
//! `java.util.List` 这类来自 JDK 的类 —— 没有它，任何用到 JDK 集合/IO/并发的源文件
//! 都会报「未找到实例方法」（因为接收者的静态类型压根解析不出来）。
fn main() {
    let mut it = std::env::args().skip(1);
    let src_path = match it.next() {
        Some(v) => v,
        None => {
            eprintln!("用法: compile_src <源文件> <输出目录> [classpath ...]");
            std::process::exit(2);
        }
    };
    let out_dir = match it.next() {
        Some(v) => v,
        None => {
            eprintln!("用法: compile_src <源文件> <输出目录> [classpath ...]");
            std::process::exit(2);
        }
    };
    let mut classpath: Vec<std::path::PathBuf> = vec![out_dir.clone().into()];
    classpath.extend(it.map(std::path::PathBuf::from));
    // 没显式给 classpath 时退回 `$VMLESS_JDK`（与 run_jdk 用同一份，避免两边不一致）
    if classpath.len() == 1 {
        if let Some(p) = std::env::var_os("VMLESS_JDK") {
            let p = std::path::PathBuf::from(p);
            let sub = p.join("java.base");
            if sub.is_dir() { classpath.push(sub); }
            if p.is_dir() { classpath.push(p); }
        }
    }
    let src = std::fs::read_to_string(&src_path).expect("读源码");
    let cu = classfile::parser::parse_compilation_unit(&src).expect("解析失败");
    // 输出目录同时当 classpath：父类在别的编译单元时从这里读它的 .class
    let classes = match classfile::compiler::compile_with_path(&cu, &classpath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("编译失败: {}", e.msg);
            std::process::exit(1);
        }
    };
    for (name, bytes) in &classes {
        // 带包名的类要落到 `out/p/C.class`（JVM 的 -cp 目录按这个布局找类）
        let file = format!("{}/{}.class", out_dir, name);
        if let Some(dir) = std::path::Path::new(&file).parent() {
            std::fs::create_dir_all(dir).expect("建输出目录");
        }
        std::fs::write(&file, bytes).expect("写出");
        println!("{name}: {} 字节 → {file}", bytes.len());
    }
}
