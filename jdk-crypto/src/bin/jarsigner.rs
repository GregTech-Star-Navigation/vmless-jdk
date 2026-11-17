//! `jarsigner` 的命令行入口（签名 / 验签），选项与真 JDK 对齐。

use std::process::ExitCode;

use vmless_crypto::hash::Alg;
use vmless_crypto::jar_sign::{sign_jar, verify_jar, SignOptions};
use vmless_crypto::pkcs12::P12;
use vmless_crypto::{sys, Error, Result};

const USAGE: &str = "\
用法: jarsigner [选项] jar 文件 别名
       jarsigner -verify [选项] jar 文件 [别名...]
       jarsigner -version

[-keystore <url>]           密钥库位置
[-storepass <口令>]         用于密钥库完整性的口令
[-storetype <类型>]         密钥库类型（只支持 PKCS12）
[-keypass <口令>]           专用密钥的口令（与密钥库口令相同）
[-sigfile <文件>]           .SF/.RSA 文件的名称
[-signedjar <文件>]         已签名的 JAR 文件的名称
[-digestalg <算法>]         摘要算法（默认 SHA-384，可 SHA-256/SHA-512/SHA-1）
[-sigalg <算法>]            签名算法（由摘要算法推导，目前只支持 *withRSA）
[-verify]                   验证已签名的 JAR 文件
[-verbose]                  签名/验证时输出详细信息
[-certs]                    验证时输出证书信息
[-help]                     显示此帮助
";

struct Args {
    keystore: String,
    storepass: Option<String>,
    storetype: String,
    sigfile: Option<String>,
    signedjar: Option<String>,
    digestalg: Alg,
    verify: bool,
    verbose: bool,
    certs: bool,
    rest: Vec<String>,
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut a = Args {
        keystore: default_keystore(),
        storepass: None,
        storetype: "PKCS12".to_string(),
        sigfile: None,
        signedjar: None,
        digestalg: Alg::Sha384,
        verify: false,
        verbose: false,
        certs: false,
        rest: Vec::new(),
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let mut next = |what: &str| -> Result<String> {
            if i + 1 >= argv.len() {
                return Err(Error::new(format!("{what} 缺少参数")));
            }
            i += 1;
            Ok(argv[i].clone())
        };
        match arg.as_str() {
            "-keystore" => a.keystore = next("-keystore")?,
            "-storepass" => a.storepass = Some(next("-storepass")?),
            "-storetype" => a.storetype = next("-storetype")?,
            "-sigfile" => a.sigfile = Some(next("-sigfile")?),
            "-signedjar" => a.signedjar = Some(next("-signedjar")?),
            "-digestalg" => {
                let v = next("-digestalg")?;
                a.digestalg = Alg::from_jca_name(&v)
                    .ok_or_else(|| Error::new(format!("不支持的摘要算法: {v}")))?;
            }
            "-sigalg" => {
                let v = next("-sigalg")?;
                let lower = v.to_ascii_lowercase();
                if !lower.ends_with("withrsa") {
                    return Err(Error::new(format!("不支持的签名算法: {v}（只支持 *withRSA）")));
                }
                if let Some(stem) = lower.strip_suffix("withrsa") {
                    if let Some(alg) = Alg::from_jca_name(stem) {
                        a.digestalg = alg;
                    }
                }
            }
            "-verify" => a.verify = true,
            "-verbose" => a.verbose = true,
            "-certs" => a.certs = true,
            "-help" | "-h" | "--help" | "-?" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-version" => {
                println!("jarsigner (vmless-crypto) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                return Err(Error::new(format!("无法识别的选项: {other}")));
            }
            other => a.rest.push(other.to_string()),
        }
        i += 1;
    }
    Ok(a)
}

fn default_keystore() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    format!("{home}/.keystore")
}

