//! Kerberos 专用加密：`aes256-cts-hmac-sha1-96`（etype 18）与
//! `aes128-cts-hmac-sha1-96`（etype 17）。
//!
//! 规范来源：RFC 3961（n-fold / DR / DK / 简化 profile）与 RFC 3962（AES-CTS、
//! string-to-key）。三件事在这里一次做对：
//!
//! 1. **密钥派生** —— `Ke = DK(base, usage|0xAA)`、`Ki = DK(base, usage|0x55)`、
//!    `Kc = DK(base, usage|0x99)`，`DK = random2key(DR)`，AES 的 random2key 是恒等。
//!    `DR(Key, Constant)` 先把 Constant n-fold 到 16 字节，再反复 `E(Key, 上一块输出)`
//!    直到凑够 k 字节。
//! 2. **加密** —— `CTS(confounder|plaintext)` 后接 `HMAC-SHA1(Ki, confounder|plaintext)`
//!    的前 12 字节。注意 HMAC 覆盖的是**未填充**的明文（填充是 CTS 内部的事），
//!    这一点与 JDK `AesDkCrypto.encryptCTS` 一致。
//! 3. **CTS 变体** —— RFC 3962 §5 明确规定「总是交换最后两个密文块」，即 CS3。
//!    长度是 16 的整数倍时**也**交换（等价于 CBC 交换末两块）。这一条最容易写错，
//!    测试里用 JDK 的真实输出钉死了 len=16/32/48/64 的情形。
//!
//! 所有向量都来自两处独立来源：RFC 3962 附录 B 与 JDK 25 的
//! `sun.security.krb5.internal.crypto.Aes256/Aes128`（见文件末尾测试）。

use vmless_crypto::cipher::Aes;
use vmless_crypto::hash::{self, Alg};
use vmless_crypto::{sys, Error, Result};

/// aes128-cts-hmac-sha1-96
pub const ETYPE_AES128: i32 = 17;
/// aes256-cts-hmac-sha1-96
pub const ETYPE_AES256: i32 = 18;
/// arcfour-hmac（未实现，遇到要给出明确错误而不是猜）
pub const ETYPE_RC4: i32 = 23;

/// AES 块大小（字节）。
pub const BLOCK: usize = 16;
/// confounder 长度（RFC 3961 §5.3：等于块大小）。
pub const CONFOUNDER: usize = 16;
/// HMAC 截断长度（96 位）。
pub const HMAC_LEN: usize = 12;

pub fn etype_name(e: i32) -> &'static str {
    match e {
        ETYPE_AES128 => "aes128-cts-hmac-sha1-96",
        ETYPE_AES256 => "aes256-cts-hmac-sha1-96",
        ETYPE_RC4 => "arcfour-hmac",
        _ => "unknown",
    }
}

/// 名字 → etype。接受 IANA 名与数字。
pub fn etype_from_name(s: &str) -> Option<i32> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "aes256-cts-hmac-sha1-96" | "aes256-cts" | "aes256" | "18" => Some(ETYPE_AES256),
        "aes128-cts-hmac-sha1-96" | "aes128-cts" | "aes128" | "17" => Some(ETYPE_AES128),
        "arcfour-hmac" | "rc4-hmac" | "23" => Some(ETYPE_RC4),
        _ => None,
    }
}

/// 该 etype 的密钥长度；不支持的 etype 返回 `None`。
pub fn key_len(etype: i32) -> Option<usize> {
    match etype {
        ETYPE_AES128 => Some(16),
        ETYPE_AES256 => Some(32),
        _ => None,
    }
}

/// 我们真正实现了的 etype（用于在 AS-REQ 的 etype 列表里挑）。
pub fn is_supported(etype: i32) -> bool {
    key_len(etype).is_some()
}

fn need_etype(etype: i32) -> Result<usize> {
    key_len(etype).ok_or_else(|| {
        Error::new(format!(
            "不支持的加密类型 {etype}（{}）：本实现只支持 aes256-cts-hmac-sha1-96(18) 与 aes128-cts-hmac-sha1-96(17)",
            etype_name(etype)
        ))
    })
}

