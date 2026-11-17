//! vmless 的密码学栈：摘要 / HMAC / RSA / ASN.1 / X.509 / PKCS#7 / PKCS#12 / JAR 签名。
//!
//! 全部算法自实现，除 `classfile` 外无任何外部依赖 —— 覆盖 JDK `keytool` /
//! `jarsigner` 需要的整套能力，可以与本机真 JDK 双向互操作。
//!
//! 模块分层：
//! - [`hash`]    摘要与 HMAC/PBKDF2/PKCS#12 KDF（RSA 与 PKCS#12 的地基）
//! - [`bigint`]  大数（u32 limb）：加/减/乘/除/模幂/求逆
//! - [`rsa`]     RSA 密钥生成、PKCS#1 v1.5 签名与验签（严格按 RFC 8017 校验填充）
//! - [`asn1`]    DER/BER 编解码
//! - [`x509`]    X.509 证书解析与自签生成
//! - [`pkcs7`]   PKCS#7 / CMS SignedData（jarsigner 的 `.RSA`）
//! - [`pkcs12`]  PKCS#12 keystore
//! - [`jar_sign`] JAR 签名与验签
//! - [`sys`]     系统随机源、时钟、错误类型

pub mod asn1;
pub mod bigint;
pub mod cipher;
pub mod jar_sign;
pub mod hash;
pub mod pkcs12;
pub mod pkcs7;
pub mod rsa;
pub mod sys;
pub mod zip;
pub mod x509;

/// 一个便于统一传播的错误类型：带上下文的可读信息，避免把底层 "parse error" 直接抛给用户。
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error {
    pub msg: String,
}

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error { msg: msg.into() }
    }
    pub fn ctx(self, prefix: &str) -> Self {
        Error { msg: format!("{}: {}", prefix, self.msg) }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(e.to_string())
    }
}

#[allow(unused_macros)]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::Error::new(format!($($arg)*)))
    };
}
#[allow(unused_imports)]
pub(crate) use bail;
