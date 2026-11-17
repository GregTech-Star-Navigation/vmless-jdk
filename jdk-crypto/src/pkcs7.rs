//! PKCS#7 / CMS SignedData（RFC 5652）：jarsigner 的 `META-INF/*.RSA` 就是这个结构。
//!
//! 生成时与 JDK 的 `sun.security.pkcs.PKCS7` 对齐：**分离内容**（contentInfo 里不带内容）、
//! 带签名属性（contentType / signingTime / messageDigest）、证书放在 `[0]` 里。

use crate::asn1::{der, oid, AlgorithmIdentifier, Name};
use crate::hash::Alg;
use crate::rsa::{self, RsaPrivateKey};
use crate::x509::Certificate;
use crate::{sys, Error, Result};

pub struct SignerInfo {
    pub issuer: Name,
    pub serial: crate::bigint::BigUint,
    pub digest_alg: AlgorithmIdentifier,
    pub signed_attrs: Option<Vec<u8>>,
    pub sig_alg: AlgorithmIdentifier,
    pub signature: Vec<u8>,
}

pub struct SignedData {
    pub digest_alg: AlgorithmIdentifier,
    pub certificates: Vec<Certificate>,
    pub signer: SignerInfo,
    pub content: Option<Vec<u8>>,
}

fn build_attribute(attr_oid: &[u8], value: Vec<u8>) -> Vec<u8> {
    der::seq(&[der::oid(attr_oid), der::set(&[value])])
}

/// 生成分离式 SignedData（`.RSA`/`.DSA` 文件的内容）。
pub fn sign_detached(
    key: &RsaPrivateKey,
    cert_der: &[u8],
    alg: Alg,
    content: &[u8],
) -> Result<Vec<u8>> {
    let digest = alg.digest(content);
    let mut attrs = vec![
        build_attribute(oid::ATTR_CONTENT_TYPE, der::oid(oid::PKCS7_DATA)),
        build_attribute(oid::ATTR_SIGNING_TIME, der::utc_time(sys::now_unix())),
        build_attribute(oid::ATTR_MESSAGE_DIGEST, der::octet_string(&digest)),
    ];
    // DER 要求 SET OF 按编码字节序排序
    attrs.sort();
    let mut attrs_body = Vec::new();
    for a in &attrs {
        attrs_body.extend_from_slice(a);
    }
    let signed_attrs_set = der::tlv(der::TAG_SET, &attrs_body);
    let signature = rsa::sign(key, alg, &signed_attrs_set)?;

    let cert = Certificate::parse(cert_der).map_err(|e| e.ctx("签名证书解析失败"))?;
    let digest_alg = AlgorithmIdentifier::digest(alg.oid());
    let sig_alg = AlgorithmIdentifier::rsa_with_digest(alg);

    let signer_info = der::seq(&[
        der::integer_u32(1),
        der::seq(&[cert.issuer.to_der(), der::integer(&cert.serial)]),
        digest_alg.to_der(),
        der::tlv(0xa0, &attrs_body),
        sig_alg.to_der(),
        der::octet_string(&signature),
    ]);

    let mut signed_data_body = Vec::new();
    signed_data_body.extend_from_slice(&der::integer_u32(1));
    signed_data_body.extend_from_slice(&der::set(&[digest_alg.to_der()]));
    signed_data_body.extend_from_slice(&der::seq(&[der::oid(oid::PKCS7_DATA)]));
    signed_data_body.extend_from_slice(&der::tlv(0xa0, cert_der));
    signed_data_body.extend_from_slice(&der::set(&[signer_info]));

    let signed_data = der::tlv(der::TAG_SEQUENCE, &signed_data_body);
    Ok(der::seq(&[
        der::oid(oid::PKCS7_SIGNED_DATA),
        der::explicit(0, &signed_data),
    ]))
}

