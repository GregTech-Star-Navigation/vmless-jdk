//! `kinit` / `klist` 的逻辑：跟 KDC 说话、拿票、写缓存。
//!
//! 网络只有两条路：UDP（默认，Kerberos 的老规矩）与 TCP（回包超过一个 UDP 包时）。
//! 两条都要有超时，且超时必须报「KDC 没回包」而不是挂死。

use crate::asn1_krb::*;
use crate::ccache::{cred_from_as_rep, CcCache, CcPrincipal, Credential};
use crate::crypto_krb;
use crate::msg::*;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use vmless_crypto::{sys, Error, Result};

/// 默认 KDC 端口。
pub const KDC_PORT: u16 = 88;
/// 默认超时（毫秒）。
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// 随机 nonce（用于把回包和请求对上）。
pub fn random_nonce() -> u32 {
    match sys::random_vec(4) {
        Ok(v) if v.len() == 4 => u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
        _ => (sys::now_unix() as u32).wrapping_mul(2654435761),
    }
}

/// KDC 地址（`host` 或 `host:port`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdcAddr {
    pub host: String,
    pub port: u16,
}

impl KdcAddr {
    pub fn parse(s: &str) -> Result<KdcAddr> {
        let t = s.trim();
        if t.is_empty() {
            return Err(Error::new("KDC 地址为空"));
        }
        // [::1]:88 / host:88 / host
        if let Some(rest) = t.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| Error::new(format!("KDC 地址不合法: {t}")))?;
            let port = match tail.strip_prefix(':') {
                Some(p) => p
                    .parse::<u16>()
                    .map_err(|_| Error::new(format!("KDC 端口不合法: {p}")))?,
                None => KDC_PORT,
            };
            return Ok(KdcAddr { host: host.to_string(), port });
        }
        match t.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => Ok(KdcAddr {
                host: h.to_string(),
                port: p
                    .parse::<u16>()
                    .map_err(|_| Error::new(format!("KDC 端口不合法: {p}")))?,
            }),
            _ => Ok(KdcAddr { host: t.to_string(), port: KDC_PORT }),
        }
    }

    /// `host:port` 形式（IPv6 会自动加方括号）。
    pub fn as_string(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl std::fmt::Display for KdcAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{}:{}", self.host, self.port))
    }
}

/// 传输方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
}

/// 一次请求/应答的往返结果。
pub struct RoundTrip {
    pub bytes: Vec<u8>,
    pub via: Transport,
    pub ms: u128,
}

/// 发一个 KDC 请求并收回包。UDP 失败（超时/包太大/回 `KRB_ERR_RESPONSE_TOO_BIG`）
/// 会自动退到 TCP；`force_tcp` 时直接用 TCP。
pub fn exchange(kdc: &KdcAddr, req: &[u8], timeout_ms: u64) -> Result<RoundTrip> {
    exchange_with(kdc, req, timeout_ms, false)
}

/// 同 [`exchange`]，可以强制走 TCP。
pub fn exchange_with(
    kdc: &KdcAddr,
    req: &[u8],
    timeout_ms: u64,
    force_tcp: bool,
) -> Result<RoundTrip> {
    let start = std::time::Instant::now();
    if !force_tcp {
        match exchange_udp(kdc, req, timeout_ms) {
            Ok(bytes) => {
                // KDC 说「太大，走 TCP」时按它说的做
                if let Some(e) = crate::msg::parse_krb_error(&bytes) {
                    if e.error_code == crate::asn1_krb::errc::KRB_ERR_RESPONSE_TOO_BIG {
                        return match exchange_tcp(kdc, req, timeout_ms) {
                            Ok(b) => Ok(RoundTrip { bytes: b, via: Transport::Tcp, ms: start.elapsed().as_millis() }),
                            Err(tcp_err) => Err(Error::new(format!(
                                "KDC 要求改用 TCP，但 TCP 也失败：{tcp_err}"
                            ))),
                        };
                    }
                }
                return Ok(RoundTrip { bytes, via: Transport::Udp, ms: start.elapsed().as_millis() });
            }
            Err(udp_err) => match exchange_tcp(kdc, req, timeout_ms) {
                Ok(bytes) => {
                    return Ok(RoundTrip { bytes, via: Transport::Tcp, ms: start.elapsed().as_millis() })
                }
                Err(tcp_err) => {
                    return Err(Error::new(format!(
                        "与 KDC {} 通信失败：UDP {udp_err}；TCP {tcp_err}",
                        kdc.as_string()
                    )))
                }
            },
        }
    }
    match exchange_tcp(kdc, req, timeout_ms) {
        Ok(bytes) => Ok(RoundTrip { bytes, via: Transport::Tcp, ms: start.elapsed().as_millis() }),
        Err(e) => Err(Error::new(format!("与 KDC {} 的 TCP 通信失败：{e}", kdc.as_string()))),
    }
}

