//! `javadoc` —— 从源码生成 HTML 文档（类/方法/字段三张表 + 索引页 + 交叉链接）。
//!
//! 前端用**我们自己的** `classfile::parser`（所以 `javadoc` 同时充当一次语法检查），
//! 注释从源文本里按 `/** … */` 与「紧跟其后的声明名」配对抽取。
//!
//! 支持：`-d` / `-sourcepath` / `-subpackages` / `-private` / `-author` / `-version` /
//! `-quiet` / `-windowtitle` / `--version` / `-h`。
//! 边界：没有 doclet 插件体系、没有 `{@link}` 之外的标签渲染（`@param`/`@return`/`@throws`/
//! `@deprecated`/`@author`/`@since`/`@see` 会按块渲染），不生成 `search-index.js` 之类的脚本。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use classfile::ast::{Member, Type, TypeDecl, TypeKind};
use vmless_tools::cli::Args;
use vmless_tools::{die, mkdirs, usage_err, EXIT_USAGE};

const TOOL: &str = "javadoc";

fn help() {
    println!("用法: javadoc [options] [sourcefiles...]");
    println!();
    println!("选项:");
    println!("  -d <directory>            输出目录（默认 .）");
    println!("  -sourcepath <pathlist>    找源码的根目录（`;` 分隔）");
    println!("  -subpackages <pkg>[,<pkg>...]  递归处理这些包下的全部源文件");
    println!("  -private                  连 private 成员一起文档化");
    println!("  -author                   输出 @author");
    println!("  -version                  输出 @version");
    println!("  -windowtitle <text>       浏览器标题");
    println!("  -quiet                    不打印进度");
    println!("  -h, -?, --help            打印这段帮助");
    println!("  --version                 版本信息");
    println!();
    println!("边界：没有 doclet 插件体系；`@param/@return/@throws/@deprecated/@author/@since/@see`");
    println!("按块渲染；不生成脚本索引（只有 HTML + 交叉链接）。");
}

struct Doc {
    /// 声明名 → 注释原文（同一文件内可能有重名，取先出现的）
    map: BTreeMap<String, String>,
}

/// 从源码里抽 `/** … */` 并把注释挂到紧随其后的声明名上
fn extract_comments(src: &str) -> Doc {
    let bytes: Vec<char> = src.chars().collect();
    let mut map = BTreeMap::new();
    let mut i = 0usize;
    let mut pending: Option<String> = None;
    while i < bytes.len() {
        if bytes[i] == '/' && i + 1 < bytes.len() && bytes[i + 1] == '*' && i + 2 < bytes.len() && bytes[i + 2] == '*' {
            // 收集注释体
            let start = i + 3;
            let mut j = start;
            let mut body = String::new();
            while j + 1 < bytes.len() {
                if bytes[j] == '*' && bytes[j + 1] == '/' {
                    break;
                }
                body.push(bytes[j]);
                j += 1;
            }
            // 去掉每行开头的 `*`
            let cleaned: Vec<String> = body
                .lines()
                .map(|l| {
                    let t = l.trim_start();
                    t.strip_prefix("* ").or_else(|| t.strip_prefix('*')).unwrap_or(t).to_string()
                })
                .collect();
            pending = Some(cleaned.join("\n").trim().to_string());
            i = j + 2;
            continue;
        }
        // 找下一个标识符，若是「声明名」就把 pending 挂上去
        if bytes[i].is_alphanumeric() || bytes[i] == '_' || bytes[i] == '$' {
            let mut j = i;
            let mut word = String::new();
            while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '_' || bytes[j] == '$') {
                word.push(bytes[j]);
                j += 1;
            }
            if let Some(c) = pending.clone() {
                // 声明名 = 后面紧跟 `(` `;` `=` `<` 的标识符，或 `class/interface/enum/record` 后面的名字
                let next = bytes[j..].iter().find(|c| !c.is_whitespace()).copied();
                let prev_ok = {
                    // 往回看最近的标识符
                    let mut k = i;
                    let mut prev = String::new();
                    while k > 0 {
                        k -= 1;
                        if bytes[k].is_whitespace() {
                            if !prev.is_empty() {
                                break;
                            }
                            continue;
                        }
                        if bytes[k].is_alphanumeric() || bytes[k] == '_' {
                            prev.insert(0, bytes[k]);
                        } else {
                            break;
                        }
                    }
                    prev
                };
                let is_keyword = matches!(
                    prev_ok.as_str(),
                    "class" | "interface" | "enum" | "record" | "extends" | "implements" | "new"
                );
                if is_keyword
                    || matches!(next, Some('(') | Some(';') | Some('=') | Some('{') | Some('<'))
                {
                    map.entry(word.clone()).or_insert_with(|| c.clone());
                    pending = None;
                }
            }
            i = j;
            continue;
        }
        if !bytes[i].is_whitespace() && pending.is_some() {
            // 注释与声明之间出现了别的符号（注解 @ 允许），其余保留
            if bytes[i] != '@' {
                pending = None;
            }
        }
        i += 1;
    }
    Doc { map }
}

