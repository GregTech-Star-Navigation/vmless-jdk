//! `jfr` —— `.jfr` 文件的 `summary` / `print` / `metadata`。
//!
//! # 输出版式是照着 `jdk.jfr.internal.tool.*` 抄的
//!
//! | 子命令 | 权威实现 |
//! | --- | --- |
//! | `summary` | `Summary.printInformation` |
//! | `print` | `Print` + `PrettyWriter` + `EventPrintWriter.getValue` |
//! | `metadata` | `Metadata.printInformation` + `tool.MetadataWriter` |
//!
//! 「对得上」不是看着像：列宽、缩进（每层 2 空格）、空行位置、时间格式
//! （`HH:mm:ss.SSS (yyyy-MM-dd)`）、时长舍入（3 位有效数字）都逐个对齐，
//! 因为不一致的地方在 diff 里会立刻现形。

use std::collections::HashMap;
use std::io::Write;

#[path = "jfr/record.rs"]
mod record;

use vmless_jfr::format::chunk::ChunkHeader;
use vmless_jfr::format::cp::ConstantPool;
use vmless_jfr::model::{FieldDef, Metadata, Value, TYPE_CLASS, TYPE_CLASS_LOADER, TYPE_METHOD, TYPE_STACK_FRAME, TYPE_THREAD};
use vmless_jfr::reader::Recording;

const USAGE: &str = "\
用法: jfr <命令> [选项] <文件>

命令:
  summary   <文件>                       录制文件概览（各事件类型的条数与字节数）
  print     [选项] <文件>                把事件按时间顺序打印出来
  metadata  [选项] [<文件>]              打印事件元数据（标签、字段布局）
  record    <文件> [--pid N] [--events N]  录制（不给 --pid 时生成演示事件，自检写出的文件真 jfr 能读）
  inspect   <文件> [--type 关键字]        诊断：把类型表连同 simpleType/constantPool 标志打出来
  version                                打印版本
  help                                   打印本帮助

print 的选项:
  --events <过滤>        只打印名字匹配的事件（逗号分隔，支持 * 通配）
  --categories <过滤>    只打印分类匹配的事件
  --stack-depth <n>      栈帧最多打印几层（默认 5）
  --exact                数字与时间戳打全精度
  --json                 输出 JSON

metadata 的选项:
  --events <过滤>        只打印匹配的事件类型
  --categories <过滤>    只打印匹配分类的事件类型
  --ids                  每行带上类型 id（调试用）

例子:
  jfr summary recording.jfr
  jfr print --events jdk.ExecutionSample recording.jfr
  jfr print --events 'jdk.GC*' --stack-depth 8 recording.jfr
  jfr metadata --events jdk.ThreadStart recording.jfr
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&argv);
    std::process::exit(code);
}

fn run(argv: &[String]) -> i32 {
    // `--stats` 可以出现在任意位置：它只是让收尾多打一行耗时 / 峰值内存
    let want_stats = argv.iter().any(|a| a == "--stats");
    let filtered: Vec<String> = argv.iter().filter(|a| *a != "--stats").cloned().collect();
    let t0 = std::time::Instant::now();
    let code = run_inner(&filtered);
    if want_stats {
        eprintln!(
            "stats: elapsed={:.3}s peak_rss={}",
            t0.elapsed().as_secs_f64(),
            match peak_rss() {
                Some(b) => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
                None => "n/a".to_string(),
            }
        );
    }
    code
}

fn run_inner(argv: &[String]) -> i32 {
    if argv.is_empty() {

        eprintln!("{}", USAGE);
        return 1;
    }
    let cmd = argv[0].as_str();
    match cmd {
        "help" | "-h" | "--help" => {
            print!("{}", USAGE);
            0
        }
        "version" | "--version" | "-v" => {
            println!("vmless-jfr version {}", vmless_jfr::VERSION);
            0
        }
        "summary" => cmd_summary(&argv[1..]),
        "print" => cmd_print(&argv[1..]),
        "metadata" => cmd_metadata(&argv[1..]),
        "inspect" => cmd_inspect(&argv[1..]),
        "record" => record::run(&argv[1..]),
        other => {
            eprintln!("错误: 未知命令 {:?}\n{}", other, USAGE);
            1
        }
    }
}

fn fail(msg: &str) -> i32 {
    eprintln!("错误: {}", msg);
    1
}

// ============================ summary ============================

fn cmd_summary(args: &[String]) -> i32 {
    let file = match args.iter().rev().find(|a| !a.starts_with("--")) {
        Some(f) => f,
        None => return fail("summary 需要一个 .jfr 文件"),
    };
    let rec = match Recording::open(file) {
        Ok(r) => r,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let s = match rec.summary() {
        Ok(s) => s,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    // 表头走平台行分隔符（`println`），表格行走字面 LF（`printf("...\n")`）。
    // 真 jfr 的 summary 就是这么分裂的，想逐字节对齐就得跟。
    let mut o = String::new();
    o.push_str(nl());
    o.push_str(&format!(" Version: {}.{}{}", s.major, s.minor, nl()));
    o.push_str(&format!(" Chunks: {}{}", s.chunks, nl()));
    o.push_str(&format!(
        " Start: {} (UTC){}",
        fmt_utc_datetime(s.start_nanos),
        nl()
    ));
    o.push_str(&format!(
        " Duration: {} s{}",
        (s.duration_nanos + 500_000_000) / 1_000_000_000,
        nl()
    ));
    o.push_str(nl());

    let header = "      Count  Size (bytes) ";
    let type_header = " Event Type";
    let min_width = s.name_width.max(type_header.len());
    o.push_str(type_header);
    o.push_str(&" ".repeat(min_width - type_header.len()));
    o.push_str(header);
    o.push_str(nl());
    o.push_str(&"=".repeat(min_width + header.len()));
    o.push_str(nl());
    for r in &s.rows {
        o.push_str(&format!(
            " {:<width$}{:>10}  {:>12}\n",
            r.name,
            r.count,
            r.size,
            width = min_width
        ));
    }
    out(&o);
    0
}

// ============================ print ============================

#[derive(Debug, Clone, Default)]
struct PrintOptions {
    events: Option<String>,
    categories: Option<String>,
    stack_depth: usize,
    exact: bool,
    json: bool,
    file: Option<String>,
}

fn parse_print_args(args: &[String]) -> Result<PrintOptions, String> {
    let mut o = PrintOptions {
        stack_depth: 5,
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--events" | "--event" => {
                if o.events.is_some() {
                    return Err("用 --events a,b,c 一次指定多个事件".into());
                }
                i += 1;
                o.events = Some(
                    args.get(i)
                        .ok_or_else(|| "--events 后面缺少过滤条件".to_string())?
                        .clone(),
                );
            }
            "--categories" | "--category" => {
                if o.categories.is_some() {
                    return Err("用 --categories a,b,c 一次指定多个分类".into());
                }
                i += 1;
                o.categories = Some(
                    args.get(i)
                        .ok_or_else(|| "--categories 后面缺少过滤条件".to_string())?
                        .clone(),
                );
            }
            "--stack-depth" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "--stack-depth 后面缺少数字".to_string())?;
                o.stack_depth = v
                    .parse()
                    .map_err(|_| format!("--stack-depth 不是合法整数: {}", v))?;
            }
            "--exact" => o.exact = true,
            "--json" => o.json = true,
            "--xml" => return Err("暂不支持 --xml（用 --json）".into()),
            other if other.starts_with("--") => {
                return Err(format!("未知选项 {}", other));
            }
            other => {
                if o.file.is_some() {
                    return Err(format!("多余的参数 {}", other));
                }
                o.file = Some(other.to_string());
            }
        }
        i += 1;
    }
    Ok(o)
}

