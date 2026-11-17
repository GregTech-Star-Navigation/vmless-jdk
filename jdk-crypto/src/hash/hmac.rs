//! HMAC（RFC 2104）、PBKDF2（RFC 8018）与 PKCS#12 KDF（RFC 7292 B.2）。

use super::Alg;

/// 常数时间比较：长度不等立即返回 false（长度本身就是公开信息），
/// 逐字节累积差异后再判定，避免提前退出泄露前缀信息。
#[inline(never)]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for i in 0..a.len() {
        acc |= a[i] ^ b[i];
    }
    core::hint::black_box(acc) == 0
}

pub fn hmac(alg: Alg, key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac_parts(alg, key, &[msg])
}

pub fn hmac_parts(alg: Alg, key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let bs = alg.block_len();
    let k = if key.len() > bs { alg.digest(key) } else { key.to_vec() };
    let mut ipad = vec![0x36u8; bs];
    let mut opad = vec![0x5cu8; bs];
    for i in 0..k.len() {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(bs + 64);
    inner.extend_from_slice(&ipad);
    for p in parts {
        inner.extend_from_slice(p);
    }
    let ih = alg.digest(&inner);
    let mut outer = Vec::with_capacity(bs + ih.len());
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&ih);
    alg.digest(&outer)
}

pub fn hmac_verify(alg: Alg, key: &[u8], msg: &[u8], mac: &[u8]) -> bool {
    let got = hmac(alg, key, msg);
    ct_eq(&got, mac)
}

/// PBKDF2（RFC 8018 §5.2）。`iterations` 必须 >= 1；`out_len` 无上限但受调用方约束。
pub fn pbkdf2(alg: Alg, password: &[u8], salt: &[u8], iterations: u32, out_len: usize) -> Vec<u8> {
    let hlen = alg.out_len();
    let blocks = out_len.div_ceil(hlen);
    let mut out = Vec::with_capacity(blocks * hlen);
    for i in 1..=blocks {
        let mut idx = Vec::with_capacity(4);
        idx.extend_from_slice(&(i as u32).to_be_bytes());
        let mut u = hmac_parts(alg, password, &[salt, &idx]);
        let mut t = u.clone();
        for _ in 1..iterations {
            u = hmac(alg, password, &u);
            for j in 0..t.len() {
                t[j] ^= u[j];
            }
        }
        out.extend_from_slice(&t);
    }
    out.truncate(out_len);
    out
}

/// PKCS#12 KDF（RFC 7292 附录 B.2）。
///
/// `id`：1=密钥，2=IV，3=MAC 密钥。`password` 是 UTF-8 口令，内部按 BMPString
/// （UTF-16BE + 双字节终止符）编码后再参与重复填充。
pub fn pkcs12_kdf(
    alg: Alg,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    id: u8,
    out_len: usize,
) -> Vec<u8> {
    let u = alg.out_len();
    let v = alg.block_len();
    let mut d = vec![id; v];

    let mut s = Vec::new();
    if !salt.is_empty() {
        let want = salt.len().div_ceil(v) * v;
        while s.len() < want {
            let take = (want - s.len()).min(salt.len());
            s.extend_from_slice(&salt[..take]);
        }
    }
    let pw = bmp_string(password);
    let mut p = Vec::new();
    if !pw.is_empty() {
        let want = pw.len().div_ceil(v) * v;
        while p.len() < want {
            let take = (want - p.len()).min(pw.len());
            p.extend_from_slice(&pw[..take]);
        }
    }
    let mut i_buf = Vec::with_capacity(s.len() + p.len());
    i_buf.extend_from_slice(&s);
    i_buf.extend_from_slice(&p);

    let mut out = Vec::with_capacity(out_len.div_ceil(u) * u);
    let blocks = out_len.div_ceil(u);
    for _ in 0..blocks {
        // (D || I)：D 是 ID 重复 v 字节，必须拼在 I 前面参与第一次哈希
        let mut input = Vec::with_capacity(d.len() + i_buf.len());
        input.extend_from_slice(&d);
        input.extend_from_slice(&i_buf);
        let mut a = alg.digest(&input);
        for _ in 1..iterations {
            a = alg.digest(&a);
        }
        out.extend_from_slice(&a);
        // I_j = (I_j + B + 1) mod 2^(v*8)
        let mut b = vec![0u8; v];
        for j in 0..v {
            b[j] = a[j % u];
        }
        let mut carry = 1u16;
        for blk in (0..i_buf.len()).step_by(v) {
            for j in (0..v).rev() {
                let sum = i_buf[blk + j] as u16 + b[j] as u16 + carry;
                i_buf[blk + j] = (sum & 0xff) as u8;
                carry = sum >> 8;
            }
        }
    }
    out.truncate(out_len);
    out
}