// ------------------------------------------------------------------ n-fold

/// RFC 3961 §5.1 的 n-fold：把任意长度输入"摊"成 `out_bytes` 字节，
/// 每个输入位对输出的贡献尽量均匀（1 补数加法 + 端回进位）。
///
/// 这是逐行照 RFC 里那段 C 伪码搬过来的，`i / inbits` 与位序都不能动 ——
/// 差一位整个 Kerberos 加密就全错。
pub fn nfold(input: &[u8], out_bytes: usize) -> Vec<u8> {
    let inbits = input.len();
    let outbits = out_bytes;
    if inbits == 0 || outbits == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; outbits];

    // lcm(inbits, outbits)
    let (mut a, mut b) = (outbits, inbits);
    while b != 0 {
        let c = b;
        b = a % b;
        a = c;
    }
    let lcm = outbits * inbits / a;

    let inbits8 = inbits * 8;
    let mut carry: usize = 0;
    for i in (0..lcm).rev() {
        // 本次要加进去的输入位（msbit）在输入里的位置
        let msbit = (inbits8 - 1 + (inbits8 + 13) * (i / inbits) + (inbits - (i % inbits)) * 8) % inbits8;
        let hi = input[(inbits - 1 - msbit / 8) % inbits] as usize;
        let lo = input[(inbits - msbit / 8) % inbits] as usize;
        let byte = (((hi << 8) | lo) >> ((msbit & 7) + 1)) & 0xff;
        carry += byte;
        carry += out[i % outbits] as usize;
        out[i % outbits] = (carry & 0xff) as u8;
        carry >>= 8;
    }
    // 端回进位
    if carry != 0 {
        for i in (0..outbits).rev() {
            carry += out[i] as usize;
            out[i] = (carry & 0xff) as u8;
            carry >>= 8;
        }
    }
    out
}

// -------------------------------------------------------------- DR / DK

/// `DR(Key, Constant)` = 取前 `out_len` 字节的 `K1|K2|...`，
/// `K1 = E(Key, n-fold(Constant))`，`K2 = E(Key, K1)` …
pub fn dr(key: &[u8], constant: &[u8], out_len: usize) -> Result<Vec<u8>> {
    if key.is_empty() {
        return Err(Error::new("DR: 基密钥为空"));
    }
    if out_len == 0 {
        return Err(Error::new("DR: 输出长度为 0"));
    }
    let aes = Aes::new(key)?;
    let mut cur = nfold(constant, BLOCK);
    if cur.len() != BLOCK {
        return Err(Error::new("DR: n-fold 失败"));
    }
    let mut out = Vec::with_capacity(out_len + BLOCK);
    while out.len() < out_len {
        let mut blk = [0u8; BLOCK];
        blk.copy_from_slice(&cur);
        let enc = aes.encrypt_block(&blk);
        out.extend_from_slice(&enc);
        cur = enc.to_vec();
    }
    out.truncate(out_len);
    Ok(out)
}

/// `DK(Key, Constant)`。AES 的 random-to-key 是恒等函数，所以 DK 与 DR 同形。
pub fn dk(key: &[u8], constant: &[u8], out_len: usize) -> Result<Vec<u8>> {
    dr(key, constant, out_len)
}

/// 一个 usage 号对应的三把派生密钥。
#[derive(Clone, Debug)]
pub struct DerivedKeys {
    pub ke: Vec<u8>,
    pub ki: Vec<u8>,
    pub kc: Vec<u8>,
}

/// 按 RFC 3961 §5.3 派生 `Ke/Ki/Kc`：常量是 `usage(4 字节大端) || 0xAA/0x55/0x99`。
pub fn derive(etype: i32, base: &[u8], usage: u32) -> Result<DerivedKeys> {
    let klen = need_etype(etype)?;
    if base.len() != klen {
        return Err(Error::new(format!(
            "{} 的密钥应为 {klen} 字节，实际 {} 字节",
            etype_name(etype),
            base.len()
        )));
    }
    let mut constant = [0u8; 5];
    constant[..4].copy_from_slice(&usage.to_be_bytes());
    let mut mk = |pad: u8| -> Result<Vec<u8>> {
        constant[4] = pad;
        dk(base, &constant, klen)
    };
    Ok(DerivedKeys { ke: mk(0xAA)?, ki: mk(0x55)?, kc: mk(0x99)? })
}