/// 解析 ContentInfo(SignedData)。只支持单签名者。
pub fn parse(data: &[u8]) -> Result<SignedData> {
    let outer = der::parse_exact(data, "PKCS#7")?;
    let ci = der::parse_all(outer.expect(der::TAG_SEQUENCE, "ContentInfo")?)?;
    if ci.len() < 2 {
        return Err(Error::new("PKCS#7 ContentInfo 字段不足"));
    }
    let ct = ci[0].expect(der::TAG_OID, "ContentInfo 类型")?;
    if ct != oid::PKCS7_SIGNED_DATA {
        return Err(Error::new("不是 SignedData（ContentInfo 类型不是 1.2.840.113549.1.7.2）"));
    }
    if ci[1].tag != 0xa0 {
        return Err(Error::new("SignedData 缺少 [0] 包装"));
    }
    let sd_tlv = der::parse_exact(ci[1].content, "SignedData")?;
    let sd = der::parse_all(sd_tlv.expect(der::TAG_SEQUENCE, "SignedData")?)?;
    if sd.len() < 4 {
        return Err(Error::new("SignedData 字段不足"));
    }
    let digest_algs = der::parse_all(sd[1].expect(der::TAG_SET, "digestAlgorithms")?)?;
    let digest_alg = match digest_algs.first() {
        Some(t) => AlgorithmIdentifier::parse(t.expect(der::TAG_SEQUENCE, "摘要算法")?)?,
        None => return Err(Error::new("digestAlgorithms 为空")),
    };
    let content_info = der::parse_all(sd[2].expect(der::TAG_SEQUENCE, "encapContentInfo")?)?;
    let mut content = None;
    if content_info.len() > 1 {
        if content_info[1].tag == 0xa0 {
            let inner = der::parse_exact(content_info[1].content, "内容")?;
            content = Some(inner.expect(der::TAG_OCTET_STRING, "内容")?.to_vec());
        }
    }
    let mut idx = 3usize;
    let mut certificates = Vec::new();
    if sd.get(idx).map(|t| t.tag == 0xa0).unwrap_or(false) {
        for c in der::parse_all(sd[idx].content)? {
            if c.tag == der::TAG_SEQUENCE {
                let der_bytes = der::tlv(c.tag, c.content);
                if let Ok(cert) = Certificate::parse(&der_bytes) {
                    certificates.push(cert);
                }
            }
        }
        idx += 1;
    }
    let signer_set = sd
        .get(idx)
        .ok_or_else(|| Error::new("SignedData 缺少 signerInfos"))?;
    let signers = der::parse_all(signer_set.expect(der::TAG_SET, "signerInfos")?)?;
    if signers.is_empty() {
        return Err(Error::new("signerInfos 为空"));
    }
    if signers.len() > 1 {
        return Err(Error::new(format!(
            "只有 {} 个签名者，本实现只支持单签名者",
            signers.len()
        )));
    }
    let si = der::parse_all(signers[0].expect(der::TAG_SEQUENCE, "SignerInfo")?)?;
    if si.len() < 5 {
        return Err(Error::new("SignerInfo 字段不足"));
    }
    let ias = der::parse_all(si[1].expect(der::TAG_SEQUENCE, "IssuerAndSerialNumber")?)?;
    if ias.len() != 2 {
        return Err(Error::new("IssuerAndSerialNumber 字段数不对"));
    }
    let issuer = Name::parse(ias[0].expect(der::TAG_SEQUENCE, "签发者")?)?;
    let serial = der::parse_unsigned(ias[1].expect(der::TAG_INTEGER, "序列号")?)?;
    let sdigest = AlgorithmIdentifier::parse(si[2].expect(der::TAG_SEQUENCE, "摘要算法")?)?;
    let mut p = 3usize;
    let mut signed_attrs = None;
    if si.get(p).map(|t| t.tag == 0xa0).unwrap_or(false) {
        signed_attrs = Some(si[p].content.to_vec());
        p += 1;
    }
    if si.len() < p + 2 {
        return Err(Error::new("SignerInfo 缺少签名算法或签名值"));
    }
    let sig_alg = AlgorithmIdentifier::parse(si[p].expect(der::TAG_SEQUENCE, "签名算法")?)?;
    let signature = si[p + 1].expect(der::TAG_OCTET_STRING, "签名值")?.to_vec();
    Ok(SignedData {
        digest_alg,
        certificates,
        signer: SignerInfo { issuer, serial, digest_alg: sdigest, signed_attrs, sig_alg, signature },
        content,
    })
}