fn ty_to_string(t: &Type) -> String {
    match t {
        Type::Prim(p) => p.to_string(),
        Type::Named { path, args } => {
            let mut s = path.join(".");
            if !args.is_empty() {
                s.push('<');
                s.push_str(&args.iter().map(ty_to_string).collect::<Vec<_>>().join(", "));
                s.push('>');
            }
            s
        }
        Type::Array(inner) => format!("{}[]", ty_to_string(inner)),
        Type::Var => "var".into(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 把注释里以 `@tag` 开头的行拆出来
fn comment_parts(text: &str) -> (String, Vec<(String, String)>) {
    let mut body = String::new();
    let mut tags: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('@') {
            let (tag, val) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a.to_string(), b.trim().to_string()),
                None => (rest.to_string(), String::new()),
            };
            tags.push((tag, val));
        } else if let Some(last) = tags.last_mut() {
            if !t.is_empty() {
                last.1.push(' ');
                last.1.push_str(t);
            }
        } else {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
    }
    (body.trim().to_string(), tags)
}

#[derive(Default)]
struct ClassDoc {
    pkg: String,
    name: String,
    kind: String,
    modifiers: Vec<String>,
    extends: Vec<String>,
    implements: Vec<String>,
    comment: String,
    fields: Vec<(Vec<String>, String, String, String)>,
    ctors: Vec<(Vec<String>, Vec<(String, String)>, String)>,
    methods: Vec<(Vec<String>, String, String, Vec<(String, String)>, String)>,
}

fn html_page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"zh\">\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n\
         <style>body{{font-family:Segoe UI,Arial,sans-serif;margin:24px;max-width:1000px}}\
         h1{{border-bottom:2px solid #888}}h2{{margin-top:28px}}\
         table{{border-collapse:collapse;width:100%}}th,td{{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}}\
         th{{background:#f0f0f0}}code{{background:#f6f6f6;padding:1px 3px}}\
         .doc{{color:#333;white-space:pre-wrap}}.tag{{color:#666;font-size:90%}}</style>\n\
         </head>\n<body>\n{}</body>\n</html>\n",
        esc(title),
        body
    )
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() {
        help();
        return;
    }
    // 注意：`-version` 在 javadoc 里表示「输出 @version」，不是打印工具版本 —— 只认 --version
    if args.raw.iter().any(|a| a == "--version") {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &["-d", "-sourcepath", "-subpackages", "-windowtitle"];
    let known: &[&str] = &[
        "-d",
        "-sourcepath",
        "-subpackages",
        "-private",
        "-author",
        "-version",
        "-quiet",
        "-windowtitle",
        "-h",
        "-?",
        "--help",
        "--version",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let out_dir = match args.value("-d") {
        Ok(Some(v)) => PathBuf::from(v),
        Ok(None) => PathBuf::from("."),
        Err(e) => usage_err(TOOL, &e),
    };
    let show_private = args.flag("-private");
    let show_author = args.flag("-author");
    let show_version = args.flag("-version");
    let quiet = args.flag("-quiet");
    let title = match args.value("-windowtitle") {
        Ok(Some(v)) => v,
        _ => "API 文档（vmless javadoc）".to_string(),
    };

    // 要处理的源文件：显式给的 + -subpackages 递归收的
    let mut files: Vec<PathBuf> = Vec::new();
    match args.positionals(takes) {
        Ok(v) => {
            for p in v {
                let path = PathBuf::from(&p);
                if !path.exists() {
                    die(TOOL, &format!("源文件不存在: {p}"));
                }
                if path.is_dir() {
                    for f in vmless_tools::walk_files(&path, false).unwrap_or_default() {
                        if f.extension().and_then(|s| s.to_str()) == Some("java") {
                            files.push(f);
                        }
                    }
                } else {
                    files.push(path);
                }
            }
        }
        Err(e) => usage_err(TOOL, &e),
    }
    let sourcepath: Vec<PathBuf> = match args.value("-sourcepath") {
        Ok(Some(v)) => vmless_tools::split_paths(&v).into_iter().map(PathBuf::from).collect(),
        _ => Vec::new(),
    };
    if let Ok(Some(subs)) = args.value("-subpackages") {
        let roots = if sourcepath.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            sourcepath.clone()
        };
        for pkg in vmless_tools::split_list(&subs) {
            for root in &roots {
                let mut dir = root.clone();
                for seg in pkg.split('.') {
                    dir.push(seg);
                }
                if !dir.is_dir() {
                    continue;
                }
                for f in vmless_tools::walk_files(&dir, false).unwrap_or_default() {
                    if f.extension().and_then(|s| s.to_str()) == Some("java") {
                        files.push(f);
                    }
                }
            }
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        usage_err(TOOL, "没有要处理的源文件（给文件、目录或 -subpackages）");
    }

    // 解析 + 收集
    let mut docs: Vec<ClassDoc> = Vec::new();
    let mut failed = 0usize;
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{TOOL}: 读 {} 失败: {e}", f.display());
                failed += 1;
                continue;
            }
        };
        let cu = match classfile::parser::parse_compilation_unit(&src) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{TOOL}: {} 解析失败: {e:?}", f.display());
                failed += 1;
                continue;
            }
        };
        let comments = extract_comments(&src);
        let pkg = cu.package.clone().unwrap_or_default();
        for t in &cu.types {
            docs.push(collect_type(&pkg, t, &comments, show_private));
        }
        if !quiet {
            println!("{TOOL}: 已处理 {}", f.display());
        }
    }
    if failed > 0 && docs.is_empty() {
        std::process::exit(1);
    }

    // 输出
    if let Err(e) = emit(&docs, &out_dir, &title, show_author, show_version) {
        die(TOOL, &e);
    }
    if !quiet {
        println!("{TOOL}: 生成 {} 个类的文档到 {}", docs.len(), out_dir.display());
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

fn visible(mods: &[String], show_private: bool) -> bool {
    if show_private {
        return true;
    }
    !mods.iter().any(|m| m == "private")
}

fn collect_type(pkg: &str, t: &TypeDecl, comments: &Doc, show_private: bool) -> ClassDoc {
    let mut d = ClassDoc {
        pkg: pkg.to_string(),
        name: t.name.clone(),
        kind: match t.kind {
            TypeKind::Class => "class",
            TypeKind::Interface => "interface",
            TypeKind::Enum => "enum",
            TypeKind::Record => "record",
            TypeKind::Annotation => "@interface",
        }
        .to_string(),
        modifiers: t.modifiers.clone(),
        extends: t.extends.iter().map(ty_to_string).collect(),
        implements: t.implements.iter().map(ty_to_string).collect(),
        comment: comments.map.get(&t.name).cloned().unwrap_or_default(),
        ..Default::default()
    };
    for m in &t.members {
        match m {
            Member::Field(f) => {
                if !visible(&f.modifiers, show_private) {
                    continue;
                }
                d.fields.push((
                    f.modifiers.clone(),
                    ty_to_string(&f.ty),
                    f.name.clone(),
                    comments.map.get(&f.name).cloned().unwrap_or_default(),
                ));
            }
            Member::Method(mm) => {
                if !visible(&mm.modifiers, show_private) {
                    continue;
                }
                let params: Vec<(String, String)> = mm
                    .params
                    .iter()
                    .map(|p| (ty_to_string(&p.ty), p.name.clone()))
                    .collect();
                let comment = comments.map.get(&mm.name).cloned().unwrap_or_default();
                let ret = mm.ret.as_ref().map(ty_to_string).unwrap_or_else(|| "void".into());
                if mm.name == t.name {
                    d.ctors.push((mm.modifiers.clone(), params, comment));
                } else {
                    d.methods.push((mm.modifiers.clone(), ret, mm.name.clone(), params, comment));
                }
            }
            Member::Type(inner) => {
                // 嵌套类型也单独成一个文档
                d.fields.push((
                    inner.modifiers.clone(),
                    format!("{} {}", type_kind_str(inner), inner.name),
                    inner.name.clone(),
                    comments.map.get(&inner.name).cloned().unwrap_or_default(),
                ));
            }
            Member::EnumConst { name, .. } => {
                d.fields.push((
                    vec!["public".into()],
                    t.name.clone(),
                    name.clone(),
                    comments.map.get(name).cloned().unwrap_or_default(),
                ));
            }
            Member::Init { .. } => {}
        }
    }
    d
}

fn type_kind_str(t: &TypeDecl) -> &'static str {
    match t.kind {
        TypeKind::Class => "class",
        TypeKind::Interface => "interface",
        TypeKind::Enum => "enum",
        TypeKind::Record => "record",
        TypeKind::Annotation => "@interface",
    }
}

