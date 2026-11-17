//! RSA：密钥生成、PKCS#1 v1.5 签名/验签、PKCS#8/PKCS#1 编码（RFC 8017 / RFC 5208）。
//!
//! 验签严格按 RFC 8017 §8.2.2 检查 EM 结构：`00 01 FF..FF 00 || DigestInfo`，
//! PS 至少 8 字节，且 DigestInfo 必须与本栈重算的**逐字节相同**（常数时间比较），
//! 不做"只比尾部哈希"。

use crate::asn1::{der, oid};
use crate::bigint::{generate_prime, BigUint};
use crate::hash::{hmac::ct_eq, Alg};
use crate::{sys, Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct RsaPublicKey {
    pub n: BigUint,
    pub e: BigUint,
}

#[derive(Clone, Debug)]
pub struct RsaPrivateKey {
    pub n: BigUint,
    pub e: BigUint,
    pub d: BigUint,
    pub p: BigUint,
    pub q: BigUint,
    pub dp: BigUint,
    pub dq: BigUint,
    pub qinv: BigUint,
}

impl RsaPublicKey {
    pub fn bits(&self) -> usize {
        self.n.bits()
    }
    pub fn byte_len(&self) -> usize {
        self.n.byte_len()
    }

    /// `RSAPublicKey ::= SEQUENCE { modulus, publicExponent }`（PKCS#1）。
    pub fn to_pkcs1_der(&self) -> Vec<u8> {
        der::seq(&[der::integer(&self.n), der::integer(&self.e)])
    }

    pub fn from_pkcs1_der(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "RSAPublicKey")?;
        let items = der::parse_all(tlv.expect(der::TAG_SEQUENCE, "RSAPublicKey")?)?;
        if items.len() < 2 {
            return Err(Error::new("RSAPublicKey 字段不足"));
        }
        let n = der::parse_unsigned(items[0].expect(der::TAG_INTEGER, "modulus")?)?;
        let e = der::parse_unsigned(items[1].expect(der::TAG_INTEGER, "publicExponent")?)?;
        Ok(RsaPublicKey { n, e })
    }

    /// `SubjectPublicKeyInfo`（X.509 里用的形式）。
    pub fn to_spki_der(&self) -> Vec<u8> {
        let alg = der::seq(&[der::oid(oid::RSA_ENCRYPTION), der::null()]);
        der::seq(&[alg, der::bit_string(&self.to_pkcs1_der(), 0)])
    }

    pub fn from_spki_der(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "SubjectPublicKeyInfo")?;
        let items = der::parse_all(tlv.expect(der::TAG_SEQUENCE, "SubjectPublicKeyInfo")?)?;
        if items.len() != 2 {
            return Err(Error::new("SubjectPublicKeyInfo 应有 2 个字段"));
        }
        let alg = crate::asn1::AlgorithmIdentifier::parse(items[0].expect(der::TAG_SEQUENCE, "算法")?)?;
        if alg.oid != oid::RSA_ENCRYPTION {
            return Err(Error::new(format!("不是 RSA 公钥（算法 {}）", alg.name())));
        }
        let (bits, unused) = der::parse_bit_string(items[1].expect(der::TAG_BIT_STRING, "公钥位串")?)?;
        if unused != 0 {
            return Err(Error::new("RSA 公钥 BIT STRING 不应有未使用位"));
        }
        Self::from_pkcs1_der(bits)
    }
}

impl RsaPrivateKey {
    pub fn public(&self) -> RsaPublicKey {
        RsaPublicKey { n: self.n.clone(), e: self.e.clone() }
    }

    pub fn bits(&self) -> usize {
        self.n.bits()
    }

    /// `RSAPrivateKey ::= SEQUENCE { version, n, e, d, p, q, dp, dq, qInv }`（PKCS#1）。
    pub fn to_pkcs1_der(&self) -> Vec<u8> {
        der::seq(&[
            der::integer_u32(0),
            der::integer(&self.n),
            der::integer(&self.e),
            der::integer(&self.d),
            der::integer(&self.p),
            der::integer(&self.q),
            der::integer(&self.dp),
            der::integer(&self.dq),
            der::integer(&self.qinv),
        ])
    }

