//! 元数据块：字符串池 + 元素树。
//!
//! `jfr metadata` / 事件字段名 / 类型定义全在这一块里。格式（`MetadataWriter`）：
//!
//! ```text
//! [varint 池大小] [字符串 × n] [元素树（递归）]
//!
//! 元素 = [varint 名字下标][varint 属性个数]{[varint 键下标][varint 值下标]}*[varint 子元素个数][子元素]*
//! ```
//!
//! 字符串的编码（`StringParser.Encoding`）是**自描述的**，一个字节打头：
//!
//! | 首字节 | 含义 | 后面跟什么 |
//! | --- | --- | --- |
//! | 0 | null | 无 |
//! | 1 | 空串 | 无 |
//! | 2 | 常量池引用 | varint 键 |
//! | 3 | UTF-8 | varint 长度 + 字节 |
//! | 4 | UTF-16 字符数组 | varint 长度 + 每个字符一个 varint |
//! | 5 | Latin-1 | varint 长度 + 字节 |
//!
//! 元数据里 Java 只写 0 和 4（`MetadataWriter.writeString`），事件里常用 4/2。
//! 五种都得读 —— 真 jfr 读得动的东西我们不能读不动。

use crate::bail;
use std::collections::HashMap;

use super::{Cursor, Error, Result, MAX_DEPTH, MAX_STRING_BYTES};

/// 常量池（或字符串池）里按下标取字符串的能力。
pub trait StringConsts {
    fn get_string(&self, key: i64) -> Option<String>;
}

impl StringConsts for Vec<String> {
    fn get_string(&self, key: i64) -> Option<String> {
        if key < 0 {
            return None;
        }
        self.get(key as usize).cloned()
    }
}

/// 一个元数据元素（`<class name="..." id="...">` 这种）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn new(name: &str) -> Self {
        Element {
            name: name.to_string(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    pub fn add_attr(&mut self, k: &str, v: impl Into<String>) {
        self.attrs.push((k.to_string(), v.into()));
    }

    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn new_child(&mut self, name: &str) -> &mut Element {
        self.children.push(Element::new(name));
        let n = self.children.len() - 1;
        &mut self.children[n]
    }
}

/// 读一个自描述编码的字符串。
pub fn read_string(cur: &mut Cursor, consts: Option<&dyn StringConsts>) -> Result<Option<String>> {
    let enc = cur.u8()?;
    match enc {
        0 => Ok(None),
        1 => Ok(Some(String::new())),
        2 => {
            let key = cur.leb()?;
            match consts {
                Some(c) => match c.get_string(key) {
                    Some(s) => Ok(Some(s)),
                    // 常量池里没有这个键：按 null 处理（真 jfr 在 CP 未解析时也返回 null）
                    None => {
                        trace(&format!("字符串常量池缺键 key={}（偏移 {}）", key, cur.pos));
                        Ok(None)
                    }
                },
                None => bail!("字符串引用了常量池（键 {}），但此处没有常量池", key),
            }
        }
        3 => {
            let len = read_len(cur)?;
            let bytes = cur.take(len)?;
            Ok(Some(match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                // 坏 UTF-8 不报错：宁可显示替换字符，也不要因为一个字段名读不出整个文件
                Err(_) => String::from_utf8_lossy(bytes).into_owned(),
            }))
        }
        4 => {
            let len = read_len(cur)?;
            let mut units = Vec::with_capacity(len.min(4096));
            for _ in 0..len {
                let v = cur.leb()?;
                units.push((v as u32 & 0xffff) as u16);
            }
            Ok(Some(match String::from_utf16(&units) {
                Ok(s) => s,
                Err(_) => String::from_utf16_lossy(&units),
            }))
        }
        5 => {
            let len = read_len(cur)?;
            let bytes = cur.take(len)?;
            Ok(Some(bytes.iter().map(|&b| b as char).collect()))
        }
        other => Err(Error::new(format!(
            "未知的字符串编码 {}（偏移 {}）",
            other,
            cur.pos
        ))),
    }
}

