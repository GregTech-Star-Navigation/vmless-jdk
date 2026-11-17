//! Kerberos v5 的 ASN.1 类型（RFC 4120 §5.2 的 KRB5 模块）。
//!
//! 全部基于 `vmless_crypto::asn1::der` 的原语自己搭，不引第三方 ASN.1 库。
//!
//! 两个容易踩的点，这里统一处理：
//!
//! 1. **KRB5 模块是 `EXPLICIT TAGS`**。所以 `pvno [1] INTEGER (5)` 编出来是
//!    `a1 03 02 01 05`（构造型上下文标签包着完整 INTEGER），而不是 `81 01 05`。
//!    `Ticket ::= [APPLICATION 1] SEQUENCE {...}` 同理是 `61 .. 30 ..`。
//! 2. **KerberosTime 是 `YYYYMMDDHHMMSSZ`**（不带小数、不带时区偏移）。
//!
//! 解析侧一律「先看 tag 再看内容」，缺字段、tag 不对、长度越界都返回 `Err`，
//! 任何入口都不 panic（见 `tests::fuzz_no_panic` 与 `bin/krbfuzz`）。

use vmless_crypto::asn1::der::{self, Tlv};
use vmless_crypto::{sys, Error, Result};

/// 协议版本号（RFC 4120：pvno 恒为 5）。
pub const PVNO: i32 = 5;
/// 票的版本号。
pub const TKT_VNO: i32 = 5;

// ---------------------------- 应用标签 ----------------------------
pub const APP_TICKET: u8 = 1;
pub const APP_AUTHENTICATOR: u8 = 2;
pub const APP_ENC_TICKET_PART: u8 = 3;
pub const APP_AS_REQ: u8 = 10;
pub const APP_AS_REP: u8 = 11;
pub const APP_TGS_REQ: u8 = 12;
pub const APP_TGS_REP: u8 = 13;
pub const APP_AP_REQ: u8 = 14;
pub const APP_AP_REP: u8 = 15;
pub const APP_ENC_AS_REP_PART: u8 = 25;
pub const APP_ENC_TGS_REP_PART: u8 = 26;
pub const APP_ENC_AP_REP_PART: u8 = 27;
pub const APP_KRB_PRIV: u8 = 21;
pub const APP_KRB_CRED: u8 = 22;
pub const APP_ENC_KRB_PRIV_PART: u8 = 28;
pub const APP_ENC_KRB_CRED_PART: u8 = 29;
pub const APP_KRB_ERROR: u8 = 30;

// ---------------------------- 消息类型 ----------------------------
pub const MSG_AS_REQ: i32 = 10;
pub const MSG_AS_REP: i32 = 11;
pub const MSG_TGS_REQ: i32 = 12;
pub const MSG_TGS_REP: i32 = 13;
pub const MSG_AP_REQ: i32 = 14;
pub const MSG_AP_REP: i32 = 15;
pub const MSG_KRB_ERROR: i32 = 30;

// ---------------------------- PA-DATA 类型 ----------------------------
pub const PA_TGS_REQ: i32 = 1;
pub const PA_ENC_TIMESTAMP: i32 = 2;
pub const PA_PW_SALT: i32 = 3;
pub const PA_ENC_UNIX_TIME: i32 = 5;
pub const PA_SANDIA_SECUREID: i32 = 6;
pub const PA_ETYPE_INFO: i32 = 11;
pub const PA_ETYPE_INFO2: i32 = 19;
pub const PA_USE_SPECIFIED_KVNO: i32 = 20;
pub const PA_SAM_REDIRECT: i32 = 22;
pub const PA_PAC_REQUEST: i32 = 128;

// ---------------------------- 名字类型 ----------------------------
pub const NT_UNKNOWN: i32 = 0;
pub const NT_PRINCIPAL: i32 = 1;
pub const NT_SRV_INST: i32 = 2;
pub const NT_SRV_HST: i32 = 3;

// ---------------------------- 错误码 ----------------------------
/// RFC 4120 §7.5.9 的错误码。
pub mod errc {
    pub const KDC_ERR_NONE: i32 = 0;
    pub const KDC_ERR_NAME_EXP: i32 = 1;
    pub const KDC_ERR_SERVICE_EXP: i32 = 2;
    pub const KDC_ERR_BAD_PVNO: i32 = 3;
    pub const KDC_ERR_C_OLD_MAST_KVNO: i32 = 4;
    pub const KDC_ERR_S_OLD_MAST_KVNO: i32 = 5;
    pub const KDC_ERR_C_PRINCIPAL_UNKNOWN: i32 = 6;
    pub const KDC_ERR_S_PRINCIPAL_UNKNOWN: i32 = 7;
    pub const KDC_ERR_PRINCIPAL_NOT_UNIQUE: i32 = 8;
    pub const KDC_ERR_NULL_KEY: i32 = 9;
    pub const KDC_ERR_CANNOT_POSTDATE: i32 = 10;
    pub const KDC_ERR_NEVER_VALID: i32 = 11;
    pub const KDC_ERR_POLICY: i32 = 12;
    pub const KDC_ERR_BADOPTION: i32 = 13;
    pub const KDC_ERR_ETYPE_NOSUPP: i32 = 14;
    pub const KDC_ERR_SUMTYPE_NOSUPP: i32 = 15;
    pub const KDC_ERR_PADATA_TYPE_NOSUPP: i32 = 16;
    pub const KDC_ERR_TRTYPE_NOSUPP: i32 = 17;
    pub const KDC_ERR_CLIENT_REVOKED: i32 = 18;
    pub const KDC_ERR_SERVICE_REVOKED: i32 = 19;
    pub const KDC_ERR_TGT_REVOKED: i32 = 20;
    pub const KDC_ERR_CLIENT_NOTYET: i32 = 21;
    pub const KDC_ERR_SERVICE_NOTYET: i32 = 22;
    pub const KDC_ERR_KEY_EXPIRED: i32 = 23;
    pub const KDC_ERR_PREAUTH_FAILED: i32 = 24;
    pub const KDC_ERR_PREAUTH_REQUIRED: i32 = 25;
    pub const KDC_ERR_SERVER_NOMATCH: i32 = 26;
    pub const KDC_ERR_MUST_USE_USER2USER: i32 = 27;
    pub const KDC_ERR_PATH_NOT_ACCEPTED: i32 = 28;
    pub const KDC_ERR_SVC_UNAVAILABLE: i32 = 29;
    pub const KRB_AP_ERR_BAD_INTEGRITY: i32 = 31;
    pub const KRB_AP_ERR_TKT_EXPIRED: i32 = 32;
    pub const KRB_AP_ERR_TKT_NYV: i32 = 33;
    pub const KRB_AP_ERR_REPEAT: i32 = 34;
    pub const KRB_AP_ERR_NOT_US: i32 = 35;
    pub const KRB_AP_ERR_BADMATCH: i32 = 36;
    pub const KRB_AP_ERR_SKEW: i32 = 37;
    pub const KRB_AP_ERR_BADADDR: i32 = 38;
    pub const KRB_AP_ERR_BADVERSION: i32 = 39;
    pub const KRB_AP_ERR_MSG_TYPE: i32 = 40;
    pub const KRB_AP_ERR_MODIFIED: i32 = 41;
    pub const KRB_AP_ERR_BADORDER: i32 = 42;
    pub const KRB_AP_ERR_BADKEYVER: i32 = 44;
    pub const KRB_AP_ERR_NOKEY: i32 = 45;
    pub const KRB_AP_ERR_MUT_FAIL: i32 = 46;
    pub const KRB_AP_ERR_BADDIRECTION: i32 = 47;
    pub const KRB_AP_ERR_METHOD: i32 = 48;
    pub const KRB_AP_ERR_BADSEQ: i32 = 49;
    pub const KRB_AP_ERR_INAPP_CKSUM: i32 = 50;
    pub const KRB_AP_PATH_NOT_ACCEPTED: i32 = 51;
    pub const KRB_ERR_RESPONSE_TOO_BIG: i32 = 52;
    pub const KRB_ERR_GENERIC: i32 = 60;
    pub const KRB_ERR_FIELD_TOOLONG: i32 = 61;
    pub const KDC_ERR_CLIENT_NOT_TRUSTED: i32 = 62;
    pub const KDC_ERR_KDC_NOT_TRUSTED: i32 = 63;
    pub const KDC_ERR_INVALID_SIG: i32 = 64;
    pub const KDC_ERR_DH_KEY_PARAMETERS_NOT_ACCEPTED: i32 = 65;
    pub const KDC_ERR_CERTIFICATE_MISMATCH: i32 = 66;
    pub const KRB_AP_ERR_NO_TGT: i32 = 67;
    pub const KDC_ERR_WRONG_REALM: i32 = 68;
    pub const KRB_AP_ERR_USER_TO_USER_REQUIRED: i32 = 69;
    pub const KDC_ERR_CANT_VERIFY_CERTIFICATE: i32 = 70;
    pub const KDC_ERR_INVALID_CERTIFICATE: i32 = 71;
    pub const KDC_ERR_REVOKED_CERTIFICATE: i32 = 72;
    pub const KDC_ERR_REVOCATION_STATUS_UNKNOWN: i32 = 73;
    pub const KDC_ERR_REVOCATION_STATUS_UNAVAILABLE: i32 = 74;
    pub const KDC_ERR_CLIENT_NAME_MISMATCH: i32 = 75;
    pub const KDC_ERR_KDC_NAME_MISMATCH: i32 = 76;

