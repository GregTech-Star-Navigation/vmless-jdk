//! MIT `FILE:` 凭据缓存（ccache）的读写。
//!
//! 格式依据 krb5 的 `doc/formats/ccache_file_format.rst`：
//!
//! ```text
//! version (16 bits) = 0x0504
//! header (v4 only) ::= length (16 bits), fields { tag(16), len(16), value }
//! principal ::= name type(32), count(32), realm(data), components(data...)
//! credential ::= client(principal), server(principal), keyblock,
//!                authtime(32), starttime(32), endtime(32), renew_till(32),
//!                is_skey(1 byte!), ticket_flags(32), addresses, authdata,
//!                ticket(data), second_ticket(data)
//! data ::= length(32) value
//! keyblock ::= enctype(16) [v3 重复一次] data
//! ```
//!
//! 三个容易写错的地方，这里都钉死了：
//! - **`is_skey` 是 1 个字节**，不是 32 位（JDK 的 `readskey()` 也只读 1 字节）；
//! - **keyblock 的长度是 32 位**（`data` 里那个 length），只有 enctype 是 16 位；
//! - v4 的 header 是「整个 header 的字节长度」，不是「tag 的长度」。
//!
//! 写 0x0504（header 长度 0），JDK 与 MIT 的 `klist` 都能读；读则兼容 0x0503/0x0504。

use crate::asn1_krb::{EncryptionKey, PrincipalName, Ticket};
use vmless_crypto::{Error, Result};

/// 我们写出的 ccache 版本号。
pub const CCACHE_VERSION: u16 = 0x0504;
/// 配置条目用的伪 realm（MIT 约定）。
pub const CONFIG_REALM: &str = "X-CACHECONF:";

/// ccache 里的 principal：名字 + realm（ASN.1 的 `PrincipalName` 不含 realm）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CcPrincipal {
    pub name_type: i32,
    pub realm: String,
    pub components: Vec<String>,
}

impl CcPrincipal {
    pub fn new(name_type: i32, realm: &str, components: Vec<String>) -> Self {
        CcPrincipal { name_type, realm: realm.to_string(), components }
    }

    pub fn from_asn1(p: &PrincipalName, realm: &str) -> Self {
        CcPrincipal {
            name_type: p.name_type,
            realm: realm.to_string(),
            components: p.components.clone(),
        }
    }

    pub fn display(&self) -> String {
        format!("{}@{}", self.components.join("/"), self.realm)
    }
}

impl std::fmt::Display for CcPrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// 一条凭据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub client: CcPrincipal,
    pub server: CcPrincipal,
    pub key: EncryptionKey,
    pub authtime: i64,
    pub starttime: i64,
    pub endtime: i64,
    pub renew_till: i64,
    pub is_skey: bool,
    pub flags: u32,
    pub addresses: Vec<(u16, Vec<u8>)>,
    pub authdata: Vec<(u16, Vec<u8>)>,
    pub ticket: Vec<u8>,
    pub second_ticket: Vec<u8>,
}

impl Credential {
    /// 是否是 `X-CACHECONF:` 配置条目（klist 不该显示它）。
    pub fn is_config_entry(&self) -> bool {
        self.server.realm == CONFIG_REALM
    }

    /// 从 AS-REP 的各个部件拼一条 TGT 凭据。
    #[allow(clippy::too_many_arguments)]
    pub fn from_tgt(
        client: CcPrincipal,
        server: CcPrincipal,
        key: EncryptionKey,
        authtime: i64,
        starttime: i64,
        endtime: i64,
        renew_till: i64,
        flags: u32,
        ticket: Vec<u8>,
    ) -> Self {
        Credential {
            client,
            server,
            key,
            authtime,
            starttime,
            endtime,
            renew_till,
            is_skey: false,
            flags,
            addresses: Vec::new(),
            authdata: Vec::new(),
            ticket,
            second_ticket: Vec::new(),
        }
    }

    /// 票的服务主体名（给 klist 用）。
    pub fn server_display(&self) -> String {
        self.server.display()
    }
}

/// 整个缓存。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CcCache {
    pub version: u16,
    pub principal: CcPrincipal,
    pub creds: Vec<Credential>,
}

