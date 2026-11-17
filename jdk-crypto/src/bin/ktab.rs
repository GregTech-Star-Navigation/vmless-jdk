//! `ktab`：Kerberos keytab 的读写（MIT keytab v0x0502 格式）。
//!
//! 文件结构：`uint16 version = 0x0502`，随后是若干条目：
//! `uint32 num_components`、`counted realm`、`counted components[]`、
//! `uint32 name_type`、`uint32 timestamp`、`uint8 vno8`、
//! `keyblock = {uint16 length, uint16 enctype, data}`，最后是可选的 vno32/flags。
//!
//! 密钥派生按 RFC 3962（AES string-to-key）：PBKDF2-HMAC-SHA1，4096 轮，
//! salt = `realm || components`，dkLen = 16/32。

use std::process::ExitCode;

use vmless_crypto::hash::{pbkdf2, Alg};
use vmless_crypto::{sys, Error, Result};

const KVNO_OFFSET: u32 = 1;

#[derive(Clone, Debug)]
pub struct KtEntry {
    pub components: Vec<String>,
    pub realm: String,
    pub name_type: u32,
    pub timestamp: u32,
    pub kvno: u8,
    pub enctype: u16,
    pub key: Vec<u8>,
}

impl KtEntry {
    pub fn principal(&self) -> String {
        format!("{}/{}@{}", self.components[0], self.components[1..].join("/"), self.realm)
            .replace("/@", "@")
    }
}

pub fn enctype_name(e: u16) -> &'static str {
    match e {
        17 => "aes128-cts-hmac-sha1-96",
        18 => "aes256-cts-hmac-sha1-96",
        16 => "des3-cbc-sha1",
        23 => "arcfour-hmac",
        _ => "unknown",
    }
}

fn enctype_from_name(s: &str) -> Result<u16> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "aes256-cts-hmac-sha1-96" | "aes256-cts" | "18" => Ok(18),
        "aes128-cts-hmac-sha1-96" | "aes128-cts" | "17" => Ok(17),
        other => Err(Error::new(format!(
            "不支持的加密类型 {other}（支持 aes256-cts / aes128-cts）"
        ))),
    }
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_counted(out: &mut Vec<u8>, s: &[u8]) -> Result<()> {
    if s.len() > u16::MAX as usize {
        return Err(Error::new("keytab 字段过长"));
    }
    push_u16(out, s.len() as u16);
    out.extend_from_slice(s);
    Ok(())
}

