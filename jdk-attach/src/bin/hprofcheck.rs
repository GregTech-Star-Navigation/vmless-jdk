//! `hprofcheck` —— 校验 hprof 文件结构。
//!
//! `jhat` 在 JDK 9 之后就没有了，`jhsdb` 只能读活进程。所以"这个 dump 能不能被打开"
//! 只能靠一个**按规范写的严格解析器**来证明（见 [`vmless_attach::hprof`]）。
//!
//! 用法：
//! ```text
//! hprofcheck <file.hprof>        打印结构摘要；结构不合法则非 0 退出
//! hprofcheck --names <file>      顺带打印前若干个 STRING（看一眼类名对不对）
//! ```

use std::io::Read;

use vmless_attach::hprof;
use vmless_attach::tool::{self, exit};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        tool::print_usage("用法: hprofcheck [--names] <file.hprof>\n");
    }
    let mut names = false;
    let mut path = None;
    for a in argv {
        if a == "--names" {
            names = true;
        } else {
            path = Some(a);
        }
    }
    let path = match path {
        Some(p) => p,
        None => tool::die_msg(exit::USAGE, "缺少文件参数"),
    };
    let mut buf = Vec::new();
    match std::fs::File::open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.read_to_end(&mut buf) {
                tool::die_msg(exit::IO, &format!("读 {} 失败: {}", path, e));
            }
        }
        Err(e) => tool::die_msg(exit::IO, &format!("打不开 {}: {}", path, e)),
    }
    match hprof::inspect(&buf) {
        Ok(s) => {
            tool::out(&format!("file        = {}\n", path));
            tool::out(&format!("bytes       = {}\n", s.total_bytes));
            tool::out(&format!("id_size     = {}\n", s.id_size));
            tool::out(&format!("timestamp   = {}\n", s.timestamp_ms));
            tool::out(&format!("strings     = {}\n", s.strings));
            tool::out(&format!("load_class  = {}\n", s.load_classes));
            tool::out(&format!("class_dump  = {}\n", s.class_dumps));
            tool::out(&format!("instances   = {}\n", s.instance_dumps));
            tool::out(&format!("roots       = {}\n", s.sticky_roots));
            tool::out(&format!("segments    = {}\n", s.segments));
            tool::out(&format!("ended       = {}\n", s.ended));
            if names {
                tool::out("first strings:\n");
                for n in &s.first_strings {
                    tool::out(&format!("  {}\n", n));
                }
            }
            tool::out("OK: 结构合法（header + 记录链 + 堆段子记录 + HEAP_DUMP_END 全部通过）\n");
        }
        Err(e) => {
            tool::out(&format!("INVALID: {}\n", e));
            std::process::exit(exit::PROTOCOL);
        }
    }
}
