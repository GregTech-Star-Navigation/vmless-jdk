//! 消息层：把 [`crate::asn1_krb`] 的类型拼成真正能发出去的报文，以及反过来。
//!
//! 这里放的是「Kerberos 协议怎么用这些结构」的知识：key usage 编号、预认证怎么塞、
//! 错误报文长什么样。ASN.1 的细节留在 `asn1_krb`，网络与口令留在 `client`/`kdc`。

use crate::asn1_krb::*;
use crate::crypto_krb;
use vmless_crypto::{Error, Result};

// ---------------------------- key usage（RFC 4120 §7.5.1） ----------------------------
/// AS-REQ 的 PA-ENC-TIMESTAMP，用客户端长期密钥加密。
pub const USAGE_AS_REQ_PA_ENC_TS: u32 = 1;
/// AS-REP / TGS-REP 里的 **Ticket**（EncTicketPart），用服务（或 krbtgt）密钥加密。
pub const USAGE_TICKET: u32 = 2;
/// AS-REP 的 enc-part（EncASRepPart），用客户端长期密钥加密。
pub const USAGE_AS_REP_ENC_PART: u32 = 3;
/// TGS-REQ 的 PA-TGS-REQ（AP-REQ 里的 authenticator），用 TGS 会话密钥加密。
pub const USAGE_TGS_REQ_AUTH: u32 = 7;
/// TGS-REP 的 enc-part（EncTGSRepPart），用 TGS 会话密钥加密。
pub const USAGE_TGS_REP_ENC_PART: u32 = 8;
/// AP-REQ 的 authenticator，用会话密钥加密。
pub const USAGE_AP_REQ_AUTH: u32 = 11;
/// AP-REP 的 enc-part（EncAPRepPart），用会话密钥加密。
pub const USAGE_AP_REP_ENC_PART: u32 = 12;

/// 默认票有效期（10 小时，与 MIT 的默认 `ticket_lifetime` 一致）。
pub const DEFAULT_LIFETIME: i64 = 10 * 3600;
/// 默认可续期上限（7 天）。
pub const DEFAULT_RENEW_LIFETIME: i64 = 7 * 24 * 3600;
/// 允许的时钟偏移（秒）。任务要求「-5 分钟内接受」，这里给 ±300 秒。
pub const CLOCK_SKEW: i64 = 300;

// ============================ 预认证 ============================

/// 造一个 `PA-ENC-TIMESTAMP`：`EncryptedData{ etype, cipher = E(key, usage=1, PA-ENC-TS-ENC) }`。
pub fn make_pa_enc_timestamp(
    etype: i32,
    key: &[u8],
    now: i64,
    usec: u32,
) -> Result<PaData> {
    let ts = PaEncTsEnc::new(now, Some(usec));
    let cipher = crypto_krb::encrypt(etype, key, USAGE_AS_REQ_PA_ENC_TS, &ts.to_der())?;
    let ed = EncryptedData::new(etype, None, cipher);
    Ok(PaData::new(PA_ENC_TIMESTAMP, ed.to_der()))
}

/// 解开 `PA-ENC-TIMESTAMP`，返回 (etype, 时间戳)。**HMAC 校验用常数时间**。
pub fn open_pa_enc_timestamp(key: &[u8], etype: i32, pa: &PaData) -> Result<PaEncTsEnc> {
    let ed = EncryptedData::from_bytes(&pa.padata_value)?;
    if ed.etype != etype {
        return Err(Error::new(format!(
            "PA-ENC-TIMESTAMP 的 etype 是 {}，与预期 {} 不一致",
            ed.etype, etype
        )));
    }
    let plain = crypto_krb::decrypt(etype, key, USAGE_AS_REQ_PA_ENC_TS, &ed.cipher)?;
    PaEncTsEnc::from_bytes(&plain)
}

/// `ETYPE-INFO2` 里的 salt / s2kparams 查询结果。
#[derive(Clone, Debug)]
pub struct EtypeHint {
    pub etype: i32,
    pub salt: Option<String>,
    pub s2kparams: Option<Vec<u8>>,
}

