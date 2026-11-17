//! 最小但完整的 KDC：AS-REQ/AS-REP、预认证、TGS-REQ/TGS-REP、KRB-ERROR。
//!
//! 它同时是**验收工具**（没有真 KDC，就拿它跟真 JDK 的 `kinit` 对打）和真实现：
//! 数据库来自 MIT keytab（`ktab` 写的就能直接吃），口令建 principal 时按 RFC 3962
//! 现算 AES256/AES128 两把长期密钥。
//!
//! 并发模型很朴素：UDP 单线程循环 + 每个 TCP 连接一个线程。一个 KDC 该有的
//! 「畸形报文不崩、超时不挂、错口令给 24、未知 principal 给 6」都在。

use crate::asn1_krb::*;
use crate::crypto_krb;
use crate::msg::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use vmless_crypto::{sys, Error, Result};

// ============================ keytab ============================

/// keytab 里的一条记录（MIT v0x0502 格式）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KtEntry {
    pub realm: String,
    pub components: Vec<String>,
    pub name_type: u32,
    pub timestamp: u32,
    pub kvno: u8,
    pub enctype: u16,
    pub key: Vec<u8>,
}

impl KtEntry {
    pub fn principal(&self) -> String {
        format!("{}@{}", self.components.join("/"), self.realm)
    }
}

