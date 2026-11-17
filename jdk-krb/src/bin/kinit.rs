//! `kinit`：拿 TGT 并写进 MIT `FILE:` 凭据缓存。

use std::process::ExitCode;

use vmless_krb::ccache::{default_cache_path, CcCache};
use vmless_krb::client::{self, KdcAddr, KinitOptions};
use vmless_krb::crypto_krb;
use vmless_crypto::Error;

const USAGE: &str = "\
用法: kinit [选项] [principal]

  principal                形如 user 或 user@REALM；省略则取环境变量 KRB5_PRINCIPAL
  -c <cache>               凭据缓存（默认取 KRB5CCNAME，否则临时目录下的 krb5cc_<user>）
  --realm <REALM>          默认 realm（环境变量 KRB5_REALM）
  -S, --kdc <host[:port]>  KDC 地址（环境变量 KRB5_KDC，默认 127.0.0.1:88）
  -l <seconds>             票有效期（默认 36000）
  -r <seconds>             可续期上限（默认 604800）
  -e <etypes>              允许的加密类型，逗号分隔（默认 18,17）
  -f                       要 forwardable 票
  -p                       要 proxiable 票
  -t <keytab>              用 keytab 里的密钥代替口令
  -k                       等价于给了 -t 时使用 keytab（兼容 MIT 写法）
  --password <pw>          直接在命令行给口令（脚本用；注意会留在 shell 历史里）
  --tgs <service>          拿到 TGT 后再换一张服务票（如 host/localhost）并追加进缓存
  --tcp                    强制用 TCP（默认先 UDP，失败或包太大才退 TCP）
  -V, --verbose            打印过程
  -h, --help               显示本帮助