// ---------------------------------------------------------- string2key

/// 默认迭代次数（RFC 3962 §4：s2kparams 缺席时是 `00 00 10 00` = 4096）。
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// RFC 3962 §4：`tkey = PBKDF2(UTF-8(password), salt, iters, klen)`，
/// 再 `key = DK(tkey, "kerberos")`。
pub fn string2key(password: &str, salt: &[u8], etype: i32, iterations: u32) -> Result<Vec<u8>> {
    let klen = need_etype(etype)?;
    if iterations == 0 {
        return Err(Error::new("string2key: 迭代次数不能为 0"));
    }
    let tkey = hash::pbkdf2(Alg::Sha1, password.as_bytes(), salt, iterations, klen);
    dk(&tkey, b"kerberos", klen)
}

/// 按「realm || 各组件」拼默认 salt（RFC 4120 §3.1.3，MIT/JDK 都用这个）。
pub fn default_salt(realm: &str, components: &[String]) -> Vec<u8> {
    let mut s = realm.as_bytes().to_vec();
    for c in components {
        s.extend_from_slice(c.as_bytes());
    }
    s
}

// ------------------------------------------------------------- AES-CTS

/// RFC 3962 §5 的 ciphertext stealing（CS3）。
///
/// 与 JDK `com.sun.crypto.provider.CipherTextStealing` 逐行对齐：
/// - 前 `n-2` 块走普通 CBC；
/// - `C_{n-1} = E(P_{n-1} ⊕ C_{n-2})` 完整算出，但只输出前 `nleft` 字节；
/// - `C_n = E((P_n ⊕ C_{n-1})[..nleft] || C_{n-1}[nleft..])`；
/// - 输出 `C_1..C_{n-2} || C_n || C_{n-1}[..nleft]`。
///
/// 长度正好是 16 的整数倍时（`nleft == 16`）上式退化成「普通 CBC + 交换末两块」，
/// 与 RFC 3962 §5 那句「总是交换最后两个块」一致。
pub fn cts_encrypt(key: &[u8], iv: &[u8; BLOCK], plain: &[u8]) -> Result<Vec<u8>> {
    if plain.is_empty() {
        return Ok(Vec::new());
    }
    let aes = Aes::new(key)?;
    let l = plain.len();
    if l <= BLOCK {
        // 只有一个块：等价于 ECB（RFC 3962 §5 明确说了）；不足一块时补零
        let mut blk = [0u8; BLOCK];
        for j in 0..l {
            blk[j] = plain[j] ^ iv[j];
        }
        let mut out = aes.encrypt_block(&blk).to_vec();
        out.truncate(l);
        return Ok(out);
    }
    let n = l.div_ceil(BLOCK);
    let nleft = l - (n - 1) * BLOCK; // 1..=16
    let mut out = Vec::with_capacity(l);
    let mut prev = *iv;
    for i in 0..n - 2 {
        let mut blk = [0u8; BLOCK];
        for j in 0..BLOCK {
            blk[j] = plain[i * BLOCK + j] ^ prev[j];
        }
        let enc = aes.encrypt_block(&blk);
        out.extend_from_slice(&enc);
        prev = enc;
    }
    let mut blk = [0u8; BLOCK];
    for j in 0..BLOCK {
        blk[j] = plain[(n - 2) * BLOCK + j] ^ prev[j];
    }
    let cn1 = aes.encrypt_block(&blk);
    let mut t = [0u8; BLOCK];
    for j in 0..nleft {
        t[j] = plain[(n - 1) * BLOCK + j] ^ cn1[j];
    }
    t[nleft..].copy_from_slice(&cn1[nleft..]);
    let cn = aes.encrypt_block(&t);
    out.extend_from_slice(&cn);
    out.extend_from_slice(&cn1[..nleft]);
    debug_assert_eq!(out.len(), l);
    Ok(out)
}