fn cmd_print(args: &[String]) -> i32 {
    let opts = match parse_print_args(args) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let file = match &opts.file {
        Some(f) => f.clone(),
        None => return fail("print 需要一个 .jfr 文件"),
    };
    let rec = match Recording::open(&file) {
        Ok(r) => r,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let mut writer = Printer::new(opts.clone());
    match writer.run(&rec) {
        Ok(text) => {
            out(&text);
            0
        }
        Err(e) => fail(&format!("{}: {}", file, e)),
    }
}

// ============================ metadata ============================

fn cmd_metadata(args: &[String]) -> i32 {
    let mut events: Option<String> = None;
    let mut categories: Option<String> = None;
    let mut show_ids = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--events" | "--event" => {
                i += 1;
                events = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--categories" | "--category" => {
                i += 1;
                categories = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--ids" => show_ids = true,
            other if other.starts_with("--") => return fail(&format!("未知选项 {}", other)),
            other => file = Some(other.to_string()),
        }
        i += 1;
    }
    let file = match file {
        Some(f) => f,
        None => return fail("metadata 需要一个 .jfr 文件（本工具没有「读取当前 JDK 元数据」的模式）"),
    };
    let rec = match Recording::open(&file) {
        Ok(r) => r,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let cd = match rec.chunk_data(0) {
        Ok(c) => c,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let meta = &cd.metadata;

    let mut types: Vec<&vmless_jfr::model::TypeDef> = meta.types.iter().collect();
    types.sort_by(|a, b| type_cmp(meta, a, b));

    let ev_filter = events.as_deref().map(Filter::new);
    let cat_filter = categories.as_deref().map(Filter::new);

    let mut o = String::new();
    for t in types {
        if ev_filter.is_some() || cat_filter.is_some() {
            // 有过滤条件时只处理事件类型（与真 jfr 一致）
            if !t.is_event() {
                continue;
            }
            let ok_ev = ev_filter.as_ref().map(|f| f.matches_name(&t.name)).unwrap_or(true);
            let ok_cat = cat_filter
                .as_ref()
                .map(|f| filter_matches_categories(t, f))
                .unwrap_or(true);
            if !(ok_ev && ok_cat) {
                continue;
            }
        }
        write_type(&mut o, meta, t, show_ids);
    }
    out_text(&o);
    0
}

/// `Metadata.TypeComparator`
fn type_cmp(_meta: &Metadata, a: &vmless_jfr::model::TypeDef, b: &vmless_jfr::model::TypeDef) -> std::cmp::Ordering {
    fn group(t: &vmless_jfr::model::TypeDef) -> i32 {
        match t.super_type.as_deref() {
            None => 1,
            Some(s) if s == vmless_jfr::model::SUPER_ANNOTATION => 3,
            Some(s) if s == vmless_jfr::model::SUPER_SETTING => 4,
            Some(s) if s == vmless_jfr::model::SUPER_EVENT => 5,
            Some(_) => 2,
        }
    }
    let (g1, g2) = (group(a), group(b));
    if g1 != g2 {
        return g1.cmp(&g2);
    }
    let pkg = |n: &str| match n.rfind('.') {
        Some(i) => n[..=i].to_string(),
        None => String::new(),
    };
    let (p1, p2) = (pkg(&a.name), pkg(&b.name));
    if p1 == p2 {
        return a.name.cmp(&b.name);
    }
    // 让 jdk.jfr 包排在最前面，用户自定义事件落最后
    if a.super_type.as_deref() == Some(vmless_jfr::model::SUPER_EVENT) {
        if p1 == "jdk.jfr." {
            return std::cmp::Ordering::Less;
        }
        if p2 == "jdk.jfr." {
            return std::cmp::Ordering::Greater;
        }
    }
    p1.cmp(&p2)
}

fn write_type(o: &mut String, meta: &Metadata, t: &vmless_jfr::model::TypeDef, show_ids: bool) {
    if show_ids {
        o.push_str(&format!("// id: {}\n", t.id));
    }
    let comment_index = t.name.chars().count() + 10;
    if t.name.contains('.') {
        o.push_str(&format!("@Name(\"{}\")\n", t.name));
    }
    write_annotations(o, meta, &t.annotations, comment_index, show_ids, 0);
    o.push_str(&format!("class {}", t.simple_name()));
    if let Some(st) = &t.super_type {
        o.push_str(&format!(" extends {}", st));
    }
    o.push_str(" {\n");
    let mut first = true;
    for f in &t.fields {
        if !first {
            o.push('\n');
        }
        first = false;
        write_annotations(o, meta, &f.annotations, comment_index, show_ids, 2);
        o.push_str("  ");
        // setting 类型的字段前面要加 static
        if let Some(ft) = meta.type_by_id(f.type_id) {
            if ft.super_type.as_deref() == Some(vmless_jfr::model::SUPER_SETTING) {
                o.push_str("static ");
            }
        }
        let tn = field_type_name(meta, f.type_id);
        o.push_str(&simple_name(&tn));
        if f.dimension > 0 {
            o.push_str("[]");
        }
        o.push_str(&format!(" {};", f.name));
        if show_ids {
            let col = 2 + simple_name(&tn).chars().count() + if f.dimension > 0 { 2 } else { 0 } + 1 + f.name.chars().count() + 1;
            if col > comment_index {
                o.push_str("  ");
            } else {
                for _ in col..comment_index {
                    o.push(' ');
                }
            }
            o.push_str(&format!(" // id={}\n", f.type_id));
        } else {
            o.push('\n');
        }
    }
    o.push_str("}\n\n");
}

fn write_annotations(
    o: &mut String,
    meta: &Metadata,
    anns: &[vmless_jfr::model::AnnotationDef],
    comment_index: usize,
    show_ids: bool,
    indent: usize,
) {
    for a in anns {
        if a.type_name == "jdk.jfr.Name" {
            continue;
        }
        o.push_str(&" ".repeat(indent));
        o.push('@');
        o.push_str(a.simple_name());
        // 注解类型里声明的字段决定打印哪些值、按什么顺序
        let fields: Vec<FieldDef> = meta
            .type_by_id(a.type_id)
            .map(|t| t.fields.clone())
            .unwrap_or_default();
        if !fields.is_empty() {
            let mut parts: Vec<String> = Vec::new();
            for v in &fields {
                // `tool.MetadataWriter.textify`：字符串要带引号；数组取 `value-0..n`，
                // 单元素直接铺开，多元素包成 `{a, b}`。
                let is_string = meta.type_name(v.type_id) == "java.lang.String";
                let value = if v.dimension > 0 {
                    let mut vals: Vec<String> = Vec::new();
                    let mut i = 0;
                    while let Some(text) = a.get(&format!("{}-{}", v.name, i)) {
                        vals.push(quote_if(is_string, text));
                        i += 1;
                    }
                    if vals.len() == 1 {
                        vals.remove(0)
                    } else {
                        format!("{{{}}}", vals.join(", "))
                    }
                } else {
                    a.get(&v.name).map(|t| quote_if(is_string, t)).unwrap_or_default()
                };
                if fields.len() == 1 && v.name == "value" {
                    parts.push(value);
                } else {
                    parts.push(format!("{}={}", v.name, value));
                }
            }
            o.push_str(&format!("({})", parts.join(", ")));
            if show_ids {
                let col = indent + 1 + a.simple_name().chars().count() + 2
                    + parts.join(", ").chars().count() + 1;
                if col > comment_index {
                    o.push_str("  ");
                } else {
                    for _ in col..comment_index {
                        o.push(' ');
                    }
                }
                o.push_str(&format!(" // id={}\n", a.type_id));
                continue;
            }
        }
        o.push('\n');
    }
}

fn filter_matches_categories(t: &vmless_jfr::model::TypeDef, f: &Filter) -> bool {
    match t.annotation("Category") {
        Some(a) => {
            // value-0, value-1, ...
            let mut i = 0;
            while let Some(v) = a.get(&format!("value-{}", i)) {
                if f.matches_name(v) || f.matches_name(&simple_name(v)) {
                    return true;
                }
                i += 1;
            }
            false
        }
        None => false,
    }
}

// ============================ 事件打印器 ============================

/// 输出缓冲：缩进每层 2 空格（`StructuredWriter.indent()`）。
struct Out2 {
    buf: String,
    indent: usize,
}

impl Out2 {
    fn new() -> Self {
        Out2 {
            buf: String::new(),
            indent: 0,
        }
    }
    fn print(&mut self, s: &str) {
        self.buf.push_str(s);
    }
    fn println(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push_str(nl());
    }
    /// 结构换行：用**平台**行分隔符。
    ///
    /// 关键区别：`print(s)` 里的换行来自**字段值**（例如 `jdk.OSInformation.osVersion`
    /// 本身就带 `\n`），那些必须原样保留 LF —— 真 jfr 也是直接把字符串写出去的。
    /// 早期版本在最后统一把 `\n` 换成 CRLF，结果这一条事件的三行内容全被改写。
    fn nl(&mut self) {
        self.buf.push_str(nl());
    }
    fn print_indent(&mut self) {
        for _ in 0..self.indent {
            self.buf.push_str("  ");
        }
    }
    fn indent(&mut self) {
        self.indent += 1;
    }
    fn retract(&mut self) {
        self.indent = self.indent.saturating_sub(1);
    }
}

struct Printer {
    opts: PrintOptions,
    out: Out2,
    /// 时间戳/时长换算用的 chunk 头（每个 chunk 独立）
    header: ChunkHeader,
    /// 时区偏移（毫秒），来自元数据 region
    offset_ms: i64,
    ev_filter: Option<Filter>,
    cat_filter: Option<Filter>,
    /// 当前事件里 `arrayElements` 的值：`printOldObject` 打数组类名要用它
    cur_array_elements: i64,
    /// 当前正在打印的值来自哪个池键（0 = 不是引用）。
    ///
    /// `RecordedClassLoader.getId()` 要的是**这条引用用的键**，不是「内容相同的任意键」：
    /// 池里可能存在两个内容相同、键不同的条目（真文件里就有 key=0 和 key=2 的
    /// AppClassLoader），按内容反查会取到错的。
    cur_ref_key: i64,
    /// 当前 chunk 的常量池：事件里的 `Value::Ref` 按需解引用（不展开、不克隆）
    cp: vmless_jfr::format::cp::ConstantPool,
}

impl Printer {
    fn new(opts: PrintOptions) -> Printer {
        Printer {
            ev_filter: opts.events.as_deref().map(Filter::new),
            cat_filter: opts.categories.as_deref().map(Filter::new),
            header: ChunkHeader {
                offset: 0,
                major: 2,
                minor: 1,
                chunk_size: 0,
                constant_pool_pos: 0,
                metadata_pos: 0,
                start_nanos: 0,
                duration_nanos: 0,
                start_ticks: 0,
                ticks_per_second: 1_000_000_000,
                file_state: 0,
                flags: 0,
            },
            offset_ms: 0,
            cur_array_elements: 0,
            cur_ref_key: 0,
            cp: vmless_jfr::format::cp::ConstantPool::new(),
            opts,
            out: Out2::new(),
        }
    }

    fn run(&mut self, rec: &Recording) -> Result<String, String> {
        let cd = rec.chunk_data(0).map_err(|e| e.to_string())?;
        self.offset_ms = cd.metadata.gmt_offset_ms + cd.metadata.dst_ms;
        let meta = cd.metadata.clone();

        // 收集匹配的事件（真 jfr 也是先收集再排序打印）
        let mut evs: Vec<(i64, usize, vmless_jfr::model::Value)> = Vec::new();
        let _ = &mut evs;
        let mut collected: Vec<Collected> = Vec::new();
        let mut stream = rec.events().map_err(|e| e.to_string())?;
        while let Some(item) = stream.next() {
            let le = item.map_err(|e| e.to_string())?;
            let t = match meta.type_by_id(le.event.type_id) {
                Some(t) => t,
                None => continue,
            };
            if !self.accepts(&meta, t) {
                continue;
            }
            collected.push(Collected {
                header: le.header,
                event: le.event,
            });
        }
        // 每个 chunk 一份常量池（引用只在本 chunk 内有意义）
        let mut chunk_cps: Vec<vmless_jfr::format::cp::ConstantPool> = Vec::new();
        for i in 0..rec.chunks().len() {
            match rec.chunk_data(i) {
                Ok(cd) => chunk_cps.push(cd.cp),
                Err(_) => chunk_cps.push(vmless_jfr::format::cp::ConstantPool::new()),
            }
        }
        // 按结束时间出队（不是排序：用真 jfr 同款堆，见 Timeline 的注释）
        let mut timeline = Timeline::with_capacity(1_000_004);
        for (i, c) in collected.iter().enumerate() {
            let end_ticks = c.event.start_ticks.wrapping_add(c.event.duration_ticks);
            let end = c.header.timestamp_nanos(end_ticks);
            timeline.offer(end_key(end), i);
        }

        if self.opts.json {
            let empty_cp = ConstantPool::new();
            let cp = chunk_cps.first().unwrap_or(&empty_cp);
            self.print_json(cp, &meta, &collected);
            return Ok(std::mem::take(&mut self.out.buf));
        }
        let _ = timeline.len();
        while let Some((_, idx)) = timeline.poll() {
            let c = &collected[idx];
            self.header = c.header;
            let cp = &chunk_cps[idx.min(chunk_cps.len().saturating_sub(1))];
            self.print_event(cp, &meta, c);
        }
        Ok(std::mem::take(&mut self.out.buf))
    }

    fn accepts(&self, meta: &Metadata, t: &vmless_jfr::model::TypeDef) -> bool {
        if let Some(f) = &self.ev_filter {
            if !f.matches_name(&t.name) {
                return false;
            }
        }
        if let Some(f) = &self.cat_filter {
            if !filter_matches_categories(t, f) {
                return false;
            }
        }
        let _ = meta;
        true
    }

    fn print_event(&mut self, cp: &ConstantPool, meta: &Metadata, c: &Collected) {
        let t = match meta.type_by_id(c.event.type_id) {
            Some(t) => t,
            None => return,
        };
        // `printOldObject` 通过 currentEvent 读它（真 jfr 也是这么拿的）
        self.cur_array_elements = t
            .field_index("arrayElements")
            .and_then(|i| c.event.values.get(i))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        self.out.print(&t.name);
        self.out.print(" ");
        self.out.println("{");
        self.out.indent();
        // 栈帧字段留到最后打（isLateField）
        for (i, f) in t.fields.iter().enumerate() {
            if f.name == "stackTrace" || f.name == "eventThread" {
                continue;
            }
            let v = match c.event.values.get(i) {
                Some(v) => v,
                None => continue,
            };
            // duration 为 0 时不打印
            if f.name == "duration" && v.as_i64() == Some(0) {
                continue;
            }
            self.out.print_indent();
            self.out.print(&f.name);
            self.out.print(" = ");
            self.print_field_value(cp, meta, f, v);
        }
        // eventThread
        if let Some(idx) = t.field_index("eventThread") {
            if let Some(v) = c.event.values.get(idx) {
                if !cp.deref(v).is_null() {
                    self.out.print_indent();
                    self.out.print("eventThread = ");
                    self.print_value(cp, meta, None, v);
                    self.out.nl();
                }
            }
        }
        // stackTrace
        if let Some(idx) = t.field_index("stackTrace") {
            if let Some(v) = c.event.values.get(idx) {
                if !cp.deref(v).is_null() {
                    self.out.print_indent();
                    self.out.print("stackTrace = ");
                    self.print_stack_trace(cp, meta, v);
                    self.out.nl();
                }
            }
        }
        self.out.retract();
        self.out.print_indent();
        self.out.println("}");
        self.out.nl();
    }

    /// 字段值：先按注解决定语义（时长 / 时间戳 / 无符号），再走通版打印。
    fn print_field_value(&mut self, cp: &ConstantPool, meta: &Metadata, f: &FieldDef, v: &Value) {
        if f.has_annotation("Timespan") {
            if let Some(raw) = v.as_i64() {
                let unit = f
                    .annotation("Timespan")
                    .and_then(|a| a.value())
                    .unwrap_or("TICKS");
                match decode_duration(raw, unit, &self.header) {
                    Dur::Zero => self.out.println("0 s"),
                    Dur::MinValue => self.out.println("N/A"),
                    Dur::Forever => self.out.println("Forever"),
                    Dur::Nanos(ns) => {
                        if self.opts.exact {
                            self.out
                                .println(&format!("{:.9} s", ns as f64 / 1_000_000_000.0));
                        } else {
                            self.out.println(&fmt_duration(ns));
                        }
                    }
                }
                return;
            }
        }
        if f.has_annotation("Timestamp") {
            if let Some(raw) = v.as_i64() {
                let unit = f
                    .annotation("Timestamp")
                    .and_then(|a| a.value())
                    .unwrap_or("TICKS");
                let ns = decode_timestamp(raw, unit, &self.header);
                self.out.println(&fmt_local_time(ns, self.offset_ms, self.opts.exact));
                return;
            }
        }
        // `@Unsigned` 只是把值加宽（Java 的 EventPrintWriter.getValue），
        // **加宽完还要再走一遍注解格式化** —— `@Unsigned @DataAmount` 的真实输出是
        // 「256.0 kB」而不是裸数字。这里一开始 return 太早，导致 GC 事件的
        // objectSize / plabSize / allocated 全是裸数字。
        if f.has_annotation("Unsigned") {
            if let Value::Int(x) = v {
                let tn = meta.type_name(f.type_id);
                let u: u64 = match tn.as_str() {
                    "byte" => (*x as u8) as u64,
                    "short" => (*x as u16) as u64,
                    "int" => (*x as u32) as u64,
                    _ => *x as u64,
                };
                match self.numeric_format(f, u as i64) {
                    Some(text) => self.out.println(&text),
                    None => self.out.println(&u.to_string()),
                }
                return;
            }
        }
        self.print_value(cp, meta, Some(f), v);
        self.out.nl();
    }

    /// `PrettyWriter.printValue`
    fn print_value(&mut self, cp: &ConstantPool, meta: &Metadata, field: Option<&FieldDef>, v: &Value) {
        // 事件体里存的是池键，打印前解一层（返回引用，不克隆）
        self.cur_ref_key = match v {
            Value::Ref { key, .. } => *key,
            _ => 0,
        };
        let v = cp.deref(v);
        if v.is_null() {
            self.out.print("N/A");
            return;
        }
        if let Some((tid, _)) = v.as_struct() {
            let tn = meta.type_name(tid);
            match tn.as_str() {
                TYPE_THREAD => {
                    self.print_thread(cp, meta, v);
                    return;
                }
                TYPE_CLASS => {
                    self.print_class(cp, meta, v);
                    return;
                }
                TYPE_CLASS_LOADER => {
                    self.print_class_loader(cp, meta, v);
                    return;
                }
                TYPE_METHOD => {
                    self.out.print(&format_method(cp, meta, v));
                    return;
                }
                TYPE_STACK_FRAME => {
                    self.print_java_frame(cp, meta, v);
                    return;
                }
                _ => {}
            }
            // 字段声明类型是 `jdk.types.OldObject` 时走引用链打印（真 jfr 的特例）
            if let Some(f) = field {
                if meta.type_name(f.type_id) == "jdk.types.OldObject" {
                    self.print_old_object(cp, meta, v);
                    return;
                }
            }
            self.print_struct(cp, meta, v);
            return;
        }
        if let Value::Array(items) = v {
            self.print_array(cp, meta, items);
            return;
        }
        // N/A 的哨兵值（与真 jfr 一致）
        if let Value::Int(x) = v {
            if let Some(f) = field {
                let tn = meta.type_name(f.type_id);
                if tn == "long" && *x == i64::MIN {
                    self.out.print("N/A");
                    return;
                }
                if tn == "int" && *x == i32::MIN as i64 {
                    self.out.print("N/A");
                    return;
                }
            }
        }
        if let Value::Double(x) = v {
            if x.is_nan() || *x == f64::NEG_INFINITY {
                self.out.print("N/A");
                return;
            }
        }
        if let Value::Float(x) = v {
            if x.is_nan() || *x == f32::NEG_INFINITY {
                self.out.print("N/A");
                return;
            }
        }
        if let Some(f) = field {
            if self.print_formatted(meta, f, v) {
                return;
            }
        }
        match v {
            Value::Str(s) => self.out.print(&format!("\"{}\"", s)),
            Value::Int(x) => {
                let tn = field.map(|f| meta.type_name(f.type_id)).unwrap_or_default();
                if tn == "char" {
                    match char::from_u32((*x as u32) & 0xffff) {
                        Some(c) => self.out.print(&c.to_string()),
                        None => self.out.print(&x.to_string()),
                    }
                } else {
                    self.out.print(&x.to_string());
                }
            }
            Value::Float(x) => self.out.print(&fmt_float(*x as f64)),
            Value::Double(x) => self.out.print(&fmt_float(*x)),
            Value::Bool(b) => self.out.print(if *b { "true" } else { "false" }),
            _ => self.out.print("N/A"),
        }
    }

    /// 注解驱动的数值格式化（`PrettyWriter.printFormatted` 的数值部分）。
    ///
    /// 返回 `None` 表示「没有能应用的注解」，调用方按普通数字打印。
    fn numeric_format(&self, f: &FieldDef, n: i64) -> Option<String> {
        if f.has_annotation("Percentage") {
            let p = 100.0 * n as f64;
            return Some(if self.opts.exact {
                format!("{:.9}%", p)
            } else {
                format!("{:.2}%", p)
            });
        }
        if f.has_annotation("DataAmount") {
            let unit = f
                .annotation("DataAmount")
                .and_then(|a| a.value())
                .unwrap_or("BYTES");
            let bits = unit == "BITS";
            let freq = f.has_annotation("Frequency");
            return Some(format_memory(n, !bits, freq, self.opts.exact));
        }
        if f.has_annotation("MemoryAddress") {
            return Some(format!("0x{:08X}", n));
        }
        if f.has_annotation("Frequency") {
            return Some(format!("{} Hz", n));
        }
        None
    }

    /// `PrettyWriter.printFormatted`：注解驱动的单位格式化。返回 true 表示已输出。
    ///
    /// 顺序与真 jfr 一致：Percentage → DataAmount → MemoryAddress → Frequency。
    /// **Percentage 必须用浮点原值**：`jdk.ThreadCPULoad.user` 是 float，
    /// 0.0049642264 一旦被截成整数就变成 0，于是整行的 `0.02%` 变成 `0.00%`。
    fn print_formatted(&mut self, _meta: &Metadata, f: &FieldDef, v: &Value) -> bool {
        if f.has_annotation("Percentage") {
            if let Some(x) = v.as_f64() {
                let p = 100.0 * x;
                if self.opts.exact {
                    self.out.print(&format!("{:.9}%", p));
                } else {
                    self.out.print(&format!("{:.2}%", p));
                }
                return true;
            }
        }
        if f.has_annotation("DataAmount") || f.has_annotation("MemoryAddress") {
            if let Some(n) = v.as_i64() {
                if let Some(text) = self.numeric_format(f, n) {
                    self.out.print(&text);
                    return true;
                }
            }
            return false;
        }
        if f.has_annotation("Frequency") {
            let text = match v {
                Value::Int(n) => n.to_string(),
                Value::Float(x) => x.to_string(),
                Value::Double(x) => x.to_string(),
                _ => return false,
            };
            self.out.print(&format!("{} Hz", text));
            return true;
        }
        false
    }

    /// `PrettyWriter.printOldObject` + `printReferenceChain`。
    ///
    /// `jdk.OldObjectSample.object` 是 `jdk.types.OldObject`，它有一条**引用链**
    /// （`referrer` 一直往上指），真 jfr 专门换了一套打印方式：
    /// 类名（数组类型要按 `arrayElements` 展开成 `byte[262144]`）+ 描述，
    /// 然后逐级打 `[i] : 类名` / `字段名 : 类名`。
    fn print_old_object(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        self.out.println(" [");
        self.out.indent();
        self.out.print_indent();
        self.print_reference_chain(cp, meta, v);
        self.out.retract();
        self.out.print_indent();
        self.out.print("]");
    }

    fn print_reference_chain(&mut self, cp: &ConstantPool, meta: &Metadata, obj: &Value) {
        self.print_object_line(cp, meta, obj, self.cur_array_elements);
        let mut cur = sget(cp, meta, obj, "referrer").cloned();
        let mut guard = 0;
        while let Some(reference) = cur {
            if reference.is_null() {
                break;
            }
            guard += 1;
            if guard > 1000 {
                break; // 坏数据里的环，最多跟 1000 层
            }
            let skip = sget_i64(cp, meta, &reference, "skip").unwrap_or(0);
            if skip > 0 {
                self.out.print_indent();
                self.out.println("...");
            }
            let mut holder = String::new();
            let mut size = i64::MIN;
            if let Some(array) = sget(cp, meta, &reference, "array") {
                if !array.is_null() {
                    let index = sget_i64(cp, meta, array, "index").unwrap_or(0);
                    size = sget_i64(cp, meta, array, "size").unwrap_or(0);
                    holder = format!("[{}]", index);
                }
            }
            if let Some(field) = sget(cp, meta, &reference, "field") {
                if !field.is_null() {
                    holder = sget_str(cp, meta, field, "name").unwrap_or("").to_string();
                }
            }
            self.out.print_indent();
            self.out.print(&holder);
            self.out.print(" : ");
            match sget(cp, meta, &reference, "object") {
                Some(o) if !o.is_null() => self.print_object_line(cp, meta, o, size),
                _ => self.out.nl(),
            }
            cur = sget(cp, meta, &reference, "referrer").cloned();
        }
    }

    /// `PrettyWriter.printObject`：一行「类名 [描述]」。
    fn print_object_line(&mut self, cp: &ConstantPool, meta: &Metadata, obj: &Value, array_size: i64) {
        if let Some(clazz) = sget(cp, meta, obj, "type") {
            if !clazz.is_null() {
                let mut name = sget_str(cp, meta, clazz, "name").unwrap_or("").replace('/', ".");
                if name.starts_with('[') {
                    let size = if array_size > 0 {
                        array_size.to_string()
                    } else {
                        String::new()
                    };
                    name = decode_descriptor_one(&name, &size);
                }
                self.out.print(&name);
                if let Some(desc) = sget_str(cp, meta, obj, "description") {
                    self.out.print(" ");
                    self.out.print(desc);
                }
            }
        }
        self.out.nl();
    }

    fn print_struct(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        let (tid, fields) = match v.as_struct() {
            Some(x) => x,
            None => return,
        };
        let t = match meta.type_by_id(tid) {
            Some(t) => t,
            None => return,
        };
        self.out.println("{");
        self.out.indent();
        for (i, f) in t.fields.iter().enumerate() {
            let fv = match fields.get(i) {
                Some(v) => v,
                None => continue,
            };
            self.out.print_indent();
            self.out.print(&f.name);
            self.out.print(" = ");
            self.print_field_value(cp, meta, f, fv);
        }
        self.out.retract();
        self.out.print_indent();
        self.out.print("}");
    }

    fn print_array(&mut self, cp: &ConstantPool, meta: &Metadata, items: &[Value]) {
        if items.is_empty() {
            self.out.print("N/A");
            return;
        }
        self.out.println("[");
        self.out.indent();
        for (i, it) in items.iter().enumerate() {
            self.out.print_indent();
            self.print_value(cp, meta, None, it);
            if i + 1 < items.len() {
                self.out.print(", ");
            }
            self.out.nl();
        }
        self.out.retract();
        self.out.print_indent();
        self.out.print("]");
    }

    fn print_thread(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        let java_id = sget_i64(cp, meta, v, "javaThreadId").unwrap_or(0);
        if java_id > 0 {
            let name = sget_str(cp, meta, v, "javaName").unwrap_or_default();
            let virt = if sget_bool(cp, meta, v, "virtual").unwrap_or(false) {
                ", virtual"
            } else {
                ""
            };
            self.out
                .print(&format!("\"{}\" (javaThreadId = {}{})", name, java_id, virt));
        } else {
            let name = sget_str(cp, meta, v, "osName").unwrap_or_default();
            let os_id = sget_i64(cp, meta, v, "osThreadId").unwrap_or(0);
            self.out
                .print(&format!("\"{}\" (osThreadId = {})", name, os_id));
        }
    }

    fn print_class(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        let loader_name = match sget(cp, meta, v, "classLoader") {
            Some(l) if !l.is_null() => match sget_str(cp, meta, l, "name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => match sget(cp, meta, l, "type") {
                    Some(t) if !t.is_null() => {
                        sget_str(cp, meta, t, "name").unwrap_or("null").to_string()
                    }
                    _ => "null".to_string(),
                },
            },
            _ => "null".to_string(),
        };
        // 类名同样是 JVM 内部形态，打印前把 `/` 换成 `.`（RecordedClass.getName）；
        // 数组类名（`[B` / `[Ljava/lang/String;`）还要解码成 `byte[]`（printClass 里的
        // `decodeDescriptors(className, "")`）。
        let mut name = sget_str(cp, meta, v, "name").unwrap_or_default().replace('/', ".");
        if name.starts_with('[') {
            name = decode_descriptor_one(&name, "");
        }
        self.out
            .print(&format!("{} (classLoader = {})", name, loader_name));
    }

    fn print_class_loader(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        match sget(cp, meta, v, "type") {
            Some(t) if !t.is_null() => {
                let name = sget_str(cp, meta, t, "name")
                    .unwrap_or_default()
                    .replace('/', ".");
                // `RecordedClassLoader.getId()` 取的是**常量池键**：
                // 真文件里 `jdk.types.ClassLoader` 只有 `type`/`name` 两个字段。
                // 我们自己写的类型里额外带 `id` 字段，优先用它。
                // `(id = N)` 是**池键**：真文件里 ClassLoader 没有 id 字段。
                // 我们自己写的文件带 id 字段（优先用它），读真文件时反查。
                let id = sget_i64(cp, meta, v, "id")
                    .filter(|n| *n != 0)
                    .or_else(|| {
                        if self.cur_ref_key != 0 {
                            Some(self.cur_ref_key)
                        } else {
                            v.type_id().and_then(|t| cp.find_key(t, v))
                        }
                    })
                    .unwrap_or(0);
                self.out.print(&format!("{} (id = {})", name, id));
            }
            _ => self.out.print("null"),
        }
    }

    fn print_java_frame(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        if let Some(m) = sget(cp, meta, v, "method") {
            if !m.is_null() {
                self.out.print(&format_method(cp, meta, m));
                let line = sget_i64(cp, meta, v, "lineNumber").unwrap_or(-1);
                if line >= 0 {
                    self.out.print(&format!(" line: {}", line));
                }
            }
        }
    }

    fn print_stack_trace(&mut self, cp: &ConstantPool, meta: &Metadata, v: &Value) {
        let truncated = sget_bool(cp, meta, v, "truncated").unwrap_or(false);
        let frames: &[Value] = match sget(cp, meta, v, "frames") {
            Some(Value::Array(a)) => a,
            _ => &[],
        };
        self.out.println("[");
        self.out.indent();
        let mut i = 0usize;
        let mut depth = 0usize;
        while i < frames.len() && depth < self.opts.stack_depth {
            let f = &frames[i];
            let is_java = match sget(cp, meta, f, "method") {
                Some(m) if !m.is_null() => !sget_bool(cp, meta, m, "hidden").unwrap_or(false),
                _ => false,
            };
            if is_java {
                self.out.print_indent();
                self.print_java_frame(cp, meta, f);
                self.out.nl();
                depth += 1;
            }
            i += 1;
        }
        if truncated || i == self.opts.stack_depth {
            self.out.print_indent();
            self.out.println("...");
        }
        self.out.retract();
        self.out.print_indent();
        self.out.print("]");
    }

    fn print_json(&mut self, cp: &ConstantPool, meta: &Metadata, collected: &[Collected]) {
        let mut rows: Vec<String> = Vec::new();
        for c in collected {
            let t = match meta.type_by_id(c.event.type_id) {
                Some(t) => t,
                None => continue,
            };
            let mut fields: Vec<String> = Vec::new();
            for (i, f) in t.fields.iter().enumerate() {
                let v = match c.event.values.get(i) {
                    Some(v) => v,
                    None => continue,
                };
                fields.push(format!("\"{}\": {}", json_escape(&f.name), json_value(cp, meta, &c.header, self.offset_ms, f, v)));
            }
            rows.push(format!(
                "  {{\n    \"type\": \"{}\",\n    \"values\": {{\n      {}\n    }}\n  }}",
                json_escape(&t.name),
                fields.join(",\n      ")
            ));
        }
        self.out
            .print(&format!("{{\n  \"recording\": {{\n  \"events\": [\n{}\n  ]\n  }}\n}}\n", rows.join(",\n")));
    }
}