    /// 短名字（`KDC_ERR_PREAUTH_FAILED` 这种），用于日志与错误输出。
    pub fn name(code: i32) -> &'static str {
        match code {
            0 => "KDC_ERR_NONE",
            1 => "KDC_ERR_NAME_EXP",
            2 => "KDC_ERR_SERVICE_EXP",
            3 => "KDC_ERR_BAD_PVNO",
            4 => "KDC_ERR_C_OLD_MAST_KVNO",
            5 => "KDC_ERR_S_OLD_MAST_KVNO",
            6 => "KDC_ERR_C_PRINCIPAL_UNKNOWN",
            7 => "KDC_ERR_S_PRINCIPAL_UNKNOWN",
            8 => "KDC_ERR_PRINCIPAL_NOT_UNIQUE",
            9 => "KDC_ERR_NULL_KEY",
            10 => "KDC_ERR_CANNOT_POSTDATE",
            11 => "KDC_ERR_NEVER_VALID",
            12 => "KDC_ERR_POLICY",
            13 => "KDC_ERR_BADOPTION",
            14 => "KDC_ERR_ETYPE_NOSUPP",
            15 => "KDC_ERR_SUMTYPE_NOSUPP",
            16 => "KDC_ERR_PADATA_TYPE_NOSUPP",
            17 => "KDC_ERR_TRTYPE_NOSUPP",
            18 => "KDC_ERR_CLIENT_REVOKED",
            19 => "KDC_ERR_SERVICE_REVOKED",
            20 => "KDC_ERR_TGT_REVOKED",
            21 => "KDC_ERR_CLIENT_NOTYET",
            22 => "KDC_ERR_SERVICE_NOTYET",
            23 => "KDC_ERR_KEY_EXPIRED",
            24 => "KDC_ERR_PREAUTH_FAILED",
            25 => "KDC_ERR_PREAUTH_REQUIRED",
            26 => "KDC_ERR_SERVER_NOMATCH",
            27 => "KDC_ERR_MUST_USE_USER2USER",
            28 => "KDC_ERR_PATH_NOT_ACCEPTED",
            29 => "KDC_ERR_SVC_UNAVAILABLE",
            31 => "KRB_AP_ERR_BAD_INTEGRITY",
            32 => "KRB_AP_ERR_TKT_EXPIRED",
            33 => "KRB_AP_ERR_TKT_NYV",
            34 => "KRB_AP_ERR_REPEAT",
            35 => "KRB_AP_ERR_NOT_US",
            36 => "KRB_AP_ERR_BADMATCH",
            37 => "KRB_AP_ERR_SKEW",
            38 => "KRB_AP_ERR_BADADDR",
            39 => "KRB_AP_ERR_BADVERSION",
            40 => "KRB_AP_ERR_MSG_TYPE",
            41 => "KRB_AP_ERR_MODIFIED",
            42 => "KRB_AP_ERR_BADORDER",
            44 => "KRB_AP_ERR_BADKEYVER",
            45 => "KRB_AP_ERR_NOKEY",
            46 => "KRB_AP_ERR_MUT_FAIL",
            47 => "KRB_AP_ERR_BADDIRECTION",
            48 => "KRB_AP_ERR_METHOD",
            49 => "KRB_AP_ERR_BADSEQ",
            50 => "KRB_AP_ERR_INAPP_CKSUM",
            51 => "KRB_AP_PATH_NOT_ACCEPTED",
            52 => "KRB_ERR_RESPONSE_TOO_BIG",
            60 => "KRB_ERR_GENERIC",
            61 => "KRB_ERR_FIELD_TOOLONG",
            62 => "KDC_ERR_CLIENT_NOT_TRUSTED",
            63 => "KDC_ERR_KDC_NOT_TRUSTED",
            64 => "KDC_ERR_INVALID_SIG",
            65 => "KDC_ERR_DH_KEY_PARAMETERS_NOT_ACCEPTED",
            66 => "KDC_ERR_CERTIFICATE_MISMATCH",
            67 => "KRB_AP_ERR_NO_TGT",
            68 => "KDC_ERR_WRONG_REALM",
            69 => "KRB_AP_ERR_USER_TO_USER_REQUIRED",
            70 => "KDC_ERR_CANT_VERIFY_CERTIFICATE",
            71 => "KDC_ERR_INVALID_CERTIFICATE",
            72 => "KDC_ERR_REVOKED_CERTIFICATE",
            73 => "KDC_ERR_REVOCATION_STATUS_UNKNOWN",
            74 => "KDC_ERR_REVOCATION_STATUS_UNAVAILABLE",
            75 => "KDC_ERR_CLIENT_NAME_MISMATCH",
            76 => "KDC_ERR_KDC_NAME_MISMATCH",
            _ => "UNKNOWN_ERROR",
        }
    }

    /// 人话解释（kinit/klist 报错时用；MIT 与 JDK 的口径尽量一致）。
    pub fn text(code: i32) -> &'static str {
        match code {
            0 => "没有错误",
            6 => "Client not found in Kerberos database",
            7 => "Server not found in Kerberos database",
            12 => "KDC policy rejects request",
            13 => "KDC cannot accommodate requested option",
            14 => "KDC has no support for encryption type",
            16 => "KDC has no support for padata type",
            18 => "Clients credentials have been revoked",
            23 => "Key has expired",
            24 => "Pre-authentication information was invalid",
            25 => "Additional pre-authentication required",
            31 => "Integrity check on decrypted field failed",
            32 => "Ticket expired",
            33 => "Ticket not yet valid",
            34 => "Request is a replay",
            35 => "The ticket isn't for us",
            36 => "Ticket and authenticator don't match",
            37 => "Clock skew too great",
            38 => "Incorrect net address",
            39 => "Protocol version mismatch",
            40 => "Invalid msg type",
            41 => "Message stream modified",
            42 => "Message out of order",
            44 => "Specified version of key is not available",
            45 => "No service key for the ticket",
            52 => "Response too big for UDP, retry with TCP",
            60 => "Generic error",
            68 => "Wrong realm",
            _ => "未分类的 Kerberos 错误",
        }
    }
}

// ---------------------------- 位标志 ----------------------------

/// Kerberos 的 BIT STRING 里第 `n` 位（0 是最高位）。
#[inline]
pub fn bit_get(flags: u32, n: u32) -> bool {
    n < 32 && (flags & (1u32 << (31 - n))) != 0
}

/// 置第 `n` 位。
#[inline]
pub fn bit_set(flags: u32, n: u32) -> u32 {
    if n < 32 {
        flags | (1u32 << (31 - n))
    } else {
        flags
    }
}

// KDC-Options（RFC 4120 §5.4.1）的位号
pub const KDCOPT_FORWARDABLE: u32 = 1;
pub const KDCOPT_FORWARDED: u32 = 2;
pub const KDCOPT_PROXIABLE: u32 = 3;
pub const KDCOPT_PROXY: u32 = 4;
pub const KDCOPT_ALLOW_POSTDATE: u32 = 5;
pub const KDCOPT_POSTDATED: u32 = 6;
pub const KDCOPT_RENEWABLE: u32 = 8;
pub const KDCOPT_RENEWABLE_OK: u32 = 27;
pub const KDCOPT_ENC_TKT_IN_SKEY: u32 = 28;
pub const KDCOPT_RENEW: u32 = 30;
pub const KDCOPT_VALIDATE: u32 = 31;

// TicketFlags（RFC 4120 §5.3）的位号
pub const TKTFLAG_FORWARDABLE: u32 = 1;
pub const TKTFLAG_FORWARDED: u32 = 2;
pub const TKTFLAG_PROXIABLE: u32 = 3;
pub const TKTFLAG_PROXY: u32 = 4;
pub const TKTFLAG_MAY_POSTDATE: u32 = 5;
pub const TKTFLAG_POSTDATED: u32 = 6;
pub const TKTFLAG_INVALID: u32 = 7;
pub const TKTFLAG_RENEWABLE: u32 = 8;
pub const TKTFLAG_INITIAL: u32 = 9;
pub const TKTFLAG_PRE_AUTHENT: u32 = 10;
pub const TKTFLAG_HW_AUTHENT: u32 = 11;
pub const TKTFLAG_TRANSITED_POLICY_CHECKED: u32 = 12;
pub const TKTFLAG_OK_AS_DELEGATE: u32 = 13;
pub const TKTFLAG_ANONYMOUS: u32 = 30;
pub const TKTFLAG_ENC_PA_REP: u32 = 31;

/// `klist -f` 那种字母缩写（MIT 口径：F f P p D d R I i H h a ...）。
pub fn flags_string(flags: u32) -> String {
    let mut s = String::new();
    let mut push = |cond: bool, c: char| {
        if cond {
            s.push(c)
        }
    };
    push(bit_get(flags, TKTFLAG_FORWARDABLE), 'F');
    push(bit_get(flags, TKTFLAG_FORWARDED), 'f');
    push(bit_get(flags, TKTFLAG_PROXIABLE), 'P');
    push(bit_get(flags, TKTFLAG_PROXY), 'p');
    push(bit_get(flags, TKTFLAG_MAY_POSTDATE), 'D');
    push(bit_get(flags, TKTFLAG_POSTDATED), 'd');
    push(bit_get(flags, TKTFLAG_RENEWABLE), 'R');
    push(bit_get(flags, TKTFLAG_INITIAL), 'I');
    push(bit_get(flags, TKTFLAG_INVALID), 'i');
    push(bit_get(flags, TKTFLAG_PRE_AUTHENT), 'H');
    push(bit_get(flags, TKTFLAG_HW_AUTHENT), 'h');
    push(bit_get(flags, TKTFLAG_ANONYMOUS), 'a');
    if s.is_empty() {
        s.push('-');
    }
    s
}

// ============================ 写入原语 ============================

/// 最小两补码整数编码。
pub fn enc_int(v: i32) -> Vec<u8> {
    let mut b = v.to_be_bytes().to_vec();
    while b.len() > 1 {
        let (f, s) = (b[0], b[1]);
        if (f == 0x00 && s & 0x80 == 0) || (f == 0xff && s & 0x80 != 0) {
            b.remove(0);
        } else {
            break;
        }
    }
    der::tlv(der::TAG_INTEGER, &b)
}

/// `[n] EXPLICIT INTEGER`
pub fn ctx_int(n: u8, v: i32) -> Vec<u8> {
    der::tlv(0xa0 | n, &enc_int(v))
}

/// `[n] EXPLICIT INTEGER`（无符号 u32）
pub fn ctx_uint(n: u8, v: u32) -> Vec<u8> {
    let mut b = v.to_be_bytes().to_vec();
    while b.len() > 1 && b[0] == 0 {
        b.remove(0);
    }
    if b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    der::tlv(0xa0 | n, &der::tlv(der::TAG_INTEGER, &b))
}