fn exchange_udp(kdc: &KdcAddr, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
    if req.len() > 60_000 {
        return Err(Error::new("请求超过 60000 字节，UDP 不合适"));
    }
    let bind = if kdc.host.contains(':') { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind).map_err(|e| Error::new(format!("UDP 绑定失败: {e}")))?;
    sock.set_read_timeout(Some(std::time::Duration::from_millis(timeout_ms)))
        .map_err(|e| Error::new(format!("设置 UDP 超时失败: {e}")))?;
    sock.send_to(req, kdc.as_string())
        .map_err(|e| Error::new(format!("UDP 发送失败: {e}")))?;
    let mut buf = vec![0u8; 65535];
    let (n, _from) = sock
        .recv_from(&mut buf)
        .map_err(|e| Error::new(format!("没收到回包（{timeout_ms}ms）: {e}")))?;
    buf.truncate(n);
    if buf.is_empty() {
        return Err(Error::new("收到空回包"));
    }
    Ok(buf)
}

fn exchange_tcp(kdc: &KdcAddr, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
    let addr = kdc.as_string();
    let stream = TcpStream::connect(&addr).map_err(|e| Error::new(format!("TCP 连接 {addr} 失败: {e}")))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(timeout_ms)))
        .map_err(|e| Error::new(format!("设置 TCP 超时失败: {e}")))?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_millis(timeout_ms)))
        .map_err(|e| Error::new(format!("设置 TCP 超时失败: {e}")))?;
    let mut s = &stream;
    s.write_all(&(req.len() as u32).to_be_bytes())
        .and_then(|_| s.write_all(req))
        .map_err(|e| Error::new(format!("TCP 发送失败: {e}")))?;
    let mut lenb = [0u8; 4];
    s.read_exact(&mut lenb).map_err(|e| Error::new(format!("TCP 读长度失败: {e}")))?;
    let n = u32::from_be_bytes(lenb) as usize;
    if n == 0 || n > 16 << 20 {
        return Err(Error::new(format!("TCP 回包长度不合理: {n}")));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).map_err(|e| Error::new(format!("TCP 读正文失败: {e}")))?;
    Ok(buf)
}

/// `kinit` 的参数。
#[derive(Clone, Debug)]
pub struct KinitOptions {
    pub realm: String,
    pub kdc: KdcAddr,
    pub cache: String,
    /// 客户端愿意用的 etype（按顺序优先）。
    pub etypes: Vec<i32>,
    pub forwardable: bool,
    pub proxiable: bool,
    pub lifetime: i64,
    pub renewable: i64,
    pub timeout_ms: u64,
    /// 给了 keytab 就不用口令
    pub keytab: Option<String>,
    /// 强制走 TCP（默认先 UDP，失败或包太大才退 TCP）
    pub force_tcp: bool,
    pub verbose: bool,
}

impl KinitOptions {
    pub fn new(realm: &str, kdc: KdcAddr, cache: &str) -> Self {
        KinitOptions {
            realm: realm.to_string(),
            kdc,
            cache: cache.to_string(),
            etypes: vec![crypto_krb::ETYPE_AES256, crypto_krb::ETYPE_AES128],
            forwardable: false,
            proxiable: false,
            lifetime: DEFAULT_LIFETIME,
            renewable: DEFAULT_RENEW_LIFETIME,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            keytab: None,
            force_tcp: false,
            verbose: false,
        }
    }
}