struct Collected {
    header: ChunkHeader,
    event: vmless_jfr::format::event::EventRecord,
}

// ============================ inspect（诊断用） ============================

/// `jfr inspect <文件>` —— 把类型表原样打出来，包括**直接看不出来的标志位**
/// （`simpleType` / `dimension` / `constantPool` / 注解类型名）。
///
/// 为什么需要它：`jfr metadata` 的输出是给人看的，`simpleType` 这类标志不打印；
/// 但排查「这个字段到底是内联字符串还是常量池键」时，只有它说得清。
fn cmd_inspect(args: &[String]) -> i32 {
    let file = match args.iter().rev().find(|a| !a.starts_with("--")) {
        Some(f) => f,
        None => return fail("inspect 需要一个 .jfr 文件"),
    };
    let rec = match Recording::open(file) {
        Ok(r) => r,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let cd = match rec.chunk_data(0) {
        Ok(c) => c,
        Err(e) => return fail(&format!("{}: {}", file, e)),
    };
    let m = &cd.metadata;
    let needle = args.iter().position(|a| a == "--type").map(|i| args[i + 1].clone());
    let mut o = String::new();
    o.push_str(&format!(
        "chunks={} types={} events={} cp_entries={} gmtOffset={} dst={} locale={}\n",
        rec.chunks().len(),
        m.types.len(),
        m.event_type_ids.len(),
        cd.cp.len(),
        m.gmt_offset_ms,
        m.dst_ms,
        m.locale
    ));
    for t in &m.types {
        if let Some(n) = &needle {
            if !t.name.contains(n.as_str()) {
                continue;
            }
        }
        o.push_str(&format!(
            "type id={} name={} super={:?} simple={} fields={}\n",
            t.id,
            t.name,
            t.super_type,
            t.simple_type,
            t.fields.len()
        ));
        for f in &t.fields {
            o.push_str(&format!(
                "  field {} : {} [{}]{} cp={} anns=[{}]\n",
                f.name,
                m.type_name(f.type_id),
                f.type_id,
                if f.dimension > 0 { "[]" } else { "" },
                f.constant_pool,
                f.annotations
                    .iter()
                    .map(|a| format!("{}({:?})", a.type_name, a.attrs))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    out_text(&o);
    0
}

// ============================ 过滤器 ============================

#[derive(Debug, Clone)]
struct Filter {
    patterns: Vec<String>,
}

impl Filter {
    fn new(spec: &str) -> Filter {
        let mut patterns = Vec::new();
        let mut cur = String::new();
        let mut quote: Option<char> = None;
        for c in spec.chars() {
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    } else {
                        cur.push(c);
                    }
                }
                None => {
                    if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == ',' {
                        if !cur.is_empty() {
                            patterns.push(std::mem::take(&mut cur));
                        }
                    } else {
                        cur.push(c);
                    }
                }
            }
        }
        if !cur.is_empty() {
            patterns.push(cur);
        }
        Filter { patterns }
    }

    fn matches_name(&self, name: &str) -> bool {
        let simple = simple_name(name);
        self.patterns
            .iter()
            .any(|p| glob_match(p, name) || glob_match(p, &simple))
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    // 经典回溯式通配（只支持 *，jfr 的过滤器也只用到 *）
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn simple_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, s)) => s.to_string(),
        None => name.to_string(),
    }
}