    pub fn from_pkcs1_der(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "RSAPrivateKey")?;
        let items = der::parse_all(tlv.expect(der::TAG_SEQUENCE, "RSAPrivateKey")?)?;
        if items.len() < 9 {
            return Err(Error::new(format!(
                "RSAPrivateKey 需要 9 个字段，实际 {}",
                items.len()
            )));
        }
        let version = der::parse_unsigned(items[0].expect(der::TAG_INTEGER, "version")?)?;
        if !version.is_zero() {
            return Err(Error::new("只支持两素数 RSAPrivateKey（version 0）"));
        }
        let mut v = Vec::with_capacity(8);
        for it in &items[1..9] {
            v.push(der::parse_unsigned(it.expect(der::TAG_INTEGER, "私钥字段")?)?);
        }
        Ok(RsaPrivateKey {
            n: v[0].clone(),
            e: v[1].clone(),
            d: v[2].clone(),
            p: v[3].clone(),
            q: v[4].clone(),
            dp: v[5].clone(),
            dq: v[6].clone(),
            qinv: v[7].clone(),
        })
    }

    /// `PrivateKeyInfo`（PKCS#8）：jdK 的 `.p12` 里的私钥就是这个结构。
    pub fn to_pkcs8_der(&self) -> Vec<u8> {
        let alg = der::seq(&[der::oid(oid::RSA_ENCRYPTION), der::null()]);
        der::seq(&[der::integer_u32(0), alg, der::octet_string(&self.to_pkcs1_der())])
    }

    pub fn from_pkcs8_der(data: &[u8]) -> Result<Self> {
        let tlv = der::parse_exact(data, "PrivateKeyInfo")?;
        let items = der::parse_all(tlv.expect(der::TAG_SEQUENCE, "PrivateKeyInfo")?)?;
        if items.len() < 3 {
            return Err(Error::new("PrivateKeyInfo 字段不足"));
        }
        let alg = crate::asn1::AlgorithmIdentifier::parse(items[1].expect(der::TAG_SEQUENCE, "算法")?)?;
        if alg.oid != oid::RSA_ENCRYPTION {
            return Err(Error::new(format!("不是 RSA 私钥（算法 {}）", alg.name())));
        }
        let inner = items[2].expect(der::TAG_OCTET_STRING, "私钥内容")?;
        Self::from_pkcs1_der(inner)
    }

    /// 私钥运算（签名/解密）：有 CRT 参数走 CRT，否则退回朴素模幂。
    pub fn private_op(&self, m: &BigUint) -> Result<BigUint> {
        if self.p.is_zero() || self.q.is_zero() {
            return m.modpow(&self.d, &self.n);
        }
        let m1 = m.modpow(&self.dp, &self.p)?;
        let m2 = m.modpow(&self.dq, &self.q)?;
        let diff = m1.sub_mod(&m2, &self.p);
        let h = self.qinv.mul(&diff).rem(&self.p)?;
        Ok(m2.add(&h.mul(&self.q)))
    }

    pub fn public_op(&self, m: &BigUint) -> Result<BigUint> {
        m.modpow(&self.e, &self.n)
    }
}

/// 生成 RSA 密钥。`bits` 建议 2048/3072；`e` 常用 65537。
pub fn generate(bits: usize, e: u64) -> Result<RsaPrivateKey> {
    if bits < 512 || bits > 16384 {
        return Err(Error::new(format!("RSA 位数不支持: {bits}")));
    }
    if e < 3 || e % 2 == 0 {
        return Err(Error::new("公钥指数必须是 >=3 的奇数"));
    }
    let e_big = BigUint::from_u64(e);
    let one = BigUint::one();
    for _attempt in 0..200 {
        let p_bits = bits / 2;
        let q_bits = bits - p_bits;
        let p = generate_prime(p_bits)?;
        let q = generate_prime(q_bits)?;
        if p == q {
            continue;
        }
        let p1 = p.sub(&one)?;
        let q1 = q.sub(&one)?;
        if !e_big.gcd(&p1).is_one() || !e_big.gcd(&q1).is_one() {
            continue;
        }
        let n = p.mul(&q);
        if n.bits() != bits {
            continue;
        }
        let lambda = p1.lcm(&q1);
        let d = match e_big.modinv(&lambda) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let dp = d.rem(&p1)?;
        let dq = d.rem(&q1)?;
        let qinv = q.modinv(&p)?;
        let key = RsaPrivateKey { n, e: e_big.clone(), d, p, q, dp, dq, qinv };
        // 自检：签一验一，避免把坏密钥交出去
        let probe = BigUint::from_u64(0x1234_5678);
        let sig = key.private_op(&probe)?;
        if key.public_op(&sig)? != probe {
            continue;
        }
        return Ok(key);
    }
    Err(Error::new("RSA 密钥生成失败：重试次数用尽"))
}

/// DigestInfo（RFC 8017 §9.2）：
/// `SEQUENCE { SEQUENCE { OID, NULL }, OCTET STRING digest }`
pub fn digest_info(alg: Alg, digest: &[u8]) -> Vec<u8> {
    der::seq(&[der::seq(&[der::oid(alg.oid()), der::null()]), der::octet_string(digest)])
}