/// `kinit` 的结果。
pub struct KinitResult {
    pub cache: CcCache,
    pub etype: i32,
    pub rounds: usize,
    pub ms: u128,
    pub via: Transport,
}

/// 口令派生出的一把或多把候选密钥（keytab 可能有多个 etype）。
struct Keys {
    candidates: Vec<KeyCandidate>,
}

impl Keys {
    fn get(&self, etype: i32) -> Option<&[u8]> {
        self.candidates.iter().find(|c| c.etype == etype).map(|c| c.key.as_slice())
    }

}

/// 真正的 `kinit`：先裸发一次拿 salt，再带 PA-ENC-TIMESTAMP 重发。
pub fn kinit(principal: &str, opts: &KinitOptions) -> Result<KinitResult> {
    let start = std::time::Instant::now();
    let (cname, realm) = split_principal(principal, &opts.realm)?;
    let now = sys::now_unix();

    // 从 keytab 或口令准备密钥（salt 先用默认值，KDC 若给了 ETYPE-INFO2 会覆盖）
    let mut keys: Option<Keys> = None;
    if let Some(kt) = &opts.keytab {
        let entries = crate::kdc::read_keytab_file(kt)?;
        let mut cands = Vec::new();
        for e in &entries {
            if e.realm != realm {
                continue;
            }
            if e.components != cname.components {
                continue;
            }
            if crypto_krb::is_supported(e.enctype as i32) {
                cands.push(KeyCandidate { etype: e.enctype as i32, key: e.key.clone() });
            }
        }
        if cands.is_empty() {
            return Err(Error::new(format!(
                "keytab {kt} 里没有 {}@{} 的可用密钥",
                cname.display(),
                realm
            )));
        }
        keys = Some(Keys { candidates: cands });
    }

    let mut params = AsReqParams::new(&realm, cname.clone(), opts.etypes.clone(), now);
    params.till = now + opts.lifetime;
    params.rtime = Some(now + opts.renewable);
    if opts.forwardable {
        params.kdc_options = bit_set(params.kdc_options, KDCOPT_FORWARDABLE);
    }
    if opts.proxiable {
        params.kdc_options = bit_set(params.kdc_options, KDCOPT_PROXIABLE);
    }
    if opts.forwardable {
        params.kdc_options = bit_set(params.kdc_options, KDCOPT_FORWARDABLE);
    }

    let mut rounds = 0usize;
    let mut salt_used: Option<(i32, Vec<u8>)> = None;

    // ---- 第一轮：不带预认证 ----
    let req = params.to_req().to_der();
    rounds += 1;
    let rt = exchange_with(&opts.kdc, &req, opts.timeout_ms, opts.force_tcp)?;
    let mut via = rt.via;
    let mut resp = rt.bytes;

    if let Some(e) = parse_krb_error(&resp) {
        if e.error_code == errc::KDC_ERR_PREAUTH_REQUIRED {
            let hints = etype_hints(&e)?;
            if opts.verbose {
                eprintln!("kinit: KDC 要求预认证，提示 {} 种 etype", hints.len());
            }
            // 选一个我们支持的 etype，并按提示的 salt 派生密钥
            let mut chosen: Option<(i32, Option<String>, Option<Vec<u8>>)> = None;
            for h in &hints {
                if !crypto_krb::is_supported(h.etype) {
                    continue;
                }
                if !opts.etypes.is_empty() && !opts.etypes.contains(&h.etype) {
                    continue;
                }
                chosen = Some((h.etype, h.salt.clone(), h.s2kparams.clone()));
                break;
            }
            let (etype, salt_str, s2k) = match chosen {
                Some(c) => c,
                None => {
                    return Err(Error::new(format!(
                        "KDC 提示的加密类型都不支持（提示：{:?}；我们支持：{:?}）",
                        hints.iter().map(|h| h.etype).collect::<Vec<_>>(),
                        opts.etypes
                    )))
                }
            };
            let salt = match salt_str {
                Some(s) => s.into_bytes(),
                None => crypto_krb::default_salt(&realm, &cname.components),
            };
            let iterations = s2k_params_to_iterations(s2k.as_deref())?;
            if keys.is_none() {
                let password = read_password(principal, &realm)?;
                let k = crypto_krb::string2key(&password, &salt, etype, iterations)?;
                keys = Some(Keys { candidates: vec![KeyCandidate { etype, key: k }] });
            }
            let key = keys
                .as_ref()
                .and_then(|k| k.get(etype))
                .ok_or_else(|| {
                    Error::new(format!(
                        "没有 {} 的密钥（keytab 里缺这个 etype？）",
                        crypto_krb::etype_name(etype)
                    ))
                })?;
            salt_used = Some((etype, salt));
            let usec = (sys::now_unix() as u32) % 1_000_000;
            params.padata = vec![make_pa_enc_timestamp(etype, key, sys::now_unix(), usec)?];

            // ---- 第二轮：带预认证 ----
            let req = params.to_req().to_der();
            rounds += 1;
            let rt = exchange_with(&opts.kdc, &req, opts.timeout_ms, opts.force_tcp)?;
            via = rt.via;
            resp = rt.bytes;
        }
    }

    // ---- 处理 AS-REP ----
    if let Some(e) = parse_krb_error(&resp) {
        return Err(Error::new(format!(
            "kinit 失败：{}（{}）",
            describe_error(&e),
            e.error_code
        )));
    }
    let rep = KdcRep::from_bytes(&resp).map_err(|e| e.ctx("解析 AS-REP"))?;
    if rep.msg_type != MSG_AS_REP {
        return Err(Error::new(format!("期望 AS-REP，收到 msg-type {}", rep.msg_type)));
    }
    if rep.cname.display() != cname.display() || rep.crealm != realm {
        return Err(Error::new(format!(
            "AS-REP 的客户端 {}@{} 与请求的 {}@{} 不一致",
            rep.cname.display(),
            rep.crealm,
            cname.display(),
            realm
        )));
    }

    // 解密 enc-part：需要客户端密钥（若第一轮没要预认证，这里按默认 salt 派生）
    let etype = rep.enc_part.etype;
    check_supported(etype, "AS-REP enc-part")?;
    if keys.is_none() {
        let salt = crypto_krb::default_salt(&realm, &cname.components);
        let password = read_password(principal, &realm)?;
        let k = crypto_krb::string2key(&password, &salt, etype, crypto_krb::DEFAULT_ITERATIONS)?;
        keys = Some(Keys { candidates: vec![KeyCandidate { etype, key: k }] });
    }
    let client_key = keys
        .as_ref()
        .and_then(|k| k.get(etype))
        .ok_or_else(|| {
            Error::new(format!(
                "AS-REP 用了 {}，但我们手上没有这把钥匙（keytab 里缺？）",
                crypto_krb::etype_name(etype)
            ))
        })?
        .to_vec();

    let plain = crypto_krb::decrypt(etype, &client_key, USAGE_AS_REP_ENC_PART, &rep.enc_part.cipher)
        .map_err(|e| e.ctx("解密 AS-REP enc-part（口令/密钥不对，或 KDC 的 salt 与客户端不一致）"))?;
    let part = EncKdcRepPart::from_bytes(&plain, false).map_err(|e| e.ctx("解析 EncASRepPart"))?;

    if part.nonce != params.nonce {
        return Err(Error::new(format!(
            "AS-REP 的 nonce {} 与请求的 {} 不一致（可能是重放或串包）",
            part.nonce, params.nonce
        )));
    }
    if part.sname.display() != params.sname.display() {
        return Err(Error::new(format!(
            "AS-REP 的服务主体 {} 与请求的 {} 不一致",
            part.sname.display(),
            params.sname.display()
        )));
    }
    if part.endtime <= sys::now_unix() {
        return Err(Error::new(format!(
            "AS-REP 的票已经过期（endtime={}）",
            part.endtime
        )));
    }
    let _ = salt_used;

    // 写缓存
    let ticket_der = rep.ticket.to_der();
    let starttime = part.starttime.unwrap_or(part.authtime);
    let cred = cred_from_as_rep(
        &rep.crealm,
        &rep.cname,
        &rep.ticket.realm,
        &rep.ticket,
        &part.key,
        part.authtime,
        starttime,
        part.endtime,
        part.renew_till.unwrap_or(0),
        part.flags,
        ticket_der,
    );
    let mut cc = CcCache::new(CcPrincipal::from_asn1(&rep.cname, &rep.crealm));
    cc.creds.push(cred);
    cc.write_file(&opts.cache)?;

    Ok(KinitResult { cache: cc, etype, rounds, ms: start.elapsed().as_millis(), via })
}

