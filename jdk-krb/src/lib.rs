//! vmless 的 Kerberos v5 最小正确子集：ASN.1 报文、AES-CTS 加密、ccache、
//! `kinit`/`klist` 客户端，以及一个能跟真 JDK 互通的最小 KDC。
//!
//! 模块分层：
//! - [`asn1_krb`]  RFC 4120 的 ASN.1 类型（只用自己的 DER）
//! - [`crypto_krb`] RFC 3961/3962 的 `aes256-cts-hmac-sha1-96` / `aes128-...`
//! - [`msg`]       消息层：key usage、预认证、错误报文
//! - [`ccache`]    MIT `FILE:` 凭据缓存
//! - [`client`]    `kinit` / `klist` / TGS 的逻辑
//! - [`kdc`]       最小 KDC（验收用，也是真实现）

pub mod asn1_krb;
pub mod ccache;
pub mod client;
pub mod crypto_krb;
pub mod kdc;
pub mod msg;
