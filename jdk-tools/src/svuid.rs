//! `serialVersionUID` 的默认值计算 —— 严格按 `java.io.ObjectStreamClass.computeDefaultSUID`
//! 的规范实现（SHA-1 over DataOutputStream 序列），不是近似。
//!
//! 顺序（漏一条结果就全错）：
//! 1. `writeUTF(类二进制名)`
//! 2. `writeInt(类修饰符 & PUBLIC|FINAL|INTERFACE|ABSTRACT)`（接口且有方法时补 ABSTRACT）
//! 3. 接口名（二进制名，按名字排序，各 `writeUTF`）
//! 4. 字段（按 名字→描述符 排序）：`writeUTF(名) writeInt(修饰符) writeUTF(描述符)`
//!    过滤：`(非 private) || (非 static 且非 transient)` —— 即 private static / private transient 跳过
//! 5. 有 `<clinit>` 时：`writeUTF("<clinit>") writeInt(STATIC) writeUTF("()V")`
//! 6. 构造器（按描述符排序）：`writeUTF("<init>") writeInt(修饰符&PUBLIC|PRIVATE|PROTECTED) writeUTF(描述符，'/'→'.')`
//! 7. 方法（按 名字→描述符 排序）：`writeUTF(名) writeInt(修饰符&…D3F) writeUTF(描述符，'/'→'.')`
//! 8. SHA-1 摘要，取前 8 字节按**小端**解释成 i64

use classfile::{ClassFile, CpEntry};

pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_PROTECTED: u16 = 0x0004;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;
pub const ACC_SYNCHRONIZED: u16 = 0x0020;
pub const ACC_VOLATILE: u16 = 0x0040;
pub const ACC_TRANSIENT: u16 = 0x0080;
pub const ACC_NATIVE: u16 = 0x0100;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;
pub const ACC_STRICT: u16 = 0x0800;

const CLASS_MODIFIERS: u16 = ACC_PUBLIC | ACC_FINAL | ACC_INTERFACE | ACC_ABSTRACT;
const FIELD_MODIFIERS: u16 =
    ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED | ACC_STATIC | ACC_FINAL | ACC_VOLATILE | ACC_TRANSIENT;
const METHOD_MODIFIERS: u16 = ACC_PUBLIC
    | ACC_PRIVATE
    | ACC_PROTECTED
    | ACC_STATIC
    | ACC_FINAL
    | ACC_SYNCHRONIZED
    | ACC_NATIVE
    | ACC_ABSTRACT
    | ACC_STRICT;
const CONSTRUCTOR_MODIFIERS: u16 = ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED;

/// Modified UTF-8（`DataOutputStream.writeUTF` 用的编码）
pub fn write_utf(out: &mut Vec<u8>, s: &str) -> Result<(), String> {
    let mut body = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp == 0 {
            body.extend_from_slice(&[0xC0, 0x80]);
        } else if cp < 0x80 {
            body.push(cp as u8);
        } else if cp < 0x800 {
            body.push(0xC0 | (cp >> 6) as u8);
            body.push(0x80 | (cp & 0x3F) as u8);
        } else if cp < 0x1_0000 {
            body.push(0xE0 | (cp >> 12) as u8);
            body.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            body.push(0x80 | (cp & 0x3F) as u8);
        } else {
            // 补充平面按代理对存（每半个 3 字节）
            let v = cp - 0x1_0000;
            let hi = 0xD800 | (v >> 10) as u16;
            let lo = 0xDC00 | (v & 0x3FF) as u16;
            for u in [hi, lo] {
                body.push(0xE0 | (u >> 12) as u8);
                body.push(0x80 | ((u >> 6) & 0x3F) as u8);
                body.push(0x80 | (u & 0x3F) as u8);
            }
        }
    }
    if body.len() > 0xFFFF {
        return Err(format!("writeUTF 的字符串超过 65535 字节：{s}"));
    }
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

fn write_int(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// 取常量池里的 Utf8 文本（名字/描述符）
fn utf8(cf: &ClassFile, idx: u16) -> Result<String, String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Utf8(b))) => Ok(String::from_utf8_lossy(b).into_owned()),
        _ => Err(format!("常量池 #{idx} 不是 Utf8（class 文件损坏）")),
    }
}