/// 从 `KDC_ERR_PREAUTH_REQUIRED` 的 e-data 里取 ETYPE-INFO2（没有就看老的 ETYPE-INFO）。
pub fn etype_hints(err: &KrbError) -> Result<Vec<EtypeHint>> {
    let md = err.method_data()?;
    let mut out = Vec::new();
    for pa in &md {
        if pa.padata_type == PA_ETYPE_INFO2 {
            for e in parse_etype_info2(&pa.padata_value)? {
                out.push(EtypeHint { etype: e.etype, salt: e.salt, s2kparams: e.s2kparams });
            }
        }
    }
    if out.is_empty() {
        for pa in &md {
            if pa.padata_type == PA_ETYPE_INFO {
                // ETYPE-INFO-ENTRY ::= SEQUENCE { etype [0] Int32, salt [1] OCTET STRING OPTIONAL }
                let t = vmless_crypto::asn1::der::parse_exact(&pa.padata_value, "ETYPE-INFO")?;
                let body = t.expect(vmless_crypto::asn1::der::TAG_SEQUENCE, "ETYPE-INFO")?;
                for item in vmless_crypto::asn1::der::parse_all(body)? {
                    let f = Fields::parse(
                        item.expect(vmless_crypto::asn1::der::TAG_SEQUENCE, "entry")?,
                        "ETYPE-INFO-ENTRY",
                    )?;
                    let etype = f.int_req(0, "ETYPE-INFO-ENTRY.etype")?;
                    let salt = f.octets(1)?.map(|s| String::from_utf8_lossy(&s).into_owned());
                    out.push(EtypeHint { etype, salt, s2kparams: None });
                }
            }
        }
    }
    Ok(out)
}

/// 造 `KDC_ERR_PREAUTH_REQUIRED` 里那份 `METHOD-DATA`：把每种 etype 的 salt 都告诉客户端。
pub fn make_preauth_required_edata(entries: &[EtypeHint]) -> Vec<u8> {
    let info: Vec<EtypeInfo2Entry> = entries
        .iter()
        .map(|h| EtypeInfo2Entry {
            etype: h.etype,
            salt: h.salt.clone(),
            s2kparams: h.s2kparams.clone(),
        })
        .collect();
    encode_method_data(&[PaData::new(PA_ETYPE_INFO2, encode_etype_info2(&info))])
}

// ============================ 报文构造 ============================

/// AS-REQ 的参数。
#[derive(Clone, Debug)]
pub struct AsReqParams {
    pub realm: String,
    pub cname: PrincipalName,
    pub sname: PrincipalName,
    pub nonce: u32,
    pub etypes: Vec<i32>,
    pub kdc_options: u32,
    pub till: i64,
    pub rtime: Option<i64>,
    pub padata: Vec<PaData>,
}

impl AsReqParams {
    /// 一个「第一次尝试」的默认参数（不带预认证，拿 KDC 的 salt）。
    pub fn new(realm: &str, cname: PrincipalName, etypes: Vec<i32>, now: i64) -> Self {
        AsReqParams {
            realm: realm.to_string(),
            cname,
            sname: PrincipalName::new(
                NT_SRV_INST,
                vec!["krbtgt".to_string(), realm.to_string()],
            ),
            nonce: crate::client::random_nonce(),
            etypes,
            kdc_options: bit_set(bit_set(0, KDCOPT_FORWARDABLE), KDCOPT_RENEWABLE),
            till: now + DEFAULT_LIFETIME,
            rtime: Some(now + DEFAULT_RENEW_LIFETIME),
            padata: Vec::new(),
        }
    }

    pub fn to_req(&self) -> KdcReq {
        KdcReq {
            pvno: PVNO,
            msg_type: MSG_AS_REQ,
            padata: self.padata.clone(),
            body: KdcReqBody {
                kdc_options: self.kdc_options,
                cname: Some(self.cname.clone()),
                realm: self.realm.clone(),
                sname: Some(self.sname.clone()),
                from: None,
                till: self.till,
                rtime: self.rtime,
                nonce: self.nonce,
                etypes: self.etypes.clone(),
                addresses: None,
                enc_authorization_data: None,
                additional_tickets: Vec::new(),
            },
        }
    }
}

