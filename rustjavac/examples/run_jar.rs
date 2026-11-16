//! 从**真 jar**（zip，deflate 压缩）里跑一个类的 main —— 证明 classpath 不再只是目录。
//!
//! 用法：
//! ```text
//! run_jar <jar 文件>             # 主类从 MANIFEST.MF 的 Main-Class 读
//! run_jar <jar 文件> <主类名>     # 也可以显式指定（a.b.C 或 a/b/C）
//! run_jar                        # 用 D:\vmless\jar_demo\hello.jar（造好存在的样例）
//! ```
fn main() {
    let mut args = std::env::args().skip(1);
    let default = r"D:\vmless\jar_demo\hello.jar";
    let jar = args.next().unwrap_or_else(|| default.to_string());
    let main = args.next();
    if !std::path::Path::new(&jar).exists() {
        println!("找不到 jar: {jar}");
        std::process::exit(2);
    }
    // 先把这个 jar 的情况打出来（条目数 / 压缩方法 / Main-Class），跑不起来时好定位
    match classfile::load::jar::JarSource::open(&jar) {
        Ok(j) => {
            let main_class = j.main_class();
            println!(
                "jar: {jar}（{} 个条目，MANIFEST 的 Main-Class = {:?}）",
                j.entry_count(),
                main_class.as_ref().ok().and_then(|m| m.clone())
            );
        }
        Err(e) => {
            println!("打开 jar 失败: {e}");
            std::process::exit(1);
        }
    }
    match classfile::vm::run_jar(&jar, main.as_deref()) {
        Ok(r) => {
            for line in &r.output {
                println!("{line}");
            }
            if let Some(exc) = &r.exception {
                eprintln!("{exc}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("执行失败: {}", e.msg);
            std::process::exit(1);
        }
    }
}