// ============================ 值的小工具 ============================
// ============================ 值的小工具 ============================

/// 按字段名取值。
///
/// 这里**必须**走元数据查名字，而不是按内置类型的固定下标：真 jfr 录出来的文件里
/// `java.lang.Thread` 的字段顺序、甚至字段集合都可能和我们内置的不一样
/// （比如多一个 `group`）。按下标硬编码就只能读自己写的文件。
/// 按字段名取值（自动解一层常量池引用）。
///
/// 事件体里存的是**池键**而不是值本身（共享、不克隆），所以每次取字段都要先
/// `cp.deref`：传进来的 `v` 可能是 `Value::Ref`，取到的字段也可能是。
fn sget<'a>(cp: &'a ConstantPool, meta: &Metadata, v: &'a Value, name: &str) -> Option<&'a Value> {
    let v = cp.deref(v);
    let (tid, fields) = v.as_struct()?;
    let t = meta.type_by_id(tid)?;
    let idx = t.field_index(name)?;
    Some(cp.deref(fields.get(idx)?))
}

fn sget_i64(cp: &ConstantPool, meta: &Metadata, v: &Value, name: &str) -> Option<i64> {
    sget(cp, meta, v, name).and_then(|x| x.as_i64())
}

fn sget_str<'a>(cp: &'a ConstantPool, meta: &Metadata, v: &'a Value, name: &str) -> Option<&'a str> {
    sget(cp, meta, v, name).and_then(|x| x.as_str())
}