/// TGS-REQ 的参数。
#[derive(Clone, Debug)]
pub struct TgsReqParams {
    pub realm: String,
    pub cname: PrincipalName,
    pub sname: PrincipalName,
    pub nonce: u32,
    pub etypes: Vec<i32>,
    pub kdc_options: u32,
    pub till: i64,
    pub rtime: Option<i64>,
    pub pa_tgs_req: PaData,
}

impl TgsReqParams {
    pub fn to_req(&self) -> KdcReq {
        KdcReq {
            pvno: PVNO,
            msg_type: MSG_TGS_REQ,
            padata: vec![self.pa_tgs_req.clone()],
            body: KdcReqBody {
                kdc_options: self.kdc_options,
                cname: Some(self.cname.clone()),
                realm: self.realm.clone(),
                sname: Some(self.sname.clone()),
                from: None,
                till: self.till,
                rtime: self.rtime,
                nonce: self.nonce,
                etypes: self.etypes.clone(),
                addresses: None,
                enc_authorization_data: None,
                additional_tickets: Vec::new(),
            },
        }
    }
}

/// 造 `PA-TGS-REQ`（AP-REQ）：`authenticator = E(session_key, usage=2, Authenticator)`。
pub fn make_pa_tgs_req(
    tgt: &Ticket,
    session_key: &EncryptionKey,
    cname: &PrincipalName,
    realm: &str,
    now: i64,
) -> Result<PaData> {
    let auth = Authenticator {
        authenticator_vno: PVNO,
        crealm: realm.to_string(),
        cname: cname.clone(),
        cusec: (now as u32) % 1_000_000,
        ctime: now,
        subkey: None,
        seq_number: None,
    };
    let cipher = crypto_krb::encrypt(
        session_key.keytype,
        &session_key.keyvalue,
        USAGE_TGS_REQ_AUTH,
        &auth.to_der(),
    )?;
    let ap = ApReq {
        pvno: PVNO,
        msg_type: MSG_AP_REQ,
        ap_options: 0,
        ticket: tgt.clone(),
        authenticator: EncryptedData::new(session_key.keytype, None, cipher),
    };
    Ok(PaData::new(PA_TGS_REQ, ap.to_der()))
}

/// 造一个 `KRB-ERROR`（KDC 出错时回给客户端）。
#[allow(clippy::too_many_arguments)]
pub fn make_krb_error(
    code: i32,
    text: Option<String>,
    e_data: Option<Vec<u8>>,
    realm: &str,
    cname: Option<&PrincipalName>,
    sname: &PrincipalName,
    ctime: Option<i64>,
    now: i64,
) -> KrbError {
    KrbError {
        pvno: PVNO,
        msg_type: MSG_KRB_ERROR,
        ctime,
        cusec: ctime.map(|t| (t as u32) % 1_000_000),
        stime: now,
        susec: (now as u32) % 1_000_000,
        error_code: code,
        crealm: cname.map(|_| realm.to_string()),
        cname: cname.cloned(),
        realm: realm.to_string(),
        sname: sname.clone(),
        e_text: text,
        e_data,
    }
}

/// 把字节流里的 `KRB-ERROR` 提出来（客户端收到错误报文时用）。
pub fn parse_krb_error(data: &[u8]) -> Option<KrbError> {
    if data.first() != Some(&app_tag(APP_KRB_ERROR)) {
        return None;
    }
    KrbError::from_bytes(data).ok()
}

/// 一个给用户看的错误串：`KDC_ERR_PREAUTH_FAILED: Pre-authentication information was invalid`。
pub fn describe_error(e: &KrbError) -> String {
    let mut s = format!(
        "{}: {}",
        errc::name(e.error_code),
        errc::text(e.error_code)
    );
    if let Some(t) = &e.e_text {
        s.push_str(&format!("（{t}）"));
    }
    s
}

/// 从客户端 keytab / 口令算出的「可尝试的 (etype, key)」列表。
#[derive(Clone, Debug)]
pub struct KeyCandidate {
    pub etype: i32,
    pub key: Vec<u8>,
}

