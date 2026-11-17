//! ASN.1 的结构化视图：元素树、`Name`（RDN）、`AlgorithmIdentifier`、GeneralNames。

use super::der::{self, Tlv};
use super::oid;
use crate::{Error, Result};

/// 通用元素树（构造型递归展开）。用于 `-printcert -v` 这类"把结构摊开"的输出。
#[derive(Clone, Debug)]
pub struct Element<'a> {
    pub tag: u8,
    pub content: &'a [u8],
    pub children: Vec<Element<'a>>,
}

impl<'a> Element<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Element<'a>> {
        let (tlv, rest) = der::parse_tlv(data)?;
        let mut children = Vec::new();
        if tlv.is_constructed() {
            let mut body = tlv.content;
            while !body.is_empty() {
                let (child, r) = der::parse_tlv(body)?;
                children.push(Element::parse(&body[..body.len() - r.len()])?);
                body = r;
                let _ = child;
            }
        }
        if !rest.is_empty() {
            return Err(Error::new("元素后面有多余数据"));
        }
        Ok(Element { tag: tlv.tag, content: tlv.content, children })
    }

    pub fn parse_multi(data: &'a [u8]) -> Result<Vec<Element<'a>>> {
        let mut out = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let (tlv, r) = der::parse_tlv(rest)?;
            out.push(Element::parse(&rest[..rest.len() - r.len()])?);
            let _ = tlv;
            rest = r;
        }
        Ok(out)
    }

    pub fn seq(tlv: &Tlv<'a>) -> Result<Vec<Element<'a>>> {
        let content = tlv.expect(der::TAG_SEQUENCE, "SEQUENCE")?;
        Element::parse_multi(content)
    }
}

/// 一个 DN：按出现顺序保存（RDN 内部可能有多值）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Name {
    pub rdns: Vec<Vec<(String, String)>>,
}

impl Name {
    pub fn from_der(der_bytes: &[u8]) -> Result<Name> {
        let tlv = der::parse_exact(der_bytes, "Name")?;
        Name::parse(tlv.expect(der::TAG_SEQUENCE, "Name")?)
    }

    pub fn parse(content: &[u8]) -> Result<Name> {
        let rdns_raw = der::parse_all(content)?;
        let mut rdns = Vec::new();
        for rdn in rdns_raw {
            let set = rdn.expect(der::TAG_SET, "RDN")?;
            let mut pairs = Vec::new();
            for atv in der::parse_all(set)? {
                let parts = der::parse_all(atv.expect(der::TAG_SEQUENCE, "AttributeTypeAndValue")?)?;
                if parts.len() != 2 {
                    return Err(Error::new("AttributeTypeAndValue 应有 2 个字段"));
                }
                let oid_content = parts[0].expect(der::TAG_OID, "属性类型")?;
                let key = match oid::name_of(oid_content) {
                    Some(n) => n.to_string(),
                    None => der::parse_oid(oid_content)?,
                };
                let val = match parts[1].tag {
                    der::TAG_UTF8_STRING
                    | der::TAG_PRINTABLE_STRING
                    | der::TAG_IA5_STRING
                    | der::TAG_T61_STRING
                    | der::TAG_BMP_STRING => der::parse_string(parts[1].tag, parts[1].content)?,
                    other => format!("<tag 0x{other:02x}>"),
                };
                pairs.push((key, val));
            }
            if pairs.is_empty() {
                return Err(Error::new("空的 RDN"));
            }
            rdns.push(pairs);
        }
        Ok(Name { rdns })
    }

    /// 逗号分隔的 DN 字符串，顺序与 DER 编码一致（与 `keytool -printcert` 的 `Owner:` 相同）。
    pub fn rfc2253(&self) -> String {
        // keytool -printcert 按 DER 里的出现顺序打印（不反转），这里与之对齐
        let mut parts = Vec::new();
        for rdn in self.rdns.iter() {
            let inner: Vec<String> = rdn
                .iter()
                .map(|(k, v)| format!("{}={}", k, escape_dn_value(v)))
                .collect();
            parts.push(inner.join("+"));
        }
        parts.join(", ")
    }

    /// `/C=CN/O=..` 形式（旧 keytool 的 `-dname` 输出）。
    pub fn slash(&self) -> String {
        let mut out = String::new();
        for rdn in &self.rdns {
            for (k, v) in rdn {
                out.push('/');
                out.push_str(k);
                out.push('=');
                out.push_str(v);
            }
        }
        out
    }

    pub fn to_der(&self) -> Vec<u8> {
        let mut rdns = Vec::new();
        for rdn in &self.rdns {
            let mut atvs = Vec::new();
            for (k, v) in rdn {
                let oid_content = oid_for_key(k);
                let value = if k == "C" && v.len() == 2 {
                    der::printable_string(v)
                } else {
                    der::utf8_string(v)
                };
                atvs.push(der::seq(&[der::oid(&oid_content), value]));
            }
            rdns.push(der::set(&atvs));
        }
        der::seq(&rdns)
    }