/// `[n] EXPLICIT OCTET STRING`
pub fn ctx_octets(n: u8, v: &[u8]) -> Vec<u8> {
    der::tlv(0xa0 | n, &der::octet_string(v))
}

/// `[n] EXPLICIT KerberosString`（GeneralString）
pub fn ctx_string(n: u8, v: &str) -> Vec<u8> {
    der::tlv(0xa0 | n, &der::tlv(0x1b, v.as_bytes()))
}

/// `[n] EXPLICIT KerberosTime`（GeneralizedTime，`YYYYMMDDHHMMSSZ`）
pub fn ctx_time(n: u8, t: i64) -> Vec<u8> {
    der::tlv(0xa0 | n, &der::generalized_time(t))
}

/// `[n] EXPLICIT BIT STRING`（32 位，无未用位）
pub fn ctx_bits32(n: u8, v: u32) -> Vec<u8> {
    der::tlv(0xa0 | n, &der::bit_string(&v.to_be_bytes(), 0))
}

/// `[n] EXPLICIT SEQUENCE { <body> }`
pub fn ctx_seq(n: u8, body: &[u8]) -> Vec<u8> {
    der::tlv(0xa0 | n, &der::seq(&[body.to_vec()]))
}

/// `[n] EXPLICIT <已编码好的 TLV>`
pub fn ctx_wrap(n: u8, tlv: &[u8]) -> Vec<u8> {
    der::tlv(0xa0 | n, tlv)
}

/// `[APPLICATION n] SEQUENCE { <body> }`
pub fn app_seq(n: u8, body: &[u8]) -> Vec<u8> {
    der::tlv(0x60 | n, &der::seq(&[body.to_vec()]))
}

/// `[APPLICATION n]` 的 tag 字节（构造型：`0x60 | n`）。
pub fn app_tag(n: u8) -> u8 {
    0x60 | (n & 0x1f)
}

// ============================ 读取原语 ============================

/// 把一段构造型内容按 `[n] EXPLICIT ...` 拆成字段表，按标签号查。
///
/// 只接受**上下文构造型**标签（`0xa0|n`），出现别的直接报错 —— Kerberos 报文里
/// 不该有别的东西，宽容解析只会把畸形报文放过去。
pub struct Fields<'a> {
    items: Vec<(u8, Tlv<'a>)>,
}

impl<'a> Fields<'a> {
    /// `content` 是 SEQUENCE 的**内容**（不含 SEQUENCE 自己的 tag/len）。
    pub fn parse(content: &'a [u8], what: &str) -> Result<Fields<'a>> {
        let mut items = Vec::new();
        let mut rest = content;
        while !rest.is_empty() {
            let (tlv, r) = der::parse_tlv(rest)?;
            if tlv.class() != 0x80 {
                return Err(Error::new(format!(
                    "{what}: 期望上下文标签，遇到 0x{:02x}",
                    tlv.tag
                )));
            }
            if !tlv.is_constructed() {
                return Err(Error::new(format!(
                    "{what}: 标签 [{}] 应为构造型（KRB5 模块是 EXPLICIT TAGS）",
                    tlv.number()
                )));
            }
            // 里面必须恰好一个 TLV
            let inner = der::parse_exact(tlv.content, what)?;
            items.push((tlv.number(), inner));
            rest = r;
        }
        Ok(Fields { items })
    }

    pub fn has(&self, n: u8) -> bool {
        self.items.iter().any(|(k, _)| *k == n)
    }

    fn find(&self, n: u8) -> Option<Tlv<'a>> {
        self.items.iter().find(|(k, _)| *k == n).map(|(_, t)| *t)
    }

    pub fn req(&self, n: u8, what: &str) -> Result<Tlv<'a>> {
        self.find(n)
            .ok_or_else(|| Error::new(format!("{what}: 缺少必需字段 [{n}]")))
    }

    pub fn int(&self, n: u8) -> Result<Option<i32>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => Ok(Some(parse_i32(t)?)),
        }
    }

    pub fn int_req(&self, n: u8, what: &str) -> Result<i32> {
        let t = self.req(n, what)?;
        parse_i32(t)
    }

    pub fn uint(&self, n: u8) -> Result<Option<u32>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => Ok(Some(parse_u32(t)?)),
        }
    }

    pub fn octets(&self, n: u8) -> Result<Option<Vec<u8>>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => {
                let c = t.expect(der::TAG_OCTET_STRING, "OCTET STRING")?;
                Ok(Some(c.to_vec()))
            }
        }
    }

    pub fn octets_req(&self, n: u8, what: &str) -> Result<Vec<u8>> {
        self.octets(n)?
            .ok_or_else(|| Error::new(format!("{what}: 缺少必需字段 [{n}]")))
    }

    pub fn string(&self, n: u8) -> Result<Option<String>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => Ok(Some(parse_kstr(t)?)),
        }
    }

    pub fn string_req(&self, n: u8, what: &str) -> Result<String> {
        self.string(n)?
            .ok_or_else(|| Error::new(format!("{what}: 缺少必需字段 [{n}]")))
    }

    pub fn time(&self, n: u8) -> Result<Option<i64>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => Ok(Some(parse_time(t)?)),
        }
    }

    pub fn time_req(&self, n: u8, what: &str) -> Result<i64> {
        self.time(n)?
            .ok_or_else(|| Error::new(format!("{what}: 缺少必需字段 [{n}]")))
    }

    pub fn bits32(&self, n: u8) -> Result<Option<u32>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => Ok(Some(parse_bits32(t)?)),
        }
    }

    pub fn bits32_req(&self, n: u8, what: &str) -> Result<u32> {
        self.bits32(n)?
            .ok_or_else(|| Error::new(format!("{what}: 缺少必需字段 [{n}]")))
    }

    /// 取一个 `SEQUENCE OF X` 字段，返回 X 的每个 TLV。
    pub fn seq_of(&self, n: u8, what: &str) -> Result<Option<Vec<Tlv<'a>>>> {
        match self.find(n) {
            None => Ok(None),
            Some(t) => {
                let body = t.expect(der::TAG_SEQUENCE, what)?;
                Ok(Some(der::parse_all(body)?))
            }
        }
    }

    /// 取一个字段的原始 TLV 字节（用于不解释、只透传的字段）。
    pub fn raw(&self, n: u8) -> Option<&'a [u8]> {
        self.find(n).map(|t| t.content)
    }
}

/// 解析一个 TLV，要求它是 `[APPLICATION n] SEQUENCE`，返回 SEQUENCE 的内容。
pub fn parse_app<'a>(data: &'a [u8], n: u8, what: &str) -> Result<&'a [u8]> {
    let tlv = der::parse_exact(data, what)?;
    let want = 0x60 | n;
    if tlv.tag != want {
        return Err(Error::new(format!(
            "{what}: 应用标签应为 0x{want:02x}（[APPLICATION {n}]），实际 0x{:02x}",
            tlv.tag
        )));
    }
    let seq = der::parse_exact(tlv.content, what)?;
    if seq.tag != der::TAG_SEQUENCE {
        return Err(Error::new(format!("{what}: [APPLICATION {n}] 里应为 SEQUENCE")));
    }
    Ok(seq.content)
}

fn parse_i32(t: Tlv<'_>) -> Result<i32> {
    let c = t.expect(der::TAG_INTEGER, "INTEGER")?;
    if c.is_empty() {
        return Err(Error::new("INTEGER 内容为空"));
    }
    if c.len() > 4 {
        // 允许 5 字节的正数（前导 0x00）
        if c.len() == 5 && c[0] == 0 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&c[1..]);
            return Ok(u32::from_be_bytes(b) as i32);
        }
        return Err(Error::new(format!("INTEGER 太长（{} 字节）", c.len())));
    }
    let mut v: i64 = if c[0] & 0x80 != 0 { -1 } else { 0 };
    for &b in c {
        v = (v << 8) | b as i64;
    }
    if v < i32::MIN as i64 || v > i32::MAX as i64 {
        return Err(Error::new("INTEGER 超出 i32 范围"));
    }
    Ok(v as i32)
}

fn parse_u32(t: Tlv<'_>) -> Result<u32> {
    let c = t.expect(der::TAG_INTEGER, "INTEGER")?;
    if c.is_empty() {
        return Err(Error::new("INTEGER 内容为空"));
    }
    if c[0] & 0x80 != 0 {
        return Err(Error::new("无符号字段出现了负数"));
    }
    if c.len() > 5 || (c.len() == 5 && c[0] != 0) {
        return Err(Error::new(format!("无符号 INTEGER 太长（{} 字节）", c.len())));
    }
    let mut v: u64 = 0;
    for &b in c {
        v = (v << 8) | b as u64;
    }
    if v > u32::MAX as u64 {
        return Err(Error::new("无符号 INTEGER 超出 u32"));
    }
    Ok(v as u32)
}

fn parse_bits32(t: Tlv<'_>) -> Result<u32> {
    let c = t.expect(der::TAG_BIT_STRING, "BIT STRING")?;
    if c.is_empty() {
        return Err(Error::new("BIT STRING 内容为空"));
    }
    let unused = c[0];
    if unused > 7 {
        return Err(Error::new(format!("BIT STRING 未用位数不合法: {unused}")));
    }
    let body = &c[1..];
    if body.len() > 4 {
        // 宽容：只取前 4 字节（有些实现对 flags 补零）
        let mut b = [0u8; 4];
        b.copy_from_slice(&body[..4]);
        return Ok(u32::from_be_bytes(b));
    }
    let mut b = [0u8; 4];
    b[4 - body.len()..].copy_from_slice(body);
    Ok(u32::from_be_bytes(b))
}

/// KerberosString：接受 GeneralString / IA5String / UTF8String / PrintableString。
pub fn parse_kstr(t: Tlv<'_>) -> Result<String> {
    let c = match t.tag {
        0x1b | 0x16 | 0x13 | 0x0c => t.content,
        other => {
            return Err(Error::new(format!(
                "KerberosString 的 tag 应为 GeneralString(0x1b)/IA5(0x16)/UTF8(0x0c)，实际 0x{other:02x}"
            )))
        }
    };
    String::from_utf8(c.to_vec()).map_err(|_| Error::new("KerberosString 不是合法的 UTF-8"))
}