/// 口令 -> BMPString（UTF-16BE，含 2 字节 0 终止符）。
pub fn bmp_string(password: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(password);
    let mut out = Vec::with_capacity(s.chars().count() * 2 + 2);
    for ch in s.chars() {
        let mut buf = [0u16; 2];
        for unit in ch.encode_utf16(&mut buf) {
            out.extend_from_slice(&unit.to_be_bytes());
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2202_hmac_sha1() {
        // RFC 2202 测试向量 1 与 2
        assert_eq!(
            crate::hash::hex(&hmac(Alg::Sha1, &[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            crate::hash::hex(&hmac(Alg::Sha1, b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[test]
    fn rfc4231_hmac_sha256() {
        assert_eq!(
            crate::hash::hex(&hmac(Alg::Sha256, &[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            crate::hash::hex(&hmac(Alg::Sha256, b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 test 6（密钥 > block size）
        assert_eq!(
            crate::hash::hex(&hmac(
                Alg::Sha256,
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn rfc6070_pbkdf2_sha1() {
        assert_eq!(
            crate::hash::hex(&pbkdf2(Alg::Sha1, b"password", b"salt", 1, 20)),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            crate::hash::hex(&pbkdf2(Alg::Sha1, b"password", b"salt", 4096, 20)),
            "4b007901b765489abead49d926f721d065a429c1"
        );
    }

    #[test]
    fn pbkdf2_sha256_rfc7914() {
        // RFC 7914 §11 (PBKDF2-HMAC-SHA-256("passwd","salt",1,64))
        assert_eq!(
            crate::hash::hex(&pbkdf2(Alg::Sha256, b"passwd", b"salt", 1, 64)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
    }

    #[test]
    fn pkcs12_kdf_vectors() {
        // RFC 7292 B.3 的输入（口令 "smeg"，salt 0x0A..0xF9）；期望值由 `openssl kdf PKCS12KDF`（OpenSSL 3.5.9）实测得出
        // RFC 7292 B.3 的 salt（以十六进制写在源码里，避免手抄漏字节）
        let salt = crate::hash::unhex("0a58cf64530d823f1ba73fb4339150938e4b1a86695746b192107f4f2b464e264c1f3e0a2f5a28e4c86ef554ed803f8ac8ca873cdc0a0b1add8e5335aaf0").expect("salt 十六进制");
        let salt = &salt[..];
        assert_eq!(
            crate::hash::hex(&pkcs12_kdf(Alg::Sha1, b"smeg", &salt, 1, 1, 20)),
            "d94fcf9187acfd86a27e8cb08264f98b7555c2d7"
        );
        assert_eq!(
            crate::hash::hex(&pkcs12_kdf(Alg::Sha1, b"smeg", &salt, 1, 2, 20)),
            "37b9334564c98bfe65c6d2b2f584ec47144fd8b5"
        );
        assert_eq!(
            crate::hash::hex(&pkcs12_kdf(Alg::Sha1, b"smeg", &salt, 1, 3, 20)),
            "76991ce9a41292daa5f94a862949e9b0cb00d64e"
        );
    }
}