pub fn write_keytab(entries: &[KtEntry]) -> Result<Vec<u8>> {
    // JDK（sun.security.krb5.internal.ktab.KeyTab）的落盘格式：
    //   2 字节版本，随后重复 { 4 字节条目长度, 条目内容 }
    // 条目内容 = 2 字节组件数 + 计数串 realm + 组件 + 4 字节 name_type
    //            + 4 字节时间戳 + 1 字节 kvno8 + 2 字节 enctype + 2 字节密钥长度 + 密钥
    let mut out = Vec::new();
    push_u16(&mut out, 0x0502);
    for e in entries {
        if e.components.is_empty() {
            return Err(Error::new("principal 至少要有一个组件"));
        }
        let mut body = Vec::new();
        push_u16(&mut body, e.components.len() as u16);
        push_counted(&mut body, e.realm.as_bytes())?;
        for c in &e.components {
            push_counted(&mut body, c.as_bytes())?;
        }
        push_u32(&mut body, e.name_type);
        push_u32(&mut body, e.timestamp);
        body.push(e.kvno);
        push_u16(&mut body, e.enctype);
        push_u16(&mut body, e.key.len() as u16);
        body.extend_from_slice(&e.key);
        push_u32(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
    }
    Ok(out)
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8> {
        let v = *self.d.get(self.p).ok_or_else(|| Error::new("keytab 截断"))?;
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.d.get(self.p..self.p + 2).ok_or_else(|| Error::new("keytab 截断"))?;
        self.p += 2;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.d.get(self.p..self.p + 4).ok_or_else(|| Error::new("keytab 截断"))?;
        self.p += 4;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn counted(&mut self) -> Result<Vec<u8>> {
        let n = self.u16()? as usize;
        let s = self.d.get(self.p..self.p + n).ok_or_else(|| Error::new("keytab 截断"))?;
        self.p += n;
        Ok(s.to_vec())
    }
    fn eof(&self) -> bool {
        self.p >= self.d.len()
    }
}

pub fn read_keytab(data: &[u8]) -> Result<Vec<KtEntry>> {
    // 先按 JDK 的「条目长度前缀」格式解析；失败再退回不带长度前缀的变体。
    match parse_entries(data, true) {
        Ok(v) => Ok(v),
        Err(first) => parse_entries(data, false).map_err(|_| first),
    }
}

fn parse_entries(data: &[u8], prefixed: bool) -> Result<Vec<KtEntry>> {
    let mut r = Reader { d: data, p: 0 };
    let ver = r.u16()?;
    if ver != 0x0502 && ver != 0x0501 {
        return Err(Error::new(format!(
            "不支持的 keytab 版本 0x{ver:04x}（只支持 0x0501/0x0502）"
        )));
    }
    let mut out = Vec::new();
    while !r.eof() {
        let end = if prefixed {
            let entry_len = r.u32()? as usize;
            if entry_len == 0 {
                break; // 删除条目后留下的空位
            }
            if r.p + entry_len > data.len() {
                return Err(Error::new("keytab 条目长度越界"));
            }
            r.p + entry_len
        } else {
            data.len()
        };
        let n = if ver == 0x0501 { r.u16()? as usize } else { r.u16()? as usize };
        if n == 0 || n > 64 {
            return Err(Error::new(format!("keytab 里的组件个数不合理: {n}")));
        }
        let realm = String::from_utf8_lossy(&r.counted()?).to_string();
        let mut components = Vec::with_capacity(n);
        for _ in 0..n {
            components.push(String::from_utf8_lossy(&r.counted()?).to_string());
        }
        let name_type = r.u32()?;
        let timestamp = r.u32()?;
        let kvno8 = r.u8()?;
        let enctype = r.u16()?;
        let keylen = r.u16()? as usize;
        let key = r
            .d
            .get(r.p..r.p + keylen)
            .ok_or_else(|| Error::new("keytab 的密钥数据截断"))?
            .to_vec();
        r.p += keylen;
        let mut kvno = kvno8;
        if prefixed {
            // 可选的 32 位 kvno（非 0 时优先）
            if end.saturating_sub(r.p) >= 4 {
                let ext = u32::from_be_bytes([
                    data[r.p],
                    data[r.p + 1],
                    data[r.p + 2],
                    data[r.p + 3],
                ]);
                if ext != 0 {
                    kvno = ext.min(255) as u8;
                }
                r.p += 4;
            }
            r.p = end;
        }
        out.push(KtEntry { components, realm, name_type, timestamp, kvno, enctype, key });
        if !prefixed {
            break;
        }
    }
    Ok(out)
}

/// RFC 3962 AES string-to-key。
pub fn string_to_key_aes(password: &[u8], salt: &[u8], enctype: u16, iterations: u32) -> Result<Vec<u8>> {
    let len = match enctype {
        17 => 16,
        18 => 32,
        other => return Err(Error::new(format!("不支持的类型 {other}"))),
    };
    // RFC 3962 §4：PBKDF2 之后再走一次 DK(tkey, "kerberos")
    let tkey = pbkdf2(Alg::Sha1, password, salt, iterations, len);
    vmless_crypto::cipher::krb_dk(&tkey, b"kerberos", len)
}

fn split_principal(s: &str) -> (Vec<String>, Option<String>) {
    let (body, realm) = match s.split_once('@') {
        Some((b, r)) => (b.to_string(), Some(r.to_string())),
        None => (s.to_string(), None),
    };
    let comps: Vec<String> = body.split('/').map(|c| c.to_string()).collect();
    (comps, realm)
}

fn default_realm() -> String {
    std::env::var("KRB5_REALM").unwrap_or_else(|_| "LOCAL.REALM".to_string())
}

const USAGE: &str = "\
用法: ktab -l [-k keytab]
       ktab -a <principal> [-k keytab] [-p <口令>] [-e <enctype>] [-V <kvno>]

  -l            列出 keytab 中的条目
  -a <principal> 添加条目（principal 形如 user/admin@REALM）
  -k <keytab>   指定 keytab 文件（默认 $KRB5_KTNAME 或 /etc/krb5.keytab）
  -p <口令>      派生密钥用的口令（省略则从标准输入读取）
  -e <enctype>   aes256-cts / aes128-cts（默认 aes256-cts）
  -V <kvno>      密钥版本号（默认 1）
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "-help" || a == "--help") {
        print!("{USAGE}");
        return if argv.is_empty() { ExitCode::from(1) } else { ExitCode::SUCCESS };
    }
    let mut mode = "";
    let mut principal = String::new();
    let mut keytab = std::env::var("KRB5_KTNAME")
        .map(|v| v.trim_start_matches("FILE:").to_string())
        .unwrap_or_else(|_| "krb5.keytab".to_string());
    let mut password: Option<String> = None;
    let mut enctype = 18u16;
    let mut kvno = KVNO_OFFSET as u8;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        let mut next = |what: &str| -> Result<String> {
            if i + 1 >= argv.len() {
                return Err(Error::new(format!("{what} 缺少参数")));
            }
            i += 1;
            Ok(argv[i].clone())
        };
        let r: Result<()> = (|| {
            match a {
                "-l" => mode = "list",
                "-a" => {
                    mode = "add";
                    principal = next("-a")?;
                }
                "-k" => keytab = next("-k")?,
                "-p" => password = Some(next("-p")?),
                "-e" => enctype = enctype_from_name(&next("-e")?)?,
                "-V" => kvno = next("-V")?.parse::<u8>().unwrap_or(1),
                other => return Err(Error::new(format!("无法识别的选项: {other}"))),
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("ktab: {e}");
            return ExitCode::from(1);
        }
        i += 1;
    }
    let result: Result<()> = (|| {
        match mode {
            "list" => {
                let data = std::fs::read(&keytab)
                    .map_err(|e| Error::new(format!("读不到 keytab {keytab}: {e}")))?;
                let entries = read_keytab(&data)?;
                println!("Keytab 名称: FILE:{keytab}");
                println!("KVNO Principal");
                println!("---- ------------------------------------------");
                for e in entries {
                    println!(
                        "{:4} {} ({})",
                        e.kvno,
                        e.principal(),
                        enctype_name(e.enctype)
                    );
                }
                Ok(())
            }
            "add" => {
                let mut entries = match std::fs::read(&keytab) {
                    Ok(d) => read_keytab(&d)?,
                    Err(_) => Vec::new(),
                };
                let (components, realm) = split_principal(&principal);
                let realm = realm.unwrap_or_else(default_realm);
                if components.is_empty() || components[0].is_empty() {
                    return Err(Error::new("principal 不合法"));
                }
                let pw = match password {
                    Some(p) => p,
                    None => {
                        use std::io::Write;
                        print!("口令: ");
                        let _ = std::io::stdout().flush();
                        let mut line = String::new();
                        std::io::stdin()
                            .read_line(&mut line)
                            .map_err(|e| Error::new(format!("读口令失败: {e}")))?;
                        line.trim_end_matches(['\r', '\n']).to_string()
                    }
                };
                let mut salt = realm.as_bytes().to_vec();
                for c in &components {
                    salt.extend_from_slice(c.as_bytes());
                }
                let key = string_to_key_aes(pw.as_bytes(), &salt, enctype, 4096)?;
                let now = sys::now_unix() as u32;
                entries.retain(|e| !(e.components == components && e.realm == realm && e.enctype == enctype));
                entries.push(KtEntry {
                    components,
                    realm,
                    name_type: 1,
                    timestamp: now,
                    kvno,
                    enctype,
                    key,
                });
                std::fs::write(&keytab, write_keytab(&entries)?)
                    .map_err(|e| Error::new(format!("写 keytab 失败: {e}")))?;
                println!("已添加条目到 {keytab}");
                Ok(())
            }
            _ => Err(Error::new("需要 -l 或 -a <principal>")),
        }
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ktab: {e}");
            ExitCode::from(1)
        }
    }
}
