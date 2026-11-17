//! `klist`：列出凭据缓存里的票（`-e` 显示 etype、`-f` 显示 flags）。

use std::process::ExitCode;

use vmless_krb::ccache::{default_cache_path, CcCache};
use vmless_krb::client::format_klist;
use vmless_crypto::{sys, Error};

const USAGE: &str = "\
用法: klist [选项]

  -c <cache>      凭据缓存（默认取 KRB5CCNAME，否则临时目录下的 krb5cc_<user>）
  -e              显示加密类型
  -f              显示票的标志位
  -a              显示地址（本实现不收地址，仅作兼容接受）
  -l              显示缓存文件名（默认行为）
  -h, --help      显示本帮助

退出码：0 有票；1 出错（缓存不存在/格式不对）；2 缓存是空的。
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("klist: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let mut cache = default_cache_path();
    let mut show_flags = false;
    let mut show_etype = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-c" => {
                if i + 1 >= argv.len() {
                    return Err(Error::new("-c 缺少参数"));
                }
                i += 1;
                cache = argv[i].clone();
            }
            "-e" => show_etype = true,
            "-f" => show_flags = true,
            "-a" | "-n" | "-l" => {}
            other => return Err(Error::new(format!("无法识别的选项 {other}（用 -h 看用法）"))),
        }
        i += 1;
    }
    vmless_krb::ccache::check_cache_type(&cache)?;
    let path = vmless_krb::ccache::strip_file_prefix(&cache);
    let cc = CcCache::read_file(&path)?;
    print!("{}", format_klist(&cc, &path, show_flags, show_etype, sys::now_unix()));
    if cc.creds.iter().all(|c| c.is_config_entry()) {
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}
