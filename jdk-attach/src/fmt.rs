//! 文本格式 —— **逐字符对齐真 JDK 的输出**。
//!
//! 所有格式串都是从真 JDK 25 的输出里量出来的（`tests/format_align.rs` 会再核一遍）：
//! 用真实工具跑一次、把输出存成期望值，再让这里生成同样的骨架去比。

use crate::heap::ClassHisto;
use crate::sys;

/// `jmap -histo` / `jcmd GC.class_histogram` 的表。
///
/// 真 jmap 的表头（模块信息版）：
/// ```text
///  num     #instances         #bytes  class name (module)
/// -------------------------------------------------------
///    1:       2273381       54561144  net.minecraft.core.BlockPos (minecraft@26.3)
/// ```
/// 列宽固定为 `%4d: %12d %12d  `，与 HotSpot 的 `heapInspection.cpp` 一致。
pub fn histogram_table(v: &[ClassHisto]) -> String {
    let mut s = String::with_capacity(64 + v.len() * 48);
    let has_module = v.iter().any(|h| h.module.is_some());
    if has_module {
        s.push_str(" num     #instances         #bytes  class name (module)\n");
        s.push_str("-------------------------------------------------------\n");
    } else {
        s.push_str(" num     #instances         #bytes  class name\n");
        s.push_str("------------------------------------------------------\n");
    }
    for (i, h) in v.iter().enumerate() {
        match &h.module {
            Some(m) => s.push_str(&format!(
                "{:>4}: {:>12} {:>12}  {} ({})\n",
                i + 1,
                h.instances,
                h.bytes,
                h.class_name,
                m
            )),
            None => s.push_str(&format!(
                "{:>4}: {:>12} {:>12}  {}\n",
                i + 1,
                h.instances,
                h.bytes,
                h.class_name
            )),
        }
    }
    s
}

/// Java `Properties.store()` 的文本形式（`jinfo` / `VM.system_properties` 用）。
///
/// 规则照 `java.util.Properties.store` 的转义：`\\ : =` 和行首空格要转义；
/// 第一行是 `#<日期>` 注释。
pub fn properties_text(props: &[(String, String)]) -> String {
    let mut s = String::new();
    s.push_str(&java_date_comment());
    s.push('\n');
    let mut sorted: Vec<&(String, String)> = props.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in sorted {
        s.push_str(&escape_prop(k));
        s.push('=');
        s.push_str(&escape_prop(v));
        s.push('\n');
    }
    s
}

/// `#Tue Nov 17 10:11:14 CST 2026`
pub fn java_date_comment() -> String {
    match sys::local_time() {
        Some(t) => format!(
            "#{} {} {:02} {:02}:{:02}:{:02} {} {}",
            WEEKDAY[(t.dow % 7) as usize],
            MONTH[((t.month as usize).saturating_sub(1)) % 12],
            t.day,
            t.hour,
            t.min,
            t.sec,
            sys::tz_abbrev(),
            t.year
        ),
        None => "#unknown date".to_string(),
    }
}

pub const WEEKDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const MONTH: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn escape_prop(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '=' => out.push_str("\\="),
            ':' => out.push_str("\\:"),
            ' ' if i == 0 => out.push_str("\\ "),
            '#' if i == 0 => out.push_str("\\#"),
            '!' if i == 0 => out.push_str("\\!"),
            _ => out.push(c),
        }
    }
    out
}

/// `jmap -clstats` 的表。
///
/// 真输出：
/// ```text
/// ClassLoader         Parent              CLD*               Classes   ChunkSz   BlockSz  Type
/// 0x0000000028140738  0x0000000028140738  0x000001a24f479fe0       7     37888     29416  java.net.URLClassLoader
/// ```
pub fn clstats_table(v: &[crate::heap::LoaderStat]) -> String {
    let mut s = String::new();
    s.push_str("ClassLoader         Parent              CLD*               Classes   ChunkSz   BlockSz  Type
");
    let mut total = (0u64, 0u64, 0u64);
    for l in v {
        s.push_str(&format!(
            "0x{:016x}  0x{:016x}  0x{:016x} {:7} {:9} {:9}  {}{}
",
            l.class_loader,
            l.parent,
            l.cld,
            l.classes,
            l.chunk_size,
            l.block_size,
            l.loader_type,
            if l.hidden { " (hidden)" } else { "" }
        ));
        total.0 += l.classes;
        total.1 += l.chunk_size;
        total.2 += l.block_size;
    }
    // 尾行与真 jmap 一致：`Total = <loader 数>` 补齐到 55 列，然后 Classes / ChunkSz / BlockSz
    // 各 11 宽右对齐（真输出里 Classes 的右边界在 66、ChunkSz 77、BlockSz 88）
    s.push_str(&format!(
        "{:<55}{:>11}{:>11}{:>11}  
",
        format!("Total = {}", v.len()),
        total.0,
        total.1,
        total.2
    ));
    s.push_str("ChunkSz: Total size of all allocated metaspace chunks
");
    s.push_str("BlockSz: Total size of all allocated metaspace blocks (each chunk has several blocks)
");
    s
}

/// 秒（3 位小数）—— jstat 的时间列都是这个格式。
pub fn secs3(ns: i64) -> String {
    format!("{:.3}", ns as f64 / 1e9)
}

/// KB（1 位小数）—— jstat 的容量列。
pub fn kb1(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_header_matches_jmap() {
        let v = vec![ClassHisto { class_name: "A".into(), module: None, instances: 1, bytes: 8 }];
        let t = histogram_table(&v);
        // 这两行是从真 jmap 输出里抄的（模块版）
        let with_mod = vec![ClassHisto { class_name: "A".into(), module: Some("m".into()), instances: 1, bytes: 8 }];
        let t2 = histogram_table(&with_mod);
        assert!(t.starts_with(" num     #instances         #bytes  class name\n"));
        assert!(t2.starts_with(" num     #instances         #bytes  class name (module)\n"));
        assert!(t2.contains("-------------------------------------------------------\n"));
        assert!(t.contains("------------------------------------------------------\n"));
        assert_eq!(t.lines().count(), 3);
        assert!(t.lines().nth(2).unwrap().starts_with("   1:"));
    }

    #[test]
    fn property_escaping() {
        // Java 的 Properties.store 会转义 `:` 和 `\`（真 jinfo 输出里就是这样：
        // `java.home=C\:\\Program Files\\...`），所以 "C:\x" → "C\:\\x"
        assert_eq!(escape_prop("C:\\x"), "C\\:\\\\x");
        assert_eq!(escape_prop("a=b"), "a\\=b");
        assert_eq!(escape_prop(" leading"), "\\ leading");
    }

    #[test]
    fn properties_sorted() {
        let p = vec![("b".into(), "2".into()), ("a".into(), "1".into())];
        let t = properties_text(&p);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with('#'));
        assert_eq!(lines[1], "a=1");
        assert_eq!(lines[2], "b=2");
    }

    #[test]
    fn secs3_and_kb1() {
        assert_eq!(secs3(1_500_000_000), "1.500");
        assert_eq!(kb1(2048), "2.0");
    }
}