/// 验证 SignedData 对 `content` 的签名，返回签名者证书。
pub fn verify(data: &[u8], content: &[u8]) -> Result<Certificate> {
    let sd = parse(data)?;
    let digest_of_cert: Alg = Alg::from_oid(&sd.digest_alg.oid)
        .ok_or_else(|| Error::new(format!("不支持的摘要算法 {}", sd.digest_alg.name())))?;
    let sig_digest = sd
        .signer
        .sig_alg
        .digest_of_signature()
        .ok_or_else(|| Error::new(format!("不支持的签名算法 {}", sd.signer.sig_alg.name())))?;
    // 找签名者证书
    let signer = sd
        .certificates
        .iter()
        .find(|c| c.subject == sd.signer.issuer && c.serial == sd.signer.serial)
        .ok_or_else(|| Error::new("SignedData 里找不到与 SignerInfo 匹配的证书"))?
        .clone();

    match &sd.signer.signed_attrs {
        Some(attrs) => {
            // 签名覆盖的是 SET 编码（tag 0x31），不是 [0]
            let set = der::tlv(der::TAG_SET, attrs);
            rsa::verify(&signer.public_key, sig_digest, &set, &sd.signer.signature)
                .map_err(|e| e.ctx("PKCS#7 签名校验失败"))?;
            // 检查 messageDigest 属性
            let mut md: Option<Vec<u8>> = None;
            for attr in der::parse_all(attrs)? {
                let parts = der::parse_all(attr.expect(der::TAG_SEQUENCE, "Attribute")?)?;
                if parts.len() != 2 {
                    return Err(Error::new("Attribute 字段数不对"));
                }
                let at = parts[0].expect(der::TAG_OID, "属性类型")?;
                if at == oid::ATTR_MESSAGE_DIGEST {
                    let vals = der::parse_all(parts[1].expect(der::TAG_SET, "属性值")?)?;
                    let v = vals
                        .first()
                        .ok_or_else(|| Error::new("messageDigest 属性没有值"))?;
                    md = Some(v.expect(der::TAG_OCTET_STRING, "messageDigest")?.to_vec());
                }
            }
            let md = md.ok_or_else(|| Error::new("签名属性里缺少 messageDigest"))?;
            let expect = digest_of_cert.digest(content);
            if !crate::hash::ct_eq(&md, &expect) {
                return Err(Error::new("SignedData 的 messageDigest 与内容摘要不一致"));
            }
        }
        None => {
            // 无签名属性：直接对内容验签
            rsa::verify(&signer.public_key, sig_digest, content, &sd.signer.signature)
                .map_err(|e| e.ctx("PKCS#7 签名校验失败"))?;
        }
    }
    Ok(signer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x509::{build_self_signed, CertSpec};

    fn material() -> (RsaPrivateKey, Vec<u8>) {
        let key = rsa::generate(1024, 65537).unwrap();
        let dn = Name::parse_dn_string("CN=Signer,O=Test").unwrap();
        let cert = build_self_signed(&key, &CertSpec::new(dn, 30)).unwrap();
        (key, cert)
    }

    #[test]
    fn sign_and_verify() {
        let (key, cert) = material();
        let content = b"Signature-Version: 1.0\r\n\r\n";
        let der_bytes = sign_detached(&key, &cert, Alg::Sha256, content).unwrap();
        let signer = verify(&der_bytes, content).unwrap();
        assert_eq!(signer.subject.rfc2253(), "CN=Signer, O=Test");
        // 内容改一个字节必须失败
        assert!(verify(&der_bytes, b"Signature-Version: 1.1\r\n\r\n").is_err());
        // 签名值改一个字节必须失败
        let mut bad = der_bytes.clone();
        let n = bad.len();
        bad[n - 1] ^= 0x01;
        assert!(verify(&bad, content).is_err());
    }

    #[test]
    fn rejects_garbage() {
        for seed in 0u32..200 {
            let mut v = Vec::new();
            let mut x = seed.wrapping_mul(2654435761).wrapping_add(3);
            for _ in 0..(seed % 60) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            let _ = parse(&v);
            let _ = verify(&v, b"x");
        }
    }
}
