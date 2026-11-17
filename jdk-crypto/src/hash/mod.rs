//! 摘要与 HMAC / KDF。
//!
//! | 算法 | 输出 | 用途 |
//! | --- | --- | --- |
//! | MD5 | 16 | jar 的 `MD5-Digest`（兼容） |
//! | SHA-1 | 20 | 旧 PKCS#12 MAC、SHA1withRSA 验签 |
//! | SHA-256 | 32 | 默认：签名、jar 摘要、PBKDF2、PKCS#12 MAC |
//! | SHA-384 | 48 | keytool 默认的证书签名算法 |
//! | SHA-512 | 64 | 可选 |

pub mod hmac;
pub mod md5;
pub mod sha1;
pub mod sha256;
pub mod sha512;

pub use hmac::{ct_eq, hmac, hmac_verify, pbkdf2, pkcs12_kdf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alg {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Alg {
    pub fn digest(self, d: &[u8]) -> Vec<u8> {
        match self {
            Alg::Md5 => md5::digest(d).to_vec(),
            Alg::Sha1 => sha1::digest(d).to_vec(),
            Alg::Sha256 => sha256::digest(d).to_vec(),
            Alg::Sha384 => sha512::digest384(d).to_vec(),
            Alg::Sha512 => sha512::digest(d).to_vec(),
        }
    }

    pub fn out_len(self) -> usize {
        match self {
            Alg::Md5 => 16,
            Alg::Sha1 => 20,
            Alg::Sha256 => 32,
            Alg::Sha384 => 48,
            Alg::Sha512 => 64,
        }
    }

    pub fn block_len(self) -> usize {
        match self {
            Alg::Md5 | Alg::Sha1 | Alg::Sha256 => 64,
            Alg::Sha384 | Alg::Sha512 => 128,
        }
    }

    /// JCA 名字（jar 摘要改属性名时要用：`SHA-256-Digest`）。
    pub fn jca_name(self) -> &'static str {
        match self {
            Alg::Md5 => "MD5",
            Alg::Sha1 => "SHA-1",
            Alg::Sha256 => "SHA-256",
            Alg::Sha384 => "SHA-384",
            Alg::Sha512 => "SHA-512",
        }
    }

    /// 摘要算法 OID 的内容字节（不含 tag/len）。
    pub fn oid(self) -> &'static [u8] {
        match self {
            Alg::Md5 => &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05],
            Alg::Sha1 => &[0x2b, 0x0e, 0x03, 0x02, 0x1a],
            Alg::Sha256 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01],
            Alg::Sha384 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02],
            Alg::Sha512 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03],
        }
    }

    pub fn from_jca_name(s: &str) -> Option<Alg> {
        let norm: String = s.trim().to_ascii_uppercase().replace('_', "-");
        match norm.as_str() {
            "MD5" => Some(Alg::Md5),
            "SHA1" | "SHA-1" => Some(Alg::Sha1),
            "SHA256" | "SHA-256" | "SHA2" => Some(Alg::Sha256),
            "SHA384" | "SHA-384" => Some(Alg::Sha384),
            "SHA512" | "SHA-512" => Some(Alg::Sha512),
            _ => None,
        }
    }

    pub fn from_oid(oid: &[u8]) -> Option<Alg> {
        [Alg::Md5, Alg::Sha1, Alg::Sha256, Alg::Sha384, Alg::Sha512]
            .into_iter()
            .find(|a| a.oid() == oid)
    }
}

pub fn hex(d: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 0xf) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.len() % 2 != 0 {
        return Err("十六进制串长度为奇数".into());
    }
    let val = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("非法十六进制字符: {}", c as char)),
        }
    };
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.chunks(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_kat() {
        assert_eq!(
            hex(&Alg::Sha256.digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&Alg::Sha256.digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Alg::Sha256.digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut s = sha256::Sha256::new();
        for chunk in data.chunks(97) {
            s.update(chunk);
        }
        assert_eq!(s.finalize().to_vec(), sha256::digest(&data).to_vec());
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128] {
            let d: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(sha256::digest(&d).to_vec(), Alg::Sha256.digest(&d), "len {len}");
        }
    }

    #[test]
    fn sha1_kat() {
        assert_eq!(hex(&Alg::Sha1.digest(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&Alg::Sha1.digest(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(&Alg::Sha1.digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&Alg::Sha1.digest(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha512_kat() {
        assert_eq!(
            hex(&Alg::Sha512.digest(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            hex(&Alg::Sha384.digest(b"abc")),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            hex(&Alg::Sha512.digest(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            )),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018\
             501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );
    }

    #[test]
    fn md5_kat() {
        assert_eq!(hex(&Alg::Md5.digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&Alg::Md5.digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&Alg::Md5.digest(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn ct_eq_behaviour() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