    /// 解析 `CN=a,OU=b,O=c` 或 `/CN=a/OU=b` 形式的 DN 字符串。
    pub fn parse_dn_string(s: &str) -> Result<Name> {
        let mut rdns: Vec<Vec<(String, String)>> = Vec::new();
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::new("DN 为空"));
        }
        let parts: Vec<String> = if s.starts_with('/') {
            s.split('/').filter(|p| !p.is_empty()).map(|p| p.to_string()).collect()
        } else {
            split_unescaped(s, ',')
        };
        for part in parts {
            let (k, v) = match part.split_once('=') {
                Some(kv) => kv,
                None => return Err(Error::new(format!("DN 片段缺少 '=': {part}"))),
            };
            let key = normalize_key(k.trim());
            if key.is_empty() {
                return Err(Error::new(format!("DN 片段缺少属性名: {part}")));
            }
            rdns.push(vec![(key, unescape_dn_value(v.trim()))]);
        }
        Ok(Name { rdns })
    }
}

fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push('\\');
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == sep {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn normalize_key(k: &str) -> String {
    let up = k.to_ascii_uppercase();
    match up.as_str() {
        "CN" => "CN".into(),
        "OU" => "OU".into(),
        "O" => "O".into(),
        "L" => "L".into(),
        "ST" | "S" => "ST".into(),
        "C" => "C".into(),
        "EMAIL" | "E" | "EMAILADDRESS" => "EMAILADDRESS".into(),
        "SERIALNUMBER" => "SERIALNUMBER".into(),
        "GIVENNAME" => "GIVENNAME".into(),
        "SURNAME" => "SURNAME".into(),
        "T" | "TITLE" => "T".into(),
        "DNQ" | "DNQUALIFIER" => "DNQ".into(),
        other => other.to_string(),
    }
}

fn oid_for_key(k: &str) -> Vec<u8> {
    let up = k.to_ascii_uppercase();
    match up.as_str() {
        "CN" => oid::AT_COMMON_NAME.to_vec(),
        "OU" => oid::AT_ORG_UNIT.to_vec(),
        "O" => oid::AT_ORG.to_vec(),
        "L" => oid::AT_LOCALITY.to_vec(),
        "ST" => oid::AT_STATE.to_vec(),
        "C" => oid::AT_COUNTRY.to_vec(),
        "EMAILADDRESS" => oid::AT_EMAIL.to_vec(),
        "SERIALNUMBER" => oid::AT_SERIAL_NUMBER.to_vec(),
        "GIVENNAME" => oid::AT_GIVEN_NAME.to_vec(),
        "SURNAME" => oid::AT_SURNAME.to_vec(),
        "T" => oid::AT_TITLE.to_vec(),
        "DNQ" => oid::AT_DN_QUALIFIER.to_vec(),
        other => der::oid_from_str(other)
            .ok()
            .and_then(|d| der::parse_tlv(&d).ok().map(|(t, _)| t.content.to_vec()))
            .unwrap_or_else(|| oid::AT_COMMON_NAME.to_vec()),
    }
}

fn escape_dn_value(v: &str) -> String {
    let mut out = String::new();
    for (i, c) in v.chars().enumerate() {
        match c {
            ',' | '+' | '"' | '\\' | '<' | '>' | ';' | '=' => {
                out.push('\\');
                out.push(c);
            }
            ' ' if i == 0 => out.push_str("\\ "),
            _ => out.push(c),
        }
    }
    out
}

fn unescape_dn_value(v: &str) -> String {
    let mut out = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY OPTIONAL }`
#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmIdentifier {
    pub oid: Vec<u8>,
    pub params: Option<Vec<u8>>,
}

impl AlgorithmIdentifier {
    pub fn parse(content: &[u8]) -> Result<AlgorithmIdentifier> {
        let items = der::parse_all(content)?;
        if items.is_empty() {
            return Err(Error::new("AlgorithmIdentifier 为空"));
        }
        let oid = items[0].expect(der::TAG_OID, "算法 OID")?.to_vec();
        // params 保存**完整 DER**（含 tag/len），这样 NULL 与 PBES2 的参数都能原样往返
        let params = if items.len() > 1 {
            Some(der::tlv(items[1].tag, items[1].content))
        } else {
            None
        };
        if items.len() > 2 {
            return Err(Error::new("AlgorithmIdentifier 字段过多"));
        }
        Ok(AlgorithmIdentifier { oid, params })
    }

    pub fn name(&self) -> String {
        match oid::name_of(&self.oid) {
            Some(n) => n.to_string(),
            None => der::parse_oid(&self.oid).unwrap_or_else(|_| "<未知 OID>".into()),
        }
    }

    pub fn to_der(&self) -> Vec<u8> {
        let mut items = vec![der::oid(&self.oid)];
        if let Some(p) = &self.params {
            items.push(p.clone());
        }
        der::seq(&items)
    }

    pub fn rsa_with_digest(alg: crate::hash::Alg) -> AlgorithmIdentifier {
        let oid = match alg {
            crate::hash::Alg::Md5 => oid::MD5.to_vec(),
            crate::hash::Alg::Sha1 => oid::SHA1_WITH_RSA.to_vec(),
            crate::hash::Alg::Sha256 => oid::SHA256_WITH_RSA.to_vec(),
            crate::hash::Alg::Sha384 => oid::SHA384_WITH_RSA.to_vec(),
            crate::hash::Alg::Sha512 => oid::SHA512_WITH_RSA.to_vec(),
        };
        AlgorithmIdentifier { oid, params: Some(der::null()) }
    }

    pub fn digest(digest_oid: &[u8]) -> AlgorithmIdentifier {
        AlgorithmIdentifier { oid: digest_oid.to_vec(), params: Some(der::null()) }
    }

    /// 从 `xxxwithRSA` 的签名算法 OID 反推摘要算法。
    pub fn digest_of_signature(&self) -> Option<crate::hash::Alg> {
        use crate::hash::Alg;
        match self.oid.as_slice() {
            o if o == oid::MD5 => Some(Alg::Md5),
            o if o == oid::SHA1_WITH_RSA => Some(Alg::Sha1),
            o if o == oid::SHA256_WITH_RSA => Some(Alg::Sha256),
            o if o == oid::SHA384_WITH_RSA => Some(Alg::Sha384),
            o if o == oid::SHA512_WITH_RSA => Some(Alg::Sha512),
            _ => None,
        }
    }
}

/// `GeneralNames ::= SEQUENCE OF GeneralName`，这里只保留我们要展示的类型。
#[derive(Clone, Debug, PartialEq)]
pub enum GeneralName {
    DnsName(String),
    Rfc822(String),
    Uri(String),
    IpAddress(String),
    DirectoryName(String),
    Other(String),
}

pub fn parse_general_names(content: &[u8]) -> Result<Vec<GeneralName>> {
    let mut out = Vec::new();
    for gn in der::parse_all(content)? {
        if !gn.is_context() {
            return Err(Error::new("GeneralName 不是 context 标签"));
        }
        let n = gn.number();
        let text = String::from_utf8_lossy(gn.content).to_string();
        out.push(match n {
            1 => GeneralName::Rfc822(text),
            2 => GeneralName::DnsName(text),
            6 => GeneralName::Uri(text),
            7 => {
                if gn.content.len() == 4 {
                    GeneralName::IpAddress(format!(
                        "{}.{}.{}.{}",
                        gn.content[0], gn.content[1], gn.content[2], gn.content[3]
                    ))
                } else if gn.content.len() == 16 {
                    let mut parts = Vec::new();
                    for c in gn.content.chunks(2) {
                        parts.push(format!("{:x}", u16::from_be_bytes([c[0], c[1]])));
                    }
                    GeneralName::IpAddress(parts.join(":"))
                } else {
                    GeneralName::Other(format!("ip({} 字节)", gn.content.len()))
                }
            }
            4 => GeneralName::DirectoryName(Name::parse(gn.content)?.rfc2253()),
            _ => GeneralName::Other(format!("[{}]", n)),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dn_roundtrip() {
        let dn = Name::parse_dn_string("CN=Test,OU=Dev,O=Example,L=City,ST=ST,C=CN").unwrap();
        let der = dn.to_der();
        let parsed = Name::from_der(&der).unwrap();
        assert_eq!(parsed.rfc2253(), "CN=Test, OU=Dev, O=Example, L=City, ST=ST, C=CN");
        assert_eq!(parsed.slash(), "/CN=Test/OU=Dev/O=Example/L=City/ST=ST/C=CN");
        assert_eq!(parsed, dn);
    }

    #[test]
    fn dn_escaping() {
        let dn = Name::parse_dn_string("CN=a\\,b,O=x").unwrap();
        let der = dn.to_der();
        let parsed = Name::from_der(&der).unwrap();
        assert_eq!(parsed.rdns[0][0].1, "a,b");
        assert_eq!(parsed.rfc2253(), "CN=a\\,b, O=x");
    }

    #[test]
    fn dn_errors() {
        assert!(Name::parse_dn_string("").is_err());
        assert!(Name::parse_dn_string("NOVALUE").is_err());
        assert!(Name::parse(&[0x31, 0x00]).is_err()); // 空 RDN
    }

    #[test]
    fn alg_id_roundtrip() {
        let a = AlgorithmIdentifier::rsa_with_digest(crate::hash::Alg::Sha256);
        let der = a.to_der();
        let (tlv, _) = der::parse_tlv(&der).unwrap();
        let b = AlgorithmIdentifier::parse(tlv.content).unwrap();
        assert_eq!(a, b);
        assert_eq!(b.name(), "SHA256withRSA");
        assert_eq!(b.digest_of_signature(), Some(crate::hash::Alg::Sha256));
    }
}
