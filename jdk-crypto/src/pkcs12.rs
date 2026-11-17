//! PKCS#12（RFC 7292）：`.p12` keystore 的读写。
//!
//! 支持范围（与 JDK 25 `keytool` 的默认产物完全对齐）：
//! - 认证安全层：`AuthenticatedSafe` + `MacData`（PKCS#12 KDF + HMAC-SHA256）
//! - 加密：PBES2 / PBKDF2-HMAC-SHA256 / AES-128|192|256-CBC
//! - 包：`pkcs8ShroudedKeyBag`、`keyBag`、`certBag`（x509Certificate）
//! - 属性：`friendlyName`、`localKeyID`
//!
//! 旧式 `PBEWithSHA1AndDESede` / `PBEWithSHA1AndRC2_40` 不在支持范围内——读到会给出明确错误，
//! 不会静默当成空口令。

use crate::asn1::{der, oid, AlgorithmIdentifier};
use crate::bigint::BigUint;
use crate::cipher;
use crate::hash::{self, Alg};
use crate::rsa::{RsaPrivateKey, RsaPublicKey};
use crate::x509::Certificate;
use crate::{sys, Error, Result};

#[derive(Clone, Debug, Default)]
pub struct P12Entry {
    pub alias: String,
    pub key_id: Vec<u8>,
    /// 证书链，叶子在前。
    pub chain: Vec<Certificate>,
    pub private_key: Option<RsaPrivateKey>,
}

impl P12Entry {
    pub fn leaf(&self) -> Result<&Certificate> {
        self.chain
            .first()
            .ok_or_else(|| Error::new(format!("别名 {} 没有证书", self.alias)))
    }
}

#[derive(Clone, Debug, Default)]
pub struct P12 {
    pub entries: Vec<P12Entry>,
}