/// [`cts_encrypt`] 的逆运算。
pub fn cts_decrypt(key: &[u8], iv: &[u8; BLOCK], ct: &[u8]) -> Result<Vec<u8>> {
    if ct.is_empty() {
        return Ok(Vec::new());
    }
    let aes = Aes::new(key)?;
    let l = ct.len();
    if l <= BLOCK {
        let mut blk = [0u8; BLOCK];
        blk[..l].copy_from_slice(ct);
        let dec = aes.decrypt_block(&blk);
        let mut out = Vec::with_capacity(l);
        for j in 0..l {
            out.push(dec[j] ^ iv[j]);
        }
        return Ok(out);
    }
    let n = l.div_ceil(BLOCK);
    let nleft = l - (n - 1) * BLOCK; // 1..=16
    let mut out = Vec::with_capacity(l);
    let mut prev = *iv;
    for i in 0..n - 2 {
        let mut blk = [0u8; BLOCK];
        blk.copy_from_slice(&ct[i * BLOCK..i * BLOCK + BLOCK]);
        let dec = aes.decrypt_block(&blk);
        for j in 0..BLOCK {
            out.push(dec[j] ^ prev[j]);
        }
        prev = blk;
    }
    let off = (n - 2) * BLOCK;
    let mut cn = [0u8; BLOCK];
    cn.copy_from_slice(&ct[off..off + BLOCK]);
    let tmp = aes.decrypt_block(&cn);
    // C_{n-1} 缺的那截尾巴正好藏在 tmp 里（见 cts_encrypt 的构造）
    let mut cn1 = [0u8; BLOCK];
    cn1[..nleft].copy_from_slice(&ct[off + BLOCK..l]);
    cn1[nleft..].copy_from_slice(&tmp[nleft..]);
    let dec1 = aes.decrypt_block(&cn1);
    for j in 0..BLOCK {
        out.push(dec1[j] ^ prev[j]);
    }
    for j in 0..nleft {
        out.push(ct[off + BLOCK + j] ^ tmp[j]);
    }
    debug_assert_eq!(out.len(), l);
    Ok(out)
}

// ------------------------------------------------------- 报文级加解密

/// 加密：`CTS(conf | plaintext) || HMAC-SHA1(Ki, conf | plaintext)[..12]`。
pub fn encrypt(etype: i32, key: &[u8], usage: u32, plain: &[u8]) -> Result<Vec<u8>> {
    let k = derive(etype, key, usage)?;
    let mut buf = sys::random_vec(CONFOUNDER)?;
    buf.extend_from_slice(plain);
    let ct = cts_encrypt(&k.ke, &[0u8; BLOCK], &buf)?;
    let mac = hash::hmac(Alg::Sha1, &k.ki, &buf);
    let mut out = ct;
    out.extend_from_slice(&mac[..HMAC_LEN]);
    Ok(out)
}

/// 解密并**常数时间**校验 HMAC。任何长度/密钥/etype 问题都是明确错误。
pub fn decrypt(etype: i32, key: &[u8], usage: u32, ct: &[u8]) -> Result<Vec<u8>> {
    if ct.len() < CONFOUNDER + HMAC_LEN {
        return Err(Error::new(format!(
            "密文太短：{} 字节（至少 {}）",
            ct.len(),
            CONFOUNDER + HMAC_LEN
        )));
    }
    let k = derive(etype, key, usage)?;
    let (body, mac) = ct.split_at(ct.len() - HMAC_LEN);
    let plain = cts_decrypt(&k.ke, &[0u8; BLOCK], body)?;
    let expect = hash::hmac(Alg::Sha1, &k.ki, &plain);
    if !hash::ct_eq(&expect[..HMAC_LEN], mac) {
        return Err(Error::new("解密失败：HMAC 校验不通过（密钥/etype/usage 不对，或报文被改过）"));
    }
    if plain.len() < CONFOUNDER {
        return Err(Error::new("解密失败：明文比 confounder 还短"));
    }
    Ok(plain[CONFOUNDER..].to_vec())
}