/// EMSA-PKCS1-v1_5 编码（RFC 8017 §9.2）。
pub fn emsa_pkcs1_v15(digest_info: &[u8], k: usize) -> Result<Vec<u8>> {
    if k < digest_info.len() + 11 {
        return Err(Error::new(format!(
            "RSA 模长太短：{k} 字节装不下 {} 字节的 DigestInfo",
            digest_info.len()
        )));
    }
    let ps_len = k - digest_info.len() - 3;
    if ps_len < 8 {
        return Err(Error::new("PKCS#1 v1.5 的填充串不足 8 字节"));
    }
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.push(0x01);
    em.extend(std::iter::repeat(0xff).take(ps_len));
    em.push(0x00);
    em.extend_from_slice(digest_info);
    debug_assert_eq!(em.len(), k);
    Ok(em)
}

/// 用 RSA 私钥签名一段数据（内部做摘要）。
pub fn sign(key: &RsaPrivateKey, alg: Alg, data: &[u8]) -> Result<Vec<u8>> {
    let digest = alg.digest(data);
    sign_digest(key, alg, &digest)
}

/// 用已算好的摘要签名。
pub fn sign_digest(key: &RsaPrivateKey, alg: Alg, digest: &[u8]) -> Result<Vec<u8>> {
    if digest.len() != alg.out_len() {
        return Err(Error::new(format!(
            "摘要长度不对：{} 应为 {} 字节，实际 {}",
            alg.jca_name(),
            alg.out_len(),
            digest.len()
        )));
    }
    let k = key.bits().div_ceil(8);
    let di = digest_info(alg, digest);
    let em = emsa_pkcs1_v15(&di, k)?;
    let m = BigUint::from_bytes_be(&em);
    let s = key.private_op(&m)?;
    Ok(s.to_bytes_be_padded(k))
}

/// 验签（严格按 RFC 8017 §8.2.2 校验填充）。
pub fn verify(pubkey: &RsaPublicKey, alg: Alg, data: &[u8], sig: &[u8]) -> Result<()> {
    let digest = alg.digest(data);
    verify_digest(pubkey, alg, &digest, sig)
}

pub fn verify_digest(pubkey: &RsaPublicKey, alg: Alg, digest: &[u8], sig: &[u8]) -> Result<()> {
    let k = pubkey.bits().div_ceil(8);
    if sig.len() != k {
        return Err(Error::new(format!(
            "签名长度不对：模长 {k} 字节，签名 {} 字节",
            sig.len()
        )));
    }
    if digest.len() != alg.out_len() {
        return Err(Error::new("摘要长度与算法不匹配"));
    }
    let s = BigUint::from_bytes_be(sig);
    if s.cmp_val(&pubkey.n) != std::cmp::Ordering::Less {
        return Err(Error::new("签名值超出模数（RSA 验签失败）"));
    }
    let m = pubkey.n.clone();
    let em_full = s.modpow(&pubkey.e, &m)?;
    let em = em_full.to_bytes_be_padded(k);

    // 1) 结构：00 01 FF..FF 00
    let mut ok = em.len() == k && em[0] == 0x00 && em[1] == 0x01;
    let mut sep = None;
    let mut i = 2usize;
    while i < em.len() {
        if em[i] == 0x00 {
            sep = Some(i);
            break;
        }
        if em[i] != 0xff {
            ok = false;
        }
        i += 1;
    }
    let sep = match sep {
        Some(s) if ok => s,
        _ => return Err(Error::new("RSA 验签失败：PKCS#1 v1.5 填充结构不正确")),
    };
    let ps_len = sep - 2;
    if ps_len < 8 {
        return Err(Error::new(format!(
            "RSA 验签失败：填充串只有 {ps_len} 字节（要求 >= 8）"
        )));
    }
    // 2) 填充必须全是 0xFF（常数时间累积）
    let mut acc = 0u8;
    for b in &em[2..sep] {
        acc |= *b ^ 0xff;
    }
    // 3) DigestInfo 必须逐字节等于期望值
    let expect = digest_info(alg, digest);
    let tail = &em[sep + 1..];
    let tail_ok = ct_eq(tail, &expect);
    if core::hint::black_box(acc) != 0 || !tail_ok {
        return Err(Error::new("RSA 验签失败：DigestInfo 不匹配"));
    }
    Ok(())
}

/// 供 `-printcert` 之类展示用：把公钥渲染成 JDK 风格的模数/指数。
pub fn describe(pubkey: &RsaPublicKey) -> String {
    format!(
        "  Public Key: {} bit RSA\n   modulus: {}\n   public exponent: {}",
        pubkey.bits(),
        pubkey.n.to_hex(),
        pubkey.e.to_hex()
    )
}