/// KerberosTime：必须是 `YYYYMMDDHHMMSSZ`；也容忍 UTCTime（老实现）。
pub fn parse_time(t: Tlv<'_>) -> Result<i64> {
    match t.tag {
        der::TAG_GENERALIZED_TIME | der::TAG_UTC_TIME => {}
        other => {
            return Err(Error::new(format!(
                "KerberosTime 的 tag 应为 GeneralizedTime(0x18)，实际 0x{other:02x}"
            )))
        }
    }
    let s = std::str::from_utf8(t.content)
        .map_err(|_| Error::new("KerberosTime 不是 ASCII"))?
        .trim();
    if !s.ends_with('Z') {
        return Err(Error::new(format!("KerberosTime 必须以 Z 结尾: {s}")));
    }
    let digits = &s[..s.len() - 1];
    if digits.len() != 14 && digits.len() != 12 {
        return Err(Error::new(format!("KerberosTime 位数不对: {s}")));
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::new(format!("KerberosTime 含非数字: {s}")));
    }
    sys::parse_stamp(s).map_err(|e| e.ctx("KerberosTime"))
}

// ============================ 具体类型 ============================

/// `PrincipalName ::= SEQUENCE { name-type [0] Int32, name-string [1] SEQUENCE OF KerberosString }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalName {
    pub name_type: i32,
    pub components: Vec<String>,
}

impl PrincipalName {
    pub fn new(name_type: i32, components: Vec<String>) -> Self {
        PrincipalName { name_type, components }
    }

    /// 从 `user`、`krbtgt/REALM`、`host/x.example.com` 这类写法建（不含 @REALM）。
    pub fn from_components(components: Vec<String>) -> Result<Self> {
        if components.is_empty() || components.iter().any(|c| c.is_empty()) {
            return Err(Error::new("principal 组件不能为空"));
        }
        let nt = if components.len() == 1 { NT_PRINCIPAL } else { NT_SRV_INST };
        Ok(PrincipalName { name_type: nt, components })
    }

    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(Error::new("principal 为空"));
        }
        if s.contains('@') {
            return Err(Error::new(format!(
                "principal {s} 里不该带 realm（realm 单独传）"
            )));
        }
        let comps: Vec<String> = s.split('/').map(|c| c.to_string()).collect();
        PrincipalName::from_components(comps)
    }

    pub fn body(&self) -> Vec<u8> {
        let mut comps = Vec::new();
        for c in &self.components {
            comps.push(der::tlv(0x1b, c.as_bytes()));
        }
        let mut b = ctx_int(0, self.name_type);
        b.extend_from_slice(&ctx_seq(1, &comps.concat()));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        der::seq(&[self.body()])
    }

    pub fn from_tlv(t: Tlv<'_>) -> Result<Self> {
        let content = t.expect(der::TAG_SEQUENCE, "PrincipalName")?;
        PrincipalName::from_content(content)
    }

    pub fn from_content(content: &[u8]) -> Result<Self> {
        let f = Fields::parse(content, "PrincipalName")?;
        let name_type = f.int(0)?.unwrap_or(NT_PRINCIPAL);
        let mut components = Vec::new();
        if let Some(items) = f.seq_of(1, "name-string")? {
            for it in items {
                components.push(parse_kstr(it)?);
            }
        }
        if components.is_empty() {
            return Err(Error::new("PrincipalName: name-string 为空"));
        }
        Ok(PrincipalName { name_type, components })
    }

    /// 不带 realm 的写法（`krbtgt/REALM`）。
    pub fn display(&self) -> String {
        self.components.join("/")
    }

    /// `krbtgt/REALM@REALM` 这种完整写法。
    pub fn full(&self, realm: &str) -> String {
        format!("{}@{}", self.display(), realm)
    }
}

impl std::fmt::Display for PrincipalName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// `EncryptedData ::= SEQUENCE { etype [0] Int32, kvno [1] UInt32 OPTIONAL, cipher [2] OCTET STRING }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedData {
    pub etype: i32,
    pub kvno: Option<u32>,
    pub cipher: Vec<u8>,
}

impl EncryptedData {
    pub fn new(etype: i32, kvno: Option<u32>, cipher: Vec<u8>) -> Self {
        EncryptedData { etype, kvno, cipher }
    }

    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.etype);
        if let Some(k) = self.kvno {
            b.extend_from_slice(&ctx_uint(1, k));
        }
        b.extend_from_slice(&ctx_octets(2, &self.cipher));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        der::seq(&[self.body()])
    }

    pub fn from_tlv(t: Tlv<'_>) -> Result<Self> {
        let f = Fields::parse(t.expect(der::TAG_SEQUENCE, "EncryptedData")?, "EncryptedData")?;
        Ok(EncryptedData {
            etype: f.int_req(0, "EncryptedData.etype")?,
            kvno: f.uint(1)?,
            cipher: f.octets_req(2, "EncryptedData.cipher")?,
        })
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        EncryptedData::from_tlv(der::parse_exact(data, "EncryptedData")?)
    }
}

/// `EncryptionKey ::= SEQUENCE { keytype [0] Int32, keyvalue [1] OCTET STRING }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionKey {
    pub keytype: i32,
    pub keyvalue: Vec<u8>,
}

impl EncryptionKey {
    pub fn new(keytype: i32, keyvalue: Vec<u8>) -> Self {
        EncryptionKey { keytype, keyvalue }
    }

    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.keytype);
        b.extend_from_slice(&ctx_octets(1, &self.keyvalue));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        der::seq(&[self.body()])
    }

    pub fn from_tlv(t: Tlv<'_>) -> Result<Self> {
        let f = Fields::parse(t.expect(der::TAG_SEQUENCE, "EncryptionKey")?, "EncryptionKey")?;
        let keytype = f.int_req(0, "EncryptionKey.keytype")?;
        let keyvalue = f.octets_req(1, "EncryptionKey.keyvalue")?;
        // 长度必须和 etype 对得上，否则后面解密时才发现就晚了
        if let Some(want) = crate::crypto_krb::key_len(keytype) {
            if keyvalue.len() != want {
                return Err(Error::new(format!(
                    "EncryptionKey: etype {keytype} 的密钥应为 {want} 字节，实际 {}",
                    keyvalue.len()
                )));
            }
        }
        Ok(EncryptionKey { keytype, keyvalue })
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        EncryptionKey::from_tlv(der::parse_exact(data, "EncryptionKey")?)
    }
}

/// `PA-DATA ::= SEQUENCE { padata-type [1] Int32, padata-value [2] OCTET STRING }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaData {
    pub padata_type: i32,
    pub padata_value: Vec<u8>,
}

impl PaData {
    pub fn new(padata_type: i32, padata_value: Vec<u8>) -> Self {
        PaData { padata_type, padata_value }
    }

    pub fn to_der(&self) -> Vec<u8> {
        let mut b = ctx_int(1, self.padata_type);
        b.extend_from_slice(&ctx_octets(2, &self.padata_value));
        der::seq(&[b])
    }

    pub fn from_tlv(t: Tlv<'_>) -> Result<Self> {
        let f = Fields::parse(t.expect(der::TAG_SEQUENCE, "PA-DATA")?, "PA-DATA")?;
        Ok(PaData {
            padata_type: f.int_req(1, "PA-DATA.padata-type")?,
            padata_value: f.octets_req(2, "PA-DATA.padata-value")?,
        })
    }
}

/// `METHOD-DATA ::= SEQUENCE OF PA-DATA`（KRB-ERROR 的 e-data）
pub fn encode_method_data(items: &[PaData]) -> Vec<u8> {
    let mut body = Vec::new();
    for it in items {
        body.extend_from_slice(&it.to_der());
    }
    der::seq(&[body])
}

pub fn parse_method_data(data: &[u8]) -> Result<Vec<PaData>> {
    let t = der::parse_exact(data, "METHOD-DATA")?;
    let body = t.expect(der::TAG_SEQUENCE, "METHOD-DATA")?;
    let mut out = Vec::new();
    for item in der::parse_all(body)? {
        out.push(PaData::from_tlv(item)?);
    }
    Ok(out)
}

/// `ETYPE-INFO2-ENTRY ::= SEQUENCE { etype [0] Int32, salt [1] KerberosString OPTIONAL, s2kparams [2] OCTET STRING OPTIONAL }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EtypeInfo2Entry {
    pub etype: i32,
    pub salt: Option<String>,
    pub s2kparams: Option<Vec<u8>>,
}

impl EtypeInfo2Entry {
    pub fn to_der(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.etype);
        if let Some(s) = &self.salt {
            b.extend_from_slice(&ctx_string(1, s));
        }
        if let Some(p) = &self.s2kparams {
            b.extend_from_slice(&ctx_octets(2, p));
        }
        der::seq(&[b])
    }

    pub fn from_tlv(t: Tlv<'_>) -> Result<Self> {
        let f = Fields::parse(t.expect(der::TAG_SEQUENCE, "ETYPE-INFO2-ENTRY")?, "ETYPE-INFO2-ENTRY")?;
        Ok(EtypeInfo2Entry {
            etype: f.int_req(0, "ETYPE-INFO2-ENTRY.etype")?,
            salt: f.string(1)?,
            s2kparams: f.octets(2)?,
        })
    }
}

/// `ETYPE-INFO2 ::= SEQUENCE OF ETYPE-INFO2-ENTRY`
pub fn encode_etype_info2(entries: &[EtypeInfo2Entry]) -> Vec<u8> {
    let mut body = Vec::new();
    for e in entries {
        body.extend_from_slice(&e.to_der());
    }
    der::seq(&[body])
}

pub fn parse_etype_info2(data: &[u8]) -> Result<Vec<EtypeInfo2Entry>> {
    let t = der::parse_exact(data, "ETYPE-INFO2")?;
    let body = t.expect(der::TAG_SEQUENCE, "ETYPE-INFO2")?;
    let mut out = Vec::new();
    for item in der::parse_all(body)? {
        out.push(EtypeInfo2Entry::from_tlv(item)?);
    }
    Ok(out)
}