/// 取 `Class` 常量指向的类名（内部名，`/` 分隔）
fn class_name_at(cf: &ClassFile, idx: u16) -> Result<String, String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Class(n))) => utf8(cf, *n),
        _ => Err(format!("常量池 #{idx} 不是 Class（class 文件损坏）")),
    }
}

/// 类的二进制名（`java.lang.String`）
pub fn binary_name(cf: &ClassFile) -> Result<String, String> {
    Ok(class_name_at(cf, cf.this_class)?.replace('/', "."))
}

/// 父类二进制名；`java/lang/Object` 或没有父类时返回 None
pub fn super_binary_name(cf: &ClassFile) -> Result<Option<String>, String> {
    if cf.super_class == 0 {
        return Ok(None);
    }
    let n = class_name_at(cf, cf.super_class)?;
    if n == "java/lang/Object" && cf.this_class_name().as_deref() == Some("java/lang/Object") {
        return Ok(None);
    }
    Ok(Some(n.replace('/', ".")))
}

/// 直接实现的接口（二进制名）
pub fn interface_names(cf: &ClassFile) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for i in &cf.interfaces {
        v.push(class_name_at(cf, *i)?.replace('/', "."));
    }
    Ok(v)
}

/// 按规范算 `serialVersionUID`（**不检查** 是否 Serializable —— 调用方负责）
pub fn compute_default(cf: &ClassFile) -> Result<i64, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    write_utf(&mut buf, &binary_name(cf)?)?;

    let mut class_mods = cf.access_flags & CLASS_MODIFIERS;
    let real_methods: Vec<&classfile::Member> =
        cf.methods.iter().filter(|m| !utf8(cf, m.name_index).map(|n| n == "<clinit>").unwrap_or(true)).collect();
    if class_mods & ACC_INTERFACE != 0 {
        if real_methods.is_empty() {
            class_mods &= !ACC_ABSTRACT;
        } else {
            class_mods |= ACC_ABSTRACT;
        }
    }
    write_int(&mut buf, class_mods as i32);

    // 接口名按名字排序
    let mut ifaces = interface_names(cf)?;
    ifaces.sort();
    for i in &ifaces {
        write_utf(&mut buf, i)?;
    }

    // 字段
    let mut fields: Vec<(String, String, u16)> = Vec::new();
    for f in &cf.fields {
        let name = utf8(cf, f.name_index)?;
        let desc = utf8(cf, f.descriptor_index)?;
        let mods = f.access_flags & FIELD_MODIFIERS;
        let skip = (mods & ACC_PRIVATE) != 0 && (mods & (ACC_STATIC | ACC_TRANSIENT)) != 0;
        if skip {
            continue;
        }
        fields.push((name, desc, mods));
    }
    // 字段：JDK 只用**名字**排序（比较器只看 name，Arrays.sort 稳定），同名字段保持声明顺序
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, desc, mods) in &fields {
        write_utf(&mut buf, name)?;
        write_int(&mut buf, *mods as i32);
        write_utf(&mut buf, desc)?;
    }

    // 静态初始化器
    let has_clinit = cf
        .methods
        .iter()
        .any(|m| utf8(cf, m.name_index).map(|n| n == "<clinit>").unwrap_or(false));
    if has_clinit {
        write_utf(&mut buf, "<clinit>")?;
        write_int(&mut buf, ACC_STATIC as i32);
        write_utf(&mut buf, "()V")?;
    }

    // 构造器：修饰符用**完整的方法掩码**，private 的整个跳过（JDK 源码如此）
    let mut ctors: Vec<(String, u16)> = Vec::new();
    for m in &cf.methods {
        if utf8(cf, m.name_index)? == "<init>" {
            let mods = m.access_flags & METHOD_MODIFIERS;
            if mods & ACC_PRIVATE != 0 {
                continue;
            }
            ctors.push((utf8(cf, m.descriptor_index)?, mods));
        }
    }
    ctors.sort_by(|a, b| a.0.cmp(&b.0));
    for (desc, mods) in &ctors {
        write_utf(&mut buf, "<init>")?;
        write_int(&mut buf, *mods as i32);
        write_utf(&mut buf, &desc.replace('/', "."))?;
    }

    // 方法：private 的整个跳过（JDK 源码如此）
    let mut methods: Vec<(String, String, u16)> = Vec::new();
    for m in &cf.methods {
        let name = utf8(cf, m.name_index)?;
        if name == "<init>" || name == "<clinit>" {
            continue;
        }
        let mods = m.access_flags & METHOD_MODIFIERS;
        if mods & ACC_PRIVATE != 0 {
            continue;
        }
        methods.push((name, utf8(cf, m.descriptor_index)?, mods));
    }
    methods.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (name, desc, mods) in &methods {
        write_utf(&mut buf, name)?;
        write_int(&mut buf, *mods as i32);
        write_utf(&mut buf, &desc.replace('/', "."))?;
    }

    let digest = crate::sha1::digest(&buf);
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    Ok(i64::from_le_bytes(first8))
}