impl P12 {
    pub fn find(&self, alias: &str) -> Option<&P12Entry> {
        let lower = alias.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|e| e.alias.eq_ignore_ascii_case(alias))
            .or_else(|| self.entries.iter().find(|e| e.alias.to_ascii_lowercase() == lower))
    }

    pub fn load(bytes: &[u8], password: &str) -> Result<P12> {
        let pfx = der::parse_exact(bytes, "PFX").map_err(|e| {
            e.ctx("不是合法的 PKCS#12 文件（DER 解析失败）")
        })?;
        let items = der::parse_all(pfx.expect(der::TAG_SEQUENCE, "PFX")?)?;
        if items.len() < 2 {
            return Err(Error::new("PFX 字段不足"));
        }
        let version = der::parse_unsigned(items[0].expect(der::TAG_INTEGER, "PFX 版本")?)?;
        if version.low32() != 3 {
            return Err(Error::new(format!("不支持的 PFX 版本 {}", version.low32())));
        }
        // authSafe
        let ci = der::parse_all(items[1].expect(der::TAG_SEQUENCE, "authSafe")?)?;
        if ci.len() < 2 || ci[0].expect(der::TAG_OID, "authSafe 类型")? != oid::PKCS7_DATA {
            return Err(Error::new("authSafe 不是 data 类型"));
        }
        if ci[1].tag != 0xa0 {
            return Err(Error::new("authSafe 缺少 [0] 包装"));
        }
        let inner = der::parse_exact(ci[1].content, "authSafe 内容")?;
        let auth_safe = inner.expect(der::TAG_OCTET_STRING, "AuthenticatedSafe")?;

        // MAC 校验
        if items.len() > 2 {
            if items[2].tag == der::TAG_SEQUENCE {
                verify_mac(items[2].content, auth_safe, password)?;
            }
        }

        let mut out = P12::default();
        let safe_seq = der::parse_exact(auth_safe, "AuthenticatedSafe")?;
        for content_info in der::parse_all(safe_seq.expect(der::TAG_SEQUENCE, "AuthenticatedSafe")?)? {
            let parts = der::parse_all(content_info.expect(der::TAG_SEQUENCE, "ContentInfo")?)?;
            if parts.len() < 2 {
                return Err(Error::new("ContentInfo 字段不足"));
            }
            let ct = parts[0].expect(der::TAG_OID, "ContentInfo 类型")?;
            let body = parts[1];
            if body.tag != 0xa0 {
                return Err(Error::new("ContentInfo 缺少 [0] 包装"));
            }
            let safe_contents_der: Vec<u8> = if ct == oid::PKCS7_DATA {
                let oct = der::parse_exact(body.content, "SafeContents(明文)")?;
                oct.expect(der::TAG_OCTET_STRING, "SafeContents(明文)")?.to_vec()
            } else if ct == oid::PKCS7_ENCRYPTED_DATA {
                decrypt_encrypted_data(body.content, password)?
            } else {
                return Err(Error::new(format!(
                    "不支持的内容类型 {}",
                    der::parse_oid(ct).unwrap_or_else(|_| "?".into())
                )));
            };
            parse_safe_contents(&safe_contents_der, &mut out, password)?;
        }
        Ok(out)
    }

    /// 生成 `.p12`（单口令、MAC 用 SHA-256、加密用 AES-256-CBC）。
    pub fn store(&self, password: &str) -> Result<Vec<u8>> {
        let mut key_safe_bags = Vec::new();
        let mut cert_safe_bags = Vec::new();
        for e in &self.entries {
            let key_id = if e.key_id.is_empty() {
                match e.chain.first() {
                    Some(c) => Alg::Sha1.digest(&c.der),
                    None => sys::random_vec(8)?,
                }
            } else {
                e.key_id.clone()
            };
            let attrs = bag_attributes(&e.alias, &key_id, e.private_key.is_none());
            if let Some(k) = &e.private_key {
                let pki = k.to_pkcs8_der();
                let (alg, ct) = pbes2_encrypt(&password.as_bytes().to_vec(), &pki)?;
                let epki = der::seq(&[alg.to_der(), der::octet_string(&ct)]);
                key_safe_bags.push(der::seq(&[
                    der::oid(oid::PKCS12_SHROUDED_KEY_BAG),
                    der::explicit(0, &epki),
                    der::set(&attrs),
                ]));
            }
            if let Some(c) = e.chain.first() {
                cert_safe_bags.push(make_cert_bag(&c.der, &attrs));
            }
            for extra in e.chain.iter().skip(1) {
                cert_safe_bags.push(make_cert_bag(&extra.der, &attrs));
            }
        }

        let mut content_infos = Vec::new();
        if !key_safe_bags.is_empty() {
            let safe = der::seq(&key_safe_bags);
            content_infos.push(encrypted_content_info(&safe, password)?);
        }
        if !cert_safe_bags.is_empty() {
            let safe = der::seq(&cert_safe_bags);
            content_infos.push(encrypted_content_info(&safe, password)?);
        }
        let auth_safe = der::seq(&content_infos);
        let auth_safe_octets = der::octet_string(&auth_safe);
        let content_info = der::seq(&[der::oid(oid::PKCS7_DATA), der::explicit(0, &auth_safe_octets)]);

        // MAC：PKCS#12 KDF(id=3) + HMAC-SHA256
        let mac_salt = sys::random_vec(20)?;
        let iterations = 10000u32;
        let mac_key = hash::pkcs12_kdf(Alg::Sha256, password.as_bytes(), &mac_salt, iterations, 3, 32);
        let mac = hash::hmac(Alg::Sha256, &mac_key, &auth_safe);
        let digest_info = der::seq(&[
            der::seq(&[der::oid(oid::SHA256), der::null()]),
            der::octet_string(&mac),
        ]);
        let mac_data = der::seq(&[digest_info, der::octet_string(&mac_salt), der::integer_u32(iterations)]);

        Ok(der::seq(&[der::integer_u32(3), content_info, mac_data]))
    }
}