/// `Ticket ::= [APPLICATION 1] SEQUENCE { tkt-vno [0] INTEGER, realm [1] Realm,
///                                       sname [2] PrincipalName, enc-part [3] EncryptedData }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub tkt_vno: i32,
    pub realm: String,
    pub sname: PrincipalName,
    pub enc_part: EncryptedData,
}

impl Ticket {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.tkt_vno);
        b.extend_from_slice(&ctx_string(1, &self.realm));
        b.extend_from_slice(&ctx_wrap(2, &self.sname.to_der()));
        b.extend_from_slice(&ctx_wrap(3, &self.enc_part.to_der()));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_TICKET, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_TICKET, "Ticket")?;
        let f = Fields::parse(content, "Ticket")?;
        Ok(Ticket {
            tkt_vno: f.int_req(0, "Ticket.tkt-vno")?,
            realm: f.string_req(1, "Ticket.realm")?,
            sname: PrincipalName::from_tlv(f.req(2, "Ticket.sname")?)?,
            enc_part: EncryptedData::from_tlv(f.req(3, "Ticket.enc-part")?)?,
        })
    }
}

/// `EncTicketPart ::= [APPLICATION 3] SEQUENCE { ... }`（票的加密部分）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncTicketPart {
    pub flags: u32,
    pub key: EncryptionKey,
    pub crealm: String,
    pub cname: PrincipalName,
    /// `TransitedEncoding ::= SEQUENCE { tr-type [0] Int32, contents [1] OCTET STRING }`
    pub transited: (i32, Vec<u8>),
    pub authtime: i64,
    pub starttime: Option<i64>,
    pub endtime: i64,
    pub renew_till: Option<i64>,
    pub caddr: Option<Vec<u8>>,
    pub authorization_data: Option<Vec<u8>>,
}

impl EncTicketPart {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_bits32(0, self.flags);
        b.extend_from_slice(&ctx_wrap(1, &self.key.to_der()));
        b.extend_from_slice(&ctx_string(2, &self.crealm));
        b.extend_from_slice(&ctx_wrap(3, &self.cname.to_der()));
        let mut tr = ctx_int(0, self.transited.0);
        tr.extend_from_slice(&ctx_octets(1, &self.transited.1));
        b.extend_from_slice(&ctx_seq(4, &tr));
        b.extend_from_slice(&ctx_time(5, self.authtime));
        if let Some(t) = self.starttime {
            b.extend_from_slice(&ctx_time(6, t));
        }
        b.extend_from_slice(&ctx_time(7, self.endtime));
        if let Some(t) = self.renew_till {
            b.extend_from_slice(&ctx_time(8, t));
        }
        if let Some(a) = &self.caddr {
            b.extend_from_slice(&ctx_seq(9, a));
        }
        if let Some(a) = &self.authorization_data {
            b.extend_from_slice(&ctx_seq(10, a));
        }
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_ENC_TICKET_PART, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_ENC_TICKET_PART, "EncTicketPart")?;
        let f = Fields::parse(content, "EncTicketPart")?;
        let transited_tlv = f.req(4, "EncTicketPart.transited")?;
        let tf = Fields::parse(
            transited_tlv.expect(der::TAG_SEQUENCE, "TransitedEncoding")?,
            "TransitedEncoding",
        )?;
        Ok(EncTicketPart {
            flags: f.bits32_req(0, "EncTicketPart.flags")?,
            key: EncryptionKey::from_tlv(f.req(1, "EncTicketPart.key")?)?,
            crealm: f.string_req(2, "EncTicketPart.crealm")?,
            cname: PrincipalName::from_tlv(f.req(3, "EncTicketPart.cname")?)?,
            transited: (tf.int(0)?.unwrap_or(0), tf.octets(1)?.unwrap_or_default()),
            authtime: f.time_req(5, "EncTicketPart.authtime")?,
            starttime: f.time(6)?,
            endtime: f.time_req(7, "EncTicketPart.endtime")?,
            renew_till: f.time(8)?,
            caddr: f.raw(9).map(|c| c.to_vec()),
            authorization_data: f.raw(10).map(|c| c.to_vec()),
        })
    }
}

/// `KDC-REQ-BODY`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KdcReqBody {
    pub kdc_options: u32,
    pub cname: Option<PrincipalName>,
    pub realm: String,
    pub sname: Option<PrincipalName>,
    pub from: Option<i64>,
    pub till: i64,
    pub rtime: Option<i64>,
    pub nonce: u32,
    pub etypes: Vec<i32>,
    /// `addresses [9]`：只透传，不解释（本实现不依赖地址限制）
    pub addresses: Option<Vec<u8>>,
    pub enc_authorization_data: Option<EncryptedData>,
    pub additional_tickets: Vec<Vec<u8>>,
}

impl KdcReqBody {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_bits32(0, self.kdc_options);
        if let Some(c) = &self.cname {
            b.extend_from_slice(&ctx_wrap(1, &c.to_der()));
        }
        b.extend_from_slice(&ctx_string(2, &self.realm));
        if let Some(s) = &self.sname {
            b.extend_from_slice(&ctx_wrap(3, &s.to_der()));
        }
        if let Some(t) = self.from {
            b.extend_from_slice(&ctx_time(4, t));
        }
        b.extend_from_slice(&ctx_time(5, self.till));
        if let Some(t) = self.rtime {
            b.extend_from_slice(&ctx_time(6, t));
        }
        b.extend_from_slice(&ctx_uint(7, self.nonce));
        let mut et = Vec::new();
        for e in &self.etypes {
            et.extend_from_slice(&enc_int(*e));
        }
        b.extend_from_slice(&ctx_seq(8, &et));
        if let Some(a) = &self.addresses {
            b.extend_from_slice(&ctx_seq(9, a));
        }
        if let Some(e) = &self.enc_authorization_data {
            b.extend_from_slice(&ctx_wrap(10, &e.to_der()));
        }
        if !self.additional_tickets.is_empty() {
            b.extend_from_slice(&ctx_seq(11, &self.additional_tickets.concat()));
        }
        b
    }

    pub fn from_content(content: &[u8]) -> Result<Self> {
        let f = Fields::parse(content, "KDC-REQ-BODY")?;
        let kdc_options = f.bits32_req(0, "KDC-REQ-BODY.kdc-options")?;
        let cname = match f.seq_of_one(1)? {
            Some(t) => Some(PrincipalName::from_tlv(t)?),
            None => None,
        };
        let realm = f.string_req(2, "KDC-REQ-BODY.realm")?;
        let sname = match f.seq_of_one(3)? {
            Some(t) => Some(PrincipalName::from_tlv(t)?),
            None => None,
        };
        let from = f.time(4)?;
        let till = f.time_req(5, "KDC-REQ-BODY.till")?;
        let rtime = f.time(6)?;
        let nonce = f
            .uint(7)?
            .ok_or_else(|| Error::new("KDC-REQ-BODY: 缺少必需字段 [7] nonce"))?;
        let mut etypes = Vec::new();
        if let Some(items) = f.seq_of(8, "KDC-REQ-BODY.etype")? {
            for it in items {
                etypes.push(parse_i32(it)?);
            }
        }
        let addresses = f.raw(9).map(|c| c.to_vec());
        let enc_authorization_data = match f.seq_of_one(10)? {
            Some(t) => Some(EncryptedData::from_tlv(t)?),
            None => None,
        };
        let mut additional_tickets = Vec::new();
        if let Some(items) = f.seq_of(11, "additional-tickets")? {
            for it in items {
                additional_tickets.push(it.content.to_vec());
            }
        }
        Ok(KdcReqBody {
            kdc_options,
            cname,
            realm,
            sname,
            from,
            till,
            rtime,
            nonce,
            etypes,
            addresses,
            enc_authorization_data,
            additional_tickets,
        })
    }
}

impl<'a> Fields<'a> {
    /// 取一个「里面就一个 TLV」的字段（例如 `[2] PrincipalName`）。
    fn seq_of_one(&self, n: u8) -> Result<Option<Tlv<'a>>> {
        Ok(self.find(n))
    }
}

/// `KDC-REQ ::= SEQUENCE { pvno [1], msg-type [2], padata [3] OPTIONAL, req-body [4] }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdcReq {
    pub pvno: i32,
    pub msg_type: i32,
    pub padata: Vec<PaData>,
    pub body: KdcReqBody,
}

impl KdcReq {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(1, self.pvno);
        b.extend_from_slice(&ctx_int(2, self.msg_type));
        if !self.padata.is_empty() {
            let mut pd = Vec::new();
            for p in &self.padata {
                pd.extend_from_slice(&p.to_der());
            }
            b.extend_from_slice(&ctx_seq(3, &pd));
        }
        b.extend_from_slice(&ctx_seq(4, &self.body.body()));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        let app = if self.msg_type == MSG_TGS_REQ { APP_TGS_REQ } else { APP_AS_REQ };
        app_seq(app, &self.body())
    }

    /// 解析 AS-REQ / TGS-REQ（两种应用标签都接受）。
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "KDC-REQ")?;
        let n = match tlv.tag {
            t if t == app_tag(APP_AS_REQ) => APP_AS_REQ,
            t if t == app_tag(APP_TGS_REQ) => APP_TGS_REQ,
            other => {
                return Err(Error::new(format!(
                    "KDC-REQ: 期望 AS-REQ(0x6a) 或 TGS-REQ(0x6c)，实际 0x{other:02x}"
                )))
            }
        };
        let content = parse_app(data, n, "KDC-REQ")?;
        let f = Fields::parse(content, "KDC-REQ")?;
        let pvno = f.int_req(1, "KDC-REQ.pvno")?;
        let msg_type = f.int_req(2, "KDC-REQ.msg-type")?;
        if msg_type != MSG_AS_REQ && msg_type != MSG_TGS_REQ {
            return Err(Error::new(format!("KDC-REQ: msg-type 不合法: {msg_type}")));
        }
        let mut padata = Vec::new();
        if let Some(items) = f.seq_of(3, "KDC-REQ.padata")? {
            for it in items {
                padata.push(PaData::from_tlv(it)?);
            }
        }
        let body_tlv = f.req(4, "KDC-REQ.req-body")?;
        let body = KdcReqBody::from_content(body_tlv.expect(der::TAG_SEQUENCE, "KDC-REQ-BODY")?)?;
        Ok(KdcReq { pvno, msg_type, padata, body })
    }

    pub fn pa(&self, pa_type: i32) -> Option<&PaData> {
        self.padata.iter().find(|p| p.padata_type == pa_type)
    }
}