impl CcCache {
    pub fn new(principal: CcPrincipal) -> Self {
        CcCache { version: CCACHE_VERSION, principal, creds: Vec::new() }
    }

    /// 解析一段 ccache 字节。**这是 fuzz 入口之一**：任何畸形输入都返回 `Err`。
    pub fn parse(data: &[u8]) -> Result<CcCache> {
        let mut r = Rd::new(data);
        let hi = r.u8()?;
        let lo = r.u8()?;
        if hi != 5 {
            return Err(Error::new(format!(
                "不是 ccache 文件：首字节是 {hi}，应为 5"
            )));
        }
        let version = ((hi as u16) << 8) | lo as u16;
        if !(3..=4).contains(&(version & 0xff)) {
            return Err(Error::new(format!(
                "不支持的 ccache 版本 0x{version:04x}（只支持 0x0503/0x0504；0x0501/0x0502 用本机字节序，已废弃）"
            )));
        }
        if version == 0x0504 {
            // header：先是整个 header 的字节长度，再是若干 {tag,len,value}
            let hdr_len = r.u16()? as usize;
            if hdr_len > 0 {
                let hdr = r.bytes(hdr_len)?;
                let mut hr = Rd::new(hdr);
                while hr.remaining() >= 4 {
                    let _tag = hr.u16()?;
                    let len = hr.u16()? as usize;
                    let _v = hr.bytes(len)?;
                }
            }
        }
        let principal = read_principal(&mut r, version)?;
        let mut creds = Vec::new();
        while r.remaining() > 0 {
            creds.push(read_cred(&mut r, version)?);
        }
        Ok(CcCache { version, principal, creds })
    }

    /// 序列化成字节（写 0x0504 + 空 header）。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x05);
        out.push((self.version & 0xff) as u8);
        if self.version == 0x0504 {
            out.extend_from_slice(&0u16.to_be_bytes()); // header 长度 0
        }
        write_principal(&mut out, &self.principal);
        for c in &self.creds {
            write_cred(&mut out, c);
        }
        out
    }

    /// 从文件读（支持 `FILE:` 前缀）。
    pub fn read_file(path: &str) -> Result<CcCache> {
        let p = strip_file_prefix(path);
        let data = std::fs::read(&p)
            .map_err(|e| Error::new(format!("读不到凭据缓存 {p}: {e}")))?;
        CcCache::parse(&data).map_err(|e| e.ctx(&format!("凭据缓存 {p}")))
    }

    pub fn write_file(&self, path: &str) -> Result<()> {
        let p = strip_file_prefix(path);
        if let Some(dir) = std::path::Path::new(&p).parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        std::fs::write(&p, self.to_bytes())
            .map_err(|e| Error::new(format!("写凭据缓存 {p} 失败: {e}")))
    }

    /// 拿第一个 TGT（`krbtgt/REALM@REALM`）。
    pub fn tgt(&self) -> Option<&Credential> {
        self.creds.iter().find(|c| {
            !c.is_config_entry() && c.server.components.len() == 2 && c.server.components[0] == "krbtgt"
        })
    }
}

/// 去掉 `FILE:` 前缀（其它缓存类型不支持，明确报错）。
pub fn strip_file_prefix(path: &str) -> String {
    let t = path.trim();
    if let Some(rest) = t.strip_prefix("FILE:") {
        return rest.to_string();
    }
    t.to_string()
}

/// 缓存路径：`KRB5CCNAME` 优先，否则临时目录下的 `krb5cc_<user>`。
pub fn default_cache_path() -> String {
    if let Ok(v) = std::env::var("KRB5CCNAME") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            if v.contains(':') && !v.starts_with("FILE:") {
                // 只支持 FILE: 类型，别的类型交给调用方报错
                return v;
            }
            return strip_file_prefix(&v);
        }
    }
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string());
    let mut p = std::env::temp_dir();
    p.push(format!("krb5cc_{user}"));
    p.to_string_lossy().into_owned()
}