fn file_name_of(pkg: &str, name: &str) -> String {
    if pkg.is_empty() {
        format!("{name}.html")
    } else {
        format!("{}/{}.html", pkg.replace('.', "/"), name)
    }
}

fn emit(
    docs: &[ClassDoc],
    out: &Path,
    title: &str,
    show_author: bool,
    show_version: bool,
) -> Result<(), String> {
    mkdirs(out)?;
    // 按包分组
    let mut by_pkg: BTreeMap<String, Vec<&ClassDoc>> = BTreeMap::new();
    for d in docs {
        by_pkg.entry(d.pkg.clone()).or_default().push(d);
    }
    // 索引页
    let mut body = format!("<h1>{}</h1>\n<h2>包</h2>\n<table><tr><th>包</th><th>类</th></tr>\n", esc(title));
    for (pkg, list) in &by_pkg {
        let pkg_disp = if pkg.is_empty() { "（默认包）" } else { pkg.as_str() };
        let pkg_href = if pkg.is_empty() {
            "package-summary.html".to_string()
        } else {
            format!("{}/package-summary.html", pkg.replace('.', "/"))
        };
        body.push_str(&format!(
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td></tr>\n",
            esc(&pkg_href),
            esc(pkg_disp),
            list.iter()
                .map(|c| format!(
                    "<a href=\"{}\">{}</a>",
                    esc(&file_name_of(&c.pkg, &c.name)),
                    esc(&c.name)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    body.push_str("</table>\n<h2>全部类</h2>\n<ul>\n");
    for d in docs {
        body.push_str(&format!(
            "<li><a href=\"{}\">{}{}</a></li>\n",
            esc(&file_name_of(&d.pkg, &d.name)),
            if d.pkg.is_empty() { String::new() } else { format!("{}.", esc(&d.pkg)) },
            esc(&d.name)
        ));
    }
    body.push_str("</ul>\n");
    std::fs::write(out.join("index.html"), html_page(title, &body))
        .map_err(|e| format!("写 index.html 失败: {e}"))?;

    // 包页
    for (pkg, list) in &by_pkg {
        let dir = if pkg.is_empty() { out.to_path_buf() } else { out.join(pkg.replace('.', "/")) };
        mkdirs(&dir)?;
        let mut b = format!("<h1>包 {}</h1>\n<table><tr><th>类型</th><th>说明</th></tr>\n", esc(if pkg.is_empty() { "（默认包）" } else { pkg }));
        let mut list_sorted: Vec<&&ClassDoc> = list.iter().collect();
        list_sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for d in list_sorted {
            let (text, _) = comment_parts(&d.comment);
            b.push_str(&format!(
                "<tr><td><a href=\"{}.html\">{}</a></td><td>{}</td></tr>\n",
                esc(&d.name),
                esc(&d.name),
                esc(&text.lines().next().unwrap_or(""))
            ));
        }
        b.push_str("</table>\n<p><a href=\"../../index.html\">返回索引</a></p>\n");
        std::fs::write(dir.join("package-summary.html"), html_page(&format!("包 {pkg}"), &b))
            .map_err(|e| format!("写包页失败: {e}"))?;
    }

    // 类页
    for d in docs {
        let rel_root = "..".repeat(if d.pkg.is_empty() { 0 } else { d.pkg.split('.').count() });
        let back = if rel_root.is_empty() { "index.html".to_string() } else { format!("{rel_root}/index.html") };
        let (doc_text, tags) = comment_parts(&d.comment);
        let mut b = String::new();
        b.push_str(&format!(
            "<p><a href=\"{}\">← 索引</a></p>\n<h1>{} {} <code>{}</code></h1>\n",
            esc(&back),
            esc(&d.modifiers.join(" ")),
            esc(&d.kind),
            esc(&d.name)
        ));
        if !d.pkg.is_empty() {
            b.push_str(&format!("<p class=\"tag\">包 {}</p>\n", esc(&d.pkg)));
        }
        if !d.extends.is_empty() {
            b.push_str(&format!("<p>extends {}</p>\n", esc(&d.extends.join(", "))));
        }
        if !d.implements.is_empty() {
            b.push_str(&format!("<p>implements {}</p>\n", esc(&d.implements.join(", "))));
        }
        if !doc_text.is_empty() {
            b.push_str(&format!("<div class=\"doc\">{}</div>\n", esc(&doc_text)));
        }
        for (tag, val) in &tags {
            if (tag == "author" && !show_author) || (tag == "version" && !show_version) {
                continue;
            }
            b.push_str(&format!("<p class=\"tag\">@{tag} {}</p>\n", esc(val)));
        }

        // 字段表
        b.push_str("<h2>字段</h2>\n<table><tr><th>修饰符与类型</th><th>字段</th><th>说明</th></tr>\n");
        for (mods, ty, name, comment) in &d.fields {
            let (text, _) = comment_parts(comment);
            b.push_str(&format!(
                "<tr><td>{} <code>{}</code></td><td><a id=\"f-{}\">{}</a></td><td>{}</td></tr>\n",
                esc(&mods.join(" ")),
                esc(ty),
                esc(name),
                esc(name),
                esc(&text)
            ));
        }
        b.push_str("</table>\n");

        // 构造器表
        b.push_str("<h2>构造器</h2>\n<table><tr><th>修饰符</th><th>签名</th><th>说明</th></tr>\n");
        for (mods, params, comment) in &d.ctors {
            let (text, _) = comment_parts(comment);
            let sig = format!(
                "{}({})",
                d.name,
                params.iter().map(|(t, n)| format!("{t} {n}")).collect::<Vec<_>>().join(", ")
            );
            b.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td></tr>\n",
                esc(&mods.join(" ")),
                esc(&sig),
                esc(&text)
            ));
        }
        b.push_str("</table>\n");

        // 方法表
        b.push_str("<h2>方法</h2>\n<table><tr><th>修饰符与返回类型</th><th>签名</th><th>说明</th></tr>\n");
        for (mods, ret, name, params, comment) in &d.methods {
            let (text, mtags) = comment_parts(comment);
            let sig = format!(
                "{}({})",
                name,
                params.iter().map(|(t, n)| format!("{t} {n}")).collect::<Vec<_>>().join(", ")
            );
            let mut note = text;
            for (tag, val) in &mtags {
                if tag == "param" || tag == "return" || tag == "throws" {
                    note.push_str(&format!("\n@{tag} {val}"));
                }
            }
            b.push_str(&format!(
                "<tr><td>{} <code>{}</code></td><td><a id=\"m-{}\"><code>{}</code></a></td><td class=\"doc\">{}</td></tr>\n",
                esc(&mods.join(" ")),
                esc(ret),
                esc(name),
                esc(&sig),
                esc(note.trim())
            ));
        }
        b.push_str("</table>\n");
        let target = out.join(file_name_of(&d.pkg, &d.name));
        if let Some(parent) = target.parent() {
            mkdirs(parent)?;
        }
        std::fs::write(&target, html_page(&format!("{} {}", d.kind, d.name), &b))
            .map_err(|e| format!("写 {} 失败: {e}", target.display()))?;
    }
    Ok(())
}