/// `KDC-REP ::= SEQUENCE { pvno [0], msg-type [1], padata [2] OPTIONAL,
///                        crealm [3], cname [4], ticket [5], enc-part [6] }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdcRep {
    pub pvno: i32,
    pub msg_type: i32,
    pub padata: Vec<PaData>,
    pub crealm: String,
    pub cname: PrincipalName,
    pub ticket: Ticket,
    pub enc_part: EncryptedData,
}

impl KdcRep {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.pvno);
        b.extend_from_slice(&ctx_int(1, self.msg_type));
        if !self.padata.is_empty() {
            let mut pd = Vec::new();
            for p in &self.padata {
                pd.extend_from_slice(&p.to_der());
            }
            b.extend_from_slice(&ctx_seq(2, &pd));
        }
        b.extend_from_slice(&ctx_string(3, &self.crealm));
        b.extend_from_slice(&ctx_wrap(4, &self.cname.to_der()));
        b.extend_from_slice(&ctx_wrap(5, &self.ticket.to_der()));
        b.extend_from_slice(&ctx_wrap(6, &self.enc_part.to_der()));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        let app = if self.msg_type == MSG_TGS_REP { APP_TGS_REP } else { APP_AS_REP };
        app_seq(app, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "KDC-REP")?;
        let n = match tlv.tag {
            t if t == app_tag(APP_AS_REP) => APP_AS_REP,
            t if t == app_tag(APP_TGS_REP) => APP_TGS_REP,
            other => {
                return Err(Error::new(format!(
                    "KDC-REP: 期望 AS-REP(0x6b) 或 TGS-REP(0x6d)，实际 0x{other:02x}"
                )))
            }
        };
        let content = parse_app(data, n, "KDC-REP")?;
        let f = Fields::parse(content, "KDC-REP")?;
        let pvno = f.int_req(0, "KDC-REP.pvno")?;
        let msg_type = f.int_req(1, "KDC-REP.msg-type")?;
        if msg_type != MSG_AS_REP && msg_type != MSG_TGS_REP {
            return Err(Error::new(format!("KDC-REP: msg-type 不合法: {msg_type}")));
        }
        let mut padata = Vec::new();
        if let Some(items) = f.seq_of(2, "KDC-REP.padata")? {
            for it in items {
                padata.push(PaData::from_tlv(it)?);
            }
        }
        Ok(KdcRep {
            pvno,
            msg_type,
            padata,
            crealm: f.string_req(3, "KDC-REP.crealm")?,
            cname: PrincipalName::from_tlv(f.req(4, "KDC-REP.cname")?)?,
            ticket: Ticket::from_bytes(&wrap_tlv(f.req(5, "KDC-REP.ticket")?)?)?,
            enc_part: EncryptedData::from_tlv(f.req(6, "KDC-REP.enc-part")?)?,
        })
    }
}

/// 把 `Fields` 取出来的内层 TLV 重新编码成独立字节串（给需要「整段再解析」的调用）。
fn wrap_tlv(t: Tlv<'_>) -> Result<Vec<u8>> {
    Ok(der::tlv(t.tag, t.content))
}

/// `EncKDCRepPart`（AS-REP 用 APPLICATION 25，TGS-REP 用 26）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncKdcRepPart {
    pub key: EncryptionKey,
    pub last_req: Vec<(i32, i64)>,
    pub nonce: u32,
    pub key_expiration: Option<i64>,
    pub flags: u32,
    pub authtime: i64,
    pub starttime: Option<i64>,
    pub endtime: i64,
    pub renew_till: Option<i64>,
    pub srealm: String,
    pub sname: PrincipalName,
    pub caddr: Option<Vec<u8>>,
}

impl EncKdcRepPart {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_wrap(0, &self.key.to_der());
        let mut lr = Vec::new();
        for (t, v) in &self.last_req {
            let mut e = ctx_int(0, *t);
            e.extend_from_slice(&ctx_time(1, *v));
            lr.extend_from_slice(&der::seq(&[e]));
        }
        b.extend_from_slice(&ctx_seq(1, &lr));
        b.extend_from_slice(&ctx_uint(2, self.nonce));
        if let Some(t) = self.key_expiration {
            b.extend_from_slice(&ctx_time(3, t));
        }
        b.extend_from_slice(&ctx_bits32(4, self.flags));
        b.extend_from_slice(&ctx_time(5, self.authtime));
        if let Some(t) = self.starttime {
            b.extend_from_slice(&ctx_time(6, t));
        }
        b.extend_from_slice(&ctx_time(7, self.endtime));
        if let Some(t) = self.renew_till {
            b.extend_from_slice(&ctx_time(8, t));
        }
        b.extend_from_slice(&ctx_string(9, &self.srealm));
        b.extend_from_slice(&ctx_wrap(10, &self.sname.to_der()));
        if let Some(a) = &self.caddr {
            b.extend_from_slice(&ctx_seq(11, a));
        }
        b
    }

    pub fn to_der(&self, tgs: bool) -> Vec<u8> {
        let app = if tgs { APP_ENC_TGS_REP_PART } else { APP_ENC_AS_REP_PART };
        app_seq(app, &self.body())
    }

    pub fn from_bytes(data: &[u8], tgs: bool) -> Result<Self> {
        let app = if tgs { APP_ENC_TGS_REP_PART } else { APP_ENC_AS_REP_PART };
        let content = parse_app(data, app, "EncKDCRepPart")?;
        let f = Fields::parse(content, "EncKDCRepPart")?;
        let mut last_req = Vec::new();
        if let Some(items) = f.seq_of(1, "EncKDCRepPart.last-req")? {
            for it in items {
                let lf = Fields::parse(it.expect(der::TAG_SEQUENCE, "LastReq 条目")?, "LastReq 条目")?;
                last_req.push((lf.int_req(0, "LastReq.lr-type")?, lf.time_req(1, "LastReq.lr-value")?));
            }
        }
        Ok(EncKdcRepPart {
            key: EncryptionKey::from_tlv(f.req(0, "EncKDCRepPart.key")?)?,
            last_req,
            nonce: f
                .uint(2)?
                .ok_or_else(|| Error::new("EncKDCRepPart: 缺少必需字段 [2] nonce"))?,
            key_expiration: f.time(3)?,
            flags: f.bits32_req(4, "EncKDCRepPart.flags")?,
            authtime: f.time_req(5, "EncKDCRepPart.authtime")?,
            starttime: f.time(6)?,
            endtime: f.time_req(7, "EncKDCRepPart.endtime")?,
            renew_till: f.time(8)?,
            srealm: f.string_req(9, "EncKDCRepPart.srealm")?,
            sname: PrincipalName::from_tlv(f.req(10, "EncKDCRepPart.sname")?)?,
            caddr: f.raw(11).map(|c| c.to_vec()),
        })
    }
}

/// `AP-REQ ::= [APPLICATION 14] SEQUENCE { pvno [0], msg-type [1], ap-options [2],
///                                        ticket [3], authenticator [4] }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApReq {
    pub pvno: i32,
    pub msg_type: i32,
    pub ap_options: u32,
    pub ticket: Ticket,
    pub authenticator: EncryptedData,
}

impl ApReq {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.pvno);
        b.extend_from_slice(&ctx_int(1, self.msg_type));
        b.extend_from_slice(&ctx_bits32(2, self.ap_options));
        b.extend_from_slice(&ctx_wrap(3, &self.ticket.to_der()));
        b.extend_from_slice(&ctx_wrap(4, &self.authenticator.to_der()));
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_AP_REQ, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_AP_REQ, "AP-REQ")?;
        let f = Fields::parse(content, "AP-REQ")?;
        Ok(ApReq {
            pvno: f.int_req(0, "AP-REQ.pvno")?,
            msg_type: f.int_req(1, "AP-REQ.msg-type")?,
            ap_options: f.bits32_req(2, "AP-REQ.ap-options")?,
            ticket: Ticket::from_bytes(&wrap_tlv(f.req(3, "AP-REQ.ticket")?)?)?,
            authenticator: EncryptedData::from_tlv(f.req(4, "AP-REQ.authenticator")?)?,
        })
    }
}

/// `Authenticator ::= [APPLICATION 2] SEQUENCE { authenticator-vno [0], crealm [1],
///   cname [2], cksum [3] OPTIONAL, cusec [4], ctime [5], subkey [6] OPTIONAL,
///   seq-number [7] OPTIONAL }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticator {
    pub authenticator_vno: i32,
    pub crealm: String,
    pub cname: PrincipalName,
    pub cusec: u32,
    pub ctime: i64,
    pub subkey: Option<EncryptionKey>,
    pub seq_number: Option<u32>,
}

impl Authenticator {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.authenticator_vno);
        b.extend_from_slice(&ctx_string(1, &self.crealm));
        b.extend_from_slice(&ctx_wrap(2, &self.cname.to_der()));
        b.extend_from_slice(&ctx_uint(4, self.cusec));
        b.extend_from_slice(&ctx_time(5, self.ctime));
        if let Some(k) = &self.subkey {
            b.extend_from_slice(&ctx_wrap(6, &k.to_der()));
        }
        if let Some(s) = self.seq_number {
            b.extend_from_slice(&ctx_uint(7, s));
        }
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_AUTHENTICATOR, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_AUTHENTICATOR, "Authenticator")?;
        let f = Fields::parse(content, "Authenticator")?;
        Ok(Authenticator {
            authenticator_vno: f.int_req(0, "Authenticator.authenticator-vno")?,
            crealm: f.string_req(1, "Authenticator.crealm")?,
            cname: PrincipalName::from_tlv(f.req(2, "Authenticator.cname")?)?,
            cusec: f.uint(4)?.unwrap_or(0),
            ctime: f.time_req(5, "Authenticator.ctime")?,
            subkey: match f.seq_of_one(6)? {
                Some(t) => Some(EncryptionKey::from_tlv(t)?),
                None => None,
            },
            seq_number: f.uint(7)?,
        })
    }
}