/// 解析 `KRB5CCNAME` 时如果给了非 FILE 类型，给出明确错误。
pub fn check_cache_type(path: &str) -> Result<()> {
    let t = path.trim();
    if let Some(idx) = t.find(':') {
        let kind = &t[..idx];
        // Windows 盘符（`C:\...`）不是缓存类型
        let is_drive = kind.len() == 1 && kind.as_bytes()[0].is_ascii_alphabetic();
        if !is_drive && kind != "FILE" && !kind.contains('\\') && !kind.contains('/') {
            return Err(Error::new(format!(
                "不支持的凭据缓存类型 {kind}:（本实现只支持 FILE:）"
            )));
        }
    }
    Ok(())
}

// ---------------------------- 读 ----------------------------

struct Rd<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(d: &'a [u8]) -> Self {
        Rd { d, p: 0 }
    }
    fn remaining(&self) -> usize {
        self.d.len() - self.p
    }
    fn u8(&mut self) -> Result<u8> {
        if self.remaining() < 1 {
            return Err(Error::new("ccache 截断：还差 1 字节"));
        }
        let v = self.d[self.p];
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        if self.remaining() < 2 {
            return Err(Error::new("ccache 截断：还差 2 字节"));
        }
        let v = u16::from_be_bytes([self.d[self.p], self.d[self.p + 1]]);
        self.p += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        if self.remaining() < 4 {
            return Err(Error::new("ccache 截断：还差 4 字节"));
        }
        let v = u32::from_be_bytes([
            self.d[self.p],
            self.d[self.p + 1],
            self.d[self.p + 2],
            self.d[self.p + 3],
        ]);
        self.p += 4;
        Ok(v)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::new(format!(
                "ccache 截断：声明 {n} 字节，只剩 {}",
                self.remaining()
            )));
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    /// `data ::= length(32) value`
    fn data(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        // 上限保护：单条 data 不该超过 1 MiB（防止畸形长度把内存吃光）
        if n > 1 << 20 {
            return Err(Error::new(format!("ccache 里的 data 长度过大: {n}")));
        }
        Ok(self.bytes(n)?.to_vec())
    }
    fn string(&mut self) -> Result<String> {
        let b = self.data()?;
        String::from_utf8(b).map_err(|_| Error::new("ccache 里的字符串不是合法 UTF-8"))
    }
}

fn read_principal(r: &mut Rd<'_>, version: u16) -> Result<CcPrincipal> {
    let name_type = r.u32()? as i32;
    let n = r.u32()? as usize;
    if n > 64 {
        return Err(Error::new(format!("ccache: principal 组件数不合理: {n}")));
    }
    let realm = r.string()?;
    let mut components = Vec::with_capacity(n);
    for _ in 0..n {
        components.push(r.string()?);
    }
    let _ = version;
    Ok(CcPrincipal { name_type, realm, components })
}

fn read_cred(r: &mut Rd<'_>, version: u16) -> Result<Credential> {
    let client = read_principal(r, version)?;
    let server = read_principal(r, version)?;
    // keyblock ::= enctype(16) [v3 再重复一次] data
    let etype = r.u16()? as i32;
    if version == 0x0503 {
        let dup = r.u16()? as i32;
        if dup != etype {
            return Err(Error::new(format!(
                "ccache: v3 的 keyblock 里 enctype 重复了两次且不一致（{etype} vs {dup}）"
            )));
        }
    }
    let keyvalue = r.data()?;
    let key = EncryptionKey { keytype: etype, keyvalue };
    let authtime = r.u32()? as i64;
    let starttime = r.u32()? as i64;
    let endtime = r.u32()? as i64;
    let renew_till = r.u32()? as i64;
    let is_skey = r.u8()? != 0;
    let flags = r.u32()?;
    let naddr = r.u32()? as usize;
    if naddr > 1024 {
        return Err(Error::new(format!("ccache: 地址条目数不合理: {naddr}")));
    }
    let mut addresses = Vec::with_capacity(naddr);
    for _ in 0..naddr {
        let t = r.u16()?;
        addresses.push((t, r.data()?));
    }
    let nauth = r.u32()? as usize;
    if nauth > 1024 {
        return Err(Error::new(format!("ccache: authdata 条目数不合理: {nauth}")));
    }
    let mut authdata = Vec::with_capacity(nauth);
    for _ in 0..nauth {
        let t = r.u16()?;
        authdata.push((t, r.data()?));
    }
    let ticket = r.data()?;
    let second_ticket = r.data()?;
    Ok(Credential {
        client,
        server,
        key,
        authtime,
        starttime,
        endtime,
        renew_till,
        is_skey,
        flags,
        addresses,
        authdata,
        ticket,
        second_ticket,
    })
}