fn sget_bool(cp: &ConstantPool, meta: &Metadata, v: &Value, name: &str) -> Option<bool> {
    match sget(cp, meta, v, name) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn format_method(cp: &ConstantPool, meta: &Metadata, m: &Value) -> String {
    let type_name = match sget(cp, meta, m, "type") {
        // 类名在常量池里是 JVM 内部形态（`java/util/Properties`），
        // 真 jfr 的 `RecordedClass.getName()` 做了 `/` → `.` 的替换
        Some(t) if !t.is_null() => sget_str(cp, meta, t, "name").unwrap_or("").replace('/', "."),
        _ => String::new(),
    };
    let name = sget_str(cp, meta, m, "name").unwrap_or("");
    let desc = sget_str(cp, meta, m, "descriptor").unwrap_or("()V");
    // 参数只打简单名（`java.lang.Object` → `Object`），与 PrettyWriter.formatMethod 一致
    let params: Vec<String> = decode_descriptor_params(desc)
        .iter()
        .map(|q| match q.rfind('.') {
            Some(i) => q[i + 1..].to_string(),
            None => q.clone(),
        })
        .collect();
    format!("{}.{}({})", type_name, name, params.join(", "))
}

fn decode_descriptor_params(desc: &str) -> Vec<String> {
    let md = desc.replace('/', ".");
    if !md.starts_with('(') {
        return Vec::new();
    }
    let end = match md.find(')') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let inner = &md[1..end];
    let chars: Vec<char> = inner.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let mut brackets = String::new();
        while i < chars.len() && chars[i] == '[' {
            brackets.push_str("[]");
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let c = chars[i];
        let ty = match c {
            'L' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != ';' {
                    j += 1;
                }
                let s: String = chars[i + 1..j.min(chars.len())].iter().collect();
                i = j;
                s
            }
            'I' => "int".to_string(),
            'J' => "long".to_string(),
            'Z' => "boolean".to_string(),
            'D' => "double".to_string(),
            'F' => "float".to_string(),
            'S' => "short".to_string(),
            'C' => "char".to_string(),
            'B' => "byte".to_string(),
            _ => "<unknown-descriptor-type>".to_string(),
        };
        out.push(format!("{}{}", ty, brackets));
        i += 1;
    }
    out
}