/// `EncAPRepPart ::= [APPLICATION 27] SEQUENCE { ctime [0], cusec [1], subkey [2] OPTIONAL, seq-number [3] OPTIONAL }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncApRepPart {
    pub ctime: i64,
    pub cusec: u32,
    pub subkey: Option<EncryptionKey>,
    pub seq_number: Option<u32>,
}

impl EncApRepPart {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_time(0, self.ctime);
        b.extend_from_slice(&ctx_uint(1, self.cusec));
        if let Some(k) = &self.subkey {
            b.extend_from_slice(&ctx_wrap(2, &k.to_der()));
        }
        if let Some(s) = self.seq_number {
            b.extend_from_slice(&ctx_uint(3, s));
        }
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_ENC_AP_REP_PART, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_ENC_AP_REP_PART, "EncAPRepPart")?;
        let f = Fields::parse(content, "EncAPRepPart")?;
        Ok(EncApRepPart {
            ctime: f.time_req(0, "EncAPRepPart.ctime")?,
            cusec: f.uint(1)?.unwrap_or(0),
            subkey: match f.seq_of_one(2)? {
                Some(t) => Some(EncryptionKey::from_tlv(t)?),
                None => None,
            },
            seq_number: f.uint(3)?,
        })
    }
}

/// `PA-ENC-TS-ENC ::= SEQUENCE { patimestamp [0] KerberosTime, pausec [1] Microseconds OPTIONAL }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaEncTsEnc {
    pub patimestamp: i64,
    pub pausec: Option<u32>,
}

impl PaEncTsEnc {
    pub fn new(patimestamp: i64, pausec: Option<u32>) -> Self {
        PaEncTsEnc { patimestamp, pausec }
    }

    pub fn to_der(&self) -> Vec<u8> {
        let mut b = ctx_time(0, self.patimestamp);
        if let Some(u) = self.pausec {
            b.extend_from_slice(&ctx_uint(1, u));
        }
        der::seq(&[b])
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let t = der::parse_exact(data, "PA-ENC-TS-ENC")?;
        let f = Fields::parse(t.expect(der::TAG_SEQUENCE, "PA-ENC-TS-ENC")?, "PA-ENC-TS-ENC")?;
        Ok(PaEncTsEnc {
            patimestamp: f.time_req(0, "PA-ENC-TS-ENC.patimestamp")?,
            pausec: f.uint(1)?,
        })
    }
}

/// `KRB-ERROR ::= [APPLICATION 30] SEQUENCE { ... }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KrbError {
    pub pvno: i32,
    pub msg_type: i32,
    pub ctime: Option<i64>,
    pub cusec: Option<u32>,
    pub stime: i64,
    pub susec: u32,
    pub error_code: i32,
    pub crealm: Option<String>,
    pub cname: Option<PrincipalName>,
    pub realm: String,
    pub sname: PrincipalName,
    pub e_text: Option<String>,
    pub e_data: Option<Vec<u8>>,
}

impl KrbError {
    pub fn body(&self) -> Vec<u8> {
        let mut b = ctx_int(0, self.pvno);
        b.extend_from_slice(&ctx_int(1, self.msg_type));
        if let Some(t) = self.ctime {
            b.extend_from_slice(&ctx_time(2, t));
        }
        if let Some(u) = self.cusec {
            b.extend_from_slice(&ctx_uint(3, u));
        }
        b.extend_from_slice(&ctx_time(4, self.stime));
        b.extend_from_slice(&ctx_uint(5, self.susec));
        b.extend_from_slice(&ctx_int(6, self.error_code));
        if let Some(r) = &self.crealm {
            b.extend_from_slice(&ctx_string(7, r));
        }
        if let Some(c) = &self.cname {
            b.extend_from_slice(&ctx_wrap(8, &c.to_der()));
        }
        b.extend_from_slice(&ctx_string(9, &self.realm));
        b.extend_from_slice(&ctx_wrap(10, &self.sname.to_der()));
        if let Some(t) = &self.e_text {
            b.extend_from_slice(&ctx_string(11, t));
        }
        if let Some(d) = &self.e_data {
            b.extend_from_slice(&ctx_octets(12, d));
        }
        b
    }

    pub fn to_der(&self) -> Vec<u8> {
        app_seq(APP_KRB_ERROR, &self.body())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let content = parse_app(data, APP_KRB_ERROR, "KRB-ERROR")?;
        let f = Fields::parse(content, "KRB-ERROR")?;
        Ok(KrbError {
            pvno: f.int_req(0, "KRB-ERROR.pvno")?,
            msg_type: f.int_req(1, "KRB-ERROR.msg-type")?,
            ctime: f.time(2)?,
            cusec: f.uint(3)?,
            stime: f.time_req(4, "KRB-ERROR.stime")?,
            susec: f.uint(5)?.unwrap_or(0),
            error_code: f.int_req(6, "KRB-ERROR.error-code")?,
            crealm: f.string(7)?,
            cname: match f.seq_of_one(8)? {
                Some(t) => Some(PrincipalName::from_tlv(t)?),
                None => None,
            },
            realm: f.string_req(9, "KRB-ERROR.realm")?,
            sname: PrincipalName::from_tlv(f.req(10, "KRB-ERROR.sname")?)?,
            e_text: f.string(11)?,
            e_data: f.octets(12)?,
        })
    }

    /// `e-data` 里的 `METHOD-DATA`（预认证提示就在这里）。
    pub fn method_data(&self) -> Result<Vec<PaData>> {
        match &self.e_data {
            Some(d) if !d.is_empty() => parse_method_data(d),
            _ => Ok(Vec::new()),
        }
    }
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_roundtrip() {
        let p = PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap();
        assert_eq!(p.name_type, NT_SRV_INST);
        assert_eq!(p.display(), "krbtgt/EXAMPLE.COM");
        let der = p.to_der();
        let back = PrincipalName::from_tlv(der::parse_exact(&der, "p").unwrap()).unwrap();
        assert_eq!(p, back);
        // 单组件是 NT_PRINCIPAL
        assert_eq!(PrincipalName::parse("alice").unwrap().name_type, NT_PRINCIPAL);
        assert!(PrincipalName::parse("").is_err());
        assert!(PrincipalName::parse("a@B").is_err());
    }

    #[test]
    fn encrypted_data_and_key_roundtrip() {
        let ed = EncryptedData::new(18, Some(3), vec![1, 2, 3, 4]);
        let back = EncryptedData::from_bytes(&ed.to_der()).unwrap();
        assert_eq!(ed, back);
        // 不带 kvno
        let ed = EncryptedData::new(17, None, vec![9; 40]);
        assert_eq!(EncryptedData::from_bytes(&ed.to_der()).unwrap(), ed);

        let k = EncryptionKey::new(18, vec![7u8; 32]);
        assert_eq!(EncryptionKey::from_bytes(&k.to_der()).unwrap(), k);
        // 密钥长度与 etype 不匹配必须报错
        let bad = EncryptionKey::new(18, vec![7u8; 16]);
        assert!(EncryptionKey::from_bytes(&bad.to_der()).is_err());
    }

    #[test]
    fn ticket_roundtrip_and_app_tag() {
        let t = Ticket {
            tkt_vno: TKT_VNO,
            realm: "EXAMPLE.COM".into(),
            sname: PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
            enc_part: EncryptedData::new(18, Some(1), vec![0xab; 120]),
        };
        let der = t.to_der();
        // 必须以 [APPLICATION 1] 开头（0x61），里面是 SEQUENCE（0x30）
        assert_eq!(der[0], 0x61);
        let outer = der::parse_exact(&der, "ticket").unwrap();
        assert_eq!(outer.tag, 0x61);
        assert_eq!(der::parse_exact(outer.content, "inner").unwrap().tag, 0x30);
        assert_eq!(Ticket::from_bytes(&der).unwrap(), t);
        // 换成 AS-REQ 的标签必须被拒
        let mut bad = der.clone();
        bad[0] = 0x6a;
        assert!(Ticket::from_bytes(&bad).is_err());
    }

    #[test]
    fn kdc_req_roundtrip() {
        let body = KdcReqBody {
            kdc_options: bit_set(bit_set(0, KDCOPT_FORWARDABLE), KDCOPT_RENEWABLE),
            cname: Some(PrincipalName::parse("alice").unwrap()),
            realm: "EXAMPLE.COM".into(),
            sname: Some(PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap()),
            from: None,
            till: 1_800_000_000,
            rtime: Some(1_800_000_000),
            nonce: 0xdead_beef,
            etypes: vec![18, 17],
            addresses: None,
            enc_authorization_data: None,
            additional_tickets: Vec::new(),
        };
        let req = KdcReq {
            pvno: PVNO,
            msg_type: MSG_AS_REQ,
            padata: vec![PaData::new(PA_ENC_TIMESTAMP, vec![1, 2, 3])],
            body,
        };
        let der = req.to_der();
        assert_eq!(der[0], 0x6a);
        let back = KdcReq::from_bytes(&der).unwrap();
        assert_eq!(back, req);
        assert!(back.pa(PA_ENC_TIMESTAMP).is_some());
        assert!(back.pa(PA_PAC_REQUEST).is_none());

        // 无 padata 的请求（JDK 第一次发的就是这种）
        let mut req2 = req.clone();
        req2.padata.clear();
        assert!(KdcReq::from_bytes(&req2.to_der()).unwrap().padata.is_empty());

        // TGS-REQ 标签
        let mut tgs = req.clone();
        tgs.msg_type = MSG_TGS_REQ;
        let der = tgs.to_der();
        assert_eq!(der[0], 0x6c);
        assert_eq!(KdcReq::from_bytes(&der).unwrap().msg_type, MSG_TGS_REQ);
    }