// ---------------------------- 写 ----------------------------

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_data(out: &mut Vec<u8>, v: &[u8]) {
    put_u32(out, v.len() as u32);
    out.extend_from_slice(v);
}

fn write_principal(out: &mut Vec<u8>, p: &CcPrincipal) {
    put_u32(out, p.name_type as u32);
    put_u32(out, p.components.len() as u32);
    put_data(out, p.realm.as_bytes());
    for c in &p.components {
        put_data(out, c.as_bytes());
    }
}

fn write_cred(out: &mut Vec<u8>, c: &Credential) {
    write_principal(out, &c.client);
    write_principal(out, &c.server);
    put_u16(out, c.key.keytype as u16);
    put_data(out, &c.key.keyvalue);
    put_u32(out, c.authtime as u32);
    put_u32(out, c.starttime as u32);
    put_u32(out, c.endtime as u32);
    put_u32(out, c.renew_till as u32);
    out.push(if c.is_skey { 1 } else { 0 });
    put_u32(out, c.flags);
    put_u32(out, c.addresses.len() as u32);
    for (t, d) in &c.addresses {
        put_u16(out, *t);
        put_data(out, d);
    }
    put_u32(out, c.authdata.len() as u32);
    for (t, d) in &c.authdata {
        put_u16(out, *t);
        put_data(out, d);
    }
    put_data(out, &c.ticket);
    put_data(out, &c.second_ticket);
}

