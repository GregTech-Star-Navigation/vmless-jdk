//! `keytool` 的命令行入口：`-list` / `-genkeypair` / `-exportcert` / `-importcert` /
//! `-printcert` / `-delete`。

use std::process::ExitCode;

use vmless_crypto::asn1::Name;
use vmless_crypto::hash::Alg;
use vmless_crypto::pkcs12::{P12, P12Entry};
use vmless_crypto::rsa;
use vmless_crypto::x509::{build_self_signed, Certificate, CertSpec};
use vmless_crypto::{sys, Error, Result};

const USAGE: &str = "\
密钥和证书管理工具

命令:

 -certreq            生成证书请求
 -delete             删除条目
 -exportcert         导出证书
 -genkeypair         生成密钥对
 -importcert         导入证书或证书链
 -list               列出密钥库中的条目
 -printcert          打印证书内容
 -version            显示程序版本

使用 \"keytool -?, -h, or --help\" 可以获取此帮助信息
使用 \"keytool -command_name --help\" 可获取 command_name 的用法。
";

#[derive(Default)]
struct Opts {
    command: String,
    keystore: Option<String>,
    storepass: Option<String>,
    storetype: String,
    alias: Option<String>,
    dname: Option<String>,
    validity: Option<i64>,
    keyalg: String,
    keysize: Option<usize>,
    sigalg: Option<String>,
    file: Option<String>,
    rfc: bool,
    verbose: bool,
    keypass: Option<String>,
    ext_san: Vec<String>,
}

fn parse(argv: &[String]) -> Result<Opts> {
    let mut o = Opts {
        storetype: "PKCS12".into(),
        keyalg: "RSA".into(),
        validity: None,
        ..Default::default()
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
            "-list" | "-genkeypair" | "-genkey" | "-exportcert" | "-importcert" | "-printcert"
            | "-delete" | "-version" | "-help" | "-h" | "--help" | "-?" => {
                o.command = arg.trim_start_matches('-').to_string();
            }
            "-keystore" => o.keystore = Some(next("-keystore")?),
            "-storepass" => o.storepass = Some(next("-storepass")?),
            "-storetype" => o.storetype = next("-storetype")?,
            "-alias" => o.alias = Some(next("-alias")?),
            "-dname" => o.dname = Some(next("-dname")?),
            "-validity" => o.validity = Some(next("-validity")?.parse::<i64>().unwrap_or(365)),
            "-keyalg" => o.keyalg = next("-keyalg")?,
            "-keysize" => o.keysize = next("-keysize")?.parse::<usize>().ok(),
            "-sigalg" => o.sigalg = Some(next("-sigalg")?),
            "-file" => o.file = Some(next("-file")?),
            "-keypass" => o.keypass = Some(next("-keypass")?),
            "-rfc" => o.rfc = true,
            "-v" => o.verbose = true,
            "-ext" => {
                let v = next("-ext")?;
                if let Some(san) = v.strip_prefix("SAN=") {
                    o.ext_san.extend(san.split(',').map(|s| s.trim().to_string()));
                }
            }
            other if other.starts_with('-') => {
                return Err(Error::new(format!("无法识别的选项: {other}")));
            }
            _ => {}
        }
        i += 1;
    }
    Ok(o)
}

