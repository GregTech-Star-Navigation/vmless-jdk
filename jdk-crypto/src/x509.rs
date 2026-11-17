//! X.509 v3 证书：解析（RFC 5280）与自签生成。

use crate::asn1::{der, oid, AlgorithmIdentifier, GeneralName, Name};
use crate::bigint::BigUint;
use crate::hash::Alg;
use crate::rsa::{self, RsaPrivateKey, RsaPublicKey};
use crate::{sys, Error, Result};

#[derive(Clone, Debug)]
pub struct Extension {
    pub oid: Vec<u8>,
    pub oid_str: String,
    pub critical: bool,
    pub value: Vec<u8>,
    pub subject_alt_names: Vec<GeneralName>,
    pub key_usage: Option<Vec<String>>,
    pub basic_ca: Option<bool>,
    pub subject_key_id: Option<Vec<u8>>,
    pub authority_key_id: Option<Vec<u8>>,
    pub ext_key_usage: Vec<String>,
}

impl Extension {
    pub fn name(&self) -> String {
        match oid::name_of(&self.oid) {
            Some(n) => n.to_string(),
            None => self.oid_str.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Certificate {
    pub der: Vec<u8>,
    pub version: u8,
    pub serial: BigUint,
    pub sig_alg: AlgorithmIdentifier,
    pub issuer: Name,
    pub not_before: i64,
    pub not_after: i64,
    pub subject: Name,
    pub spki: Vec<u8>,
    pub public_key: RsaPublicKey,
    pub signature: Vec<u8>,
    pub extensions: Vec<Extension>,
}

fn parse_extension(content: &[u8]) -> Result<Extension> {
    let items = der::parse_all(content)?;
    if items.len() < 2 {
        return Err(Error::new("Extension 字段不足"));
    }
    let oid_bytes = items[0].expect(der::TAG_OID, "扩展 OID")?.to_vec();
    let oid_str = der::parse_oid(&oid_bytes)?;
    let mut idx = 1;
    let mut critical = false;
    if items[idx].tag == der::TAG_BOOLEAN {
        critical = der::parse_bool(items[idx].content)?;
        idx += 1;
    }
    if idx >= items.len() {
        return Err(Error::new("Extension 缺少 extnValue"));
    }
    let value = items[idx].expect(der::TAG_OCTET_STRING, "扩展值")?.to_vec();
    let mut ext = Extension {
        oid: oid_bytes.clone(),
        oid_str,
        critical,
        value: value.clone(),
        subject_alt_names: Vec::new(),
        key_usage: None,
        basic_ca: None,
        subject_key_id: None,
        authority_key_id: None,
        ext_key_usage: Vec::new(),
    };
    match oid_bytes.as_slice() {
        o if o == oid::EXT_SUBJECT_ALT_NAME => {
            if let Ok(tlv) = der::parse_exact(&value, "SubjectAltName") {
                ext.subject_alt_names = crate::asn1::types::parse_general_names(tlv.content).unwrap_or_default();
            }
        }
        o if o == oid::EXT_KEY_USAGE => {
            if let Ok(tlv) = der::parse_exact(&value, "KeyUsage") {
                if let Ok((bits, unused)) = der::parse_bit_string(tlv.content) {
                    ext.key_usage = Some(decode_key_usage(bits, unused));
                }
            }
        }
        o if o == oid::EXT_BASIC_CONSTRAINTS => {
            if let Ok(tlv) = der::parse_exact(&value, "BasicConstraints") {
                if let Ok(items) = der::parse_all(tlv.content) {
                    for it in items {
                        if it.tag == der::TAG_BOOLEAN {
                            ext.basic_ca = Some(der::parse_bool(it.content).unwrap_or(false));
                        }
                    }
                    if ext.basic_ca.is_none() {
                        ext.basic_ca = Some(false);
                    }
                }
            }
        }
        o if o == oid::EXT_SUBJECT_KEY_ID => {
            if let Ok(tlv) = der::parse_exact(&value, "SubjectKeyIdentifier") {
                ext.subject_key_id = Some(tlv.content.to_vec());
            }
        }
        o if o == oid::EXT_AUTHORITY_KEY_ID => {
            if let Ok(tlv) = der::parse_exact(&value, "AuthorityKeyIdentifier") {
                if let Ok(items) = der::parse_all(tlv.content) {
                    for it in items {
                        if it.tag == 0x80 {
                            ext.authority_key_id = Some(it.content.to_vec());
                        }
                    }
                }
            }
        }
        o if o == oid::EXT_EXT_KEY_USAGE => {
            if let Ok(tlv) = der::parse_exact(&value, "ExtendedKeyUsage") {
                if let Ok(items) = der::parse_all(tlv.content) {
                    for it in items {
                        if it.tag == der::TAG_OID {
                            ext.ext_key_usage.push(
                                der::parse_oid(it.content).unwrap_or_else(|_| "?".into()),
                            );
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(ext)
}

fn decode_key_usage(bits: &[u8], unused: usize) -> Vec<String> {
    const NAMES: [&str; 9] = [
        "digitalSignature",
        "nonRepudiation",
        "keyEncipherment",
        "dataEncipherment",
        "keyAgreement",
        "keyCertSign",
        "cRLSign",
        "encipherOnly",
        "decipherOnly",
    ];
    let total_bits = bits.len() * 8 - unused;
    let mut out = Vec::new();
    for (i, name) in NAMES.iter().enumerate() {
        if i >= total_bits {
            break;
        }
        let byte = i / 8;
        let bit = 7 - (i % 8);
        if byte < bits.len() && (bits[byte] >> bit) & 1 == 1 {
            out.push((*name).to_string());
        }
    }
    out
}

fn encode_key_usage(bits: &[bool]) -> Vec<u8> {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, set) in bits.iter().enumerate() {
        if *set {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let total = bits.len();
    let unused = (8 - total % 8) % 8;
    der::bit_string(&bytes, unused)
}

impl Certificate {
    pub fn parse(der_bytes: &[u8]) -> Result<Certificate> {
        let outer = der::parse_exact(der_bytes, "Certificate")?;
        let items = der::parse_all(outer.expect(der::TAG_SEQUENCE, "Certificate")?)?;
        if items.len() != 3 {
            return Err(Error::new(format!(
                "Certificate 应有 3 个字段，实际 {}",
                items.len()
            )));
        }
        let tbs_tlv = items[0];
        let tbs = der::parse_all(tbs_tlv.expect(der::TAG_SEQUENCE, "TBSCertificate")?)?;
        let mut i = 0usize;
        let mut version = 1u8;
        if tbs.first().map(|t| t.tag == 0xa0).unwrap_or(false) {
            let ver_tlv = der::parse_exact(tbs[0].content, "版本")?;
            let v = der::parse_unsigned(ver_tlv.expect(der::TAG_INTEGER, "版本")?)?;
            if v.bits() > 8 {
                return Err(Error::new("证书版本号过大"));
            }
            version = v.low32() as u8 + 1;
            i = 1;
        }
        let need = i + 6;
        if tbs.len() < need {
            return Err(Error::new("TBSCertificate 字段不足"));
        }
        let serial = der::parse_unsigned(tbs[i].expect(der::TAG_INTEGER, "序列号")?)?;
        i += 1;
        let sig_alg = AlgorithmIdentifier::parse(tbs[i].expect(der::TAG_SEQUENCE, "签名算法")?)?;
        i += 1;
        let issuer = Name::parse(tbs[i].expect(der::TAG_SEQUENCE, "签发者")?)?;
        i += 1;
        let validity = der::parse_all(tbs[i].expect(der::TAG_SEQUENCE, "有效期")?)?;
        if validity.len() != 2 {
            return Err(Error::new("Validity 应有 2 个时间字段"));
        }
        let not_before = der::parse_time(validity[0].tag, validity[0].content)?;
        let not_after = der::parse_time(validity[1].tag, validity[1].content)?;
        i += 1;
        let subject = Name::parse(tbs[i].expect(der::TAG_SEQUENCE, "主体")?)?;
        i += 1;
        let spki_tlv = tbs[i];
        let spki = der::tlv(spki_tlv.tag, spki_tlv.content);
        let public_key = RsaPublicKey::from_spki_der(&spki)
            .map_err(|e| e.ctx("解析证书公钥失败"))?;
        i += 1;

        let mut extensions = Vec::new();
        while i < tbs.len() {
            let t = tbs[i];
            if t.tag == 0xa3 {
                let seq_tlv = der::parse_exact(t.content, "Extensions")?;
                for ext in der::parse_all(seq_tlv.expect(der::TAG_SEQUENCE, "Extensions")?)? {
                    extensions.push(
                        parse_extension(ext.expect(der::TAG_SEQUENCE, "Extension")?)
                            .map_err(|e| e.ctx("解析扩展失败"))?,
                    );
                }
            }
            i += 1;
        }

        let signature = {
            let (bits, unused) = der::parse_bit_string(items[2].expect(der::TAG_BIT_STRING, "签名值")?)?;
            if unused != 0 {
                return Err(Error::new("签名 BIT STRING 有未使用位"));
            }
            bits.to_vec()
        };
        let sig_alg_outer = AlgorithmIdentifier::parse(items[1].expect(der::TAG_SEQUENCE, "签名算法")?)?;
        if sig_alg_outer.oid != sig_alg.oid {
            return Err(Error::new("内外签名算法不一致"));
        }
        Ok(Certificate {
            der: der_bytes.to_vec(),
            version,
            serial,
            sig_alg,
            issuer,
            not_before,
            not_after,
            subject,
            spki,
            public_key,
            signature,
            extensions,
        })
    }

    /// 自签名证书的自校验（只支持 RSA）。
    pub fn verify_self_signature(&self) -> Result<()> {
        let alg = self
            .sig_alg
            .digest_of_signature()
            .ok_or_else(|| Error::new(format!("不支持的签名算法 {}", self.sig_alg.name())))?;
        // TBS 的 DER 需要重新取：证书里第一段就是它
        let outer = der::parse_exact(&self.der, "Certificate")?;
        let items = der::parse_all(outer.expect(der::TAG_SEQUENCE, "Certificate")?)?;
        let tbs_der = der::tlv(items[0].tag, items[0].content);
        rsa::verify(&self.public_key, alg, &tbs_der, &self.signature)
    }

    pub fn ski(&self) -> Option<&Vec<u8>> {
        self.extensions.iter().find_map(|e| e.subject_key_id.as_ref())
    }

    pub fn san(&self) -> Vec<GeneralName> {
        let mut out = Vec::new();
        for e in &self.extensions {
            out.extend(e.subject_alt_names.iter().cloned());
        }
        out
    }

    /// keytool 风格的可读输出。
    pub fn describe(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("Owner: {}\n", self.subject.rfc2253()));
        s.push_str(&format!("Issuer: {}\n", self.issuer.rfc2253()));
        s.push_str(&format!("Serial number: {}\n", self.serial.to_hex()));
        let off = sys::local_offset_secs();
        s.push_str(&format!(
            "Valid from: {} until: {}\n",
            sys::human_stamp(self.not_before, off),
            sys::human_stamp(self.not_after, off)
        ));
        s.push_str(&format!("Signature algorithm name: {}\n", self.sig_alg.name()));
        s.push_str(&format!(
            "Subject Public Key Algorithm: {}-bit RSA key\n",
            self.public_key.bits()
        ));
        s.push_str(&format!("Version: {}\n", self.version));
        if !self.extensions.is_empty() {
            s.push_str("Extensions:\n");
            for (i, ext) in self.extensions.iter().enumerate() {
                s.push_str(&format!(
                    "#{}: ObjectId: {} Criticality={}\n",
                    i + 1,
                    dotted_oid(&ext.oid),
                    ext.critical
                ));
                if let Some(ski) = &ext.subject_key_id {
                    s.push_str("SubjectKeyIdentifier [\n  ");
                    s.push_str(&ski.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"));
                    s.push_str("\n]\n");
                }
                if !ext.subject_alt_names.is_empty() {
                    s.push_str("SubjectAlternativeName [\n");
                    for n in &ext.subject_alt_names {
                        let (kind, val) = match n {
                            GeneralName::DnsName(v) => ("DNSName", v.clone()),
                            GeneralName::Rfc822(v) => ("RFC822Name", v.clone()),
                            GeneralName::Uri(v) => ("URI", v.clone()),
                            GeneralName::IpAddress(v) => ("IPAddress", v.clone()),
                            GeneralName::DirectoryName(v) => ("DirectoryName", v.clone()),
                            GeneralName::Other(v) => ("Other", v.clone()),
                        };
                        s.push_str(&format!("  {kind}: {val}\n"));
                    }
                    s.push_str("]\n");
                }
                if let Some(ku) = &ext.key_usage {
                    s.push_str(&format!("KeyUsage [\n  {}\n]\n", ku.join(", ")));
                }
                if let Some(ca) = ext.basic_ca {
                    s.push_str(&format!("BasicConstraints: CA={ca}\n"));
                }
            }
        }
        s
    }
}

pub fn dotted_oid(o: &[u8]) -> String {
    match oid::name_of(o) {
        Some(n) => match der::parse_oid(o) {
            Ok(d) => format!("{d} ({n})"),
            Err(_) => n.to_string(),
        },
        None => der::parse_oid(o).unwrap_or_else(|_| "?".into()),
    }
}

/// 自签证书的构建参数。
pub struct CertSpec {
    pub subject: Name,
    pub serial: BigUint,
    pub not_before: i64,
    pub not_after: i64,
    pub sig_alg: Alg,
    pub san: Vec<String>,
    pub key_usage: Option<Vec<bool>>,
    pub basic_ca: Option<bool>,
    pub include_ski: bool,
}

impl CertSpec {
    /// keytool `-genkeypair` 的默认画像：v3、SKI 扩展、可选 SAN。
    pub fn new(subject: Name, days: i64) -> CertSpec {
        let now = sys::now_unix();
        CertSpec {
            subject,
            serial: BigUint::from_u64(sys::now_unix() as u64),
            not_before: now,
            not_after: now + days * 86400,
            sig_alg: Alg::Sha256,
            san: Vec::new(),
            key_usage: None,
            basic_ca: None,
            include_ski: true,
        }
    }
}

fn build_extensions(spec: &CertSpec, key: &RsaPrivateKey) -> Vec<Vec<u8>> {
    let mut exts = Vec::new();
    if spec.include_ski {
        let ski = Alg::Sha1.digest(&key.public().to_pkcs1_der());
        exts.push(der::seq(&[
            der::oid(oid::EXT_SUBJECT_KEY_ID),
            der::octet_string(&der::octet_string(&ski)),
        ]));
    }
    if let Some(ku) = &spec.key_usage {
        exts.push(der::seq(&[
            der::oid(oid::EXT_KEY_USAGE),
            der::boolean(true),
            der::octet_string(&encode_key_usage(ku)),
        ]));
    }
    if let Some(ca) = spec.basic_ca {
        let mut inner = Vec::new();
        if ca {
            inner.push(der::boolean(true));
        }
        exts.push(der::seq(&[
            der::oid(oid::EXT_BASIC_CONSTRAINTS),
            der::boolean(true),
            der::octet_string(&der::seq(&inner)),
        ]));
    }
    if !spec.san.is_empty() {
        let mut names = Vec::new();
        for n in &spec.san {
            if let Ok(ip) = n.parse::<std::net::IpAddr>() {
                match ip {
                    std::net::IpAddr::V4(v4) => names.push(der::context_primitive(7, &v4.octets())),
                    std::net::IpAddr::V6(v6) => names.push(der::context_primitive(7, &v6.octets())),
                }
            } else if n.contains('@') {
                names.push(der::context_primitive(1, n.as_bytes()));
            } else if n.starts_with("http://") || n.starts_with("https://") {
                names.push(der::context_primitive(6, n.as_bytes()));
            } else {
                names.push(der::context_primitive(2, n.as_bytes()));
            }
        }
        exts.push(der::seq(&[
            der::oid(oid::EXT_SUBJECT_ALT_NAME),
            der::octet_string(&der::seq(&names)),
        ]));
    }
    exts
}

/// 生成自签证书（DER）。
pub fn build_self_signed(key: &RsaPrivateKey, spec: &CertSpec) -> Result<Vec<u8>> {
    let pubkey = key.public();
    let spki = pubkey.to_spki_der();
    let sig_alg = AlgorithmIdentifier::rsa_with_digest(spec.sig_alg);
    let sig_alg_der = sig_alg.to_der();
    let exts = build_extensions(spec, key);
    let mut tbs_items = vec![
        der::explicit(0, &der::integer_u32(2)), // v3
        der::integer(&spec.serial),
        sig_alg_der.clone(),
        spec.subject.to_der(),
        der::seq(&[der::utc_time(spec.not_before), der::generalized_time(spec.not_after)]),
        spec.subject.to_der(),
        spki,
    ];
    if !exts.is_empty() {
        tbs_items.push(der::explicit(3, &der::seq(&exts)));
    }
    let tbs = der::seq(&tbs_items);
    let sig = rsa::sign(key, spec.sig_alg, &tbs)?;
    let cert = der::seq(&[tbs, sig_alg_der, der::bit_string(&sig, 0)]);
    // 自检：解析 + 验签，坏证书不出门
    let parsed = Certificate::parse(&cert)?;
    parsed.verify_self_signature()?;
    Ok(cert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hex;

    fn test_key() -> RsaPrivateKey {
        rsa::generate(1024, 65537).unwrap()
    }

    #[test]
    fn build_and_parse() {
        let key = test_key();
        let dn = Name::parse_dn_string("CN=Test,OU=Dev,O=Example,L=City,ST=ST,C=CN").unwrap();
        let mut spec = CertSpec::new(dn.clone(), 365);
        spec.san = vec!["example.com".into(), "10.0.0.1".into()];
        spec.key_usage = Some(vec![true, false, true]);
        let cert_der = build_self_signed(&key, &spec).unwrap();
        let cert = Certificate::parse(&cert_der).unwrap();
        assert_eq!(cert.version, 3);
        assert_eq!(cert.subject, dn);
        assert_eq!(cert.issuer, dn);
        assert_eq!(cert.public_key, key.public());
        assert_eq!(cert.sig_alg.name(), "SHA256withRSA");
        assert!(cert.san().len() == 2);
        assert_eq!(
            cert.extensions.iter().find_map(|e| e.key_usage.clone()).unwrap(),
            vec!["digitalSignature", "keyEncipherment"]
        );
        cert.verify_self_signature().unwrap();
        // 篡改一个字节必须验签失败
        let mut bad = cert_der.clone();
        let n = bad.len();
        bad[n - 1] ^= 0x01;
        match Certificate::parse(&bad) {
            Ok(c) => assert!(c.verify_self_signature().is_err()),
            Err(_) => {}
        }
    }

    #[test]
    fn rejects_truncated() {
        let key = test_key();
        let dn = Name::parse_dn_string("CN=T").unwrap();
        let cert = build_self_signed(&key, &CertSpec::new(dn, 1)).unwrap();
        for cut in [0usize, 1, 4, 10, cert.len() / 2, cert.len() - 1] {
            assert!(Certificate::parse(&cert[..cut]).is_err(), "截断到 {cut} 应该报错");
        }
    }

    #[test]
    fn ski_matches_rfc5280_method1() {
        let key = test_key();
        let dn = Name::parse_dn_string("CN=T").unwrap();
        let cert = Certificate::parse(&build_self_signed(&key, &CertSpec::new(dn, 1)).unwrap()).unwrap();
        let expect = Alg::Sha1.digest(&key.public().to_pkcs1_der());
        assert_eq!(hex(cert.ski().unwrap()), hex(&expect));
    }
}