fn read_len(cur: &mut Cursor) -> Result<usize> {
    let n = cur.leb()?;
    if n < 0 {
        bail!("长度为负数（{}）", n);
    }
    let n = n as usize;
    // 先按剩余字节数卡一道，避免 `vec![0; huge]` 式的分配炸弹
    if n > MAX_STRING_BYTES || n > cur.remaining().saturating_add(1) {
        bail!(
            "字符串长度 {} 不合理（偏移 {}，剩余 {} 字节）",
            n,
            cur.pos,
            cur.remaining()
        );
    }
    Ok(n)
}

/// 写一个字符串（UTF-16 形态，与 `MetadataWriter.writeString` 一致）。
///
/// 为什么用 4 而不是 3：元数据块里 Java 只写 4，保持与真 jfr 一致方便对照；
/// 而且 UTF-16 对代理对（emoji）是无损的。
pub fn write_string(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(0),
        Some(s) => {
            out.push(4);
            let units: Vec<u16> = s.encode_utf16().collect();
            super::write_leb(out, units.len() as i64);
            for u in units {
                super::write_leb(out, u as i64);
            }
        }
    }
}

/// 读字符串池：`[varint 个数][字符串]*`。
pub fn read_pool(cur: &mut Cursor) -> Result<Vec<String>> {
    let count = cur.leb()?;
    if count < 0 {
        bail!("元数据字符串池大小是负数（{}）", count);
    }
    let count = count as usize;
    // 每个字符串至少 1 字节
    if count > cur.remaining() + 1 {
        bail!(
            "元数据字符串池声明 {} 个字符串，但只剩 {} 字节",
            count,
            cur.remaining()
        );
    }
    let mut v = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        v.push(read_string(cur, None)?.unwrap_or_default());
    }
    Ok(v)
}

/// 写字符串池。返回「字符串 → 下标」的映射（写元素树时要用）。
pub fn write_pool(out: &mut Vec<u8>, pool: &[String]) -> HashMap<String, i64> {
    super::write_leb(out, pool.len() as i64);
    let mut lookup = HashMap::with_capacity(pool.len() * 2);
    for (i, s) in pool.iter().enumerate() {
        lookup.insert(s.clone(), i as i64);
        write_string(out, Some(s));
    }
    lookup
}

/// 收集元素树里出现的全部字符串（去重，保持首次出现顺序）。
///
/// 顺序必须是确定的：池里的下标和元素树里的引用要对得上，且同样的输入要产出
/// 同样的文件（否则 diff 测试没法做）。
pub fn collect_strings(e: &Element, out: &mut Vec<String>, seen: &mut HashMap<String, ()>) {
    fn add(s: &str, out: &mut Vec<String>, seen: &mut HashMap<String, ()>) {
        if seen.insert(s.to_string(), ()).is_none() {
            out.push(s.to_string());
        }
    }
    add(&e.name, out, seen);
    for (k, v) in &e.attrs {
        add(k, out, seen);
        add(v, out, seen);
    }
    for c in &e.children {
        collect_strings(c, out, seen);
    }
}

/// 读一个元素（递归）。
pub fn read_element(cur: &mut Cursor, pool: &[String], depth: usize) -> Result<Element> {
    if depth > MAX_DEPTH * 4 {
        bail!("元数据元素树过深（>{} 层）", MAX_DEPTH * 4);
    }
    let name_idx = cur.leb()?;
    let name = pool_get(pool, name_idx)?;
    let mut e = Element {
        name,
        attrs: Vec::new(),
        children: Vec::new(),
    };
    let ac = cur.leb()?;
    if ac < 0 || ac as usize > cur.remaining() {
        bail!("元素 {} 的属性个数不合理：{}", e.name, ac);
    }
    for _ in 0..ac {
        let k = pool_get(pool, cur.leb()?)?;
        let v = pool_get(pool, cur.leb()?)?;
        e.attrs.push((k, v));
    }
    let cc = cur.leb()?;
    if cc < 0 || cc as usize > cur.remaining() {
        bail!("元素 {} 的子元素个数不合理：{}", e.name, cc);
    }
    for _ in 0..cc {
        e.children.push(read_element(cur, pool, depth + 1)?);
    }
    Ok(e)
}

fn pool_get(pool: &[String], idx: i64) -> Result<String> {
    if idx < 0 {
        bail!("字符串下标是负数：{}", idx);
    }
    match pool.get(idx as usize) {
        Some(s) => Ok(s.clone()),
        None => Err(Error::new(format!(
            "字符串下标 {} 越界（池大小 {}）",
            idx,
            pool.len()
        ))),
    }
}