// ============================ 时间 / 数字格式化 ============================

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_epoch(nanos: i64, offset_ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000);
    let local = secs + offset_ms.div_euclid(1000);
    let days = local.div_euclid(86_400);
    let sod = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (sod / 3600) as u32,
        ((sod % 3600) / 60) as u32,
        (sod % 60) as u32,
        (frac / 1_000_000) as u32,
    )
}

/// `HH:mm:ss.SSS (yyyy-MM-dd)` —— `PrettyWriter.TIME_FORMAT`
fn fmt_local_time(nanos: i64, offset_ms: i64, exact: bool) -> String {
    let (y, mo, d, h, mi, s, ms) = split_epoch(nanos, offset_ms);
    if exact {
        let frac = nanos.rem_euclid(1_000_000_000);
        format!(
            "{:02}:{:02}:{:02}.{:09} ({:04}-{:02}-{:02})",
            h, mi, s, frac, y, mo, d
        )
    } else {
        format!(
            "{:02}:{:02}:{:02}.{:03} ({:04}-{:02}-{:02})",
            h, mi, s, ms, y, mo, d
        )
    }
}

/// `yyyy-MM-dd HH:mm:ss`（UTC）—— `Summary.DATE_FORMAT`
fn fmt_utc_datetime(nanos: i64) -> String {
    let (y, mo, d, h, mi, s, _) = split_epoch(nanos, 0);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s)
}

fn fmt_float(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{:.1}", x)
    } else {
        format!("{}", x)
    }
}

fn count_length(v: i64) -> i32 {
    if v <= 0 {
        return 1;
    }
    (v as f64).log10() as i32 + 1
}

/// `ValueFormatter.formatDuration`
fn fmt_duration(nanos: i64) -> String {
    const MICRO: i64 = 1_000;
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    let ns = round_duration(nanos);
    if ns == 0 {
        return "0 s".to_string();
    }
    if ns < 0 {
        return format!("-{}", fmt_positive_duration(-ns));
    }
    fmt_positive_duration(ns)
}