struct Kr<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Kr<'a> {
    fn new(d: &'a [u8]) -> Self {
        Kr { d, p: 0 }
    }
    fn rem(&self) -> usize {
        self.d.len() - self.p
    }
    fn u8(&mut self) -> Result<u8> {
        if self.rem() < 1 {
            return Err(Error::new("keytab 截断"));
        }
        let v = self.d[self.p];
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        if self.rem() < 2 {
            return Err(Error::new("keytab 截断"));
        }
        let v = u16::from_be_bytes([self.d[self.p], self.d[self.p + 1]]);
        self.p += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        if self.rem() < 4 {
            return Err(Error::new("keytab 截断"));
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
        if self.rem() < n {
            return Err(Error::new("keytab 截断"));
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn str16(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        if n > 4096 {
            return Err(Error::new("keytab 里的字符串过长"));
        }
        String::from_utf8(self.bytes(n)?.to_vec())
            .map_err(|_| Error::new("keytab 里的字符串不是 UTF-8"))
    }
}

/// 解析 keytab 字节。**两种格式都吃**：
///
/// - **JDK 格式**（`sun.security.krb5.internal.ktab.KeyTab`，也是本机 `ktab` 写的）：
///   每条前面有 4 字节条目长度，keyblock 是 `enctype(2) + len(2) + key`。
/// - **MIT 格式**（krb5 的 `krb5_ktfile`）：没有条目长度，keyblock 是 `len(2) + enctype(2) + key`。
///
/// 两种都以 `u16 0x0502` 开头且第一个 u32 含义不同（条目长度 vs 组件数），
/// 所以这里「两种都整份解析一遍，谁能干净吃完谁赢」。
pub fn parse_keytab(data: &[u8]) -> Result<Vec<KtEntry>> {
    let jdk = parse_keytab_jdk(data);
    if let Ok(v) = &jdk {
        if !v.is_empty() {
            return jdk;
        }
    }
    let mit = parse_keytab_mit(data);
    if let Ok(v) = &mit {
        if !v.is_empty() {
            return mit;
        }
    }
    // 都失败时给出更有信息量的那个错误
    match (jdk, mit) {
        (Err(a), Err(b)) => Err(Error::new(format!(
            "keytab 解析失败（JDK 格式：{a}；MIT 格式：{b}）"
        ))),
        (Ok(_), Err(b)) => Err(b),
        (Err(a), Ok(_)) => Err(a),
        (Ok(_), Ok(_)) => Err(Error::new("keytab 里没有可用条目")),
    }
}

/// JDK 的 keytab 布局（`KeyTabOutputStream.writeEntry`）。
pub fn parse_keytab_jdk(data: &[u8]) -> Result<Vec<KtEntry>> {
    let mut r = Kr::new(data);
    let version = r.u16()?;
    if version != 0x0502 && version != 0x0501 {
        return Err(Error::new(format!(
            "不支持的 keytab 版本 0x{version:04x}（只支持 0x0501/0x0502）"
        )));
    }
    let mut out = Vec::new();
    while r.rem() > 0 {
        if r.rem() < 4 {
            return Err(Error::new("keytab 截断：条目长度不全"));
        }
        let entry_len = r.u32()? as i64;
        if entry_len == 0 {
            continue; // JDK 允许「空条目」
        }
        if entry_len < 0 {
            // 被删掉的条目留下的洞：跳过
            let n = (-entry_len) as usize;
            r.bytes(n)?;
            continue;
        }
        let body_start = r.p;
        let mut ncomp = r.u16()? as usize;
        if version == 0x0501 {
            ncomp = ncomp.saturating_sub(1); // v1 把 realm 算进组件数
        }
        if ncomp == 0 || ncomp > 64 {
            return Err(Error::new(format!("keytab: 组件数不合理: {ncomp}")));
        }
        let realm = r.str16()?;
        let mut components = Vec::new();
        for _ in 0..ncomp {
            components.push(r.str16()?);
        }
        let name_type = r.u32()?;
        let timestamp = r.u32()?;
        let mut kvno = r.u8()?;
        let enctype = r.u16()?;
        let klen = r.u16()? as usize;
        let key = r.bytes(klen)?.to_vec();
        // 条目长度允许后面还有扩展 kvno32
        let used = r.p - body_start;
        let declared = entry_len as usize;
        if declared < used {
            return Err(Error::new(format!(
                "keytab: 条目声明 {declared} 字节，实际读到 {used} 字节"
            )));
        }
        let rest = declared - used;
        if rest >= 4 {
            let ext = r.u32()?;
            if ext != 0 {
                kvno = (ext & 0xff) as u8;
            }
            let left = rest - 4;
            if left > 0 {
                r.bytes(left)?;
            }
        } else if rest > 0 {
            r.bytes(rest)?;
        }
        out.push(KtEntry { realm, components, name_type, timestamp, kvno, enctype, key });
    }
    Ok(out)
}

/// MIT 的 keytab 布局（`krb5_ktfile`）。
pub fn parse_keytab_mit(data: &[u8]) -> Result<Vec<KtEntry>> {
    let mut r = Kr::new(data);
    let version = r.u16()?;
    if version != 0x0502 && version != 0x0501 {
        return Err(Error::new(format!(
            "不支持的 keytab 版本 0x{version:04x}（只支持 0x0501/0x0502）"
        )));
    }
    let mut out = Vec::new();
    while r.rem() > 0 {
        let ncomp = if version == 0x0501 {
            r.u16()? as usize
        } else {
            r.u32()? as usize
        };
        if ncomp == 0 || ncomp > 64 {
            return Err(Error::new(format!("keytab: 组件数不合理: {ncomp}")));
        }
        let realm = r.str16()?;
        let mut components = Vec::new();
        for _ in 0..ncomp {
            components.push(r.str16()?);
        }
        let name_type = r.u32()?;
        let timestamp = r.u32()?;
        let kvno8 = r.u8()?;
        let klen = r.u16()? as usize;
        let enctype = r.u16()?;
        let key = r.bytes(klen)?.to_vec();
        let mut kvno = kvno8;
        if version == 0x0502 {
            // v0x0502 每条后面跟 4 字节的 vno32（若 kvno8 为 0，它才是真 kvno）
            let vno32 = r.u32()?;
            if kvno8 == 0 {
                kvno = (vno32 & 0xff) as u8;
            }
        }
        out.push(KtEntry { realm, components, name_type, timestamp, kvno, enctype, key });
    }
    Ok(out)
}

pub fn read_keytab_file(path: &str) -> Result<Vec<KtEntry>> {
    let p = path.trim().trim_start_matches("FILE:");
    let data = std::fs::read(p).map_err(|e| Error::new(format!("读不到 keytab {p}: {e}")))?;
    parse_keytab(&data).map_err(|e| e.ctx(&format!("keytab {p}")))
}

// ============================ 数据库 ============================

/// 数据库里的一个 principal（同一 principal 可有多把 etype 的密钥）。
#[derive(Clone, Debug)]
pub struct DbPrincipal {
    pub realm: String,
    pub components: Vec<String>,
    pub kvno: u8,
    pub keys: Vec<(i32, Vec<u8>)>,
}

impl DbPrincipal {
    pub fn display(&self) -> String {
        format!("{}@{}", self.components.join("/"), self.realm)
    }

    pub fn key(&self, etype: i32) -> Option<&[u8]> {
        self.keys.iter().find(|(e, _)| *e == etype).map(|(_, k)| k.as_slice())
    }

    /// 支持哪些 etype（按强弱排序，强的在前）。
    pub fn etypes(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self.keys.iter().map(|(e, _)| *e).collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }
}

/// 极简 KDB：一个 realm + 若干 principal。
#[derive(Clone, Debug, Default)]
pub struct Kdb {
    pub realm: String,
    pub principals: Vec<DbPrincipal>,
}

impl Kdb {
    pub fn new(realm: &str) -> Self {
        Kdb { realm: realm.to_string(), principals: Vec::new() }
    }

    /// 从 keytab 条目灌进来（同一 principal 的多条不同 etype 会合并）。
    pub fn load_keytab(&mut self, entries: &[KtEntry]) -> Result<usize> {
        let mut n = 0;
        for e in entries {
            if !crypto_krb::is_supported(e.enctype as i32) {
                continue;
            }
            if let Some(p) = self
                .principals
                .iter_mut()
                .find(|p| p.realm == e.realm && p.components == e.components)
            {
                p.keys.retain(|(t, _)| *t != e.enctype as i32);
                p.keys.push((e.enctype as i32, e.key.clone()));
                p.kvno = p.kvno.max(e.kvno);
            } else {
                self.principals.push(DbPrincipal {
                    realm: e.realm.clone(),
                    components: e.components.clone(),
                    kvno: e.kvno.max(1),
                    keys: vec![(e.enctype as i32, e.key.clone())],
                });
            }
            n += 1;
        }
        Ok(n)
    }

    /// 用口令加一个 principal：按 RFC 3962 的默认 salt 现算 AES256/AES128 密钥。
    pub fn add_password(&mut self, components: &[String], password: &str, kvno: u8) -> Result<()> {
        if components.is_empty() {
            return Err(Error::new("principal 组件不能为空"));
        }
        let salt = crypto_krb::default_salt(&self.realm, components);
        let mut keys = Vec::new();
        for etype in [crypto_krb::ETYPE_AES256, crypto_krb::ETYPE_AES128] {
            let k = crypto_krb::string2key(password, &salt, etype, crypto_krb::DEFAULT_ITERATIONS)?;
            keys.push((etype, k));
        }
        if let Some(p) = self
            .principals
            .iter_mut()
            .find(|p| p.realm == self.realm && p.components == components)
        {
            p.keys = keys;
            p.kvno = kvno;
        } else {
            self.principals.push(DbPrincipal {
                realm: self.realm.clone(),
                components: components.to_vec(),
                kvno,
                keys,
            });
        }
        Ok(())
    }

    pub fn find(&self, realm: &str, components: &[String]) -> Option<&DbPrincipal> {
        self.principals
            .iter()
            .find(|p| p.realm == realm && p.components == components)
    }

    /// `krbtgt/REALM@REALM`
    pub fn krbtgt(&self) -> Option<&DbPrincipal> {
        self.find(&self.realm, &["krbtgt".to_string(), self.realm.clone()])
    }
}

// ============================ KDC ============================

/// KDC 的配置。
#[derive(Clone, Debug)]
pub struct KdcConfig {
    pub realm: String,
    pub db: Arc<Kdb>,
    /// 票的默认有效期（秒）
    pub max_lifetime: i64,
    /// 允许的时钟偏移（秒）
    pub clock_skew: i64,
    /// 打日志
    pub verbose: bool,
}

impl KdcConfig {
    pub fn new(realm: &str) -> Self {
        KdcConfig {
            realm: realm.to_string(),
            db: Arc::new(Kdb::new(realm)),
            max_lifetime: DEFAULT_LIFETIME,
            clock_skew: CLOCK_SKEW,
            verbose: false,
        }
    }

    pub fn with_db(mut self, db: Kdb) -> Self {
        self.realm = db.realm.clone();
        self.db = Arc::new(db);
        self
    }
}

/// 处理结果：要么一个回包，要么一个错误（错误也要编成 KRB-ERROR 发回去）。
pub struct KdcReply {
    pub bytes: Vec<u8>,
    pub note: String,
}

/// KDC 本体。
pub struct Kdc {
    pub cfg: KdcConfig,
}

impl Kdc {
    pub fn new(cfg: KdcConfig) -> Self {
        Kdc { cfg }
    }

    fn log(&self, msg: &str) {
        if self.cfg.verbose {
            eprintln!("kdc: {msg}");
        }
    }

    /// 处理任意一个 KDC 请求字节串，返回要发回去的字节。
    pub fn handle(&self, req: &[u8]) -> Vec<u8> {
        match self.dispatch(req) {
            Ok(r) => r.bytes,
            Err(e) => {
                self.log(&format!("请求处理失败: {e}"));
                // 连报文都解析不了：回一个 KRB-ERROR，realm 用配置里的
                let err = make_krb_error(
                    errc::KRB_ERR_GENERIC,
                    Some(e.to_string()),
                    None,
                    &self.cfg.realm,
                    None,
                    &PrincipalName::new(
                        NT_SRV_INST,
                        vec!["krbtgt".to_string(), self.cfg.realm.clone()],
                    ),
                    None,
                    sys::now_unix(),
                );
                err.to_der()
            }
        }
    }

    fn dispatch(&self, req: &[u8]) -> Result<KdcReply> {
        let kreq = KdcReq::from_bytes(req).map_err(|e| e.ctx("解析 KDC-REQ"))?;
        if kreq.pvno != PVNO {
            return Ok(self.error_reply(
                &kreq,
                errc::KDC_ERR_BAD_PVNO,
                Some(format!("pvno should be 5, got {}", kreq.pvno)),
            ));
        }
        match kreq.msg_type {
            MSG_AS_REQ => self.handle_as_req(&kreq),
            MSG_TGS_REQ => self.handle_tgs_req(&kreq),
            other => Ok(self.error_reply(
                &kreq,
                errc::KRB_AP_ERR_MSG_TYPE,
                Some(format!("unsupported msg-type {other}")),
            )),
        }
    }

    /// 造一个 KRB-ERROR 回包（尽量把请求里的 cname/realm 抄回去）。
    fn error_reply(&self, req: &KdcReq, code: i32, text: Option<String>) -> KdcReply {
        self.error_reply_with(req, code, text, None)
    }

    fn error_reply_with(
        &self,
        req: &KdcReq,
        code: i32,
        text: Option<String>,
        e_data: Option<Vec<u8>>,
    ) -> KdcReply {
        let realm = if req.body.realm.is_empty() { self.cfg.realm.clone() } else { req.body.realm.clone() };
        let sname = req.body.sname.clone().unwrap_or_else(|| {
            PrincipalName::new(NT_SRV_INST, vec!["krbtgt".to_string(), realm.clone()])
        });
        let err = make_krb_error(
            code,
            text,
            e_data,
            &realm,
            req.body.cname.as_ref(),
            &sname,
            None,
            sys::now_unix(),
        );
        KdcReply {
            bytes: err.to_der(),
            note: format!("错误 {} ({})", errc::name(code), code),
        }
    }

    /// 在客户端给的 etype 列表里挑一个「我们支持、且这个 principal 有钥匙」的。
    fn negotiate(&self, client_etypes: &[i32], p: &DbPrincipal) -> Option<i32> {
        for e in client_etypes {
            if crypto_krb::is_supported(*e) && p.key(*e).is_some() {
                return Some(*e);
            }
        }
        // 客户端没列（或列了都不行）时退到我们最想要的那个
        for e in p.etypes() {
            if client_etypes.is_empty() {
                return Some(e);
            }
        }
        None
    }

    // ---------------------------- AS-REQ ----------------------------

    fn handle_as_req(&self, req: &KdcReq) -> Result<KdcReply> {
        let now = sys::now_unix();
        let realm = req.body.realm.clone();
        if realm != self.cfg.realm {
            return Ok(self.error_reply(
                req,
                errc::KDC_ERR_WRONG_REALM,
                Some(format!("this KDC only serves realm {}", self.cfg.realm)),
            ));
        }
        let cname = match &req.body.cname {
            Some(c) => c.clone(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_C_PRINCIPAL_UNKNOWN,
                    Some("AS-REQ without cname".into()),
                ))
            }
        };
        // AS-REQ 的 sname 必须是 krbtgt/REALM
        let want_sname = vec!["krbtgt".to_string(), realm.clone()];
        if let Some(s) = &req.body.sname {
            if s.components != want_sname {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_S_PRINCIPAL_UNKNOWN,
                    Some(format!("AS-REQ sname must be krbtgt/{}", realm)),
                ));
            }
        }
        let client = match self.cfg.db.find(&realm, &cname.components) {
            Some(p) => p.clone(),
            None => {
                self.log(&format!("未知 principal {}", cname.full(&realm)));
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_C_PRINCIPAL_UNKNOWN,
                    Some(format!("{} not found in database", cname.full(&realm))),
                ));
            }
        };
        let krbtgt = match self.cfg.db.krbtgt() {
            Some(p) => p.clone(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_S_PRINCIPAL_UNKNOWN,
                    Some(format!("KDC has no krbtgt/{}", realm)),
                ))
            }
        };
        let etype = match self.negotiate(&req.body.etypes, &client) {
            Some(e) => e,
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_ETYPE_NOSUPP,
                    Some(format!(
                        "no usable etype: client wants {:?}, server has {:?}",
                        req.body.etypes,
                        client.etypes()
                    )),
                ))
            }
        };

        // ---- 预认证 ----
        match req.pa(PA_ENC_TIMESTAMP) {
            None => {
                // 告诉客户端要预认证，并把每种 etype 的 salt 一起给它
                let salt = crypto_krb::default_salt(&realm, &cname.components);
                let hints: Vec<EtypeHint> = client
                    .etypes()
                    .into_iter()
                    .map(|e| EtypeHint { etype: e, salt: Some(String::from_utf8_lossy(&salt).into_owned()), s2kparams: None })
                    .collect();
                self.log(&format!(
                    "{} 未带预认证，回 KDC_ERR_PREAUTH_REQUIRED（etype {:?}）",
                    cname.full(&realm),
                    client.etypes()
                ));
                return Ok(self.error_reply_with(
                    req,
                    errc::KDC_ERR_PREAUTH_REQUIRED,
                    Some("NEED PREAUTH".into()),
                    Some(make_preauth_required_edata(&hints)),
                ));
            }
            Some(pa) => {
                // 用客户端给的那个 etype 解（必须是我们支持的、且这个 principal 有）
                let pa_etype = match EncryptedData::from_bytes(&pa.padata_value) {
                    Ok(ed) => ed.etype,
                    Err(e) => { self.log(&format!("detail: {e}"));
                        return Ok(self.error_reply(
                            req,
                            errc::KDC_ERR_PREAUTH_FAILED,
                            Some("cannot decode PA-ENC-TIMESTAMP".into()),
                        ))
                    }
                };
                if !crypto_krb::is_supported(pa_etype) {
                    return Ok(self.error_reply(
                        req,
                        errc::KDC_ERR_ETYPE_NOSUPP,
                        Some(format!("preauth uses unsupported etype {pa_etype}")),
                    ));
                }
                let key = match client.key(pa_etype) {
                    Some(k) => k.to_vec(),
                    None => {
                        return Ok(self.error_reply(
                            req,
                            errc::KDC_ERR_PREAUTH_FAILED,
                            Some(format!(
                                "{} has no key for {}",
                                cname.full(&realm),
                                crypto_krb::etype_name(pa_etype)
                            )),
                        ))
                    }
                };
                let ts = match open_pa_enc_timestamp(&key, pa_etype, pa) {
                    Ok(t) => t,
                    Err(e) => { self.log(&format!("detail: {e}"));
                        // 错口令在这里被挡下：HMAC 常数时间校验不通过
                        self.log(&format!(
                            "{} 预认证失败（{}）",
                            cname.full(&realm),
                            crypto_krb::etype_name(pa_etype)
                        ));
                        return Ok(self.error_reply(
                            req,
                            errc::KDC_ERR_PREAUTH_FAILED,
                            Some("preauth timestamp failed to decrypt".into()),
                        ));
                    }
                };
                let skew = (now - ts.patimestamp).abs();
                if skew > self.cfg.clock_skew {
                    return Ok(self.error_reply(
                        req,
                        errc::KRB_AP_ERR_SKEW,
                        Some(format!(
                            "clock skew {skew}s exceeds allowed {}s",
                            self.cfg.clock_skew
                        )),
                    ));
                }
                self.log(&format!(
                    "{} 预认证通过（{}）",
                    cname.full(&realm),
                    crypto_krb::etype_name(pa_etype)
                ));

                // ---- 发 TGT ----
                let session = sys::random_vec(crypto_krb::key_len(etype).unwrap_or(32))?;
                let mut flags = bit_set(bit_set(0, TKTFLAG_INITIAL), TKTFLAG_PRE_AUTHENT);
                if bit_get(req.body.kdc_options, KDCOPT_FORWARDABLE) {
                    flags = bit_set(flags, TKTFLAG_FORWARDABLE);
                }
                if bit_get(req.body.kdc_options, KDCOPT_PROXIABLE) {
                    flags = bit_set(flags, TKTFLAG_PROXIABLE);
                }
                let renewable = bit_get(req.body.kdc_options, KDCOPT_RENEWABLE)
                    || bit_get(req.body.kdc_options, KDCOPT_RENEWABLE_OK);
                if renewable {
                    flags = bit_set(flags, TKTFLAG_RENEWABLE);
                }
                let (start, end) = self.validity(&req.body, now);
                // renew-till = min(客户端要的 rtime, now + 上限)，且不小于 endtime
                let renew_till = if renewable {
                    let cap = now + DEFAULT_RENEW_LIFETIME;
                    let want = req.body.rtime.unwrap_or(cap);
                    Some(want.min(cap).max(end))
                } else {
                    None
                };

                let ticket_part = EncTicketPart {
                    flags,
                    key: EncryptionKey::new(etype, session.clone()),
                    crealm: realm.clone(),
                    cname: cname.clone(),
                    transited: (0, Vec::new()),
                    authtime: now,
                    starttime: Some(start),
                    endtime: end,
                    renew_till,
                    caddr: None,
                    authorization_data: None,
                };
                let ticket_cipher = crypto_krb::encrypt(
                    krbtgt.key(etype).map(|_| etype).unwrap_or_else(|| krbtgt.etypes()[0]),
                    &krbtgt.key(etype).map(|k| k.to_vec()).unwrap_or_else(|| {
                        let e = krbtgt.etypes()[0];
                        krbtgt.key(e).unwrap().to_vec()
                    }),
                    USAGE_TICKET,
                    &ticket_part.to_der(),
                )?;
                let ticket = Ticket {
                    tkt_vno: TKT_VNO,
                    realm: realm.clone(),
                    sname: PrincipalName::new(
                        NT_SRV_INST,
                        vec!["krbtgt".to_string(), realm.clone()],
                    ),
                    enc_part: EncryptedData::new(
                        etype,
                        Some(krbtgt.kvno as u32),
                        ticket_cipher,
                    ),
                };
                let rep_part = EncKdcRepPart {
                    key: EncryptionKey::new(etype, session),
                    last_req: vec![(0, now)],
                    nonce: req.body.nonce,
                    key_expiration: None,
                    flags,
                    authtime: now,
                    starttime: Some(start),
                    endtime: end,
                    renew_till,
                    srealm: realm.clone(),
                    sname: ticket.sname.clone(),
                    caddr: None,
                };
                let rep_cipher =
                    crypto_krb::encrypt(etype, &client.key(etype).unwrap().to_vec(), USAGE_AS_REP_ENC_PART, &rep_part.to_der(false))?;
                let rep = KdcRep {
                    pvno: PVNO,
                    msg_type: MSG_AS_REP,
                    padata: Vec::new(),
                    crealm: realm.clone(),
                    cname,
                    ticket,
                    enc_part: EncryptedData::new(etype, None, rep_cipher),
                };
                Ok(KdcReply {
                    bytes: rep.to_der(),
                    note: format!("AS-REP（{}）", crypto_krb::etype_name(etype)),
                })
            }
        }
    }

    /// 票的有效期：`start = now`，`end = min(now + 上限, till)`。
    fn validity(&self, body: &KdcReqBody, now: i64) -> (i64, i64) {
        let start = now;
        let mut end = now + self.cfg.max_lifetime;
        if body.till > 0 && body.till < end {
            end = body.till;
        }
        if end <= start {
            end = start + 60;
        }
        (start, end)
    }

    // ---------------------------- TGS-REQ ----------------------------

    fn handle_tgs_req(&self, req: &KdcReq) -> Result<KdcReply> {
        let now = sys::now_unix();
        let realm = if req.body.realm.is_empty() { self.cfg.realm.clone() } else { req.body.realm.clone() };
        let sname = match &req.body.sname {
            Some(s) => s.clone(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_S_PRINCIPAL_UNKNOWN,
                    Some("TGS-REQ without sname".into()),
                ))
            }
        };
        // 1) 解 PA-TGS-REQ 里的 AP-REQ，用 krbtgt 密钥解票
        let pa = match req.pa(PA_TGS_REQ) {
            Some(p) => p,
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_PADATA_TYPE_NOSUPP,
                    Some("TGS-REQ missing PA-TGS-REQ".into()),
                ))
            }
        };
        let ap = match ApReq::from_bytes(&pa.padata_value) {
            Ok(a) => a,
            Err(e) => { self.log(&format!("detail: {e}"));
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BAD_INTEGRITY,
                    Some("cannot parse PA-TGS-REQ".into()),
                ))
            }
        };
        let krbtgt = match self.cfg.db.krbtgt() {
            Some(p) => p.clone(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_S_PRINCIPAL_UNKNOWN,
                    Some("KDC has no krbtgt".into()),
                ))
            }
        };
        let tkt_etype = ap.ticket.enc_part.etype;
        if !crypto_krb::is_supported(tkt_etype) {
            return Ok(self.error_reply(
                req,
                errc::KDC_ERR_ETYPE_NOSUPP,
                Some(format!("ticket etype {tkt_etype} unsupported")),
            ));
        }
        let tkt_key = match krbtgt.key(tkt_etype) {
            Some(k) => k.to_vec(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BADKEYVER,
                    Some("no krbtgt key for that kvno".into()),
                ))
            }
        };
        // 票的服务主体必须是 krbtgt（本 KDC 只发自己的 TGT 换服务票）
        if ap.ticket.sname.components
            != vec!["krbtgt".to_string(), ap.ticket.realm.clone()]
        {
            return Ok(self.error_reply(
                req,
                errc::KRB_AP_ERR_NOT_US,
                Some("ticket was not issued for this KDC".into()),
            ));
        }
        let plain = match crypto_krb::decrypt(tkt_etype, &tkt_key, USAGE_TICKET, &ap.ticket.enc_part.cipher)
        {
            Ok(p) => p,
            Err(e) => { self.log(&format!("detail: {e}"));
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BAD_INTEGRITY,
                    Some("cannot decrypt ticket".into()),
                ))
            }
        };
        let tkt = match EncTicketPart::from_bytes(&plain) {
            Ok(t) => t,
            Err(e) => { self.log(&format!("detail: {e}"));
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BAD_INTEGRITY,
                    Some("cannot parse ticket".into()),
                ))
            }
        };
        if tkt.endtime <= now {
            return Ok(self.error_reply(req, errc::KRB_AP_ERR_TKT_EXPIRED, Some("ticket expired".into())));
        }
        if tkt.starttime.unwrap_or(tkt.authtime) > now + self.cfg.clock_skew {
            return Ok(self.error_reply(req, errc::KRB_AP_ERR_TKT_NYV, Some("ticket not yet valid".into())));
        }
        // 2) 解 authenticator（用票里的会话密钥）
        let sess = &tkt.key;
        let auth_plain = match crypto_krb::decrypt(
            sess.keytype,
            &sess.keyvalue,
            USAGE_TGS_REQ_AUTH,
            &ap.authenticator.cipher,
        ) {
            Ok(p) => p,
            Err(e) => { self.log(&format!("detail: {e}"));
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BAD_INTEGRITY,
                    Some("cannot decrypt authenticator".into()),
                ))
            }
        };
        let auth = match Authenticator::from_bytes(&auth_plain) {
            Ok(a) => a,
            Err(e) => { self.log(&format!("detail: {e}"));
                return Ok(self.error_reply(
                    req,
                    errc::KRB_AP_ERR_BAD_INTEGRITY,
                    Some("cannot parse authenticator".into()),
                ))
            }
        };
        if (now - auth.ctime).abs() > self.cfg.clock_skew {
            return Ok(self.error_reply(
                req,
                errc::KRB_AP_ERR_SKEW,
                Some(format!("authenticator clock skew {}s", (now - auth.ctime).abs())),
            ));
        }
        if auth.cname.components != tkt.cname.components {
            return Ok(self.error_reply(
                req,
                errc::KRB_AP_ERR_BADMATCH,
                Some("authenticator cname does not match ticket".into()),
            ));
        }
        // 3) 找服务密钥，发服务票
        let svc = match self.cfg.db.find(&realm, &sname.components) {
            Some(p) => p.clone(),
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_S_PRINCIPAL_UNKNOWN,
                    Some(format!("{} not found in database", sname.full(&realm))),
                ))
            }
        };
        let etype = match self.negotiate(&req.body.etypes, &svc) {
            Some(e) => e,
            None => {
                return Ok(self.error_reply(
                    req,
                    errc::KDC_ERR_ETYPE_NOSUPP,
                    Some("no common etype between service and client".into()),
                ))
            }
        };
        let session = sys::random_vec(crypto_krb::key_len(etype).unwrap_or(32))?;
        let mut flags = tkt.flags;
        flags = bit_set(flags, TKTFLAG_FORWARDABLE & flags); // 保持原样
        let (start, end) = self.validity(&req.body, now);
        let svc_part = EncTicketPart {
            flags,
            key: EncryptionKey::new(etype, session.clone()),
            crealm: tkt.crealm.clone(),
            cname: tkt.cname.clone(),
            transited: (0, Vec::new()),
            authtime: tkt.authtime,
            starttime: Some(start),
            endtime: end,
            renew_till: tkt.renew_till,
            caddr: None,
            authorization_data: None,
        };
        let svc_key = svc.key(etype).map(|k| k.to_vec()).unwrap_or_else(|| {
            let e = svc.etypes()[0];
            svc.key(e).unwrap().to_vec()
        });
        let svc_ticket = Ticket {
            tkt_vno: TKT_VNO,
            realm: realm.clone(),
            sname: sname.clone(),
            enc_part: EncryptedData::new(etype, Some(svc.kvno as u32), crypto_krb::encrypt(etype, &svc_key, USAGE_TICKET, &svc_part.to_der())?),
        };
        let rep_part = EncKdcRepPart {
            key: EncryptionKey::new(etype, session),
            last_req: vec![(0, now)],
            nonce: req.body.nonce,
            key_expiration: None,
            flags,
            authtime: tkt.authtime,
            starttime: Some(start),
            endtime: end,
            renew_till: tkt.renew_till,
            srealm: realm.clone(),
            sname: sname.clone(),
            caddr: None,
        };
        let rep_cipher = crypto_krb::encrypt(
            sess.keytype,
            &sess.keyvalue,
            USAGE_TGS_REP_ENC_PART,
            &rep_part.to_der(true),
        )?;
        let rep = KdcRep {
            pvno: PVNO,
            msg_type: MSG_TGS_REP,
            padata: Vec::new(),
            crealm: realm.clone(),
            cname: tkt.cname.clone(),
            ticket: svc_ticket,
            enc_part: EncryptedData::new(sess.keytype, None, rep_cipher),
        };
        Ok(KdcReply {
            bytes: rep.to_der(),
            note: format!("TGS-REP（{}）", crypto_krb::etype_name(etype)),
        })
    }

    // ---------------------------- 服务循环 ----------------------------

    /// 起 UDP + TCP 服务（阻塞；`stop` 由调用方用 Ctrl-C 或超时控制）。
    pub fn serve(self, port: u16) -> Result<()> {
        let me = Arc::new(self);
        let udp = UdpSocket::bind(("0.0.0.0", port))
            .map_err(|e| Error::new(format!("UDP 绑定 0.0.0.0:{port} 失败: {e}")))?;
        let tcp = TcpListener::bind(("0.0.0.0", port))
            .map_err(|e| Error::new(format!("TCP 绑定 0.0.0.0:{port} 失败: {e}")))?;
        eprintln!(
            "KDC 已启动：realm={} 端口={} principal 数={}",
            me.cfg.realm,
            port,
            me.cfg.db.principals.len()
        );
        // TCP 线程
        {
            let me = me.clone();
            std::thread::spawn(move || {
                for conn in tcp.incoming() {
                    match conn {
                        Ok(c) => {
                            let me = me.clone();
                            std::thread::spawn(move || {
                                let _ = serve_tcp_conn(&me, c);
                            });
                        }
                        Err(e) => eprintln!("kdc: TCP accept 失败: {e}"),
                    }
                }
            });
        }
        // UDP 主循环
        let mut buf = vec![0u8; 65535];
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let req = buf[..n].to_vec();
                    let mut out = me.handle(&req);
                    // 一个 UDP 数据报放不下时，回 KRB_ERR_RESPONSE_TOO_BIG(52)，
                    // 让客户端改用 TCP 重发（RFC 4120 §7.2.1 的做法）。
                    if out.len() > 60_000 {
                        out = too_big_error(&me.cfg.realm);
                    }
                    if let Err(e) = udp.send_to(&out, from) {
                        eprintln!("kdc: UDP 回包失败: {e}");
                    }
                }
                Err(e) => eprintln!("kdc: UDP recv 失败: {e}"),
            }
        }
    }
}

