//! class 文件的数据模型与编解码入口。
//!
//! 数据模型（`ClassFile` / `Member` / `CpEntry` / `Attribute`）、错误类型，
//! 以及运行期共用的文本工具（`ret_of` / `java_double_to_string`）留在本模块；
//! 编解码按职责拆到 `pool` / `attr` / `read` / `write`，对外仍暴露 `parse` / `write`。

use std::fmt;

pub mod attr;
pub mod pool;
pub mod read;
pub mod write;

pub use read::parse;
pub use write::write;


pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Truncated(&'static str),
    BadMagic(u32),
    BadConstantPool(u8),
    Utf8(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(w) => write!(f, "unexpected end of class file while reading {w}"),
            Error::BadMagic(m) => write!(f, "bad magic 0x{m:08x}"),
            Error::BadConstantPool(t) => write!(f, "bad constant pool tag {t}"),
            Error::Utf8(s) => write!(f, "utf8 error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq)]
pub enum CpEntry {
    /// 原始 Modified UTF-8 字节，保持逐字节一致
    Utf8(Vec<u8>),
    Integer(u32),
    Float(u32),
    Long(u64),
    Double(u64),
    Class(u16),
    String(u16),
    FieldRef(u16, u16),
    MethodRef(u16, u16),
    InterfaceMethodRef(u16, u16),
    NameAndType(u16, u16),
    MethodHandle(u8, u16),
    MethodType(u16),
    Dynamic(u16, u16),
    InvokeDynamic(u16, u16),
    Module(u16),
    Package(u16),
}

impl CpEntry {
    /// long/double 在常量池里占两个槽位
    pub fn slots(&self) -> usize {
        match self {
            CpEntry::Long(_) | CpEntry::Double(_) => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attribute {
    pub name_index: u16,
    pub info: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Member {
    pub access_flags: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    /// 索引 0 未使用；long/double 后的槽位为 None
    pub constant_pool: Vec<Option<CpEntry>>,
    pub access_flags: u16,
    pub this_class: u16,
    pub super_class: u16,
    pub interfaces: Vec<u16>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
    pub attributes: Vec<Attribute>,
}

impl ClassFile {
    pub fn major(&self) -> u16 {
        self.major_version
    }

    /// 常量池里的 Utf8 文本（用于调试/后续符号表）
    pub fn utf8(&self, index: u16) -> Option<&[u8]> {
        match self.constant_pool.get(index as usize) {
            Some(Some(CpEntry::Utf8(b))) => Some(b),
            _ => None,
        }
    }

    pub fn this_class_name(&self) -> Option<String> {
        let idx = match self.constant_pool.get(self.this_class as usize) {
            Some(Some(CpEntry::Class(n))) => *n,
            _ => return None,
        };
        self.utf8(idx).map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

// ------------------------------------------------------- double 的文本形式

/// 方法描述符的返回类型字符（`)` 后面那一个）：`(J)V` → `V`，`()[I` → `[`
pub fn ret_of(desc: &str) -> Option<u8> {
    let bytes = desc.as_bytes();
    let close = bytes.iter().position(|b| *b == b')')?;
    bytes.get(close + 1).copied()
}

/// `Double.toString` 的等价实现（解释器与 JIT 的 `println(double)` 共用）。
///
/// Java 的规则（JDK 19 起的规范）：
/// - NaN / Infinity / -Infinity 用这三个字面量；
/// - 0.0 与 -0.0 保留符号，写成 `0.0` / `-0.0`；
/// - 十进制数字取「能唯一还原该值的最短表示」，**末位平局按取偶**；
/// - 值落在 `[1e-3, 1e7)` 用普通写法，且**至少保留一位小数**（`100.0`、`0.001`）；
/// - 否则用科学计数法 `d.ddd...E<指数>`，尾数同样至少一位小数（`1.0E7`、`4.9E-324`）。
pub fn java_double_to_string(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let a = d.abs();
    let (digits, exp) = shortest_digits(17, a, a < f64::MIN_POSITIVE, |k| {
        format!("{:.*e}", k - 1, a).parse::<f64>().ok() == Some(a)
    }, |k| format!("{:.*e}", k - 1, a));
    render_fp(&digits, exp, d < 0.0)
}

/// `Float.toString` 的等价实现（解释器与 JIT 的 `println(float)` 共用）。
///
/// 规则与 double 同构，只是「有效数字」要对 **f32** 求（`Float.MAX_VALUE` →
/// `3.4028235E38`、`1.0f/3.0f` → `0.33333334`）。
pub fn java_float_to_string(v: f32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let a = v.abs();
    let (digits, exp) = shortest_digits(9, a as f64, a < f32::MIN_POSITIVE, |k| {
        format!("{:.*e}", k - 1, a).parse::<f32>().ok() == Some(a)
    }, |k| format!("{:.*e}", k - 1, a));
    render_fp(&digits, exp, v < 0.0)
}

/// 求「最短且能唯一还原」的十进制有效数字与指数。
///
/// **不能用 Rust 的 `{:e}`**：它虽然也是「最短往返」，但末位平局时往上取
/// （`200.078125` → `200.07813`），而 Java 取偶数（`200.07812`）—— 实测 60 万个
/// f32 位型里有 1184 个因此不同。这里改成逐位试：从 1 位试到 `max_k` 位，取第一个
/// 能还原成同一个值的 `{:.k-1e}`（Rust 的定精度格式是正确舍入、末位取偶）。
///
/// `subnormal` 时 Java 会**多留一位**（最小 subnormal 是 `1.4E-45` 而不是
/// `1.0E-45`），所以 k 试到 1 时再取 2 位；这条规则是拿 160 万个位型与 JDK 25
/// 逐字符比对定下来的（0 处不同）。
fn shortest_digits(
    max_k: usize,
    _abs: f64,
    subnormal: bool,
    roundtrips: impl Fn(usize) -> bool,
    sci: impl Fn(usize) -> String,
) -> (Vec<char>, i32) {
    let mut k = 1usize;
    let mut digits: Vec<char> = Vec::new();
    let mut exp = 0i32;
    while k <= max_k {
        if roundtrips(k) {
            let s = sci(k);
            let (m, e) = s.split_once('e').expect("{:e} 应含指数");
            digits = m.chars().filter(|c| *c != '.').collect();
            exp = e.parse().expect("{:e} 的指数应可解析");
            break;
        }
        k += 1;
    }
    if digits.is_empty() {
        return (vec!['0'], 0); // 理论上到不了：最坏情况 max_k 位总能还原
    }
    // Java 在 subnormal 段至少写两位有效数字
    if subnormal && digits.len() == 1 {
        let s = sci(2);
        let (m, e) = s.split_once('e').expect("{:e} 应含指数");
        digits = m.chars().filter(|c| *c != '.').collect();
        exp = e.parse().expect("{:e} 的指数应可解析");
    }
    while digits.len() > 1 && *digits.last().unwrap() == '0' {
        digits.pop();
    }
    (digits, exp)
}

/// 把 (有效数字, 指数) 渲染成 Java 的文本形态：`[1e-3, 1e7)` 用普通写法，否则科学计数法。
fn render_fp(digits: &[char], exp: i32, neg: bool) -> String {
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-3..7).contains(&exp) {
        // 普通写法：小数点在 digits 里第 exp+1 位之后
        let int_len = exp + 1;
        if int_len > 0 {
            for i in 0..int_len {
                out.push(*digits.get(i as usize).unwrap_or(&'0'));
            }
        } else {
            out.push('0');
        }
        out.push('.');
        if int_len < 0 {
            for _ in 0..-int_len {
                out.push('0');
            }
        }
        let frac: String = if int_len >= 0 {
            digits.iter().skip(int_len as usize).collect()
        } else {
            digits.iter().collect()
        };
        if frac.is_empty() {
            out.push('0');
        } else {
            out.push_str(&frac);
        }
    } else {
        // 科学计数法
        out.push(digits[0]);
        out.push('.');
        if digits.len() > 1 {
            out.extend(digits.iter().skip(1));
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值来自 JDK 25 实测（`System.out.println(float)` 的原始输出）
    #[test]
    fn float_text_matches_java() {
        let cases: &[(f32, &str)] = &[
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.33333334"),
            (1.0e10, "1.0E10"),
            (f32::MAX, "3.4028235E38"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (1.0e-3, "0.001"),
            (1.0e-4, "1.0E-4"),
            (1.0e7, "1.0E7"),
            (9999999.0, "9999999.0"),
            (123456.0, "123456.0"),
            (2.5e-10, "2.5E-10"),
            (6.02e23, "6.02E23"),
            (f32::INFINITY, "Infinity"),
            (f32::NEG_INFINITY, "-Infinity"),
            (f32::NAN, "NaN"),
            (-3.5, "-3.5"),
            // 最小正规数（`Float.MIN_NORMAL` 量级）
            (1.17549435e-38, "1.1754944E-38"),
            // 末位平局：Java 取偶（200.078125 恰好是 f32 可精确表示的）
            (200.078125, "200.07812"),
            (423.140625, "423.14062"),
            // 16777217 在 f32 里就是 16777216
            (16777217.0, "1.6777216E7"),
        ];
        for (v, want) in cases {
            assert_eq!(&java_float_to_string(*v), want, "值 {v:?}");
        }
    }

    /// 期望值来自 JDK 25 实测（`System.out.println(double)` 的原始输出）
    #[test]
    fn double_text_matches_java() {
        let cases: &[(f64, &str)] = &[
            (3.5, "3.5"),
            (5050.0, "5050.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0 / 3.0, "0.3333333333333333"),
            (2.0, "2.0"),
            (0.5, "0.5"),
            (100.0, "100.0"),
            (1.0, "1.0"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0e7, "1.0E7"),
            (9999999.0, "9999999.0"),
            (1.0e-3, "0.001"),
            (1.0e-4, "1.0E-4"),
            (123456789.0, "1.23456789E8"),
            (1.0e20, "1.0E20"),
            (1.0e300, "1.0E300"),
            (f64::MAX, "1.7976931348623157E308"),
            (1234567.0, "1234567.0"),
            (-3.5, "-3.5"),
            (1.0e-7, "1.0E-7"),
            (6.02e23, "6.02E23"),
            (2.5e-10, "2.5E-10"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (f64::NAN, "NaN"),
            (7.0 % 3.0, "1.0"),
            (-7.0 % 3.0, "-1.0"),
        ];
        for (v, want) in cases {
            assert_eq!(&java_double_to_string(*v), want, "值 {v:?}");
        }
    }

    fn roundtrip(bytes: &[u8]) -> Vec<u8> {
        let cf = parse(bytes).expect("parse");
        write(&cf)
    }

    #[test]
    fn roundtrip_is_byte_identical() {
        // 用 javac 编译出来的 class 文件（仓库内 samples/）
        let dir = std::path::Path::new("samples");
        if !dir.exists() {
            return;
        }
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("class") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            let out = roundtrip(&bytes);
            assert_eq!(bytes, out, "往返不一致: {}", path.display());
            checked += 1;
        }
        assert!(checked > 0, "samples/ 里没有 class 文件");
    }

    #[test]
    fn reads_this_class_name() {
        let dir = std::path::Path::new("samples");
        if !dir.exists() {
            return;
        }
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("class") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            let cf = parse(&bytes).expect("parse");
            assert!(cf.this_class_name().is_some());
            assert_eq!(cf.major(), 69);
        }
    }
}
