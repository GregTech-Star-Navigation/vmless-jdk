//! 最小 KDC（验收用，也是真实现）。
//!
//! ```text
//! kdc --realm VMKRB.LOCAL --port 88 --keytab krb5.keytab --verbose
//! kdc --realm VMKRB.LOCAL --add testuser:secretpw --add krbtgt/VMKRB.LOCAL:krbtgt-pw
//! kdc --realm VMKRB.LOCAL --keytab krb5.keytab --dump      # 只打印数据库，不监听
//! ```

use std::process::ExitCode;

use vmless_krb::kdc::{read_keytab_file, Kdb, Kdc, KdcConfig};
use vmless_crypto::{sys, Error};

const USAGE: &str = "\
用法: kdc [选项]

  --realm <REALM>           必填，本 KDC 服务的 realm
  -p, --port <port>         监听端口（UDP+TCP 同一个端口，默认 88）
  -k, --keytab <file>       从 MIT keytab 载入 principal（可重复）
  --add <name:password>     用口令现算密钥加一个 principal（可重复）；
                            name 用 `user` 或 `krbtgt/REALM` 这类写法，realm 取 --realm
  --dump                    只打印数据库内容然后退出（不监听）
  -V, --verbose             打印每次请求的处理结果
  -h, --help                显示本帮助

例：
  ktab -a krbtgt/VMKRB.LOCAL@VMKRB.LOCAL -k krb5.keytab -p krbtgt-pw
  ktab -a testuser@VMKRB.LOCAL -k krb5.keytab -p secretpw
  kdc --realm VMKRB.LOCAL --port 88 --keytab krb5.keytab --verbose
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kdc: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let mut realm = String::new();
    let mut port: u16 = 88;
    let mut keytabs: Vec<String> = Vec::new();
    let mut adds: Vec<String> = Vec::new();
    let mut dump = false;
    let mut verbose = false;

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
            "--realm" => realm = next("--realm")?,
            "-p" | "--port" => {
                let v = next("--port")?;
                port = v
                    .parse()
                    .map_err(|_| Error::new(format!("端口不合法: {v}")))?;
            }
            "-k" | "--keytab" => keytabs.push(next("--keytab")?),
            "--add" => adds.push(next("--add")?),
            "--dump" => dump = true,
            "-V" | "--verbose" => verbose = true,
            other => return Err(Error::new(format!("无法识别的选项 {other}（用 -h 看用法）"))),
        }
        i += 1;
    }
    if realm.is_empty() {
        return Err(Error::new("必须给 --realm"));
    }

    let mut db = Kdb::new(&realm);
    for kt in &keytabs {
        let entries = read_keytab_file(kt)?;
        let n = db.load_keytab(&entries)?;
        if verbose {
            eprintln!("kdc: 从 {kt} 载入 {n} 条密钥");
        }
    }
    for spec in &adds {
        let (name, pw) = spec
            .rsplit_once(':')
            .ok_or_else(|| Error::new(format!("--add 的写法应为 name:password，收到 {spec}")))?;
        let components: Vec<String> = name.split('/').map(|s| s.to_string()).collect();
        db.add_password(&components, pw, 1)?;
        if verbose {
            eprintln!("kdc: 口令建 principal {}", name);
        }
    }
    if db.principals.is_empty() {
        return Err(Error::new(
            "数据库是空的：至少要有 krbtgt/REALM（用 --keytab 或 --add）",
        ));
    }
    if db.krbtgt().is_none() {
        return Err(Error::new(format!(
            "数据库里没有 krbtgt/{realm}：AS-REQ 没法签票"
        )));
    }

    if dump {
        println!("realm: {realm}");
        println!("{:<40} {:>5}  {}", "principal", "kvno", "etypes");
        for p in &db.principals {
            let ets: Vec<String> = p
                .etypes()
                .iter()
                .map(|e| vmless_krb::crypto_krb::etype_name(*e).to_string())
                .collect();
            println!("{:<40} {:>5}  {}", p.display(), p.kvno, ets.join(", "));
        }
        return Ok(());
    }

    let mut cfg = KdcConfig::new(&realm).with_db(db);
    cfg.verbose = verbose;
    if verbose {
        eprintln!("kdc: 启动于 {}，realm={realm}", sys::utc_stamp(sys::now_unix()));
    }
    Kdc::new(cfg).serve(port)
}