/// `s2kparams`（4 字节大端迭代次数）→ 迭代次数。缺席就是 4096。
fn s2k_params_to_iterations(p: Option<&[u8]>) -> Result<u32> {
    match p {
        None => Ok(crypto_krb::DEFAULT_ITERATIONS),
        Some(b) => {
            if b.len() != 4 {
                return Err(Error::new(format!(
                    "s2kparams 应为 4 字节，实际 {} 字节",
                    b.len()
                )));
            }
            let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            if v == 0 {
                return Err(Error::new("s2kparams 的迭代次数为 0（表示 2^32，拒绝执行）"));
            }
            Ok(v)
        }
    }
}

/// `user@REALM` 拆开；没写 realm 就用默认 realm。
pub fn split_principal(s: &str, default_realm: &str) -> Result<(PrincipalName, String)> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::new("principal 为空"));
    }
    let (name, realm) = match t.rsplit_once('@') {
        Some((n, r)) if !r.is_empty() => (n, r.to_string()),
        Some((n, _)) => (n, default_realm.to_string()),
        None => (t, default_realm.to_string()),
    };
    if realm.is_empty() {
        return Err(Error::new("没有 realm：请用 user@REALM 或在参数里给 realm"));
    }
    Ok((PrincipalName::parse(name)?, realm))
}