fn round_duration(ns: i64) -> i64 {
    if ns == 0 {
        return 0;
    }
    if ns < 0 {
        return -round_duration(-ns);
    }
    const MICRO: i64 = 1_000;
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    if ns < MICRO {
        ns
    } else if ns < SECOND {
        let part = ns % SECOND;
        let len = count_length(part);
        let round_value = 10i64.pow((len - 3).max(0) as u32);
        let rounded = ((part as f64 / round_value as f64).round() as i64) * round_value;
        (ns / SECOND) * SECOND + rounded
    } else if ns < MINUTE {
        let secs = ns / SECOND;
        let len = count_length(secs);
        let round_value = 10i64.pow(len.max(0) as u32);
        let millis = (ns % SECOND) / 1_000_000;
        let rounded = ((millis as f64 / round_value as f64).round() as i64) * round_value;
        secs * SECOND + rounded * 1_000_000
    } else if ns < HOUR {
        ((ns + 500_000_000) / SECOND) * SECOND
    } else if ns < DAY {
        ((ns + 30_000_000_000) / MINUTE) * MINUTE
    } else {
        ((ns + 1_800_000_000_000) / HOUR) * HOUR
    }
}

fn fmt_positive_duration(ns: i64) -> String {
    const MICRO: i64 = 1_000;
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    if ns < MICRO {
        format!("{:.6} ms", ns as f64 / 1_000_000.0)
    } else if ns < SECOND {
        let part = ns % SECOND;
        let len = count_length(part);
        let precision = (9 - len).max(0) as usize;
        format!("{:.*} ms", precision, part as f64 / 1_000_000.0)
    } else if ns < MINUTE {
        let secs = ns / SECOND;
        let millis = (ns % SECOND) / 1_000_000;
        let digits = (3 - count_length(secs)).max(0) as usize;
        let v = secs as f64 + millis as f64 / 1000.0;
        format!("{:.*} s", digits, v)
    } else if ns < HOUR {
        format!("{} m {} s", ns / MINUTE, (ns % MINUTE) / SECOND)
    } else if ns < DAY {
        format!("{} h {} m", ns / HOUR, (ns % HOUR) / MINUTE)
    } else {
        format!("{} d {} h", ns / DAY, (ns % DAY) / HOUR)
    }
}

/// `PrettyWriter.formatMemory`
fn format_memory(value: i64, bytes_unit: bool, frequency: bool, exact: bool) -> String {
    if exact {
        let mut s = format!("{} {}", value, if bytes_unit { "byte" } else { "bit" });
        if value > 1 {
            s.push('s');
        }
        if frequency {
            s.push_str("/s");
        }
        return s;
    }
    if frequency {
        if bytes_unit {
            if (-1024..1024).contains(&value) {
                format!("{} byte/s", value)
            } else {
                format_data_amount("%.1f %cB/s", value)
            }
        } else if (-1024..1024).contains(&value) {
            format!("{} bps", value)
        } else {
            format_data_amount("%.1f %cbps", value)
        }
    } else if bytes_unit {
        if value == 1 || value == -1 {
            format!("{} byte", value)
        } else if (-1024..1024).contains(&value) {
            format!("{} bytes", value)
        } else {
            format_data_amount("%.1f %cB", value)
        }
    } else if value == 1 || value == -1 {
        format!("{} bit", value)
    } else if (-1024..1024).contains(&value) {
        format!("{} bits", value)
    } else {
        format_data_amount("%.1f %cbit", value)
    }
}

fn format_data_amount(fmt: &str, amount: i64) -> String {
    if amount == i64::MIN {
        return "N/A".to_string();
    }
    let exp = ((amount.abs().max(1) as f64).log(1024.0)) as i32;
    let units = ['k', 'M', 'G', 'T', 'P', 'E'];
    let unit = if exp >= 1 && (exp as usize) <= units.len() {
        units[(exp - 1) as usize]
    } else {
        'k'
    };
    let v = amount as f64 / 1024f64.powi(exp.max(1));
    // fmt 形如 "%.1f %cB"
    let body = fmt.replace("%.1f", "%.1f").replace("%c", &unit.to_string());
    let num = format!("{:.1}", v);
    body.replacen("%.1f", &num, 1)
}

// ============================ JSON ============================

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn json_value(cp: &ConstantPool, meta: &Metadata, h: &ChunkHeader, offset_ms: i64, f: &FieldDef, v: &Value) -> String {
    let v = cp.deref(v);
    if f.has_annotation("Timestamp") {
        if let Some(t) = v.as_i64() {
            return format!("\"{}\"", iso_time(h.timestamp_nanos(t), offset_ms));
        }
    }
    if f.has_annotation("Timespan") {
        if let Some(t) = v.as_i64() {
            return format!("\"{}\"", fmt_iso_duration(h.timespan_nanos(t)));
        }
    }
    json_plain(cp, meta, v)
}

fn json_plain(cp: &ConstantPool, meta: &Metadata, v: &Value) -> String {
    let v = cp.deref(v);
    match v {
        Value::Null | Value::Ref { .. } => "null".to_string(),
        Value::Str(s) => format!("\"{}\"", json_escape(s)),
        Value::Bool(b) => b.to_string(),
        Value::Int(x) => x.to_string(),
        Value::Float(x) => fmt_float(*x as f64),
        Value::Double(x) => fmt_float(*x),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(|i| json_plain(cp, meta, i)).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Struct { type_id, fields, .. } => {
            let t = match meta.type_by_id(*type_id) {
                Some(t) => t,
                None => return "{}".to_string(),
            };
            let mut parts = Vec::new();
            for (i, f) in t.fields.iter().enumerate() {
                if let Some(fv) = fields.get(i) {
                    parts.push(format!("\"{}\": {}", json_escape(&f.name), json_plain(cp, meta, fv)));
                }
            }
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn iso_time(nanos: i64, offset_ms: i64) -> String {
    let (y, mo, d, h, mi, s, ms) = split_epoch(nanos, offset_ms);
    let frac = nanos.rem_euclid(1_000_000_000);
    let _ = ms;
    let sign = if offset_ms < 0 { '-' } else { '+' };
    let off = offset_ms.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}{:02}:{:02}",
        y,
        mo,
        d,
        h,
        mi,
        s,
        frac,
        sign,
        off / 3_600_000,
        (off % 3_600_000) / 60_000
    )
}

fn fmt_iso_duration(nanos: i64) -> String {
    let secs = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000);
    format!("PT{}.{:09}S", secs, frac)
}

// ============================ 输出 ============================

fn out(s: &str) {
    let mut so = std::io::stdout();
    // 被 head 之类截断时不该 panic
    let _ = so.write_all(s.as_bytes());
    let _ = so.flush();
}

/// 平台行分隔符（`System.lineSeparator()`）。
fn nl() -> &'static str {
    if cfg!(windows) {
        "\r\n"
    } else {
        "\n"
    }
}