fn make_cert_bag(cert_der: &[u8], attrs: &[Vec<u8>]) -> Vec<u8> {
    let cert_bag = der::seq(&[
        der::oid(oid::PKCS9_X509_CERTIFICATE),
        der::explicit(0, &der::octet_string(cert_der)),
    ]);
    der::seq(&[
        der::oid(oid::PKCS12_CERT_BAG),
        der::explicit(0, &cert_bag),
        der::set(attrs),
    ])
}

fn bag_attributes(alias: &str, key_id: &[u8], trusted: bool) -> Vec<Vec<u8>> {
    let mut friendly = Vec::new();
    for unit in alias.encode_utf16() {
        friendly.extend_from_slice(&unit.to_be_bytes());
    }
    let mut attrs = vec![
        der::seq(&[der::oid(oid::PKCS9_FRIENDLY_NAME), der::set(&[der::tlv(der::TAG_BMP_STRING, &friendly)])]),
        der::seq(&[der::oid(oid::PKCS9_LOCAL_KEY_ID), der::set(&[der::octet_string(key_id)])]),
    ];
    if trusted {
        // JDK 只有在 certBag 带 TrustedKeyUsage 属性时才把它当成可信证书条目；
        // 否则只当作私钥链的缓存，-list 里看不到。
        attrs.push(trusted_key_usage_attr());
    }
    attrs.sort();
    attrs
}

/// 2.16.840.1.113894.746875.1.1 = TrustedKeyUsage（JDK 私有扩展）
fn trusted_key_usage_attr() -> Vec<u8> {
    let attr_oid = der::oid_from_str("2.16.840.1.113894.746875.1.1").expect("OID");
    let any_eku = der::oid_from_str("2.5.29.37.0").expect("OID");
    let attr_content = der::parse_tlv(&attr_oid).map(|(t, _)| t.content.to_vec()).unwrap_or_default();
    let eku_content = der::parse_tlv(&any_eku).map(|(t, _)| t.content.to_vec()).unwrap_or_default();
    der::seq(&[der::oid(&attr_content), der::set(&[der::oid(&eku_content)])])
}

/// PBES2 + PBKDF2-HMAC-SHA256 + AES-256-CBC。
fn pbes2_encrypt(password: &[u8], plain: &[u8]) -> Result<(AlgorithmIdentifier, Vec<u8>)> {
    let salt = sys::random_vec(20)?;
    let iv = sys::random_vec(16)?;
    let iterations = 10000u32;
    let key = hash::pbkdf2(Alg::Sha256, password, &salt, iterations, 32);
    let ct = cipher::cbc_encrypt(&key, &iv, plain)?;
    let kdf = der::seq(&[
        der::oid(oid::PBKDF2),
        der::seq(&[
            der::octet_string(&salt),
            der::integer_u32(iterations),
            der::integer_u32(32),
            der::seq(&[der::oid(oid::HMAC_WITH_SHA256), der::null()]),
        ]),
    ]);
    let scheme = der::seq(&[der::oid(oid::AES256_CBC), der::octet_string(&iv)]);
    let params = der::seq(&[kdf, scheme]);
    Ok((AlgorithmIdentifier { oid: oid::PBES2.to_vec(), params: Some(params) }, ct))
}