    #[test]
    fn kdc_rep_and_enc_part_roundtrip() {
        let rep = KdcRep {
            pvno: PVNO,
            msg_type: MSG_AS_REP,
            padata: Vec::new(),
            crealm: "EXAMPLE.COM".into(),
            cname: PrincipalName::parse("alice").unwrap(),
            ticket: Ticket {
                tkt_vno: TKT_VNO,
                realm: "EXAMPLE.COM".into(),
                sname: PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
                enc_part: EncryptedData::new(18, Some(1), vec![0xcd; 200]),
            },
            enc_part: EncryptedData::new(18, None, vec![0xef; 160]),
        };
        let der = rep.to_der();
        assert_eq!(der[0], 0x6b);
        assert_eq!(KdcRep::from_bytes(&der).unwrap(), rep);

        let part = EncKdcRepPart {
            key: EncryptionKey::new(18, vec![3u8; 32]),
            last_req: vec![(0, 1_700_000_000)],
            nonce: 42,
            key_expiration: None,
            flags: bit_set(bit_set(bit_set(0, TKTFLAG_INITIAL), TKTFLAG_PRE_AUTHENT), TKTFLAG_FORWARDABLE),
            authtime: 1_700_000_000,
            starttime: Some(1_700_000_000),
            endtime: 1_700_036_000,
            renew_till: Some(1_700_600_000),
            srealm: "EXAMPLE.COM".into(),
            sname: PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
            caddr: None,
        };
        let der = part.to_der(false);
        assert_eq!(der[0], 0x79);
        assert_eq!(EncKdcRepPart::from_bytes(&der, false).unwrap(), part);
        // 换成 TGS 的标签（26 = 0x7a）解析必须失败
        assert!(EncKdcRepPart::from_bytes(&der, true).is_err());
    }

    #[test]
    fn ap_req_and_authenticator_roundtrip() {
        let auth = Authenticator {
            authenticator_vno: 5,
            crealm: "EXAMPLE.COM".into(),
            cname: PrincipalName::parse("alice").unwrap(),
            cusec: 123456,
            ctime: 1_700_000_000,
            subkey: Some(EncryptionKey::new(18, vec![5u8; 32])),
            seq_number: Some(7),
        };
        let der = auth.to_der();
        assert_eq!(der[0], 0x62);
        assert_eq!(Authenticator::from_bytes(&der).unwrap(), auth);

        let ap = ApReq {
            pvno: PVNO,
            msg_type: MSG_AP_REQ,
            ap_options: 0,
            ticket: Ticket {
                tkt_vno: TKT_VNO,
                realm: "EXAMPLE.COM".into(),
                sname: PrincipalName::parse("host/localhost").unwrap(),
                enc_part: EncryptedData::new(18, Some(2), vec![1; 100]),
            },
            authenticator: EncryptedData::new(18, None, vec![2; 80]),
        };
        let der = ap.to_der();
        assert_eq!(der[0], 0x6e);
        assert_eq!(ApReq::from_bytes(&der).unwrap(), ap);
    }

    #[test]
    fn krb_error_and_method_data() {
        let info = vec![PaData::new(
            PA_ETYPE_INFO2,
            encode_etype_info2(&[
                EtypeInfo2Entry { etype: 18, salt: Some("EXAMPLE.COMalice".into()), s2kparams: None },
                EtypeInfo2Entry { etype: 17, salt: None, s2kparams: Some(vec![0, 0, 0x10, 0]) },
            ]),
        )];
        let e = KrbError {
            pvno: PVNO,
            msg_type: MSG_KRB_ERROR,
            ctime: None,
            cusec: None,
            stime: 1_700_000_000,
            susec: 0,
            error_code: errc::KDC_ERR_PREAUTH_REQUIRED,
            crealm: Some("EXAMPLE.COM".into()),
            cname: Some(PrincipalName::parse("alice").unwrap()),
            realm: "EXAMPLE.COM".into(),
            sname: PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap(),
            e_text: Some("NEED PREAUTH".into()),
            e_data: Some(encode_method_data(&info)),
        };
        let der = e.to_der();
        assert_eq!(der[0], 0x7e);
        let back = KrbError::from_bytes(&der).unwrap();
        assert_eq!(back, e);
        let md = back.method_data().unwrap();
        assert_eq!(md.len(), 1);
        let entries = parse_etype_info2(&md[0].padata_value).unwrap();
        assert_eq!(entries[0].etype, 18);
        assert_eq!(entries[0].salt.as_deref(), Some("EXAMPLE.COMalice"));
        assert_eq!(entries[1].s2kparams.as_deref(), Some(&[0u8, 0, 0x10, 0][..]));
        assert_eq!(errc::name(back.error_code), "KDC_ERR_PREAUTH_REQUIRED");
    }

    #[test]
    fn time_format_is_rfc4120() {
        let enc = ctx_time(5, 1_797_000_000);
        // [5] 构造型 + GeneralizedTime + "YYYYMMDDHHMMSSZ"
        let tlv = der::parse_exact(&enc, "t").unwrap();
        assert_eq!(tlv.tag, 0xa5);
        let inner = der::parse_exact(tlv.content, "inner").unwrap();
        assert_eq!(inner.tag, der::TAG_GENERALIZED_TIME);
        let s = std::str::from_utf8(inner.content).unwrap();
        assert_eq!(s.len(), 15);
        assert!(s.ends_with('Z'));
        assert_eq!(parse_time(inner).unwrap(), 1_797_000_000);
        // 不接受没有 Z 的时间
        let bad = Tlv { tag: der::TAG_GENERALIZED_TIME, content: b"20261117113000" };
        assert!(parse_time(bad).is_err());
        let bad = Tlv { tag: der::TAG_GENERALIZED_TIME, content: b"2026-11-17T11:30:00Z" };
        assert!(parse_time(bad).is_err());
    }

    #[test]
    fn flags_and_bits() {
        assert!(bit_get(0x8000_0000, 0));
        assert_eq!(bit_set(0, TKTFLAG_INITIAL), 0x0040_0000);
        assert_eq!(bit_set(0, TKTFLAG_PRE_AUTHENT), 0x0020_0000);
        assert_eq!(bit_set(0, TKTFLAG_FORWARDABLE), 0x4000_0000);
        let f = bit_set(bit_set(bit_set(0, TKTFLAG_FORWARDABLE), TKTFLAG_INITIAL), TKTFLAG_PRE_AUTHENT);
        assert_eq!(flags_string(f), "FIH");
        assert_eq!(flags_string(0), "-");
        // 32 位 flags 编解码
        let enc = ctx_bits32(4, f);
        let tlv = der::parse_exact(&enc, "t").unwrap();
        assert_eq!(parse_bits32(der::parse_exact(tlv.content, "i").unwrap()).unwrap(), f);
    }

    #[test]
    fn rejects_malformed() {
        // 空、截断、错误 tag、隐式标签（少了 EXPLICIT 的壳）
        assert!(KdcReq::from_bytes(&[]).is_err());
        let der = {
            let body = KdcReqBody {
                kdc_options: 0,
                cname: None,
                realm: "R".into(),
                sname: None,
                from: None,
                till: 1,
                rtime: None,
                nonce: 1,
                etypes: vec![18],
                addresses: None,
                enc_authorization_data: None,
                additional_tickets: Vec::new(),
            };
            KdcReq { pvno: PVNO, msg_type: MSG_AS_REQ, padata: vec![], body }.to_der()
        };
        for cut in 1..der.len() {
            assert!(KdcReq::from_bytes(&der[..cut]).is_err(), "截断 {cut} 应报错");
        }
        // 隐式标签（81 01 05 代替 a1 03 02 01 05）必须被拒
        let mut implicit = der.clone();
        implicit[2 + 0] = 0xa1;
        // 手工构造：APPLICATION 10 { SEQUENCE { 81 01 05, ... } }
        let bad = der::tlv(
            0x6a,
            &der::seq(&[{
                let mut b = der::tlv(0x81, &[0x05]);
                b.extend_from_slice(&der[4..]); // 其余照抄
                b
            }]),
        );
        let _ = implicit;
        assert!(KdcReq::from_bytes(&bad).is_err());
    }

    #[test]
    fn fuzz_no_panic() {
        // 随机字节 + 「真报文头部 + 随机尾巴」两种，任何一个入口都不许 panic
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let real = {
            let body = KdcReqBody {
                kdc_options: 0,
                cname: Some(PrincipalName::parse("alice").unwrap()),
                realm: "EXAMPLE.COM".into(),
                sname: Some(PrincipalName::parse("krbtgt/EXAMPLE.COM").unwrap()),
                from: None,
                till: 1_800_000_000,
                rtime: None,
                nonce: 7,
                etypes: vec![18],
                addresses: None,
                enc_authorization_data: None,
                additional_tickets: Vec::new(),
            };
            KdcReq { pvno: PVNO, msg_type: MSG_AS_REQ, padata: vec![], body }.to_der()
        };
        for i in 0..20_000 {
            let mut buf = Vec::new();
            if i % 2 == 0 {
                let n = (next() % 64) as usize;
                for _ in 0..n {
                    buf.push(next() as u8);
                }
            } else {
                buf.extend_from_slice(&real);
                let n = (next() % 8) as usize;
                for _ in 0..n {
                    let idx = (next() as usize) % buf.len();
                    buf[idx] = next() as u8;
                }
            }
            let _ = KdcReq::from_bytes(&buf);
            let _ = KdcRep::from_bytes(&buf);
            let _ = Ticket::from_bytes(&buf);
            let _ = ApReq::from_bytes(&buf);
            let _ = Authenticator::from_bytes(&buf);
            let _ = EncKdcRepPart::from_bytes(&buf, false);
            let _ = KrbError::from_bytes(&buf);
            let _ = EncryptedData::from_bytes(&buf);
            let _ = EncryptionKey::from_bytes(&buf);
            let _ = PaEncTsEnc::from_bytes(&buf);
            let _ = parse_method_data(&buf);
            let _ = parse_etype_info2(&buf);
            let _ = EncApRepPart::from_bytes(&buf);
        }
    }
}