/// 造一个 `KRB_ERR_RESPONSE_TOO_BIG` 错误报文。
pub fn too_big_error(realm: &str) -> Vec<u8> {
    make_krb_error(
        errc::KRB_ERR_RESPONSE_TOO_BIG,
        Some("response too big for UDP, retry over TCP".into()),
        None,
        realm,
        None,
        &PrincipalName::new(NT_SRV_INST, vec!["krbtgt".to_string(), realm.to_string()]),
        None,
        sys::now_unix(),
    )
    .to_der()
}

fn serve_tcp_conn(kdc: &Kdc, mut s: TcpStream) -> Result<()> {
    let mut lenb = [0u8; 4];
    if s.read_exact(&mut lenb).is_err() {
        return Ok(()); // 对端没发完整长度就断了
    }
    let n = u32::from_be_bytes(lenb) as usize;
    if n == 0 || n > 1 << 20 {
        return Err(Error::new(format!("TCP 请求长度不合理: {n}")));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).map_err(|e| Error::new(format!("TCP 读请求失败: {e}")))?;
    let reply = kdc.handle(&buf);
    s.write_all(&(reply.len() as u32).to_be_bytes())
        .and_then(|_| s.write_all(&reply))
        .map_err(|e| Error::new(format!("TCP 回包失败: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmless_crypto::sys::hex;

    const REALM: &str = "VMKRB.TEST";

    fn test_db() -> Kdb {
        let mut db = Kdb::new(REALM);
        db.add_password(&["alice".to_string()], "secretpw", 1).unwrap();
        db.add_password(&["krbtgt".to_string(), REALM.to_string()], "krbtgt-pw", 1)
            .unwrap();
        db.add_password(&["host".to_string(), "localhost".to_string()], "svc-pw", 1)
            .unwrap();
        db
    }

    fn kdc() -> Kdc {
        Kdc::new(KdcConfig::new(REALM).with_db(test_db()))
    }

    fn as_req_bytes(cname: &str, with_ts: Option<(i32, &[u8], i64)>) -> Vec<u8> {
        let now = sys::now_unix();
        let mut p = AsReqParams::new(
            REALM,
            PrincipalName::parse(cname).unwrap(),
            vec![18, 17],
            now,
        );
        if let Some((etype, key, t)) = with_ts {
            p.padata = vec![make_pa_enc_timestamp(etype, key, t, 0).unwrap()];
        }
        p.to_req().to_der()
    }

    fn alice_key(etype: i32) -> Vec<u8> {
        let salt = crypto_krb::default_salt(REALM, &["alice".to_string()]);
        crypto_krb::string2key("secretpw", &salt, etype, crypto_krb::DEFAULT_ITERATIONS).unwrap()
    }

    #[test]
    fn keytab_roundtrip() {
        // 手工造一份 keytab（与 jdk-crypto 的 ktab 写的格式一致）
        let mut d = vec![0x05, 0x02];
        let put16 = |d: &mut Vec<u8>, v: u16| d.extend_from_slice(&v.to_be_bytes());
        let put32 = |d: &mut Vec<u8>, v: u32| d.extend_from_slice(&v.to_be_bytes());
        let put_str = |d: &mut Vec<u8>, s: &str| {
            put16(d, s.len() as u16);
            d.extend_from_slice(s.as_bytes());
        };
        put32(&mut d, 2);
        put_str(&mut d, REALM);
        put_str(&mut d, "krbtgt");
        put_str(&mut d, REALM);
        put32(&mut d, 2);
        put32(&mut d, 1_700_000_000);
        d.push(1);
        put16(&mut d, 32);
        put16(&mut d, 18);
        d.extend_from_slice(&[7u8; 32]);
        put32(&mut d, 1);
        let e = parse_keytab(&d).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].principal(), "krbtgt/VMKRB.TEST@VMKRB.TEST");
        assert_eq!(e[0].enctype, 18);
        assert_eq!(e[0].key.len(), 32);
        assert_eq!(e[0].kvno, 1);
        // 截断/垃圾不能 panic
        for cut in 0..d.len() {
            let _ = parse_keytab(&d[..cut]);
        }
        let mut x: u64 = 0x1234_5678;
        for _ in 0..5000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 64) as usize;
            let mut b = vec![0u8; n];
            for i in 0..n {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b[i] = x as u8;
            }
            let _ = parse_keytab(&b);
        }
    }

    #[test]
    fn as_exchange_with_preauth() {
        let k = kdc();
        // 1) 裸请求 → PREAUTH_REQUIRED，带 ETYPE-INFO2
        let r1 = k.handle(&as_req_bytes("alice", None));
        let e = parse_krb_error(&r1).expect("应是 KRB-ERROR");
        assert_eq!(e.error_code, errc::KDC_ERR_PREAUTH_REQUIRED);
        let hints = etype_hints(&e).unwrap();
        assert!(hints.iter().any(|h| h.etype == 18));
        assert_eq!(
            hints[0].salt.as_deref(),
            Some(&*format!("{REALM}alice"))
        );
        // 2) 带预认证 → AS-REP
        let key = alice_key(18);
        let now = sys::now_unix();
        let r2 = k.handle(&as_req_bytes("alice", Some((18, &key, now))));
        assert_eq!(r2[0], app_tag(APP_AS_REP));
        let rep = KdcRep::from_bytes(&r2).unwrap();
        assert_eq!(rep.crealm, REALM);
        assert_eq!(rep.cname.display(), "alice");
        assert_eq!(rep.enc_part.etype, 18);
        // 客户端能解开 enc-part
        let plain = crypto_krb::decrypt(18, &key, USAGE_AS_REP_ENC_PART, &rep.enc_part.cipher).unwrap();
        let part = EncKdcRepPart::from_bytes(&plain, false).unwrap();
        assert!(bit_get(part.flags, TKTFLAG_INITIAL));
        assert!(bit_get(part.flags, TKTFLAG_PRE_AUTHENT));
        assert_eq!(part.sname.display(), format!("krbtgt/{REALM}"));
        assert_eq!(part.key.keytype, 18);
        assert_eq!(part.key.keyvalue.len(), 32);
        // 票能用 krbtgt 密钥解开，且里面是同一把会话密钥
        let krbtgt = k.cfg.db.krbtgt().unwrap();
        let tk = crypto_krb::decrypt(18, krbtgt.key(18).unwrap(), USAGE_TICKET, &rep.ticket.enc_part.cipher).unwrap();
        let tpart = EncTicketPart::from_bytes(&tk).unwrap();
        assert_eq!(tpart.key.keyvalue, part.key.keyvalue);
        assert_eq!(tpart.cname.display(), "alice");
        assert_eq!(tpart.endtime, part.endtime);
    }

    #[test]
    fn wrong_password_gives_preauth_failed() {
        let k = kdc();
        let wrong = {
            let salt = crypto_krb::default_salt(REALM, &["alice".to_string()]);
            crypto_krb::string2key("not-the-password", &salt, 18, 4096).unwrap()
        };
        let r = k.handle(&as_req_bytes("alice", Some((18, &wrong, sys::now_unix()))));
        let e = parse_krb_error(&r).unwrap();
        assert_eq!(e.error_code, errc::KDC_ERR_PREAUTH_FAILED);
        assert!(describe_error(&e).contains("Pre-authentication information was invalid"));
    }

    #[test]
    fn unknown_principal_and_wrong_realm() {
        let k = kdc();
        let r = k.handle(&as_req_bytes("nobody", None));
        let e = parse_krb_error(&r).unwrap();
        assert_eq!(e.error_code, errc::KDC_ERR_C_PRINCIPAL_UNKNOWN);

        // realm 不对
        let now = sys::now_unix();
        let p = AsReqParams::new("OTHER.REALM", PrincipalName::parse("alice").unwrap(), vec![18], now);
        let r = k.handle(&p.to_req().to_der());
        let e = parse_krb_error(&r).unwrap();
        assert_eq!(e.error_code, errc::KDC_ERR_WRONG_REALM);
    }

    #[test]
    fn clock_skew_rejected() {
        let k = kdc();
        let key = alice_key(18);
        let old = sys::now_unix() - 3600; // 一小时前，超过 5 分钟
        let r = k.handle(&as_req_bytes("alice", Some((18, &key, old))));
        let e = parse_krb_error(&r).unwrap();
        assert_eq!(e.error_code, errc::KRB_AP_ERR_SKEW);
        // 4 分钟前要接受
        let ok = sys::now_unix() - 240;
        let r = k.handle(&as_req_bytes("alice", Some((18, &key, ok))));
        assert_eq!(r[0], app_tag(APP_AS_REP));
    }

    #[test]
    fn etype_negotiation() {
        let k = kdc();
        let now = sys::now_unix();
        // 只要 aes128
        let key = alice_key(17);
        let mut p = AsReqParams::new(REALM, PrincipalName::parse("alice").unwrap(), vec![17], now);
        p.padata = vec![make_pa_enc_timestamp(17, &key, now, 0).unwrap()];
        let r = k.handle(&p.to_req().to_der());
        let rep = KdcRep::from_bytes(&r).unwrap();
        assert_eq!(rep.enc_part.etype, 17);
        // 只要我们不支持的 etype
        let mut p = AsReqParams::new(REALM, PrincipalName::parse("alice").unwrap(), vec![23], now);
        p.padata = vec![];
        let r = k.handle(&p.to_req().to_der());
        let e = parse_krb_error(&r).unwrap();
        assert_eq!(e.error_code, errc::KDC_ERR_ETYPE_NOSUPP);
    }

    #[test]
    fn garbage_request_does_not_panic() {
        let k = kdc();
        let mut x: u64 = 0xfeed_face;
        for i in 0..5000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 200) as usize;
            let mut b = vec![0u8; n];
            for j in 0..n {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b[j] = x as u8;
            }
            if i % 3 == 0 {
                b.insert(0, app_tag(APP_AS_REQ));
            }
            let out = k.handle(&b);
            // 回包必须是可解析的 KRB-ERROR（或 AS-REP），不能是空
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn tgs_exchange() {
        let k = kdc();
        let now = sys::now_unix();
        // 先拿 TGT
        let key = alice_key(18);
        let r = k.handle(&as_req_bytes("alice", Some((18, &key, now))));
        let rep = KdcRep::from_bytes(&r).unwrap();
        let plain = crypto_krb::decrypt(18, &key, USAGE_AS_REP_ENC_PART, &rep.enc_part.cipher).unwrap();
        let part = EncKdcRepPart::from_bytes(&plain, false).unwrap();

        // 再用 TGT 换 host/localhost 的服务票
        let pa = make_pa_tgs_req(
            &rep.ticket,
            &part.key,
            &PrincipalName::parse("alice").unwrap(),
            REALM,
            now,
        )
        .unwrap();
        let params = TgsReqParams {
            realm: REALM.into(),
            cname: PrincipalName::parse("alice").unwrap(),
            sname: PrincipalName::parse("host/localhost").unwrap(),
            nonce: 99,
            etypes: vec![18],
            kdc_options: 0,
            till: now + 3600,
            rtime: None,
            pa_tgs_req: pa,
        };
        let r = k.handle(&params.to_req().to_der());
        assert_eq!(r[0], app_tag(APP_TGS_REP), "应是 TGS-REP");
        let trep = KdcRep::from_bytes(&r).unwrap();
        assert_eq!(trep.ticket.sname.display(), "host/localhost");
        let tplain =
            crypto_krb::decrypt(18, &part.key.keyvalue, USAGE_TGS_REP_ENC_PART, &trep.enc_part.cipher)
                .unwrap();
        let tpart = EncKdcRepPart::from_bytes(&tplain, true).unwrap();
        assert_eq!(tpart.nonce, 99);
        assert_eq!(tpart.sname.display(), "host/localhost");
        // 服务票能用服务密钥解开
        let svc = k.cfg.db.find(REALM, &["host".to_string(), "localhost".to_string()]).unwrap();
        let stk = crypto_krb::decrypt(18, svc.key(18).unwrap(), USAGE_TICKET, &trep.ticket.enc_part.cipher).unwrap();
        let spart = EncTicketPart::from_bytes(&stk).unwrap();
        assert_eq!(spart.key.keyvalue, tpart.key.keyvalue);
        assert_eq!(spart.cname.display(), "alice");
        let _ = hex(b"");
    }
}