fn ask_pass() -> String {
    use std::io::Write;
    print!("输入密钥库口令: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        print!("{USAGE}");
        return ExitCode::from(1);
    }
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("jarsigner: {e}");
            return ExitCode::from(1);
        }
    };
    let result = if args.verify {
        run_verify(&args)
    } else {
        run_sign(&args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jarsigner: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_verify(args: &Args) -> Result<()> {
    if args.rest.is_empty() {
        return Err(Error::new("用法: jarsigner -verify [选项] jar 文件"));
    }
    let path = &args.rest[0];
    let bytes = std::fs::read(path).map_err(|e| Error::new(format!("读不到 {path}: {e}")))?;
    if args.verbose {
        println!("正在验证 {path}");
    }
    let report = verify_jar(&bytes)?;
    if args.verbose || args.certs {
        let cert = vmless_crypto::x509::Certificate::parse(&report.signer_cert_der)?;
        println!("{}", cert.describe());
        println!();
    }
    if args.verbose {
        println!("签名者: {}", report.signer);
        println!("签名算法: {}, 摘要算法: {}", report.signature_alg, report.digest_alg);
        for (name, ok, note) in &report.entries {
            println!("  {} {}", if *ok { "已签名" } else { "未通过" }, name);
            if !note.is_empty() {
                println!("       {note}");
            }
        }
    }
    for w in &report.warnings {
        println!("警告: {w}");
    }
    if report.all_ok() {
        println!("jar 已验证。");
        Ok(())
    } else if report.manifest_ok {
        println!("jar 已验证，但含有未签名的条目。");
        Err(Error::new("存在未签名的条目"))
    } else {
        Err(Error::new("签名校验失败：清单或条目摘要不匹配"))
    }
}

fn run_sign(args: &Args) -> Result<()> {
    if args.rest.len() < 2 {
        return Err(Error::new("用法: jarsigner [选项] jar 文件 别名"));
    }
    let path = &args.rest[0];
    let alias = &args.rest[1];
    if !args.storetype.eq_ignore_ascii_case("pkcs12")
        && !args.storetype.eq_ignore_ascii_case("pkcs12store")
    {
        return Err(Error::new(format!(
            "只支持 PKCS12 密钥库（-storetype {}）",
            args.storetype
        )));
    }
    let storepass = match &args.storepass {
        Some(p) => p.clone(),
        None => ask_pass(),
    };
    let ks_bytes =
        std::fs::read(&args.keystore).map_err(|e| Error::new(format!("读不到密钥库 {}: {e}", args.keystore)))?;
    let p12 = P12::load(&ks_bytes, &storepass)?;
    let entry = p12
        .find(alias)
        .ok_or_else(|| Error::new(format!("别名 <{alias}> 不存在")))?
        .clone();
    let key = entry
        .private_key
        .clone()
        .ok_or_else(|| Error::new(format!("别名 <{alias}> 没有私钥，无法签名")))?;
    if entry.chain.is_empty() {
        return Err(Error::new(format!("别名 <{alias}> 没有证书链")));
    }
    let chain: Vec<Vec<u8>> = entry.chain.iter().map(|c| c.der.clone()).collect();
    let jar = std::fs::read(path).map_err(|e| Error::new(format!("读不到 {path}: {e}")))?;
    let mut opt = SignOptions::new(alias, key, chain);
    opt.digest_alg = args.digestalg;
    if let Some(sigfile) = &args.sigfile {
        opt.alias = sigfile.clone();
    }
    if args.verbose {
        println!("使用 {} 签名 {path}", opt.digest_alg.jca_name());
    }
    let signed = sign_jar(&jar, &opt)?;
    let out = args.signedjar.clone().unwrap_or_else(|| path.clone());
    std::fs::write(&out, &signed).map_err(|e| Error::new(format!("写不到 {out}: {e}")))?;
    if args.verbose {
        println!("已写入 {out}（{} 字节）", signed.len());
    }
    let _ = sys::now_unix();
    Ok(())
}