/// 按平台行分隔符输出。
///
/// 为什么不能无脑全用 `\n` 或全用 `\r\n`：真 `jfr` 自己就不统一 ——
/// `summary` 的表头走 `println`（平台分隔符），表格行却走
/// `printf("...\n")`（**字面 LF**）；`print` / `metadata` 则全走 `println`。
/// 想逐字节对齐，只能跟着它一起分裂。
fn out_text(s: &str) {
    if nl() == "\r\n" {
        out(&s.replace('\n', "\r\n"));
    } else {
        out(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_filters() {
        let f = Filter::new("jdk.ExecutionSample,Thread*");
        assert!(f.matches_name("jdk.ExecutionSample"));
        assert!(f.matches_name("jdk.ThreadStart"));
        assert!(!f.matches_name("jdk.GCPhasePause"));
        let f = Filter::new("\"jdk.GC*\"");
        assert!(f.matches_name("jdk.GCPhasePause"));
        assert!(glob_match("jdk.*", "jdk.GCPhasePause"));
        assert!(!glob_match("jdk.*.Foo", "jdk.GCPhasePause"));
    }

    #[test]
    fn civil_dates() {
        // 2026-11-17 04:40:03 UTC = 第 20774 天 + 04:40:03
        let nanos = (20774i64 * 86_400 + 4 * 3_600 + 40 * 60 + 3) * 1_000_000_000;
        assert_eq!(fmt_utc_datetime(nanos), "2026-11-17 04:40:03");
        // 同一时刻在东八区
        assert_eq!(
            fmt_local_time(nanos, 8 * 3_600_000, false),
            "12:40:03.000 (2026-11-17)"
        );
        // 跨年/闰年边界
        let y2024 = (19723i64 * 86_400) * 1_000_000_000;
        assert_eq!(fmt_utc_datetime(y2024), "2024-01-01 00:00:00");
    }

    #[test]
    fn durations() {
        // 与 jdk.jfr.internal.util.ValueFormatter.formatDuration 逐条对齐：
        // < 1µs → 6 位小数的 ms；µs~s → 3 位有效数字；≥ s → 3 位有效数字的 s
        assert_eq!(fmt_duration(0), "0 s");
        assert_eq!(fmt_duration(4_038_000), "4.04 ms");
        assert_eq!(fmt_duration(1_500), "0.00150 ms");
        assert_eq!(fmt_duration(1_234_000_000), "1.23 s");
        assert_eq!(fmt_duration(65_000_000_000), "1 m 5 s");
        assert_eq!(fmt_duration(3_723_000_000_000), "1 h 2 m");
        assert_eq!(fmt_duration(-4_038_000), "-4.04 ms");
    }

    #[test]
    fn descriptors() {
        assert_eq!(
            decode_descriptor_params("(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            vec!["java.lang.Object", "java.lang.Object"]
        );
        assert_eq!(decode_descriptor_params("([Ljava/lang/String;I)V"), vec![
            "java.lang.String[]",
            "int"
        ]);
        assert!(decode_descriptor_params("()V").is_empty());
    }
}

/// 解码**单个**类型描述符，数组形态带上长度（`[B` + "262144" → `byte[262144]`）。
///
/// 与 `ValueFormatter.decodeDescriptors(descriptor, arraySize)` 的第一个元素一致：
/// 只用于 `printObject` 里把数组类名 `[B` 还原成人能读的 `byte[262144]`。
/// `@Timespan` 的单位（`RecordedObject.getDuration`）。
///
/// 单位**不是**统一的 TICKS：GC 的 `flushInterval` 是 `MILLISECONDS`、
/// `maxAge` 是 `NANOSECONDS`。全按 ticks 换算会出现
/// 「1.00 s 打成 0.000434 ms」这种差 2.3e6 倍的结果 —— 而且看着不像坏数据。
enum Dur {
    Zero,
    MinValue,
    Forever,
    Nanos(i64),
}

fn decode_duration(value: i64, unit: &str, h: &ChunkHeader) -> Dur {
    if value == 0 {
        return Dur::Zero;
    }
    if value == i64::MIN {
        return Dur::MinValue;
    }
    if value == i64::MAX {
        return Dur::Forever;
    }
    let ns = match unit {
        "MICROSECONDS" => value.saturating_mul(1_000),
        "SECONDS" => value.saturating_mul(1_000_000_000),
        "MILLISECONDS" => value.saturating_mul(1_000_000),
        "NANOSECONDS" => value,
        // 默认按 ticks 算（真 jfr 对未知单位会抛异常；这里降级成 ticks 更皮实）
        _ => h.timespan_nanos(value),
    };
    Dur::Nanos(ns)
}

/// `@Timestamp` 的单位：`TICKS` 或 `MILLISECONDS_SINCE_EPOCH`。
fn decode_timestamp(value: i64, unit: &str, h: &ChunkHeader) -> i64 {
    match unit {
        "MILLISECONDS_SINCE_EPOCH" => value.saturating_mul(1_000_000),
        _ => h.timestamp_nanos(value),
    }
}

fn decode_descriptor_one(desc: &str, array_size: &str) -> String {
    let chars: Vec<char> = desc.chars().collect();
    let mut i = 0usize;
    let mut brackets = String::new();
    let mut size = array_size.to_string();
    while i < chars.len() && chars[i] == '[' {
        brackets.push_str(&format!("[{}]", size));
        size.clear();
        i += 1;
    }
    if i >= chars.len() {
        return desc.to_string();
    }
    let ty = match chars[i] {
        'L' => {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != ';' {
                j += 1;
            }
            chars[i + 1..j.min(chars.len())].iter().collect::<String>()
        }
        'I' => "int".to_string(),
        'J' => "long".to_string(),
        'Z' => "boolean".to_string(),
        'D' => "double".to_string(),
        'F' => "float".to_string(),
        'S' => "short".to_string(),
        'C' => "char".to_string(),
        'B' => "byte".to_string(),
        _ => "<unknown-descriptor-type>".to_string(),
    };
    format!("{}{}", ty.replace('/', "."), brackets)
}

/// 字符串加引号，其余原样（`tool.MetadataWriter.quoteIfNeeded`）。
fn quote_if(is_string: bool, text: &str) -> String {
    if is_string {
        format!("\"{}\"", text)
    } else {
        text.to_string()
    }
}

/// 字段的**声明类型名**（`ValueDescriptor.getTypeName`）。
///
/// `simpleType` 的类型要**下潜到它唯一的字段**：`jdk.types.Symbol`（只有
/// `String string`）在 `metadata` 输出里显示成 `String`，而不是 `Symbol`。
/// 这也是 `java.lang.String` 那几个包装类型（Symbol / FlagValueOrigin /
/// ThreadState …）在元数据里看起来像原生字符串的原因。
fn field_type_name(meta: &Metadata, type_id: i64) -> String {
    let mut id = type_id;
    for _ in 0..8 {
        let t = match meta.type_by_id(id) {
            Some(t) => t,
            None => return meta.type_name(id),
        };
        if t.simple_type && t.fields.len() == 1 {
            id = t.fields[0].type_id;
            continue;
        }
        return t.name.clone();
    }
    meta.type_name(id)
}

// ============================ 事件时间线 ============================

/// `PrettyWriter` 用的 `PriorityQueue<Timestamp>` 的忠实移植。
///
/// 为什么要照抄 Java 的堆而不是自己 `sort`：**同一时间戳的事件顺序由堆的
/// sift 规则决定**，不是插入顺序。自己 sort（哪怕稳定排序）会得到一份内容相同、
/// 顺序不同的输出 —— 在 diff 里就是几千行「看起来全错」。
/// 时间戳完全相同的事件在真 jfr 里成批出现（一次 flush 里几十条），
/// 所以这一条直接影响 `print` 能不能逐字节对上。
struct Timeline {
    heap: Vec<((i64, i64), usize)>,
}

impl Timeline {
    fn with_capacity(n: usize) -> Timeline {
        Timeline {
            heap: Vec::with_capacity(n),
        }
    }

    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn offer(&mut self, key: (i64, i64), value: usize) {
        self.heap.push((key, value));
        let mut k = self.heap.len() - 1;
        let x = self.heap[k];
        while k > 0 {
            let parent = (k - 1) >> 1;
            if x.0 >= self.heap[parent].0 {
                break;
            }
            self.heap[k] = self.heap[parent];
            k = parent;
        }
        self.heap[k] = x;
    }

    fn poll(&mut self) -> Option<((i64, i64), usize)> {
        if self.heap.is_empty() {
            return None;
        }
        let result = self.heap[0];
        let last = self.heap.pop()?;
        let n = self.heap.len();
        if n > 0 {
            self.heap[0] = last;
            Self::sift_down(&mut self.heap, 0, n);
        }
        Some(result)
    }

    fn sift_down(h: &mut Vec<((i64, i64), usize)>, mut k: usize, n: usize) {
        let x = h[k];
        let half = n >> 1;
        while k < half {
            let mut child = (k << 1) + 1;
            let mut c = h[child];
            let right = child + 1;
            if right < n && c.0 > h[right].0 {
                child = right;
                c = h[right];
            }
            if x.0 <= c.0 {
                break;
            }
            h[k] = c;
            k = child;
        }
        h[k] = x;
    }
}

/// `PrettyWriter.Timestamp.createEnd`：`(epochSecond, 2*nano + 1)`。
///
/// 秒和纳秒**分开比**（先比秒再比纳秒），与 `Instant.compareTo` 一致；
/// 直接拿总纳秒比在跨秒边界上会差一生效点。
fn end_key(nanos: i64) -> (i64, i64) {
    let secs = nanos.div_euclid(1_000_000_000);
    let nanos_part = nanos.rem_euclid(1_000_000_000);
    (secs, 2 * nanos_part + 1)
}

/// 本进程的**峰值**工作集（字节）。
///
/// 为什么要自己问系统：`.jfr` 的解析峰值内存是硬指标（50MB 文件不能吃 500MB），
/// 而在进程外量峰值只能靠采样，容易错过尖峰。`PeakWorkingSetSize` 是内核记的，
/// 不会漏。没有这条 API 的平台返回 `None`。
#[cfg(windows)]
fn peak_rss() -> Option<u64> {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    unsafe {
        let mut c: ProcessMemoryCounters = std::mem::zeroed();
        c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            None
        } else {
            Some(c.peak_working_set_size as u64)
        }
    }
}

#[cfg(not(windows))]
fn peak_rss() -> Option<u64> {
    None
}