/// 解出 PBES2/PBKDF2/AES-CBC 的明文。
fn pbes2_decrypt(password: &[u8], alg: &AlgorithmIdentifier, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let params = alg
        .params
        .as_ref()
        .ok_or_else(|| Error::new("PBES2 缺少参数"))?;
    let p = der::parse_exact(params, "PBES2 参数")?;
    let items = der::parse_all(p.expect(der::TAG_SEQUENCE, "PBES2 参数")?)?;
    if items.len() < 2 {
        return Err(Error::new("PBES2 参数字段不足"));
    }
    let kdf = der::parse_all(items[0].expect(der::TAG_SEQUENCE, "KDF")?)?;
    if kdf.len() < 2 || kdf[0].expect(der::TAG_OID, "KDF 类型")? != oid::PBKDF2 {
        let name = kdf
            .first()
            .and_then(|t| der::parse_oid(t.content).ok())
            .unwrap_or_else(|| "?".into());
        return Err(Error::new(format!("不支持的密钥派生函数 {name}（只支持 PBKDF2）")));
    }
    let kp = der::parse_all(kdf[1].expect(der::TAG_SEQUENCE, "PBKDF2 参数")?)?;
    if kp.len() < 2 {
        return Err(Error::new("PBKDF2 参数不足"));
    }
    let salt = kp[0].expect(der::TAG_OCTET_STRING, "salt")?;
    let iterations = der::parse_unsigned(kp[1].expect(der::TAG_INTEGER, "迭代次数")?)?;
    let iter = iterations.low32();
    if iterations.bits() > 32 || iter == 0 {
        return Err(Error::new("PBKDF2 迭代次数不合法"));
    }
    let mut prf = Alg::Sha1; // 默认 hmacWithSHA1
    let mut key_len_hint: Option<usize> = None;
    for extra in &kp[2..] {
        match extra.tag {
            der::TAG_INTEGER => {
                let v = der::parse_unsigned(extra.content)?;
                key_len_hint = Some(v.low32() as usize);
            }
            der::TAG_SEQUENCE => {
                let prf_alg = AlgorithmIdentifier::parse(extra.content)?;
                prf = match prf_alg.oid.as_slice() {
                    o if o == oid::HMAC_WITH_SHA256 => Alg::Sha256,
                    o if o == oid::HMAC_WITH_SHA1 => Alg::Sha1,
                    o if o == oid::SHA1 => Alg::Sha1,
                    o if o == oid::SHA256 => Alg::Sha256,
                    o if o == oid::SHA384 => Alg::Sha384,
                    o if o == oid::SHA512 => Alg::Sha512,
                    _ => {
                        return Err(Error::new(format!(
                            "不支持的 PBKDF2 PRF {}",
                            prf_alg.name()
                        )))
                    }
                };
            }
            _ => {}
        }
    }
    // 加密方案
    let scheme = der::parse_all(items[1].expect(der::TAG_SEQUENCE, "加密方案")?)?;
    if scheme.len() < 2 {
        return Err(Error::new("加密方案参数不足"));
    }
    let enc_oid = scheme[0].expect(der::TAG_OID, "加密算法")?;
    let key_len = match enc_oid {
        o if o == oid::AES128_CBC => 16,
        o if o == oid::AES256_CBC => 32,
        _ => {
            let name = der::parse_oid(enc_oid).unwrap_or_else(|_| "?".into());
            return Err(Error::new(format!(
                "不支持的对称加密算法 {name}（只支持 AES-128/192/256-CBC；\
                 3DES/RC2 的旧式 p12 请先用 keytool -importkeystore 转到新格式）"
            )));
        }
    };
    if let Some(hint) = key_len_hint {
        if hint != key_len {
            return Err(Error::new(format!(
                "PBKDF2 的 keyLength({hint}) 与加密算法所需({key_len}) 不一致"
            )));
        }
    }
    let iv = scheme[1].expect(der::TAG_OCTET_STRING, "IV")?;
    let key = hash::pbkdf2(prf, password, salt, iter, key_len);
    cipher::cbc_decrypt(&key, iv, ciphertext)
}

