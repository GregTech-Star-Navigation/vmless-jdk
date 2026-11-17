//! 字节码级的通用改写：`jlink --strip-debug` 用到的「去调试信息」。
//!
//! 只动属性表，不碰常量池 —— 输出仍是逐字节合法的 class 文件
//! （多余的 Utf8 留在池里，与 `javac -g:none` 不完全等价，但语义一致）。

use std::collections::HashMap;

use classfile::{Attribute, ClassFile};

const DEBUG_ATTRS: [&str; 5] = [
    "LineNumberTable",
    "LocalVariableTable",
    "LocalVariableTypeTable",
    "SourceFile",
    "SourceDebugExtension",
];

/// 去掉 Code 里的调试子属性与类级的 `SourceFile`/`SourceDebugExtension`
pub fn strip_debug(cf: &mut ClassFile) -> Result<(), String> {
    // 先把常量池里的 Utf8 拍成 map：后面要可变借用 cf，再读 cf 就会冲突
    let names: HashMap<u16, String> = cf
        .constant_pool
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Some(classfile::CpEntry::Utf8(b)) => {
                Some((i as u16, String::from_utf8_lossy(b).into_owned()))
            }
            _ => None,
        })
        .collect();
    let is_debug = |a: &Attribute| -> bool {
        names
            .get(&a.name_index)
            .map(|n| DEBUG_ATTRS.contains(&n.as_str()))
            .unwrap_or(false)
    };
    let is_code = |a: &Attribute| -> bool {
        names.get(&a.name_index).map(|n| n == "Code").unwrap_or(false)
    };

    cf.attributes.retain(|a| !is_debug(a));
    for m in cf.methods.iter_mut().chain(cf.fields.iter_mut()) {
        m.attributes.retain(|a| !is_debug(a));
    }
    for m in cf.methods.iter_mut() {
        for a in m.attributes.iter_mut() {
            if is_code(a) {
                a.info = strip_code_debug(&names, &a.info)?;
            }
        }
    }
    Ok(())
}

/// Code 属性体：`u2 max_stack | u2 max_locals | u4 code_length | code | exception_table | attributes`
fn strip_code_debug(names: &HashMap<u16, String>, info: &[u8]) -> Result<Vec<u8>, String> {
    let trunc = || "Code 属性被截断（class 文件损坏）".to_string();
    let mut p = 0usize;
    if info.len() < 8 {
        return Err(trunc());
    }
    let code_len = u32::from_be_bytes([info[4], info[5], info[6], info[7]]) as usize;
    p = 8 + code_len;
    if p + 2 > info.len() {
        return Err(trunc());
    }
    let exc_len = u16::from_be_bytes([info[p], info[p + 1]]) as usize;
    p += 2 + exc_len * 8;
    if p + 2 > info.len() {
        return Err(trunc());
    }
    let attr_start = p; // 属性个数字段的位置
    let nattr = u16::from_be_bytes([info[p], info[p + 1]]) as usize;
    p += 2;
    let mut kept: Vec<u8> = Vec::new();
    let mut kept_count = 0u16;
    for _ in 0..nattr {
        if p + 6 > info.len() {
            return Err(trunc());
        }
        let name_idx = u16::from_be_bytes([info[p], info[p + 1]]);
        let len = u32::from_be_bytes([info[p + 2], info[p + 3], info[p + 4], info[p + 5]]) as usize;
        p += 6;
        if p + len > info.len() {
            return Err(trunc());
        }
        let name = names.get(&name_idx).cloned().unwrap_or_default();
        if !DEBUG_ATTRS.contains(&name.as_str()) {
            kept.extend_from_slice(&name_idx.to_be_bytes());
            kept.extend_from_slice(&(len as u32).to_be_bytes());
            kept.extend_from_slice(&info[p..p + len]);
            kept_count += 1;
        }
        p += len;
    }
    let mut out = Vec::with_capacity(attr_start + 2 + kept.len());
    out.extend_from_slice(&info[..attr_start]);
    out.extend_from_slice(&kept_count.to_be_bytes());
    out.extend_from_slice(&kept);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_keeps_parseable() {
        let p = std::path::Path::new("D:/vmless/patch/img/java.base/java/lang/Object.class");
        if !p.is_file() {
            return; // 环境里没有就跳过（不把测试绑死在某个盘）
        }
        let bytes = std::fs::read(p).unwrap();
        let mut cf = classfile::parse(&bytes).unwrap();
        let before = cf.methods.len();
        strip_debug(&mut cf).unwrap();
        assert_eq!(cf.methods.len(), before);
        let out = classfile::write(&cf);
        let again = classfile::parse(&out).expect("去调试信息后必须仍能 parse");
        assert_eq!(again.methods.len(), before);
    }
}