/// 在 AS-REP / TGS-REP 里挑一个我们能解的 etype（KDC 若回了别的就明确报错）。
pub fn check_supported(etype: i32, what: &str) -> Result<()> {
    if !crypto_krb::is_supported(etype) {
        return Err(Error::new(format!(
            "{what} 用了不支持的加密类型 {etype}（{}）",
            crypto_krb::etype_name(etype)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pa_enc_timestamp_roundtrip() {
        let key = vec![0x11u8; 32];
        let now = 1_700_000_000;
        let pa = make_pa_enc_timestamp(18, &key, now, 123).unwrap();
        assert_eq!(pa.padata_type, PA_ENC_TIMESTAMP);
        let ts = open_pa_enc_timestamp(&key, 18, &pa).unwrap();
        assert_eq!(ts.patimestamp, now);
        assert_eq!(ts.pausec, Some(123));
        // 换一把钥匙必须失败
        let other = vec![0x22u8; 32];
        assert!(open_pa_enc_timestamp(&other, 18, &pa).is_err());
        // etype 报错要一致
        assert!(open_pa_enc_timestamp(&key, 17, &pa).is_err());
    }

    #[test]
    fn preauth_required_edata_roundtrip() {
        let hints = vec![
            EtypeHint { etype: 18, salt: Some("EXAMPLE.COMalice".into()), s2kparams: None },
            EtypeHint { etype: 17, salt: Some("EXAMPLE.COMalice".into()), s2kparams: Some(vec![0, 0, 0x10, 0]) },
        ];
        let edata = make_preauth_required_edata(&hints);
        let err = make_krb_error(
            errc::KDC_ERR_PREAUTH_REQUIRED,
            Some("NEED PREAUTH".into()),
            Some(edata),
            "EXAMPLE.COM",
            Some(&PrincipalName::parse("alice").unwrap()),
            &PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
            None,
            1_700_000_000,
        );
        let bytes = err.to_der();
        let back = parse_krb_error(&bytes).unwrap();
        assert_eq!(back.error_code, 25);
        let got = etype_hints(&back).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].etype, 18);
        assert_eq!(got[0].salt.as_deref(), Some("EXAMPLE.COMalice"));
        assert_eq!(got[1].s2kparams.as_deref(), Some(&[0u8, 0, 0x10, 0][..]));
        assert!(describe_error(&back).contains("KDC_ERR_PREAUTH_REQUIRED"));
    }

    #[test]
    fn as_req_shape() {
        let p = AsReqParams::new("EXAMPLE.COM", PrincipalName::parse("alice").unwrap(), vec![18, 17], 1_700_000_000);
        let req = p.to_req();
        let bytes = req.to_der();
        let back = KdcReq::from_bytes(&bytes).unwrap();
        assert_eq!(back.msg_type, MSG_AS_REQ);
        assert_eq!(back.pvno, PVNO);
        assert_eq!(back.body.realm, "EXAMPLE.COM");
        assert_eq!(back.body.nonce, p.nonce);
        assert_eq!(back.body.etypes, vec![18, 17]);
        assert_eq!(back.body.sname.as_ref().unwrap().display(), "krbtgt/EXAMPLE.COM");
        assert!(back.padata.is_empty());
    }

    #[test]
    fn pa_tgs_req_shape() {
        let tgt = Ticket {
            tkt_vno: TKT_VNO,
            realm: "EXAMPLE.COM".into(),
            sname: PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
            enc_part: EncryptedData::new(18, Some(1), vec![1; 64]),
        };
        let sk = EncryptionKey::new(18, vec![9u8; 32]);
        let pa = make_pa_tgs_req(&tgt, &sk, &PrincipalName::parse("alice").unwrap(), "EXAMPLE.COM", 1_700_000_000).unwrap();
        assert_eq!(pa.padata_type, PA_TGS_REQ);
        let ap = ApReq::from_bytes(&pa.padata_value).unwrap();
        assert_eq!(ap.msg_type, MSG_AP_REQ);
        // authenticator 能用会话密钥解开
        let plain = crypto_krb::decrypt(18, &sk.keyvalue, USAGE_TGS_REQ_AUTH, &ap.authenticator.cipher).unwrap();
        let auth = Authenticator::from_bytes(&plain).unwrap();
        assert_eq!(auth.cname.display(), "alice");
        assert_eq!(auth.ctime, 1_700_000_000);
    }
}