fn verify_mac(mac_data: &[u8], auth_safe: &[u8], password: &str) -> Result<()> {
    let items = der::parse_all(mac_data)?;
    if items.len() < 2 {
        return Err(Error::new("MacData 字段不足"));
    }
    let di = der::parse_all(items[0].expect(der::TAG_SEQUENCE, "DigestInfo")?)?;
    if di.len() < 2 {
        return Err(Error::new("MacData 的 DigestInfo 字段不足"));
    }
    let alg = AlgorithmIdentifier::parse(di[0].expect(der::TAG_SEQUENCE, "MAC 算法")?)?;
    let mac_in_file = di[1].expect(der::TAG_OCTET_STRING, "MAC 值")?;
    let salt = items[1].expect(der::TAG_OCTET_STRING, "MAC salt")?;
    let iterations = if items.len() > 2 {
        let v = der::parse_unsigned(items[2].expect(der::TAG_INTEGER, "MAC 迭代次数")?)?;
        if v.bits() > 32 {
            return Err(Error::new("MAC 迭代次数过大"));
        }
        v.low32()
    } else {
        1
    };
    let alg = match alg.oid.as_slice() {
        o if o == oid::SHA1 => Alg::Sha1,
        o if o == oid::SHA256 => Alg::Sha256,
        o if o == oid::SHA384 => Alg::Sha384,
        o if o == oid::SHA512 => Alg::Sha512,
        _ => return Err(Error::new(format!("不支持的 MAC 摘要算法 {}", alg.name()))),
    };
    let key = hash::pkcs12_kdf(alg, password.as_bytes(), salt, iterations, 3, alg.out_len());
    let expect = hash::hmac(alg, &key, auth_safe);
    if !hash::ct_eq(&expect, mac_in_file) {
        return Err(Error::new(
            "口令错误或文件损坏：PKCS#12 的完整性校验（MAC）不匹配",
        ));
    }
    Ok(())
}

fn decrypt_encrypted_data(content: &[u8], password: &str) -> Result<Vec<u8>> {
    let ed = der::parse_exact(content, "EncryptedData")?;
    let items = der::parse_all(ed.expect(der::TAG_SEQUENCE, "EncryptedData")?)?;
    if items.len() < 2 {
        return Err(Error::new("EncryptedData 字段不足"));
    }
    let eci = der::parse_all(items[1].expect(der::TAG_SEQUENCE, "EncryptedContentInfo")?)?;
    if eci.len() < 3 {
        return Err(Error::new("EncryptedContentInfo 字段不足"));
    }
    let alg = AlgorithmIdentifier::parse(eci[1].expect(der::TAG_SEQUENCE, "内容加密算法")?)?;
    if alg.oid != oid::PBES2 {
        return Err(Error::new(format!(
            "不支持的 keystore 加密算法 {}（只支持 PBES2；旧式 PBEWithSHA1AndDESede/RC2 的 p12 请先用 keytool -importkeystore 升级）",
            alg.name()
        )));
    }
    let ct_tag = eci.get(2).ok_or_else(|| Error::new("EncryptedData 缺少密文"))?;
    let ct = match ct_tag.tag {
        0x80 | 0xa0 => ct_tag.content,
        _ => return Err(Error::new("EncryptedData 的密文标签不对")),
    };
    pbes2_decrypt(password.as_bytes(), &alg, ct)
}

fn encrypted_content_info(safe_contents: &[u8], password: &str) -> Result<Vec<u8>> {
    let (alg, ct) = pbes2_encrypt(password.as_bytes(), safe_contents)?;
    let eci = der::seq(&[
        der::oid(oid::PKCS7_DATA),
        alg.to_der(),
        der::tlv(0x80, &ct),
    ]);
    let encrypted_data = der::seq(&[der::integer_u32(0), eci]);
    Ok(der::seq(&[
        der::oid(oid::PKCS7_ENCRYPTED_DATA),
        der::explicit(0, &encrypted_data),
    ]))
}