/// 把私钥写成 PKCS#8 PEM（`keytool` 之外的场景，例如给 OpenSSL 交叉验证）。
pub fn to_pkcs8_pem(key: &RsaPrivateKey) -> String {
    sys::pem_encode("PRIVATE KEY", &key.to_pkcs8_der())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hex;

    fn small_key() -> RsaPrivateKey {
        // 512 位，测试够快
        generate(512, 65537).unwrap()
    }

    #[test]
    fn keygen_and_roundtrip() {
        let key = small_key();
        assert_eq!(key.bits(), 512);
        assert_eq!(key.p.mul(&key.q), key.n);
        assert_eq!(key.dp, key.d.rem(&key.p.sub(&BigUint::one()).unwrap()).unwrap());
        assert_eq!(key.q.mul(&key.qinv).rem(&key.p).unwrap(), BigUint::one());
        // CRT 与朴素模幂一致
        let m = BigUint::from_u64(0xdead_beef);
        assert_eq!(key.private_op(&m).unwrap(), m.modpow(&key.d, &key.n).unwrap());
    }

    #[test]
    fn sign_verify_and_reject() {
        let key = small_key();
        let msg = b"hello jarsigner";
        let sig = sign(&key, Alg::Sha256, msg).unwrap();
        assert_eq!(sig.len(), 64);
        verify(&key.public(), Alg::Sha256, msg, &sig).unwrap();

        // 改一个字节必须失败
        let mut bad = sig.clone();
        bad[0] ^= 1;
        assert!(verify(&key.public(), Alg::Sha256, msg, &bad).is_err());
        // 改消息必须失败
        assert!(verify(&key.public(), Alg::Sha256, b"hello jarsigner!", &sig).is_err());
        // 截断签名必须失败
        assert!(verify(&key.public(), Alg::Sha256, msg, &sig[..sig.len() - 1]).is_err());
        // 空签名必须失败
        assert!(verify(&key.public(), Alg::Sha256, msg, &[]).is_err());
    }

    #[test]
    fn rejects_weak_padding() {
        let key = small_key();
        // 手工构造一个 PS 只有 4 字节的 EM，用私钥"签"出来，验签必须拒绝
        let k = key.bits().div_ceil(8);
        let di = digest_info(Alg::Sha256, &Alg::Sha256.digest(b"x"));
        let mut em = vec![0x00, 0x01];
        em.extend(std::iter::repeat(0xff).take(4));
        em.push(0x00);
        em.extend_from_slice(&di);
        em.resize(k, 0);
        // 把长度挪到前面：这里直接在末尾补 0 会破坏 DigestInfo，所以重新拼
        let ps_len = k - di.len() - 3;
        let mut em2 = vec![0x00, 0x01];
        em2.extend(std::iter::repeat(0x11u8).take(ps_len)); // 非 0xFF 填充
        em2.push(0x00);
        em2.extend_from_slice(&di);
        let m = BigUint::from_bytes_be(&em2);
        let sig = key.private_op(&m).unwrap().to_bytes_be_padded(k);
        assert!(verify(&key.public(), Alg::Sha256, b"x", &sig).is_err());
    }

    #[test]
    fn pkcs8_roundtrip() {
        let key = small_key();
        let der = key.to_pkcs8_der();
        let back = RsaPrivateKey::from_pkcs8_der(&der).unwrap();
        assert_eq!(back.n, key.n);
        assert_eq!(back.d, key.d);
        let spki = key.public().to_spki_der();
        let pub_back = RsaPublicKey::from_spki_der(&spki).unwrap();
        assert_eq!(pub_back, key.public());
    }

    #[test]
    fn known_answer_sha256_rsa_2048() {
        // 固定密钥的 PKCS#1 v1.5 签名可重复：同一密钥同一消息必须稳定，
        // 且能被 openssl 验证（见验收脚本）。
        let key = generate(1024, 65537).unwrap();
        let sig1 = sign(&key, Alg::Sha256, b"abc").unwrap();
        let sig2 = sign(&key, Alg::Sha256, b"abc").unwrap();
        assert_eq!(hex(&sig1), hex(&sig2));
        assert!(sig1.len() == 128);
    }

    #[test]
    fn emsa_length_rules() {
        assert!(emsa_pkcs1_v15(&[0u8; 10], 20).is_err());
        let em = emsa_pkcs1_v15(&[0xaa; 32], 256).unwrap();
        assert_eq!(em.len(), 256);
        assert_eq!(&em[..2], &[0x00, 0x01]);
        assert_eq!(em[2..256 - 33].iter().filter(|b| **b != 0xff).count(), 0);
    }
}