/// 常量池里有没有 `@Deprecated` 注解 / `Deprecated` 属性
pub fn is_deprecated(cf: &ClassFile) -> bool {
    has_deprecated_attr(cf) || has_deprecated_annotation(cf)
}

fn has_deprecated_attr(cf: &ClassFile) -> bool {
    cf.attributes.iter().any(|a| {
        cf.utf8(a.name_index)
            .map(|b| b == b"Deprecated")
            .unwrap_or(false)
    })
}

fn has_deprecated_annotation(cf: &ClassFile) -> bool {
    // RuntimeVisibleAnnotations 属性里出现 `Ljava/lang/Deprecated;`
    let dep: &[u8] = b"Ljava/lang/Deprecated;";
    for a in &cf.attributes {
        if cf.utf8(a.name_index).map(|b| b == b"RuntimeVisibleAnnotations").unwrap_or(false) {
            if let Some(p) = cf.constant_pool.iter().position(|e| {
                matches!(e, Some(CpEntry::Utf8(v)) if v.as_slice() == dep)
            }) {
                // 属性 info 里以 u2 索引引用注解类型，扫一遍这个索引
                let idx = p as u16;
                for w in a.info.windows(2) {
                    if u16::from_be_bytes([w[0], w[1]]) == idx {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// 成员级（字段/方法）的 @Deprecated
pub fn member_deprecated(cf: &ClassFile, m: &classfile::Member) -> bool {
    m.attributes.iter().any(|a| {
        cf.utf8(a.name_index)
            .map(|b| b == b"Deprecated" || b == b"RuntimeVisibleAnnotations")
            .unwrap_or(false)
            && (cf.utf8(a.name_index).map(|b| b == b"Deprecated").unwrap_or(false)
                || {
                    let dep: &[u8] = b"Ljava/lang/Deprecated;";
                    match cf
                        .constant_pool
                        .iter()
                        .position(|e| matches!(e, Some(CpEntry::Utf8(v)) if v.as_slice() == dep))
                    {
                        Some(p) => {
                            let idx = p as u16;
                            a.info.windows(2).any(|w| u16::from_be_bytes([w[0], w[1]]) == idx)
                        }
                        None => false,
                    }
                })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf_pieces() {
        let mut v = Vec::new();
        write_utf(&mut v, "AB").unwrap();
        assert_eq!(v, vec![0, 2, b'A', b'B']);
        let mut v = Vec::new();
        write_utf(&mut v, "\0").unwrap();
        assert_eq!(v, vec![0, 2, 0xC0, 0x80]);
    }
}

/// 类是否自己声明了 `serialVersionUID`（`static final long`），有就返回声明值。
/// `ObjectStreamClass.lookup` 的语义：**声明值优先**，没有才按规范算默认值。
pub fn declared(cf: &ClassFile) -> Result<Option<i64>, String> {
    for f in &cf.fields {
        let name = match cf.utf8(f.name_index) {
            Some(b) => String::from_utf8_lossy(b).into_owned(),
            None => continue,
        };
        if name != "serialVersionUID" {
            continue;
        }
        let desc = cf
            .utf8(f.descriptor_index)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        if desc != "J" {
            continue;
        }
        if f.access_flags & ACC_STATIC == 0 || f.access_flags & ACC_FINAL == 0 {
            continue;
        }
        // ConstantValue 属性
        for a in &f.attributes {
            let an = cf
                .utf8(a.name_index)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            if an != "ConstantValue" || a.info.len() < 2 {
                continue;
            }
            let idx = u16::from_be_bytes([a.info[0], a.info[1]]);
            if let Some(Some(CpEntry::Long(v))) = cf.constant_pool.get(idx as usize) {
                return Ok(Some(*v as i64));
            }
        }
    }
    Ok(None)
}