/// 命令行直接给了口令时用这个槽（取用一次即清空，不落盘、不进环境变量）。
static ONESHOT_PASSWORD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 预先塞一个口令（`kinit --password xxx` 或 `kinit principal password`）。
pub fn set_oneshot_password(pw: String) {
    if let Ok(mut g) = ONESHOT_PASSWORD.lock() {
        *g = Some(pw);
    }
}

fn take_oneshot_password() -> Option<String> {
    ONESHOT_PASSWORD.lock().ok().and_then(|mut g| g.take())
}

/// 读口令：`-` 或非终端时从 stdin 读一行，否则提示（不回显）。
fn read_password(principal: &str, realm: &str) -> Result<String> {
    if let Some(pw) = take_oneshot_password() {
        if pw.is_empty() {
            return Err(Error::new("口令为空"));
        }
        return Ok(pw);
    }
    let display = if principal.contains('@') {
        principal.to_string()
    } else {
        format!("{principal}@{realm}")
    };
    let quiet = std::env::var("VMLESS_KRB_PASSWORD_FROM_STDIN").is_ok() || !atty_stdin();
    if !quiet {
        eprint!("Password for {display}: ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Error::new(format!("读口令失败: {e}")))?;
    let pw = line.trim_end_matches(['\r', '\n']).to_string();
    if pw.is_empty() {
        return Err(Error::new("口令为空"));
    }
    Ok(pw)
}

/// 极简的「stdin 是不是终端」判断（不引依赖）。
fn atty_stdin() -> bool {
    // 环境变量明确要求「从 stdin 读、别提示」时就当非终端。默认按终端处理。
    std::env::var("VMLESS_KRB_PASSWORD_FROM_STDIN").is_err()
}

/// 用 TGT 去要一张服务票（TGS-REQ）。这是 KDC 的 TGS 路径的客户端侧。
pub fn get_service_ticket(
    cc: &CcCache,
    service: &str,
    kdc: &KdcAddr,
    timeout_ms: u64,
) -> Result<(Credential, i32, u128)> {
    let start = std::time::Instant::now();
    let tgt = cc
        .tgt()
        .ok_or_else(|| Error::new("凭据缓存里没有 TGT（先 kinit）"))?;
    let ticket = Ticket::from_bytes(&tgt.ticket).map_err(|e| e.ctx("解析缓存里的 TGT"))?;
    let (sname, srealm) = split_principal(service, &cc.principal.realm)?;
    let now = sys::now_unix();
    let pa = make_pa_tgs_req(
        &ticket,
        &tgt.key,
        &PrincipalName::new(cc.principal.name_type, cc.principal.components.clone()),
        &cc.principal.realm,
        now,
    )?;
    let params = TgsReqParams {
        realm: cc.principal.realm.clone(),
        cname: PrincipalName::new(cc.principal.name_type, cc.principal.components.clone()),
        sname,
        nonce: random_nonce(),
        etypes: vec![tgt.key.keytype],
        kdc_options: 0,
        till: now + DEFAULT_LIFETIME,
        rtime: None,
        pa_tgs_req: pa,
    };
    let req = params.to_req().to_der();
    let rt = exchange_with(kdc, &req, timeout_ms, false)?;
    if let Some(e) = parse_krb_error(&rt.bytes) {
        return Err(Error::new(format!("TGS 失败：{}", describe_error(&e))));
    }
    let rep = KdcRep::from_bytes(&rt.bytes).map_err(|e| e.ctx("解析 TGS-REP"))?;
    let plain = crypto_krb::decrypt(
        tgt.key.keytype,
        &tgt.key.keyvalue,
        USAGE_TGS_REP_ENC_PART,
        &rep.enc_part.cipher,
    )
    .map_err(|e| e.ctx("解密 TGS-REP enc-part"))?;
    let part = EncKdcRepPart::from_bytes(&plain, true).map_err(|e| e.ctx("解析 EncTGSRepPart"))?;
    let cred = cred_from_as_rep(
        &rep.crealm,
        &rep.cname,
        &rep.ticket.realm,
        &rep.ticket,
        &part.key,
        part.authtime,
        part.starttime.unwrap_or(part.authtime),
        part.endtime,
        part.renew_till.unwrap_or(0),
        part.flags,
        rep.ticket.to_der(),
    );
    let _ = srealm;
    Ok((cred, part.key.keytype, start.elapsed().as_millis()))
}

// ---------------------------- klist ----------------------------

/// `klist` 的输出。`-e` 显示 etype、`-f` 显示 flags。
pub fn format_klist(cc: &CcCache, path: &str, show_flags: bool, show_etype: bool, now: i64) -> String {
    let offset = sys::local_offset_secs();
    let mut out = String::new();
    out.push_str(&format!("Ticket cache: FILE:{path}\n"));
    out.push_str(&format!("Default principal: {}\n\n", cc.principal.display()));
    let visible: Vec<&Credential> = cc.creds.iter().filter(|c| !c.is_config_entry()).collect();
    if visible.is_empty() {
        out.push_str("No tickets in cache\n");
        return out;
    }
    out.push_str("Valid starting       Expires              Service principal\n");
    for c in visible {
        let expired = if c.endtime <= now { "  Expired" } else { "" };
        out.push_str(&format!(
            "{}  {}  {}{}\n",
            fmt_time(c.starttime, offset),
            fmt_time(c.endtime, offset),
            c.server_display(),
            expired
        ));
        let mut detail = Vec::new();
        if c.renew_till > 0 {
            detail.push(format!("renew until {}", fmt_time(c.renew_till, offset)));
        }
        if show_flags {
            detail.push(format!("Flags: {}", flags_string(c.flags)));
        }
        if show_etype {
            detail.push(format!("Etype (skey, tkt): {}", crypto_krb::etype_name(c.key.keytype)));
        }
        if !detail.is_empty() {
            out.push_str(&format!("\t{}\n", detail.join(", ")));
        }
    }
    out
}

/// `MM/dd/yyyy HH:MM:SS`（本地时区），和 JDK 的 klist 看起来是一路的。
fn fmt_time(t: i64, offset_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = sys::civil_from_unix(t + offset_secs);
    format!("{mo:02}/{d:02}/{y:04} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kdc_addr_parse() {
        assert_eq!(KdcAddr::parse("127.0.0.1").unwrap(), KdcAddr { host: "127.0.0.1".into(), port: 88 });
        assert_eq!(KdcAddr::parse("127.0.0.1:1088").unwrap().port, 1088);
        assert_eq!(KdcAddr::parse("kdc.example.com:99").unwrap().host, "kdc.example.com");
        assert_eq!(KdcAddr::parse("[::1]:88").unwrap().host, "::1");
        assert_eq!(KdcAddr::parse("::1").unwrap().host, "::1");
        assert!(KdcAddr::parse("").is_err());
        assert!(KdcAddr::parse("h:notaport").is_err());
    }

    #[test]
    fn principal_split() {
        let (p, r) = split_principal("alice@EXAMPLE.COM", "OTHER").unwrap();
        assert_eq!(p.display(), "alice");
        assert_eq!(r, "EXAMPLE.COM");
        let (p, r) = split_principal("alice", "EXAMPLE.COM").unwrap();
        assert_eq!(p.display(), "alice");
        assert_eq!(r, "EXAMPLE.COM");
        let (p, r) = split_principal("krbtgt/EXAMPLE.COM@EXAMPLE.COM", "X").unwrap();
        assert_eq!(p.display(), "krbtgt/EXAMPLE.COM");
        assert_eq!(r, "EXAMPLE.COM");
        assert!(split_principal("", "R").is_err());
        assert!(split_principal("a@", "").is_err());
    }

    #[test]
    fn s2kparams() {
        assert_eq!(s2k_params_to_iterations(None).unwrap(), 4096);
        assert_eq!(s2k_params_to_iterations(Some(&[0, 0, 0x20, 0])).unwrap(), 8192);
        assert!(s2k_params_to_iterations(Some(&[0, 0, 0])).is_err());
        assert!(s2k_params_to_iterations(Some(&[0, 0, 0, 0])).is_err());
    }

    #[test]
    fn klist_rendering() {
        let mut cc = CcCache::new(CcPrincipal::new(1, "EXAMPLE.COM", vec!["alice".into()]));
        cc.creds.push(Credential::from_tgt(
            CcPrincipal::new(1, "EXAMPLE.COM", vec!["alice".into()]),
            CcPrincipal::new(2, "EXAMPLE.COM", vec!["krbtgt".into(), "EXAMPLE.COM".into()]),
            EncryptionKey::new(18, vec![1; 32]),
            1_700_000_000,
            1_700_000_000,
            1_700_036_000,
            1_700_604_800,
            bit_set(bit_set(0, TKTFLAG_INITIAL), TKTFLAG_PRE_AUTHENT),
            vec![1, 2, 3],
        ));
        let s = format_klist(&cc, "/tmp/x", true, true, 1_700_000_100);
        assert!(s.contains("Ticket cache: FILE:/tmp/x"));
        assert!(s.contains("Default principal: alice@EXAMPLE.COM"));
        assert!(s.contains("krbtgt/EXAMPLE.COM@EXAMPLE.COM"));
        assert!(s.contains("Flags: IH"));
        assert!(s.contains("aes256-cts-hmac-sha1-96"));
        assert!(s.contains("renew until"));
        // 过期要标出来
        let s2 = format_klist(&cc, "/tmp/x", false, false, 1_800_000_000);
        assert!(s2.contains("Expired"));
        // 空缓存
        let empty = CcCache::new(CcPrincipal::new(1, "R", vec!["u".into()]));
        assert!(format_klist(&empty, "/tmp/y", false, false, 0).contains("No tickets in cache"));
    }
}