/// 写一个元素（递归）。
pub fn write_element(out: &mut Vec<u8>, e: &Element, lookup: &HashMap<String, i64>) -> Result<()> {
    let idx = |s: &str| -> Result<i64> {
        match lookup.get(s) {
            Some(i) => Ok(*i),
            None => Err(Error::new(format!("元数据字符串池缺 {:?}", s))),
        }
    };
    super::write_leb(out, idx(&e.name)?);
    super::write_leb(out, e.attrs.len() as i64);
    for (k, v) in &e.attrs {
        super::write_leb(out, idx(k)?);
        super::write_leb(out, idx(v)?);
    }
    super::write_leb(out, e.children.len() as i64);
    for c in &e.children {
        write_element(out, c, lookup)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Cursor;

    #[test]
    fn string_encodings_roundtrip() {
        let mut b = Vec::new();
        write_string(&mut b, None);
        write_string(&mut b, Some(""));
        write_string(&mut b, Some("jdk.ExecutionSample"));
        write_string(&mut b, Some("中文 🚀 名字"));
        let mut c = Cursor::new(&b);
        assert_eq!(read_string(&mut c, None).unwrap(), None);
        assert_eq!(read_string(&mut c, None).unwrap(), Some(String::new()));
        assert_eq!(
            read_string(&mut c, None).unwrap(),
            Some("jdk.ExecutionSample".to_string())
        );
        assert_eq!(
            read_string(&mut c, None).unwrap(),
            Some("中文 🚀 名字".to_string())
        );
        assert!(c.at_end());
    }

    #[test]
    fn pool_and_element_roundtrip() {
        let mut root = Element::new("root");
        let md = root.new_child("metadata");
        let mut cl = md.new_child("class");
        cl.add_attr("name", "jdk.ExecutionSample");
        cl.add_attr("id", "100");
        let f = cl.new_child("field");
        f.add_attr("name", "startTime");
        f.add_attr("class", "20");
        let region = root.new_child("region");
        region.add_attr("gmtOffset", "28800000");

        let mut strings = Vec::new();
        let mut seen = HashMap::new();
        collect_strings(&root, &mut strings, &mut seen);

        let mut b = Vec::new();
        let lookup = write_pool(&mut b, &strings);
        write_element(&mut b, &root, &lookup).unwrap();

        let mut c = Cursor::new(&b);
        let pool = read_pool(&mut c).unwrap();
        assert_eq!(pool, strings);
        let g = read_element(&mut c, &pool, 0).unwrap();
        assert_eq!(g, root);
        assert!(c.at_end());
    }

    #[test]
    fn huge_lengths_are_rejected() {
        // 声称 0x7fffffff 个字符
        let b = [4u8, 0xff, 0xff, 0xff, 0xff, 0x07];
        let mut c = Cursor::new(&b);
        assert!(read_string(&mut c, None).is_err());
    }

    #[test]
    fn deep_tree_is_rejected() {
        // 构造 200 层嵌套
        let mut root = Element::new("root");
        {
            let mut cur = &mut root;
            for _ in 0..200 {
                cur = cur.new_child("x");
            }
        }
        let mut strings = Vec::new();
        let mut seen = HashMap::new();
        collect_strings(&root, &mut strings, &mut seen);
        let mut b = Vec::new();
        let lookup = write_pool(&mut b, &strings);
        write_element(&mut b, &root, &lookup).unwrap();
        let mut c = Cursor::new(&b);
        let pool = read_pool(&mut c).unwrap();
        assert!(read_element(&mut c, &pool, 0).is_err());
    }
}

/// 诊断输出：只有设置了 `VMLESS_JFR_TRACE=1` 才会往 stderr 写一行。
///
/// 为什么留着它：`.jfr` 出问题时（某个字段莫名变成 null），唯一的办法是看
/// 「哪一次查找失败了」。正式路径上它一个字节都不写，所以不影响输出对比。
pub fn trace(msg: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var("VMLESS_JFR_TRACE").map(|v| v != "0").unwrap_or(false));
    if on {
        eprintln!("[trace] {}", msg);
    }
}