fn keystore_path(o: &Opts) -> String {
    if let Some(k) = &o.keystore {
        return k.clone();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    format!("{home}/.keystore")
}

fn storepass(o: &Opts) -> String {
    if let Some(p) = &o.storepass {
        return p.clone();
    }
    use std::io::Write;
    print!("输入密钥库口令: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

/// 按文件头判断密钥库类型。只有 PKCS12 后端，所以这个函数存在的意义是**说出真话**：
/// 拿一个 JKS 进来时，`P12::load` 只会吐一句「ASN.1 解析失败」，用户根本猜不到是格式不对。
fn store_type_of(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFE, 0xED, 0xFE, 0xED, ..] => "JKS",
        [0x30, 0x82, ..] | [0x30, 0x81, ..] => "PKCS12",
        _ => "未知格式",
    }
}

/// `-storetype` 只接受 PKCS12。
///
/// 为什么不"接受 JKS 但悄悄写 PKCS12"：那是**静默的错误输出** —— 脚本拿真 JKS 读会炸，
/// 而炸点离这里很远。JKS 自己的私有 KeyProtector（基于 SHA-1 的 XOR 掩码）没有实现，
/// 与其假装支持，不如当场说清。
fn check_storetype(o: &Opts) -> Result<()> {
    let t = o.storetype.trim().to_ascii_uppercase();
    match t.as_str() {
        "" | "PKCS12" | "PKCS#12" | "P12" => Ok(()),
        "JKS" | "JCEKS" => Err(Error::new(format!(
            "-storetype {t} 不受支持：本实现只有 PKCS12 后端（JKS/JCEKS 的私有 KeyProtector 未实现）。\
             去掉 -storetype，或写 -storetype PKCS12。"
        ))),
        other => Err(Error::new(format!(
            "-storetype {other} 不受支持：只支持 PKCS12"
        ))),
    }
}

fn load_store(o: &Opts) -> Result<(P12, String)> {
    let path = keystore_path(o);
    let pass = storepass(o);
    let bytes = std::fs::read(&path)
        .map_err(|e| Error::new(format!("密钥库文件不存在: {path} ({e})")))?;
    if store_type_of(&bytes) == "JKS" {
        return Err(Error::new(format!(
            "{path} 是 JKS 密钥库（文件头 FE ED FE ED），本实现只能读 PKCS12"
        )));
    }
    Ok((P12::load(&bytes, &pass)?, pass))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        print!("{USAGE}");
        return ExitCode::from(1);
    }
    let o = match parse(&argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("keytool 错误: {e}");
            return ExitCode::from(1);
        }
    };
    // 读写密钥库的命令才校验 -storetype（-version / -printcert 与密钥库无关）
    match o.command.as_str() {
        "list" | "genkeypair" | "genkey" | "exportcert" | "importcert" | "delete" => {
            if let Err(e) = check_storetype(&o) {
                eprintln!("keytool 错误: {e}");
                return ExitCode::from(1);
            }
        }
        _ => {}
    }
    let result = match o.command.as_str() {
        "" | "help" | "h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "version" => {
            println!("keytool (vmless-crypto) {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        "list" => cmd_list(&o),
        "genkeypair" | "genkey" => cmd_genkeypair(&o),
        "exportcert" => cmd_exportcert(&o),
        "importcert" => cmd_importcert(&o),
        "printcert" => cmd_printcert(&o),
        "delete" => cmd_delete(&o),
        other => Err(Error::new(format!("不支持的命令 -{other}"))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("keytool 错误: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_list(o: &Opts) -> Result<()> {
    // 类型按文件内容说，不回显 -storetype：那只是用户的请求，不是事实
    let detected = std::fs::read(keystore_path(o))
        .map(|b| store_type_of(&b))
        .unwrap_or("未知格式");
    let (p12, _) = load_store(o)?;
    println!("密钥库类型: {detected}");
    println!("密钥库提供方: SUN");
    println!();
    if p12.entries.is_empty() {
        println!("密钥库不包含任何条目");
        return Ok(());
    }
    let filtered: Vec<&P12Entry> = match &o.alias {
        Some(a) => p12
            .entries
            .iter()
            .filter(|e| e.alias.eq_ignore_ascii_case(a))
            .collect(),
        None => p12.entries.iter().collect(),
    };
    if filtered.is_empty() {
        return Err(Error::new(format!(
            "别名 <{}> 不存在",
            o.alias.clone().unwrap_or_default()
        )));
    }
    println!("您的密钥库包含 {} 个条目", filtered.len());
    println!();
    for e in filtered {
        let kind = if e.private_key.is_some() {
            "PrivateKeyEntry"
        } else {
            "trustedCertEntry"
        };
        let date = match e.chain.first() {
            Some(c) => sys::human_stamp(c.not_before, sys::local_offset_secs()),
            None => "-".into(),
        };
        let date = date.split(' ').next().unwrap_or("-").to_string();
        println!("{}, {}, {}", e.alias, date, kind);
        if let Some(c) = e.chain.first() {
            let fp = vmless_crypto::pkcs12::fingerprint(c, Alg::Sha256);
            println!("证书指纹 (SHA-256): {fp}");
            if o.verbose {
                println!();
                println!("{}", c.describe());
            }
        }
        println!();
    }
    Ok(())
}

fn cmd_genkeypair(o: &Opts) -> Result<()> {
    let alias = o
        .alias
        .clone()
        .ok_or_else(|| Error::new("必须指定 -alias"))?;
    let dname = o
        .dname
        .clone()
        .ok_or_else(|| Error::new("必须指定 -dname，例如 -dname \"CN=test, O=org, C=CN\""))?;
    if !o.keyalg.eq_ignore_ascii_case("rsa") {
        return Err(Error::new(format!("只支持 -keyalg RSA（收到 {}）", o.keyalg)));
    }
    let bits = o.keysize.unwrap_or(2048);
    if bits != 2048 && bits != 3072 && bits != 4096 && bits != 1024 {
        return Err(Error::new(format!("不支持的 RSA 位数 {bits}")));
    }
    let path = keystore_path(o);
    let pass = storepass(o);
    let mut p12 = match std::fs::read(&path) {
        Ok(bytes) => P12::load(&bytes, &pass)?,
        Err(_) => P12::default(),
    };
    if p12.find(&alias).is_some() {
        return Err(Error::new(format!(
            "别名 <{alias}> 已存在（本实现不覆盖已有条目）"
        )));
    }
    let dn = Name::parse_dn_string(&dname)?;
    let key = rsa::generate(bits, 65537)?;
    let days = o.validity.unwrap_or(90);
    let mut spec = CertSpec::new(dn.clone(), days);
    spec.san = o.ext_san.clone();
    if let Some(sigalg) = &o.sigalg {
        let lower = sigalg.to_ascii_lowercase();
        if let Some(stem) = lower.strip_suffix("withrsa") {
            spec.sig_alg = Alg::from_jca_name(stem)
                .ok_or_else(|| Error::new(format!("不支持的签名算法 {sigalg}")))?;
        }
    } else {
        spec.sig_alg = Alg::Sha384; // 与 JDK 25 keytool 的默认一致
    }
    spec.key_usage = Some(vec![true, false, true]);
    let cert_der = build_self_signed(&key, &spec)?;
    let cert = Certificate::parse(&cert_der)?;
    let key_id = Alg::Sha1.digest(&cert_der);
    p12.entries.push(P12Entry {
        alias: alias.clone(),
        key_id,
        chain: vec![cert],
        private_key: Some(key),
    });
    let bytes = p12.store(&pass)?;
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, &bytes).map_err(|e| Error::new(format!("写密钥库失败: {e}")))?;
    println!("正在为以下对象生成 {} 位 RSA 密钥对和自签名证书 (SHA384withRSA):", bits);
    println!("\t{}", dn.rfc2253());
    println!("已写入 {}", path);
    Ok(())
}

fn cmd_exportcert(o: &Opts) -> Result<()> {
    let (p12, _) = load_store(o)?;
    let alias = o.alias.clone().ok_or_else(|| Error::new("必须指定 -alias"))?;
    let entry = p12
        .find(&alias)
        .ok_or_else(|| Error::new(format!("别名 <{alias}> 不存在")))?;
    let cert = entry.leaf()?;
    if o.rfc {
        let pem = sys::pem_encode("CERTIFICATE", &cert.der);
        match &o.file {
            Some(f) => std::fs::write(f, pem.as_bytes())
                .map_err(|e| Error::new(format!("写 {f} 失败: {e}")))?,
            None => print!("{pem}"),
        }
    } else {
        match &o.file {
            Some(f) => std::fs::write(f, &cert.der)
                .map_err(|e| Error::new(format!("写 {f} 失败: {e}")))?,
            None => println!("{}", sys::base64_encode(&cert.der)),
        }
    }
    if o.verbose {
        eprintln!("已存储在文件 {}", o.file.clone().unwrap_or_else(|| "<stdout>".into()));
    }
    Ok(())
}

fn cmd_printcert(o: &Opts) -> Result<()> {
    let file = o
        .file
        .clone()
        .ok_or_else(|| Error::new("必须指定 -file"))?;
    let bytes = std::fs::read(&file).map_err(|e| Error::new(format!("读不到 {file}: {e}")))?;
    let der = if bytes.starts_with(b"-----BEGIN") {
        let text = String::from_utf8_lossy(&bytes).to_string();
        sys::pem_decode(&text, Some("CERTIFICATE"))?
    } else {
        bytes
    };
    let cert = Certificate::parse(&der)?;
    print!("{}", cert.describe());
    Ok(())
}

fn cmd_importcert(o: &Opts) -> Result<()> {
    let alias = o.alias.clone().ok_or_else(|| Error::new("必须指定 -alias"))?;
    let file = o.file.clone().ok_or_else(|| Error::new("必须指定 -file"))?;
    let bytes = std::fs::read(&file).map_err(|e| Error::new(format!("读不到 {file}: {e}")))?;
    let der = if bytes.starts_with(b"-----BEGIN") {
        sys::pem_decode(&String::from_utf8_lossy(&bytes), Some("CERTIFICATE"))?
    } else {
        bytes
    };
    let cert = Certificate::parse(&der)?;
    let path = keystore_path(o);
    let pass = storepass(o);
    let mut p12 = match std::fs::read(&path) {
        Ok(b) => P12::load(&b, &pass)?,
        Err(_) => P12::default(),
    };
    let key_id = Alg::Sha1.digest(&cert.der);
    match p12.entries.iter_mut().find(|e| e.alias.eq_ignore_ascii_case(&alias)) {
        Some(e) => {
            if !e.chain.iter().any(|c| c.der == cert.der) {
                e.chain.push(cert.clone());
            }
            e.key_id = key_id;
        }
        None => p12.entries.push(P12Entry {
            alias: alias.clone(),
            key_id,
            chain: vec![cert.clone()],
            private_key: None,
        }),
    }
    let out = p12.store(&pass)?;
    std::fs::write(&path, &out).map_err(|e| Error::new(format!("写密钥库失败: {e}")))?;
    println!("证书已添加到密钥库中");
    println!("{}", cert.describe());
    Ok(())
}

fn cmd_delete(o: &Opts) -> Result<()> {
    let alias = o.alias.clone().ok_or_else(|| Error::new("必须指定 -alias"))?;
    let path = keystore_path(o);
    let pass = storepass(o);
    let bytes = std::fs::read(&path).map_err(|e| Error::new(format!("读不到密钥库: {e}")))?;
    let mut p12 = P12::load(&bytes, &pass)?;
    let before = p12.entries.len();
    p12.entries.retain(|e| !e.alias.eq_ignore_ascii_case(&alias));
    if p12.entries.len() == before {
        return Err(Error::new(format!("别名 <{alias}> 不存在")));
    }
    std::fs::write(&path, p12.store(&pass)?).map_err(|e| Error::new(format!("写密钥库失败: {e}")))?;
    Ok(())
}
