//! JAR 签名与验签（JAR File Specification / `jarsigner` 的磁盘格式）。
//!
//! 签名产出三样东西：
//! 1. `META-INF/MANIFEST.MF`：每个条目一条 `<ALG>-Digest`
//! 2. `META-INF/<ALIAS>.SF`：清单摘要 + 每个清单条目的摘要
//! 3. `META-INF/<ALIAS>.RSA`：对 `.SF` 的 PKCS#7 SignedData（分离内容）
//!
//! 摘要约定（与 JDK 完全一致，已用真 jarsigner 产物逐字节核对）：
//! - 条目摘要覆盖清单里该条目**包含结尾空行**的原始字节
//! - `-Digest-Manifest` 覆盖整个 `MANIFEST.MF` 文件
//! - `-Digest-Manifest-Main-Attributes` 覆盖主属性段（含结尾空行）

use crate::hash::Alg;
use crate::pkcs7;
use crate::rsa::RsaPrivateKey;
use crate::zip::Zip;
use crate::{sys, Error, Result};

pub struct SignOptions {
    pub alias: String,
    pub digest_alg: Alg,
    pub key: RsaPrivateKey,
    pub cert_chain: Vec<Vec<u8>>,
    pub created_by: String,
}

impl SignOptions {
    pub fn new(alias: &str, key: RsaPrivateKey, cert_chain: Vec<Vec<u8>>) -> SignOptions {
        SignOptions {
            alias: alias.to_string(),
            digest_alg: Alg::Sha384,
            key,
            cert_chain,
            created_by: format!("vmless-crypto {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

fn is_signature_file(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if !upper.starts_with("META-INF/") {
        return false;
    }
    let rest = &upper["META-INF/".len()..];
    if rest.contains('/') {
        return false;
    }
    rest.ends_with(".SF") || rest.ends_with(".RSA") || rest.ends_with(".DSA") || rest.ends_with(".EC")
}

/// 按 JAR 规范折行：每行不超过 72 字节，续行以单个空格开头。
fn write_attribute(out: &mut Vec<u8>, key: &str, value: &str) {
    let mut line = format!("{key}: {value}");
    while line.len() > 72 {
        // 按 UTF-8 边界切，避免切坏多字节字符
        let mut cut = 72;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        let rest = line.split_off(cut);
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
        line = format!(" {rest}");
    }
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn b64(d: &[u8]) -> String {
    sys::base64_encode(d)
}

/// 清单主属性段的原始字节（含结尾空行）。
fn main_section_bytes(manifest: &[u8]) -> Vec<u8> {
    let mut pos = 0usize;
    while pos < manifest.len() {
        let end = match manifest[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => pos + i + 1,
            None => manifest.len(),
        };
        let line = &manifest[pos..end];
        pos = end;
        if line == b"\r\n" || line == b"\n" {
            return manifest[..pos].to_vec();
        }
    }
    manifest.to_vec()
}

/// 解析清单类文件（MANIFEST.MF / .SF）为「段列表」，保留每段原始字节。
pub fn parse_sections(data: &[u8]) -> Vec<(Vec<u8>, Vec<(String, String)>)> {
    let mut sections = Vec::new();
    let mut raw = Vec::new();
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let end = match data[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => pos + i + 1,
            None => data.len(),
        };
        let line = &data[pos..end];
        raw.extend_from_slice(line);
        pos = end;
        let content = line
            .strip_suffix(b"\n")
            .unwrap_or(line)
            .strip_suffix(b"\r")
            .unwrap_or(line.strip_suffix(b"\n").unwrap_or(line));
        if content.is_empty() {
            // 段落结束
            sections.push((std::mem::take(&mut raw), std::mem::take(&mut attrs)));
            continue;
        }
        if content[0] == b' ' {
            if let Some(last) = attrs.last_mut() {
                last.1.push_str(&String::from_utf8_lossy(&content[1..]));
            }
            continue;
        }
        let text = String::from_utf8_lossy(content);
        if let Some((k, v)) = text.split_once(':') {
            attrs.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    if !raw.is_empty() || !attrs.is_empty() {
        sections.push((raw, attrs));
    }
    sections
}

fn attr_of(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

fn digest_attr_name(alg: Alg) -> String {
    // JDK 用 "SHA-384-Digest" 这种写法（SHA1 例外，写作 "SHA1-Digest"）
    let name = match alg {
        Alg::Md5 => "MD5",
        Alg::Sha1 => "SHA1",
        Alg::Sha256 => "SHA-256",
        Alg::Sha384 => "SHA-384",
        Alg::Sha512 => "SHA-512",
    };
    format!("{name}-Digest")
}

/// 从属性名反推摘要算法（`SHA-384-Digest` / `SHA1-Digest`）。
fn alg_from_digest_attr(key: &str) -> Option<Alg> {
    let stem = key.strip_suffix("-Digest")?;
    match stem.to_ascii_uppercase().replace('-', "").as_str() {
        "MD5" => Some(Alg::Md5),
        "SHA1" => Some(Alg::Sha1),
        "SHA256" => Some(Alg::Sha256),
        "SHA384" => Some(Alg::Sha384),
        "SHA512" => Some(Alg::Sha512),
        _ => None,
    }
}

/// 从 `SHA-384-Digest-Manifest` / `SHA-384-Digest-Manifest-Main-Attributes` 这类键里取算法。
fn alg_from_key_suffix(key: &str, suffix: &str) -> Option<Alg> {
    Alg::from_jca_name(key.strip_suffix(suffix)?)
}

fn signature_file_base(alias: &str) -> String {
    alias.to_ascii_uppercase()
}

/// 给 JAR 签名。
pub fn sign_jar(jar: &[u8], opt: &SignOptions) -> Result<Vec<u8>> {
    if opt.cert_chain.is_empty() {
        return Err(Error::new("签名需要至少一张证书"));
    }
    let mut zip = Zip::open(jar).map_err(|e| e.ctx("读取 JAR 失败"))?;
    let alg = opt.digest_alg;

    // 1) 重建清单：保留主属性，重算全部条目段
    let old_manifest = zip.read("META-INF/MANIFEST.MF")?;
    let mut main_attrs: Vec<(String, String)> = Vec::new();
    if let Some(m) = &old_manifest {
        let sections = parse_sections(m);
        if let Some((_, attrs)) = sections.first() {
            for (k, v) in attrs {
                if k.eq_ignore_ascii_case("Manifest-Version") || k.eq_ignore_ascii_case("Created-By") {
                    continue;
                }
                main_attrs.push((k.clone(), v.clone()));
            }
        }
    }
    let mut manifest = Vec::new();
    write_attribute(&mut manifest, "Manifest-Version", "1.0");
    write_attribute(&mut manifest, "Created-By", &opt.created_by);
    for (k, v) in &main_attrs {
        write_attribute(&mut manifest, k, v);
    }
    manifest.extend_from_slice(b"\r\n");

    // 收集要签的条目（顺序与压缩包一致，跳过目录与签名文件）
    let names: Vec<String> = zip
        .entries
        .iter()
        .map(|e| e.name.clone())
        .filter(|n| !n.ends_with('/'))
        .filter(|n| n != "META-INF/MANIFEST.MF")
        .filter(|n| !is_signature_file(n))
        .collect();
    let mut entry_digests: Vec<(String, Vec<u8>)> = Vec::new();
    for name in &names {
        let data = zip
            .read(name)?
            .ok_or_else(|| Error::new(format!("JAR 里读不到条目 {name}")))?;
        let d = alg.digest(&data);
        let mut section = Vec::new();
        write_attribute(&mut section, "Name", name);
        write_attribute(&mut section, &digest_attr_name(alg), &b64(&d));
        section.extend_from_slice(b"\r\n");
        manifest.extend_from_slice(&section);
        entry_digests.push((name.clone(), section));
    }

    // 2) .SF
    let main_section = main_section_bytes(&manifest);
    let mut sf = Vec::new();
    write_attribute(&mut sf, "Signature-Version", "1.0");
    write_attribute(&mut sf, "Created-By", &opt.created_by);
    let manifest_digest = alg.digest(&manifest);
    let main_digest = alg.digest(&main_section);
    let digest_key = {
        let d = digest_attr_name(alg);
        d.strip_suffix("-Digest").unwrap_or("SHA-384").to_string()
    };
    write_attribute(&mut sf, &format!("{digest_key}-Digest-Manifest"), &b64(&manifest_digest));
    write_attribute(
        &mut sf,
        &format!("{digest_key}-Digest-Manifest-Main-Attributes"),
        &b64(&main_digest),
    );
    sf.extend_from_slice(b"\r\n");
    for (name, section) in &entry_digests {
        let d = alg.digest(section);
        let mut sec = Vec::new();
        write_attribute(&mut sec, "Name", name);
        write_attribute(&mut sec, &digest_attr_name(alg), &b64(&d));
        sec.extend_from_slice(b"\r\n");
        sf.extend_from_slice(&sec);
    }

    // 3) .RSA
    let p7 = pkcs7::sign_detached(&opt.key, &opt.cert_chain[0], alg, &sf)?;

    // 4) 写回
    let base = signature_file_base(&opt.alias);
    let sf_name = format!("META-INF/{base}.SF");
    let rsa_name = format!("META-INF/{base}.RSA");
    zip.put_stored("META-INF/MANIFEST.MF", &manifest);
    zip.put_stored(&sf_name, &sf);
    zip.put_stored(&rsa_name, &p7);
    // 与真 jarsigner 的落盘顺序一致：清单 → 签名文件 → 其余条目。
    // 否则 JDK 会报"JarFile 与 JarInputStream 读到的内容不一致"。
    zip.entries.sort_by_key(|e| {
        if e.name == "META-INF/MANIFEST.MF" {
            0
        } else if e.name == sf_name || e.name == rsa_name {
            1
        } else {
            2
        }
    });
    zip.write()
}

#[derive(Debug)]
pub struct VerifyReport {
    pub signer: String,
    pub signer_cert_der: Vec<u8>,
    pub signature_alg: String,
    pub digest_alg: String,
    pub entries: Vec<(String, bool, String)>,
    pub manifest_ok: bool,
    pub warnings: Vec<String>,
}

impl VerifyReport {
    pub fn all_ok(&self) -> bool {
        self.manifest_ok && self.entries.iter().all(|(_, ok, _)| *ok)
    }
}

/// 验签 + 逐条目校验摘要。
pub fn verify_jar(jar: &[u8]) -> Result<VerifyReport> {
    let mut zip = Zip::open(jar).map_err(|e| e.ctx("读取 JAR 失败"))?;
    // 先找签名块：没有签名块是最常见的"没签名"，优先于清单缺失报告
    let mut sig_names: Vec<String> = Vec::new();
    for e in &zip.entries {
        let upper = e.name.to_ascii_uppercase();
        if upper.starts_with("META-INF/")
            && !upper["META-INF/".len()..].contains('/')
            && (upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC"))
        {
            sig_names.push(e.name.clone());
        }
    }
    if sig_names.is_empty() {
        return Err(Error::new("这个 JAR 没有签名（找不到 META-INF/*.RSA）"));
    }
    let manifest = zip
        .read("META-INF/MANIFEST.MF")?
        .ok_or_else(|| Error::new("这不是签名过的 JAR：缺少 META-INF/MANIFEST.MF"))?;
    if sig_names.len() > 1 {
        return Err(Error::new(format!(
            "JAR 里有 {} 个签名块，本实现一次只校验一个",
            sig_names.len()
        )));
    }
    let block_name = &sig_names[0];
    let stem = block_name
        .trim_end_matches(".RSA")
        .trim_end_matches(".DSA")
        .trim_end_matches(".EC")
        .trim_end_matches(".rsa")
        .trim_end_matches(".dsa")
        .trim_end_matches(".ec")
        .to_string();
    let sf_name = format!("{stem}.SF");
    let sf = zip
        .read(&sf_name)?
        .ok_or_else(|| Error::new(format!("缺少签名文件 {sf_name}")))?;
    let block = zip
        .read(block_name)?
        .ok_or_else(|| Error::new(format!("缺少签名块 {block_name}")))?;
    let block_fmt = if block_name.to_ascii_uppercase().ends_with(".RSA") {
        "RSA"
    } else {
        return Err(Error::new("只支持 RSA 签名块（*.RSA）"));
    };

    let signer_cert = pkcs7::verify(&block, &sf).map_err(|e| e.ctx("签名校验失败"))?;
    let sd = pkcs7::parse(&block)?;
    let sf_digest_alg = Alg::from_oid(&sd.digest_alg.oid)
        .ok_or_else(|| Error::new("不支持的签名摘要算法"))?;

    // .SF 与清单的关系
    let sf_sections = parse_sections(&sf);
    if sf_sections.is_empty() {
        return Err(Error::new("签名文件为空"));
    }
    let manifest_sections = parse_sections(&manifest);
    if manifest_sections.is_empty() {
        return Err(Error::new("清单文件为空"));
    }
    let main_attrs = &sf_sections[0].1;
    let mut manifest_ok = true;
    let mut warnings = Vec::new();

    // 整份清单摘要
    let mut checked_whole = false;
    for (k, v) in main_attrs {
        if k.ends_with("-Digest-Manifest") {
            if let Some(alg) = alg_from_key_suffix(k, "-Digest-Manifest") {
                let expect = alg.digest(&manifest);
                match sys::base64_decode(v) {
                    Ok(got) if crate::hash::ct_eq(&expect, &got) => checked_whole = true,
                    _ => {
                        manifest_ok = false;
                        warnings.push(format!("{k} 与 MANIFEST.MF 不一致"));
                    }
                }
            }
        }
    }
    if !checked_whole {
        // 没有整份摘要时退而校验主属性段摘要
        for (k, v) in main_attrs {
            if k.ends_with("-Digest-Manifest-Main-Attributes") {
                if let Some(alg) = alg_from_key_suffix(k, "-Digest-Manifest-Main-Attributes") {
                    let expect = alg.digest(&main_section_bytes(&manifest));
                    if let Ok(got) = sys::base64_decode(v) {
                        if !crate::hash::ct_eq(&expect, &got) {
                            manifest_ok = false;
                            warnings.push(format!("{k} 与清单主属性不一致"));
                        }
                    }
                }
            }
        }
    }
    // .SF 里每个条目段：校验它对清单条目段的摘要
    let mut sf_entry_digests: Vec<(String, Alg, Vec<u8>)> = Vec::new();
    for (_, attrs) in sf_sections.iter().skip(1) {
        let name = match attr_of(attrs, "Name") {
            Some(n) => n,
            None => continue,
        };
        for (k, v) in attrs {
            if let Some(alg) = alg_from_digest_attr(k) {
                if let Ok(d) = sys::base64_decode(v) {
                    sf_entry_digests.push((name.clone(), alg, d));
                }
            }
        }
    }
    let mut manifest_entry_ok: Vec<String> = Vec::new();
    for (name, alg, want) in &sf_entry_digests {
        let section = manifest_sections
            .iter()
            .find(|(_, attrs)| attr_of(attrs, "Name").as_deref() == Some(name.as_str()))
            .map(|(raw, _)| raw);
        match section {
            Some(raw) => {
                let got = alg.digest(raw);
                if !crate::hash::ct_eq(&got, want) {
                    manifest_ok = false;
                    warnings.push(format!("清单条目 {name} 的 .SF 摘要不匹配"));
                } else {
                    manifest_entry_ok.push(name.clone());
                }
            }
            None => {
                manifest_ok = false;
                warnings.push(format!(".SF 里的条目 {name} 在清单里不存在"));
            }
        }
    }

    // 逐条目校验真实内容
    let mut entries = Vec::new();
    for (_, attrs) in manifest_sections.iter().skip(1) {
        let name = match attr_of(attrs, "Name") {
            Some(n) => n,
            None => continue,
        };
        let mut ok = true;
        let mut note = String::new();
        let mut any = false;
        for (k, v) in attrs {
            if let Some(alg) = alg_from_digest_attr(k) {
                any = true;
                let data = match zip.read(&name)? {
                    Some(d) => d,
                    None => {
                        ok = false;
                        note = "JAR 里缺少该条目".into();
                        continue;
                    }
                };
                let got = alg.digest(&data);
                match sys::base64_decode(v) {
                    Ok(want) if crate::hash::ct_eq(&got, &want) => {}
                    _ => {
                        ok = false;
                        note = format!("{k} 不匹配");
                    }
                }
            }
        }
        if !any {
            ok = false;
            note = "清单条目没有摘要".into();
        }
        if ok && !manifest_entry_ok.iter().any(|n| n == &name) {
            ok = false;
            note = "该条目没有被签名文件覆盖".into();
        }
        entries.push((name, ok, note));
    }

    Ok(VerifyReport {
        signer: signer_cert.subject.rfc2253(),
        signer_cert_der: signer_cert.der.clone(),
        signature_alg: sd.signer.sig_alg.name(),
        digest_alg: format!("{sf_digest_alg:?} ({block_fmt})"),
        entries,
        manifest_ok,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asn1::Name;
    use crate::rsa;
    use crate::x509::{build_self_signed, CertSpec};

    fn material() -> (RsaPrivateKey, Vec<u8>) {
        let key = rsa::generate(1024, 65537).unwrap();
        let dn = Name::parse_dn_string("CN=JarSigner,O=Test").unwrap();
        let cert = build_self_signed(&key, &CertSpec::new(dn, 365)).unwrap();
        (key, cert)
    }

    fn sample_jar() -> Vec<u8> {
        let mut z = Zip::default();
        z.put_stored("a.txt", b"hello world\n");
        z.put_stored("dir/b.txt", b"second\n");
        z.write().unwrap()
    }

    #[test]
    fn sign_and_verify() {
        let (key, cert) = material();
        let jar = sample_jar();
        let opt = SignOptions::new("test", key, vec![cert]);
        let signed = sign_jar(&jar, &opt).unwrap();
        let report = verify_jar(&signed).unwrap();
        assert!(report.all_ok(), "warnings: {:?}", report.warnings);
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.signer, "CN=JarSigner, O=Test");
    }

    #[test]
    fn tampered_entry_is_detected() {
        let (key, cert) = material();
        let opt = SignOptions::new("test", key, vec![cert]);
        let signed = sign_jar(&sample_jar(), &opt).unwrap();
        let mut z = Zip::open(&signed).unwrap();
        z.put_stored("a.txt", b"tampered\n");
        let bad = z.write().unwrap();
        let report = verify_jar(&bad).unwrap();
        assert!(!report.all_ok());
        assert!(report.entries.iter().any(|(n, ok, _)| n == "a.txt" && !ok));
    }

    #[test]
    fn tampered_manifest_is_detected() {
        let (key, cert) = material();
        let opt = SignOptions::new("test", key, vec![cert]);
        let signed = sign_jar(&sample_jar(), &opt).unwrap();
        let mut z = Zip::open(&signed).unwrap();
        let m = z.read("META-INF/MANIFEST.MF").unwrap().unwrap();
        let mut text = String::from_utf8_lossy(&m).to_string();
        text = text.replace("Created-By: ", "Created-By: x");
        z.put_stored("META-INF/MANIFEST.MF", text.as_bytes());
        let bad = z.write().unwrap();
        let report = verify_jar(&bad).unwrap();
        assert!(!report.manifest_ok);
    }

    #[test]
    fn unsigned_jar_reports_clearly() {
        let err = verify_jar(&sample_jar()).unwrap_err().to_string();
        assert!(err.contains("没有签名"), "实际: {err}");
    }

    #[test]
    fn wrapping_respects_72_bytes() {
        let mut out = Vec::new();
        let long = "A".repeat(200);
        write_attribute(&mut out, "SHA-384-Digest", &long);
        for line in out.split(|b| *b == b'\n') {
            assert!(line.len() <= 73, "行过长: {}", line.len());
        }
    }
}