fn parse_safe_contents(der_bytes: &[u8], out: &mut P12, password: &str) -> Result<()> {
    let seq = der::parse_exact(der_bytes, "SafeContents")?;
    for bag in der::parse_all(seq.expect(der::TAG_SEQUENCE, "SafeContents")?)? {
        let parts = der::parse_all(bag.expect(der::TAG_SEQUENCE, "SafeBag")?)?;
        if parts.len() < 2 {
            return Err(Error::new("SafeBag 字段不足"));
        }
        let bag_id = parts[0].expect(der::TAG_OID, "bagId")?;
        if parts[1].tag != 0xa0 {
            return Err(Error::new("SafeBag 的 bagValue 缺少 [0] 包装"));
        }
        let value_tlv = der::parse_exact(parts[1].content, "bagValue")?;
        let mut alias = String::new();
        let mut key_id = Vec::new();
        if parts.len() > 2 {
            for attr in der::parse_all(parts[2].expect(der::TAG_SET, "bagAttributes")?)? {
                let ap = der::parse_all(attr.expect(der::TAG_SEQUENCE, "Attribute")?)?;
                if ap.len() < 2 {
                    continue;
                }
                let at = ap[0].expect(der::TAG_OID, "属性类型")?;
                let vals = der::parse_all(ap[1].expect(der::TAG_SET, "属性值")?)?;
                let v = match vals.first() {
                    Some(v) => v,
                    None => continue,
                };
                if at == oid::PKCS9_FRIENDLY_NAME {
                    alias = der::parse_string(v.tag, v.content).unwrap_or_default();
                } else if at == oid::PKCS9_LOCAL_KEY_ID {
                    key_id = v.content.to_vec();
                }
            }
        }
        if bag_id == oid::PKCS12_CERT_BAG {
            let cb = der::parse_all(value_tlv.expect(der::TAG_SEQUENCE, "CertBag")?)?;
            if cb.len() < 2 {
                return Err(Error::new("CertBag 字段不足"));
            }
            let cert_type = cb[0].expect(der::TAG_OID, "certId")?;
            if cert_type != oid::PKCS9_X509_CERTIFICATE {
                return Err(Error::new("只支持 x509Certificate 类型的证书包"));
            }
            let inner = der::parse_exact(cb[1].content, "证书内容")?;
            let cert_der = inner.expect(der::TAG_OCTET_STRING, "证书内容")?;
            let cert = Certificate::parse(cert_der).map_err(|e| e.ctx("keystore 里的证书解析失败"))?;
            push_cert(out, alias, key_id, cert);
        } else if bag_id == oid::PKCS12_SHROUDED_KEY_BAG {
            let epki = der::parse_all(value_tlv.expect(der::TAG_SEQUENCE, "EncryptedPrivateKeyInfo")?)?;
            if epki.len() < 2 {
                return Err(Error::new("EncryptedPrivateKeyInfo 字段不足"));
            }
            let alg = AlgorithmIdentifier::parse(epki[0].expect(der::TAG_SEQUENCE, "加密算法")?)?;
            let ct = epki[1].expect(der::TAG_OCTET_STRING, "密文")?;
            let pki = pbes2_decrypt(password.as_bytes(), &alg, ct)
                .map_err(|e| e.ctx("解不开私钥包"))?;
            let key = RsaPrivateKey::from_pkcs8_der(&pki)?;
            let entry = entry_for(out, alias, key_id);
            entry.private_key = Some(key);
        } else if bag_id == oid::PKCS12_KEY_BAG {
            let pki = value_tlv;
            let key = RsaPrivateKey::from_pkcs8_der(&der::tlv(pki.tag, pki.content))?;
            let entry = entry_for(out, alias, key_id);
            entry.private_key = Some(key);
        } else {
            // 其它 bag 类型（crlBag/secretBag/safeContentsBag）忽略
        }
    }
    Ok(())
}

fn entry_for<'a>(p12: &'a mut P12, alias: String, key_id: Vec<u8>) -> &'a mut P12Entry {
    let idx = match p12.entries.iter().position(|e| {
        (!alias.is_empty() && e.alias == alias) || (!key_id.is_empty() && e.key_id == key_id)
    }) {
        Some(i) => i,
        None => {
            p12.entries.push(P12Entry {
                alias: if alias.is_empty() {
                    format!("entry-{}", p12.entries.len() + 1)
                } else {
                    alias
                },
                key_id,
                ..Default::default()
            });
            p12.entries.len() - 1
        }
    };
    &mut p12.entries[idx]
}