口令从标准输入读一行（管道里跑也能用）；`-` 作为 principal 之后的位置参数表示从 stdin 读。
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kinit: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") || argv.is_empty() {
        print!("{USAGE}");
        return if argv.is_empty() { Err(Error::new("缺少 principal")) } else { Ok(()) };
    }

    let mut realm = std::env::var("KRB5_REALM").unwrap_or_default();
    let mut kdc = std::env::var("KRB5_KDC").unwrap_or_else(|_| "127.0.0.1:88".to_string());
    let mut cache = default_cache_path();
    let mut lifetime: i64 = vmless_krb::msg::DEFAULT_LIFETIME;
    let mut renewable: i64 = vmless_krb::msg::DEFAULT_RENEW_LIFETIME;
    let mut etypes: Vec<i32> = vec![crypto_krb::ETYPE_AES256, crypto_krb::ETYPE_AES128];
    let mut forwardable = false;
    let mut proxiable = false;
    let mut keytab: Option<String> = None;
    let mut tgs: Option<String> = None;
    let mut force_tcp = false;
    let mut verbose = false;
    let mut principal: Option<String> = None;
    let mut password: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        let mut next = |what: &str| -> Result<String, Error> {
            if i + 1 >= argv.len() {
                return Err(Error::new(format!("{what} 缺少参数")));
            }
            i += 1;
            Ok(argv[i].clone())
        };
        match a {
            "-c" => cache = next("-c")?,
            "--realm" => realm = next("--realm")?,
            "-S" | "--kdc" => kdc = next("--kdc")?,
            "-l" => {
                let v = next("-l")?;
                lifetime = parse_secs(&v)?;
            }
            "-r" => {
                let v = next("-r")?;
                renewable = parse_secs(&v)?;
            }
            "-e" => {
                let v = next("-e")?;
                let mut list = Vec::new();
                for part in v.split([',', ' ']).filter(|s| !s.is_empty()) {
                    match crypto_krb::etype_from_name(part) {
                        Some(e) if crypto_krb::is_supported(e) => list.push(e),
                        Some(e) => {
                            return Err(Error::new(format!(
                                "不支持的加密类型 {part}（{}）",
                                crypto_krb::etype_name(e)
                            )))
                        }
                        None => return Err(Error::new(format!("不认识的加密类型 {part}"))),
                    }
                }
                if list.is_empty() {
                    return Err(Error::new("-e 至少要给一个加密类型"));
                }
                etypes = list;
            }
            "-f" => forwardable = true,
            "-p" => proxiable = true,
            "-t" => keytab = Some(next("-t")?),
            "--tgs" => tgs = Some(next("--tgs")?),
            "--tcp" => force_tcp = true,
            "-k" => {}
            "--password" => password = Some(next("--password")?),
            "-V" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-" => {
                // 位置参数 `-`：从 stdin 读口令
                if principal.is_none() {
                    principal = Some("-".to_string());
                }
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(Error::new(format!("无法识别的选项 {other}（用 -h 看用法）")))
            }
            other => {
                if principal.is_none() {
                    principal = Some(other.to_string());
                } else if password.is_none() {
                    password = Some(other.to_string()); // 兼容 JDK 的 `kinit principal password`
                } else {
                    return Err(Error::new(format!("多余的参数 {other}")));
                }
            }
        }
        i += 1;
    }

    let principal = principal
        .or_else(|| std::env::var("KRB5_PRINCIPAL").ok())
        .ok_or_else(|| Error::new("缺少 principal（用 -h 看用法）"))?;
    if principal == "-" {
        return Err(Error::new(
            "principal 不能是 `-`：请写成 `kinit user@REALM -` 让口令从 stdin 读",
        ));
    }
    vmless_krb::ccache::check_cache_type(&cache)?;
    let addr = KdcAddr::parse(&kdc)?;

    // 口令：命令行给了就用，否则走 stdin（read_password 会处理提示）
    let mut opts = KinitOptions::new(&realm, addr.clone(), &cache);
    opts.etypes = etypes;
    opts.lifetime = lifetime;
    opts.renewable = renewable;
    opts.forwardable = forwardable;
    opts.proxiable = proxiable;
    opts.keytab = keytab;
    opts.force_tcp = force_tcp;
    opts.verbose = verbose;

    // 若给了口令，塞进环境让 client 直接读（避免再问一次）
    if let Some(pw) = password {
        // 不用环境变量传口令（会泄漏给子进程），改用一次性全局
        vmless_krb::client::set_oneshot_password(pw);
    }

    let res = client::kinit(&principal, &opts)?;
    let tgt = res.cache.tgt().ok_or_else(|| Error::new("内部错误：写进缓存却没有 TGT"))?;
    println!(
        "kinit: 已拿到 TGT，缓存 {}（{}，{} 轮，{}ms，经 {}）",
        cache,
        crypto_krb::etype_name(res.etype),
        res.rounds,
        res.ms,
        match res.via {
            client::Transport::Udp => "UDP",
            client::Transport::Tcp => "TCP",
        }
    );
    println!(
        "  {}  ->  {}  （有效期至 {}）",
        tgt.client.display(),
        tgt.server.display(),
        vmless_crypto::sys::utc_stamp(tgt.endtime)
    );

    // 可选：顺带用 TGT 换一张服务票（等价于 MIT 的 kgetcred）
    if let Some(svc) = tgs {
        let (cred, stype, ms) = client::get_service_ticket(&res.cache, &svc, &addr, opts.timeout_ms)?;
        let mut cc = res.cache.clone();
        cc.creds.push(cred.clone());
        cc.write_file(&cache)?;
        println!(
            "kinit: 已换到服务票 {}（{}，{}ms）并追加进缓存",
            cred.server.display(),
            crypto_krb::etype_name(stype),
            ms
        );
    }

    let _ = CcCache::read_file(&cache)?; // 读回来确认确实写对了
    Ok(())
}

/// `10h` / `600` / `30m` / `7d` 这类写法都收。
fn parse_secs(s: &str) -> Result<i64, Error> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::new("时间长度为空"));
    }
    let (num, mult) = match t.chars().last().unwrap() {
        's' => (&t[..t.len() - 1], 1),
        'm' => (&t[..t.len() - 1], 60),
        'h' => (&t[..t.len() - 1], 3600),
        'd' => (&t[..t.len() - 1], 86400),
        _ => (t, 1),
    };
    let v: i64 = num
        .trim()
        .parse()
        .map_err(|_| Error::new(format!("时间长度不合法: {s}")))?;
    if v <= 0 {
        return Err(Error::new(format!("时间长度必须为正: {s}")));
    }
    Ok(v * mult)
}
