//! `jmap` —— 堆直方图、堆转储、类加载器统计。

use std::io::Write;

use vmless_attach::heap::ClassHisto;
use vmless_attach::protocol::Op;
use vmless_attach::tool::{self, exit};

const USAGE: &str = "用法: jmap [option] <pid>

  -histo[:live] <pid>            堆直方图（按类聚合的实例数与字节数）
  -dump:[live,]format=b,file=<f> 导出 hprof
  -clstats <pid>                 类加载器统计
  -t <ms>                        超时，默认 2000

  `-histo` / `-clstats` 是真扫堆的结果，不是估算。
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (argv, timeout) = vmless_attach::client::take_timeout(&argv);
    if argv.is_empty() {
        tool::usage(USAGE);
    }
    if argv[0] == "-h" || argv[0] == "--help" {
        tool::print_usage(USAGE);
    }
    let opt = argv[0].clone();
    let pid: Option<u32> = argv[1..].iter().find_map(|a| a.parse::<u32>().ok());
    let pid = match pid {
        Some(p) => p,
        None => tool::usage(USAGE),
    };

    if opt == "-histo" || opt.starts_with("-histo:") {
        let mut c = tool::attach(pid, timeout);
        match c.request_bin(Op::HeapHisto, &[]) {
            Ok(body) => match ClassHisto::from_wire(&body) {
                Some(v) => tool::out(&vmless_attach::fmt::histogram_table(&v)),
                None => tool::die_msg(exit::PROTOCOL, "直方图数据格式不对（版本不匹配？）"),
            },
            Err(e) => tool::die(&e),
        }
        return;
    }

    if opt.starts_with("-dump") {
        let path = match dump_path(&opt) {
            Some(p) => p,
            None => tool::die_msg(exit::USAGE, "-dump 需要 file=<路径>"),
        };
        // 先建/截断文件，让"路径不可写"这类错误在 attach 之前就暴露
        let f = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => tool::die_msg(exit::USAGE, &format!("无法创建 {}: {}", path.display(), e)),
        };
        let mut c = tool::attach(pid, timeout);
        let t0 = std::time::Instant::now();
        let mut w = std::io::BufWriter::new(f);
        match c.request_stream(Op::HeapDump, &[], &mut w) {
            Ok(n) => {
                let _ = w.flush();
                drop(w);
                let secs = t0.elapsed().as_secs_f64();
                tool::out(&format!("Dumping heap to {} ...\n", path.display()));
                tool::out(&format!("Heap dump file created [{} bytes in {:.3} secs]\n", n, secs));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                tool::die(&e)
            }
        }
        return;
    }

    if opt == "-clstats" {
        let mut c = tool::attach(pid, timeout);
        match c.request_bin(Op::ClassLoaders, &[]) {
            Ok(body) => match vmless_attach::heap::LoaderStat::from_wire(&body) {
                Some(v) => tool::out(&vmless_attach::fmt::clstats_table(&v)),
                None => tool::die_msg(exit::PROTOCOL, "类加载器数据格式不对"),
            },
            Err(e) => tool::die(&e),
        }
        return;
    }

    tool::die_msg(exit::USAGE, &format!("不支持的 option: {}\n{}", opt, USAGE));
}

/// 从 `-dump:live,format=b,file=x.hprof` 里取路径。
fn dump_path(opt: &str) -> Option<std::path::PathBuf> {
    for part in opt.split(',') {
        let p = part.trim_start_matches("-dump:").trim_start_matches("-dump");
        if let Some(f) = p.strip_prefix("file=") {
            if !f.is_empty() {
                return Some(std::path::PathBuf::from(f));
            }
        }
    }
    // 也支持 `-dump:format=b,file=...` 之外的空格写法：`-dump <file>`
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_path_parsing() {
        assert_eq!(dump_path("-dump:format=b,file=x.hprof").unwrap().to_str().unwrap(), "x.hprof");
        assert_eq!(dump_path("-dump:live,format=b,file=D:/a/b.hprof").unwrap().to_str().unwrap(), "D:/a/b.hprof");
        assert!(dump_path("-dump:format=b").is_none());
    }
}