fn push_cert(p12: &mut P12, alias: String, key_id: Vec<u8>, cert: Certificate) {
    let entry = entry_for(p12, alias, key_id);
    if entry.chain.is_empty() {
        entry.chain.push(cert);
    } else if !entry.chain.iter().any(|c| c.der == cert.der) {
        entry.chain.push(cert);
    }
}

/// 便捷构造：单个 RSA 私钥 + 证书链。
pub fn build_single(alias: &str, key: RsaPrivateKey, chain: Vec<Certificate>) -> P12 {
    let key_id = chain
        .first()
        .map(|c| Alg::Sha1.digest(&c.der))
        .unwrap_or_default();
    P12 {
        entries: vec![P12Entry { alias: alias.to_string(), key_id, chain, private_key: Some(key) }],
    }
}

/// 供 `keytool -list` 用的指纹字符串。
pub fn fingerprint(cert: &Certificate, alg: Alg) -> String {
    let d = alg.digest(&cert.der);
    d.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// 公钥 -> base64（`keytool -list -v` 里的 Subject Public Key 展示）
pub fn spki_b64(cert: &Certificate) -> String {
    sys::base64_encode(&cert.spki)
}

/// 用于打印的 serial。
pub fn serial_hex(serial: &BigUint) -> String {
    let h = serial.to_hex();
    if h.len() % 2 == 0 {
        h
    } else {
        format!("0{h}")
    }
}

/// 让编译器确认 `RsaPublicKey` 在这条链路上可用（解析密钥时已用到）。
#[allow(dead_code)]
fn _public_of(cert: &Certificate) -> &RsaPublicKey {
    &cert.public_key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asn1::Name;
    use crate::rsa;
    use crate::x509::{build_self_signed, CertSpec};

    fn material() -> (RsaPrivateKey, Vec<u8>) {
        let key = rsa::generate(1024, 65537).unwrap();
        let dn = Name::parse_dn_string("CN=Keystore,O=Test,C=CN").unwrap();
        let cert = build_self_signed(&key, &CertSpec::new(dn, 365)).unwrap();
        (key, cert)
    }

    #[test]
    fn roundtrip() {
        let (key, cert_der) = material();
        let cert = Certificate::parse(&cert_der).unwrap();
        let p12 = build_single("mykey", key.clone(), vec![cert]);
        let bytes = p12.store("changeit").unwrap();
        let back = P12::load(&bytes, "changeit").unwrap();
        let e = back.find("mykey").expect("应有 mykey");
        assert_eq!(e.private_key.as_ref().unwrap().n, key.n);
        assert_eq!(e.leaf().unwrap().der, cert_der);
        assert_eq!(e.leaf().unwrap().subject.rfc2253(), "CN=Keystore, O=Test, C=CN");
    }

    #[test]
    fn wrong_password_is_reported_clearly() {
        let (key, cert_der) = material();
        let cert = Certificate::parse(&cert_der).unwrap();
        let bytes = build_single("k", key, vec![cert]).store("right").unwrap();
        let err = P12::load(&bytes, "wrong").unwrap_err().to_string();
        assert!(err.contains("口令错误或文件损坏"), "实际错误: {err}");
    }

    #[test]
    fn tampering_is_detected() {
        let (key, cert_der) = material();
        let cert = Certificate::parse(&cert_der).unwrap();
        let mut bytes = build_single("k", key, vec![cert]).store("pw").unwrap();
        let n = bytes.len();
        bytes[n / 2] ^= 0x01;
        assert!(P12::load(&bytes, "pw").is_err());
    }

    #[test]
    fn no_panic_on_garbage() {
        for seed in 0u32..300 {
            let mut v = Vec::new();
            let mut x = seed.wrapping_mul(2246822519).wrapping_add(11);
            for _ in 0..(seed % 80) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            let _ = P12::load(&v, "x");
        }
    }
}