/// 从 AS-REP 拼一条 TGT 凭据（`client`/`server` 的 realm 由调用方给）。
pub fn cred_from_as_rep(
    crealm: &str,
    cname: &PrincipalName,
    realm: &str,
    ticket: &Ticket,
    key: &EncryptionKey,
    authtime: i64,
    starttime: i64,
    endtime: i64,
    renew_till: i64,
    flags: u32,
    ticket_der: Vec<u8>,
) -> Credential {
    Credential::from_tgt(
        CcPrincipal::from_asn1(cname, crealm),
        CcPrincipal::from_asn1(&ticket.sname, realm),
        key.clone(),
        authtime,
        starttime,
        endtime,
        renew_till,
        flags,
        ticket_der,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmless_crypto::sys::hex;

    fn sample() -> CcCache {
        let mut cc = CcCache::new(CcPrincipal::new(1, "EXAMPLE.COM", vec!["alice".into()]));
        cc.creds.push(Credential::from_tgt(
            CcPrincipal::new(1, "EXAMPLE.COM", vec!["alice".into()]),
            CcPrincipal::new(2, "EXAMPLE.COM", vec!["krbtgt".into(), "EXAMPLE.COM".into()]),
            EncryptionKey::new(18, vec![0x11; 32]),
            1_700_000_000,
            1_700_000_000,
            1_700_036_000,
            1_700_604_800,
            0x40e0_0000,
            vec![0x61, 0x82, 0x00, 0x10, 1, 2, 3],
        ));
        cc
    }

    #[test]
    fn roundtrip() {
        let cc = sample();
        let bytes = cc.to_bytes();
        // 头：05 04 00 00（版本 + 空 header）
        assert_eq!(hex(&bytes[..4]), "05040000");
        let back = CcCache::parse(&bytes).unwrap();
        assert_eq!(back, cc);
        assert_eq!(back.tgt().unwrap().server.display(), "krbtgt/EXAMPLE.COM@EXAMPLE.COM");
    }

    #[test]
    fn is_skey_is_one_byte() {
        // 这条是防回归：is_skey 若按 32 位写，后面所有字段都会错位
        let cc = sample();
        let bytes = cc.to_bytes();
        let back = CcCache::parse(&bytes).unwrap();
        assert_eq!(back.creds[0].flags, 0x40e0_0000);
        // 手工把 is_skey 置 1 再解析
        let mut c2 = cc.clone();
        c2.creds[0].is_skey = true;
        let b2 = c2.to_bytes();
        assert_eq!(b2.len(), bytes.len()); // 置位不改变长度
        assert!(CcCache::parse(&b2).unwrap().creds[0].is_skey);
    }

    #[test]
    fn config_entry_is_skipped_by_tgt() {
        let mut cc = sample();
        cc.creds.push(Credential::from_tgt(
            cc.principal.clone(),
            CcPrincipal::new(2, CONFIG_REALM, vec!["krb5_ccache_conf_data".into(), "pa_type".into()]),
            EncryptionKey::new(18, vec![0; 32]),
            0,
            0,
            0,
            0,
            0,
            b"2".to_vec(),
        ));
        assert!(cc.creds[1].is_config_entry());
        assert_eq!(cc.tgt().unwrap().server.components[0], "krbtgt");
    }

    #[test]
    fn truncated_and_garbage_do_not_panic() {
        let bytes = sample().to_bytes();
        // 任何截断都只能得到 Err 或「少了条凭据」，绝不许 panic
        for cut in 0..bytes.len() {
            let _ = CcCache::parse(&bytes[..cut]);
        }
        // 切在结构中间必须报错（切在条目边界上则合法：格式本身没有条目计数）
        assert!(CcCache::parse(&bytes[..bytes.len() - 1]).is_err());
        assert!(CcCache::parse(&bytes[..4 + 3]).is_err());
        assert!(CcCache::parse(&[]).is_err());
        assert!(CcCache::parse(&[4, 4, 0, 0]).is_err()); // 版本不对
        assert!(CcCache::parse(&[5, 1, 0, 0]).is_err()); // v1 不支持
        // 随机字节
        let mut x: u64 = 0xdead_beef_cafe_1234;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 96) as usize;
            let mut buf = vec![0u8; n];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let _ = CcCache::parse(&buf);
        }
    }

    #[test]
    fn reads_v3_with_duplicated_enctype() {
        // 手工拼一份 v3：keyblock 里 enctype 出现两次
        let mut out = vec![0x05u8, 0x03];
        let p = CcPrincipal::new(1, "R", vec!["u".into()]);
        write_principal(&mut out, &p); // 默认 principal
        write_principal(&mut out, &p); // 凭据的 client
        write_principal(&mut out, &p); // 凭据的 server
        put_u16(&mut out, 18);
        put_u16(&mut out, 18); // v3 的重复
        put_data(&mut out, &[7u8; 32]);
        for _ in 0..4 {
            put_u32(&mut out, 100);
        }
        out.push(0);
        put_u32(&mut out, 0);
        put_u32(&mut out, 0); // addresses
        put_u32(&mut out, 0); // authdata
        put_data(&mut out, b"ticket");
        put_data(&mut out, b"");
        let cc = CcCache::parse(&out).unwrap();
        assert_eq!(cc.version, 0x0503);
        assert_eq!(cc.creds[0].key.keyvalue.len(), 32);
        assert_eq!(cc.creds[0].ticket, b"ticket");
        // 重复的 enctype 不一致要报错
        let mut bad = out.clone();
        bad[2 + 4 + 4 + 4 + 1 + 4 + 1 + 4 + 4 + 2] = 17;
        assert!(CcCache::parse(&bad).is_err());
    }

    #[test]
    fn file_io_and_env() {
        let dir = std::env::temp_dir().join("vmless-krb-test-ccache");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cc1");
        let ps = path.to_string_lossy().into_owned();
        let cc = sample();
        cc.write_file(&ps).unwrap();
        assert_eq!(CcCache::read_file(&ps).unwrap(), cc);
        // FILE: 前缀
        assert_eq!(CcCache::read_file(&format!("FILE:{ps}")).unwrap(), cc);
        assert!(CcCache::read_file(&format!("{ps}.nope")).is_err());
        // 非 FILE 类型要报错
        assert!(check_cache_type("KEYRING:persistent:0").is_err());
        assert!(check_cache_type(&ps).is_ok());
        assert!(check_cache_type("FILE:/tmp/x").is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