/// 校验和：`HMAC-SHA1(Kc, data)[..12]`。
pub fn checksum(etype: i32, key: &[u8], usage: u32, data: &[u8]) -> Result<Vec<u8>> {
    let k = derive(etype, key, usage)?;
    let mac = hash::hmac(Alg::Sha1, &k.kc, data);
    Ok(mac[..HMAC_LEN].to_vec())
}

/// 常数时间比较校验和。
pub fn verify_checksum(etype: i32, key: &[u8], usage: u32, data: &[u8], want: &[u8]) -> bool {
    match checksum(etype, key, usage, data) {
        Ok(got) => hash::ct_eq(&got, want),
        Err(_) => false,
    }
}

// ------------------------------------------------------------------ 测试

#[cfg(test)]
mod tests {
    use super::*;
    use vmless_crypto::sys::hex;

    fn unhex(s: &str) -> Vec<u8> {
        let t: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        assert_eq!(t.len() % 2, 0);
        (0..t.len() / 2)
            .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    // ---------------- RFC 3961 附录 A.1：n-fold ----------------
    #[test]
    fn nfold_rfc3961_a1() {
        let cases: &[(&str, usize, &str)] = &[
            ("012345", 8, "be072631276b1955"),
            ("password", 7, "78a07b6caf85fa"),
            ("Rough Consensus, and Running Code", 8, "bb6ed30870b7f0e0"),
            ("password", 21, "59e4a8ca7c0385c3c37b3f6d2000247cb6e6bd5b3e"),
            (
                "MASSACHVSETTS INSTITVTE OF TECHNOLOGY",
                24,
                "db3b0d8f0b061e603282b308a50841229ad798fab9540c1b",
            ),
            ("Q", 21, "518a54a215a8452a518a54a215a8452a518a54a215"),
            ("ba", 21, "fb25d531ae8974499f52fd92ea9857c4ba24cf297e"),
            ("kerberos", 8, "6b65726265726f73"),
            ("kerberos", 16, "6b65726265726f737b9b5b2b93132b93"),
            ("kerberos", 21, "8372c236344e5f1550cd0747e15d62ca7a5a3bcea4"),
            (
                "kerberos",
                32,
                "6b65726265726f737b9b5b2b93132b935c9bdcdad95c9899c4cae4dee6d6cae4",
            ),
        ];
        for (input, out, want) in cases {
            assert_eq!(hex(&nfold(input.as_bytes(), *out)), *want, "nfold({input:?}, {out})");
        }
        // 退化输入不能 panic
        assert!(nfold(b"", 16).is_empty());
        assert!(nfold(b"x", 0).is_empty());
    }

    // ---------------- RFC 3962 附录 B：string-to-key ----------------
    #[test]
    fn string2key_rfc3962_b() {
        // (迭代次数, 口令, salt, aes128, aes256)
        let cases: &[(u32, &str, &str, &str, &str)] = &[
            (
                1,
                "password",
                "ATHENA.MIT.EDUraeburn",
                "42263c6e89f4fc28b8df68ee09799f15",
                "fe697b52bc0d3ce14432ba036a92e65bbb52280990a2fa27883998d72af30161",
            ),
            (
                2,
                "password",
                "ATHENA.MIT.EDUraeburn",
                "c651bf29e2300ac27fa469d693bdda13",
                "a2e16d16b36069c135d5e9d2e25f896102685618b95914b467c67622225824ff",
            ),
            (
                1200,
                "password",
                "ATHENA.MIT.EDUraeburn",
                "4c01cd46d632d01e6dbe230a01ed642a",
                "55a6ac740ad17b4846941051e1e8b0a7548d93b0ab30a8bc3ff16280382b8c2a",
            ),
            (
                1200,
                "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "pass phrase equals block size",
                "59d1bb789a828b1aa54ef9c2883f69ed",
                "89adee3608db8bc71f1bfbfe459486b05618b70cbae22092534e56c553ba4b34",
            ),
            (
                1200,
                "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "pass phrase exceeds block size",
                "cb8005dc5f90179a7f02104c0018751d",
                "d78c5c9cb872a8c9dad4697f0bb5b2d21496c82beb2caeda2112fceea057401b",
            ),
        ];
        for (iters, pw, salt, k128, k256) in cases {
            let got = string2key(pw, salt.as_bytes(), ETYPE_AES128, *iters).unwrap();
            assert_eq!(hex(&got), *k128, "aes128 iters={iters}");
            let got = string2key(pw, salt.as_bytes(), ETYPE_AES256, *iters).unwrap();
            assert_eq!(hex(&got), *k256, "aes256 iters={iters}");
        }
        // 0x1234567878563412 作为 salt 的那组（salt 是二进制，不是 ASCII）
        let salt = unhex("1234567878563412");
        assert_eq!(
            hex(&string2key("password", &salt, ETYPE_AES128, 5).unwrap()),
            "e9b23d52273747dd5c35cb55be619d8e"
        );
        assert_eq!(
            hex(&string2key("password", &salt, ETYPE_AES256, 5).unwrap()),
            "97a4e786be20d81a382d5ebc96d5909cabcdadc87ca48f574504159f16c36e31"
        );
        // g-clef 那一组（口令是 4 字节 UTF-8）
        assert_eq!(
            hex(&string2key("\u{1d11e}", b"EXAMPLE.COMpianist", ETYPE_AES256, 50).unwrap()),
            "4b6d9839f84406df1f09cc166db4b83c571848b784a3d6bdc346589a3e393f9e"
        );
        assert_eq!(
            hex(&string2key("\u{1d11e}", b"EXAMPLE.COMpianist", ETYPE_AES128, 50).unwrap()),
            "f149c1f2e154a73452d43e7fe62a56e5"
        );
        // 不支持的 etype / 迭代次数要报错而不是猜
        assert!(string2key("x", b"s", ETYPE_RC4, 4096).is_err());
        assert!(string2key("x", b"s", ETYPE_AES256, 0).is_err());
    }

    // ------------- JDK 25 的 string-to-key（Aes256/Aes128.stringToKey） -------------
    #[test]
    fn string2key_matches_jdk() {
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "password",
                "ATHENA.MIT.EDUraeburn",
                "01b897121d933ab44b47eb5494db15e50eb74530dbdae9b634d65020ff5d88c1",
                "fca822951813fb252154c883f5ee1cf4",
            ),
            (
                "secretpw",
                "VMKRB.LOCALtestuser",
                "655de777d90b1816cfd9c73af3d4413da36357592c6eeefb512bd6402d911a01",
                "ee27bcb100bd726553fb565e7fc9595c",
            ),
            (
                "P@ssw0rd!",
                "EXAMPLE.COMalice",
                "92b85157340ac3f9b2849361cc7c242b9efbcb3d5a6a8256beb2615e125307d6",
                "4a5f41950cb97877a92e160631e17423",
            ),
            (
                "",
                "REALM",
                "aeffea28733b24163253070c69a7367fbfb76a09919295e3c08a6e45da483c5d",
                "2a652d5e8a5aef797a63d8d732ab47e4",
            ),
        ];
        for (pw, salt, k256, k128) in cases {
            assert_eq!(
                hex(&string2key(pw, salt.as_bytes(), ETYPE_AES256, DEFAULT_ITERATIONS).unwrap()),
                *k256,
                "jdk aes256 {pw:?}/{salt}"
            );
            assert_eq!(
                hex(&string2key(pw, salt.as_bytes(), ETYPE_AES128, DEFAULT_ITERATIONS).unwrap()),
                *k128,
                "jdk aes128 {pw:?}/{salt}"
            );
        }
    }

    // ---------------- RFC 3962 附录 B：CBC-CTS ----------------
    #[test]
    fn cts_rfc3962_b() {
        let key = b"chicken teriyaki";
        let iv = [0u8; 16];
        let cases: &[(&str, &str)] = &[
            (
                "I would like the ",
                "c6353568f2bf8cb4d8a580362da7ff7f97",
            ),
            (
                "I would like the General Gau's ",
                "fc00783e0efdb2c1d445d4c8eff7ed2297687268d6ecccc0c07b25e25ecfe5",
            ),
            (
                "I would like the General Gau's C",
                "39312523a78662d5be7fcbcc98ebf5a897687268d6ecccc0c07b25e25ecfe584",
            ),
            (
                "I would like the General Gau's Chicken, please,",
                "97687268d6ecccc0c07b25e25ecfe584b3fffd940c16a18c1b5549d2f838029e39312523a78662d5be7fcbcc98ebf5",
            ),
        ];
        for (plain, want) in cases {
            let got = cts_encrypt(key, &iv, plain.as_bytes()).unwrap();
            assert_eq!(hex(&got), *want, "cts encrypt {plain:?}");
            let back = cts_decrypt(key, &iv, &got).unwrap();
            assert_eq!(back, plain.as_bytes(), "cts decrypt {plain:?}");
        }
    }

    // ------- JDK 25 的 encryptRaw（含 HMAC，钉死「块整数倍也交换」这条） -------
    #[test]
    fn aes_cts_hmac_matches_jdk() {
        let key = unhex("fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefe");
        // JDK 的 encryptRaw 输出 = CTS(plaintext) || HMAC(Ki, plaintext)[..12]，
        // 所以这里把 encrypt() 的随机 confounder 换成固定的空串来对齐：
        // 直接比 CTS 部分 + 单独比 HMAC。
        let cases: &[(usize, &str)] = &[
            (16, "707c38186ff41291e2ea6dc195060591af8a598281ac11bb6583a4ae"),
            (17, "0735b62ab31b133d510a65738e193f9c701c5367729d5be4cfc89680b4"),
            (31, "c0d6ce4cca62f0458db617afb9690ba6707c38186ff41291e2ea6dc1950605894779053e6408d9fc8a86e0"),
            (32, "5ce16a1a8f3222dd68de33a43f94a6d8707c38186ff41291e2ea6dc195060591e68ee86601ed738fee9fa368"),
            (33, "707c38186ff41291e2ea6dc19506059109a2df2aae5985454ca3ac2fe50fabbd5c0441071cbf123b370efd6545"),
            (47, "707c38186ff41291e2ea6dc195060591e02610102b947fdb68df898f3ed9fc845ce16a1a8f3222dd68de33a43f94a6fd8b406702d13b8f5510746a"),
            (48, "707c38186ff41291e2ea6dc19506059169c5e654290e742c8471cad25a7acac85ce16a1a8f3222dd68de33a43f94a6d8eb3a51633fa033f280bfee84"),
            (64, "707c38186ff41291e2ea6dc1950605915ce16a1a8f3222dd68de33a43f94a6d800bbab9e0a1bdad9fa68965be69682ad69c5e654290e742c8471cad25a7acac86227f9f2785be88f5a74ee2a"),
        ];
        let k = derive(ETYPE_AES256, &key, 2).unwrap();
        for (len, want_full) in cases {
            let want = unhex(want_full);
            let plain: Vec<u8> = (0..*len).map(|i| 0x41 + (i % 26) as u8).collect();
            let ct = cts_encrypt(&k.ke, &[0u8; 16], &plain).unwrap();
            let mac = hash::hmac(Alg::Sha1, &k.ki, &plain);
            let mut full = ct.clone();
            full.extend_from_slice(&mac[..HMAC_LEN]);
            assert_eq!(hex(&full), hex(&want), "jdk encryptRaw len={len}");
            // 回程也要对
            assert_eq!(cts_decrypt(&k.ke, &[0u8; 16], &ct).unwrap(), plain, "jdk cts decrypt len={len}");
        }
        // aes128 / usage=3
        let key128 = unhex("636869636b656e207465726979616b69");
        let k = derive(ETYPE_AES128, &key128, 3).unwrap();
        let plain: Vec<u8> = (0..17).map(|i| 0x41 + (i % 26) as u8).collect();
        let ct = cts_encrypt(&k.ke, &[0u8; 16], &plain).unwrap();
        let mac = hash::hmac(Alg::Sha1, &k.ki, &plain);
        let mut full = ct;
        full.extend_from_slice(&mac[..HMAC_LEN]);
        assert_eq!(
            hex(&full),
            "f4fb14d63070a585384b260d9578400d7452b35153b7a0431a319ee980"
        );
    }

    // ------------- JDK 25 的 calculateChecksum -------------
    #[test]
    fn checksum_matches_jdk() {
        let key = unhex("fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefe");
        assert_eq!(
            hex(&checksum(ETYPE_AES256, &key, 2, b"abcd").unwrap()),
            "4c86ce60912aae692383e368"
        );
        assert_eq!(
            hex(&checksum(ETYPE_AES256, &key, 2, b"").unwrap()),
            "1bb70de39aa82f29f6d244ff"
        );
        let key128 = unhex("636869636b656e207465726979616b69");
        assert_eq!(
            hex(&checksum(ETYPE_AES128, &key128, 3, b"abcd").unwrap()),
            "df35b0075159b3e2796de390"
        );
        assert!(verify_checksum(ETYPE_AES256, &key, 2, b"abcd", &unhex("4c86ce60912aae692383e368")));
        assert!(!verify_checksum(ETYPE_AES256, &key, 2, b"abcd", &unhex("4c86ce60912aae692383e369")));
        // 长度不对不能 panic
        assert!(!verify_checksum(ETYPE_AES256, &key, 2, b"abcd", &[]));
        assert!(!verify_checksum(ETYPE_AES256, &key, 2, b"abcd", &[0u8; 40]));
    }

    // ------------- 加密/解密回环 + 篡改检测 -------------
    #[test]
    fn encrypt_roundtrip_and_tamper() {
        let key = unhex("636869636b656e207465726979616b69");
        for etype in [ETYPE_AES128, ETYPE_AES256] {
            let klen = key_len(etype).unwrap();
            let mut kb = key.clone();
            kb.extend_from_slice(&key); // 拼成 32 字节给 aes256 用
            let k = &kb[..klen];
            for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 100] {
                let plain: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
                for usage in [2u32, 3, 8, 9, 11, 12, 23] {
                    let ct = encrypt(etype, k, usage, &plain).unwrap();
                    assert_eq!(ct.len(), plain.len() + 16 + 12);
                    let back = decrypt(etype, k, usage, &ct).unwrap();
                    assert_eq!(back, plain, "etype={etype} usage={usage} len={len}");
                    // 改一个字节必须失败
                    let mut bad = ct.clone();
                    let i = bad.len() / 2;
                    bad[i] ^= 1;
                    assert!(decrypt(etype, k, usage, &bad).is_err());
                    // 截断必须失败
                    assert!(decrypt(etype, k, usage, &ct[..ct.len() - 1]).is_err());
                    // usage 不对必须失败
                    assert!(decrypt(etype, k, usage + 1, &ct).is_err());
                }
            }
        }
        // 空密文 / 超短密文不能 panic
        assert!(decrypt(ETYPE_AES256, &key[..16], 2, &[]).is_err());
        assert!(decrypt(ETYPE_AES256, &key[..16], 2, &[0u8; 20]).is_err());
        assert!(decrypt(ETYPE_RC4, &key[..16], 2, &[0u8; 40]).is_err());
    }

    #[test]
    fn cts_fuzz_no_panic() {
        // 随机长度/内容 + 随机密钥，只要求不 panic、出错是 Err
        let mut x: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..3000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 200) as usize;
            let mut buf = vec![0u8; len];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let klen = if x & 1 == 0 { 16 } else { 32 };
            let key = vec![x as u8; klen];
            let _ = cts_decrypt(&key, &[0u8; 16], &buf);
            let _ = cts_encrypt(&key, &[0u8; 16], &buf);
            let etype = if klen == 16 { ETYPE_AES128 } else { ETYPE_AES256 };
            let _ = decrypt(etype, &key, 2, &buf);
        }
    }
}
