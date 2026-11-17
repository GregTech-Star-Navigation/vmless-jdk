//! `jshell` —— 交互式 Java REPL，走**我们自己**的前端 + 编译器 + 解释器（不用真 JDK）。
//!
//! 每一行输入都编译成一个 `Snippet` 类，再用 `classfile::vm` 执行。
//! 会话里声明过的变量/方法会被**重放**进后续每一段（这是 JShell 的语义，
//! 也是我们唯一的实现方式：解释器不保存跨进程状态）。
//!
//! 与真 jshell 的差异（都在 `--help` 里写明）：
//! * 表达式结果直接打印**值本身**（真 jshell 打 `$1 ==> 3`）；
//! * `Ctrl-C` 不能只中断当前片段（要处理控制台信号得引 winapi），按 EOF 退出；
//! * 没有补全、没有 `/save`（没有序列化会话状态的入口）。

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use vmless_tools::cli::Args;
use vmless_tools::{die, EXIT_USAGE};

const TOOL: &str = "jshell";
const MAIN_CLASS: &str = "Snippet";

fn help() {
    println!("Usage:   jshell <options>... <file>");
    println!("         jshell <options>... -");
    println!();
    println!("where <options> includes:");
    println!("   --class-path <path>   Specify where to find the classes");
    println!("   -f <file>             Load a file with snippets (after the session starts)");
    println!("   -h, -?, --help        Print this help message");
    println!("   -q, --quiet           Quiet mode (no banner)");
    println!("   -s, --startup <file>  Run a startup snippet file");
    println!("   --version             Print version information");
    println!();
    println!("会话内命令：");
    println!("   /exit         退出        /vars    列出已声明的变量（现值）");
    println!("   /methods      列出已声明的方法   /imports  列出已导入的包");
    println!("   /list         列出全部片段   /reset    清空会话");
    println!("   /help         打印这段帮助");
    println!();
    println!("实现边界：解释器不保存跨片段状态，声明会被重放（有副作用的声明会重复执行一次）；");
    println!("表达式结果直接打印值（不是真 jshell 的 `$1 ==> 3`）；Ctrl-C 会结束进程（无控制台信号处理）。");
}

/// 一段会话的状态：全部是可重放的源码文本
struct Session {
    imports: Vec<String>,
    decls: Vec<String>,
    methods: Vec<String>,
    snippets: Vec<String>,
    /// 编译用的 class 目录（模块树会被展开成每个模块一个目录）
    cp_dirs: Vec<PathBuf>,
    /// 运行时给 VM 的 spec（模块树/目录/jimage 都行）
    vm_spec: String,
    counter: usize,
    tmp: PathBuf,
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &["--class-path", "-f", "-s", "--startup"];
    let known: &[&str] = &[
        "--class-path",
        "-f",
        "-s",
        "--startup",
        "-q",
        "--quiet",
        "--version",
        "-h",
        "-?",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let quiet = args.flag("-q") || args.flag("--quiet");
    let (cp_spec, vm_spec) = match resolve_classpath(&args) {
        Ok(v) => v,
        Err(e) => die(TOOL, &e),
    };
    let tmp = std::env::temp_dir().join(format!("vmless_jshell_{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        die(TOOL, &format!("建临时目录 {} 失败: {e}", tmp.display()));
    }
    let mut s = Session {
        imports: Vec::new(),
        decls: Vec::new(),
        methods: Vec::new(),
        snippets: Vec::new(),
        cp_dirs: cp_spec,
        vm_spec,
        counter: 0,
        tmp: tmp.clone(),
    };
    if !quiet {
        println!("Welcome to JShell (vmless) -- target JDK 25");
        println!("我们自己实现的 REPL：编译走 classfile::compiler，执行走 classfile::vm。");
        if s.cp_dirs.is_empty() {
            println!("注意：没有找到 java.base 的字节码（设 VMLESS_JDK 指向 patch/img 或 JDK 的 lib/modules），");
            println!("      java.* 将由内建替身提供，复杂 API 可能不可用。");
        }
        println!("输入 /help 看命令，/exit 退出。");
    }
    // -s/--startup 在会话开头执行
    if let Some(f) = args.value("-s").ok().flatten().or_else(|| args.value("--startup").ok().flatten()) {
        match std::fs::read_to_string(&f) {
            Ok(text) => {
                for line in text.lines() {
                    run_line(&mut s, line, quiet);
                }
            }
            Err(e) => eprintln!("{TOOL}: 读启动文件 {f} 失败: {e}"),
        }
    }
    // -f 与位置参数都是「要执行的片段文件」
    let mut files: Vec<String> = Vec::new();
    if let Ok(Some(f)) = args.value("-f") {
        files.push(f);
    }
    if let Ok(pos) = args.positionals(takes) {
        files.extend(pos);
    }
    let mut ran_file = false;
    for f in &files {
        ran_file = true;
        match std::fs::read_to_string(f) {
            Ok(text) => {
                for line in text.lines() {
                    run_line(&mut s, line, quiet);
                }
            }
            Err(e) => eprintln!("{TOOL}: 读 {f} 失败: {e}"),
        }
    }
    if ran_file {
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }

    let tty = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    loop {
        if tty {
            print!("jshell> ");
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("{TOOL}: 读输入失败: {e}");
                break;
            }
        }
        if !keep_running(&mut s, &line, quiet) {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 返回 false 表示要退出
fn keep_running(s: &mut Session, line: &str, quiet: bool) -> bool {
    let l = line.trim();
    if l.is_empty() {
        return true;
    }
    if l.starts_with('/') {
        return command(s, l, quiet);
    }
    run_line(s, l, quiet);
    true
}

fn command(s: &mut Session, l: &str, quiet: bool) -> bool {
    let (cmd, rest) = match l.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (l, ""),
    };
    match cmd {
        "/exit" | "/quit" => {
            if !quiet {
                println!("Goodbye");
            }
            false
        }
        "/help" => {
            help();
            true
        }
        "/reset" => {
            s.imports.clear();
            s.decls.clear();
            s.methods.clear();
            s.snippets.clear();
            println!("|  已重置会话");
            true
        }
        "/imports" => {
            if s.imports.is_empty() {
                println!("|  没有 import");
            } else {
                for i in &s.imports {
                    println!("{i}");
                }
            }
            true
        }
        "/vars" => {
            if s.decls.is_empty() {
                println!("|  没有变量");
                return true;
            }
            for d in s.decls.clone() {
                let name = var_name(&d).unwrap_or_default();
                if name.is_empty() {
                    println!("{}", d.trim());
                    continue;
                }
                let src = s.render(Some(&name));
                match s.exec(&src) {
                    Ok(out) => {
                        let v = out.join(" ");
                        println!("{} ==> {}", d.trim(), v)
                    }
                    Err(e) => println!("{} ==> <求值失败: {e}>", d.trim()),
                }
            }
            true
        }
        "/methods" => {
            if s.methods.is_empty() {
                println!("|  没有方法");
            } else {
                for m in &s.methods {
                    println!("{}", m.trim());
                }
            }
            true
        }
        "/list" => {
            for x in &s.snippets {
                println!("{}", x.trim());
            }
            true
        }
        "/class-path" if !rest.is_empty() => {
            match expand_classpath(rest) {
                Ok((dirs, spec)) => {
                    s.cp_dirs = dirs;
                    s.vm_spec = spec;
                    println!("|  已设置 class-path: {rest}");
                }
                Err(e) => println!("|  {e}"),
            }
            true
        }
        other => {
            println!("|  不认识的命令 {other}（用 /help）");
            true
        }
    }
}

struct CompileErr(String);

fn run_line(s: &mut Session, line: &str, _quiet: bool) {
    let l = line.trim();
    if l.is_empty() {
        return;
    }
    if l.starts_with("import ") {
        s.imports.push(l.to_string());
        s.snippets.push(l.to_string());
        println!("|  已导入: {}", l.trim_start_matches("import ").trim_end_matches(';'));
        return;
    }
    // 方法声明：进类体
    if looks_like_method(l) {
        s.methods.push(l.trim_end_matches(';').to_string());
        s.snippets.push(l.to_string());
        println!("|  已创建方法 {}", first_ident_after_space(l));
        return;
    }
    // 变量声明：进 main 的可重放头部
    if looks_like_decl(l) {
        // 先确认能编过再记（否则会话里会留一条坏声明，后面全错）
        let candidate = {
            let mut copy = s.render(None);
            let _ = &mut copy;
            let mut tmp = Vec::new();
            tmp.push(l.trim().trim_end_matches(';').trim().to_string());
            let src = render_with(s, &tmp, None);
            match s.exec(&src) {
                Ok(_) => {
                    s.decls.extend(tmp.clone());
                    s.snippets.push(l.to_string());
                    None
                }
                Err(e) => Some(e),
            }
        };
        if let Some(e) = candidate {
            println!("|  错误: {e}");
            return;
        }
        // 打印当前值
        let name = var_name(l).unwrap_or_default();
        if !name.is_empty() {
            let src = s.render(Some(&name));
            if let Ok(out) = s.exec(&src) {
                if !out.is_empty() {
                    println!("{}", out.join(" "));
                }
            }
        }
        return;
    }
    // 语句 / 表达式
    let (stmts, expr) = split_last_expr(l);
    let src = render_with(s, &stmts, expr.as_deref());
    match s.exec(&src) {
        Ok(out) => {
            for line in out {
                println!("{line}");
            }
            s.snippets.push(l.to_string());
        }
        Err(e) => println!("|  错误: {e}"),
    }
}

impl Session {
    fn render(&self, print_var: Option<&str>) -> String {
        render_with(self, &[], print_var)
    }

    fn exec(&mut self, src: &str) -> Result<Vec<String>, String> {
        self.counter += 1;
        let cu = classfile::parser::parse_compilation_unit(src)
            .map_err(|e| format!("parse: {:?}", e))?;
        let classes = classfile::compiler::compile_with_path(&cu, &self.cp_dirs)
            .map_err(|e| format!("compile: {}", e.msg))?;
        let dir = self.tmp.join(format!("s{}", self.counter));
        std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
        let mut main_written = false;
        for (name, bytes) in &classes {
            let mut p = dir.clone();
            for seg in name.split('/') {
                p.push(seg);
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
            }
            std::fs::write(p.with_extension("class"), bytes)
                .map_err(|e| format!("写 class 失败: {e}"))?;
            if name == MAIN_CLASS {
                main_written = true;
            }
        }
        if !main_written {
            return Err("编译器没有产出 Snippet 类".into());
        }
        let spec = if self.vm_spec.is_empty() {
            dir.to_string_lossy().to_string()
        } else {
            format!("{};{}", dir.to_string_lossy(), self.vm_spec)
        };
        let r = classfile::vm::Vm::open(&spec, MAIN_CLASS)
            .map_err(|e| format!("执行失败: {}", e.msg))?
            .run_main()
            .map_err(|e| format!("执行失败: {}", e.msg))?;
        if let Some(exc) = r.exception {
            return Err(exc);
        }
        Ok(r.output)
    }
}

fn render_with(s: &Session, stmts: &[String], expr: Option<&str>) -> String {
    let mut src = String::new();
    for i in &s.imports {
        src.push_str(i);
        src.push('\n');
    }
    src.push_str(&format!("public class {MAIN_CLASS} {{\n"));
    for m in &s.methods {
        src.push_str("  ");
        src.push_str(m);
        src.push('\n');
    }
    src.push_str("  public static void main(String[] args) {\n");
    for d in &s.decls {
        src.push_str("    ");
        src.push_str(expand_var_decl(d).trim_end_matches(';'));
        src.push_str(";\n");
    }
    for st in stmts {
        let t = st.trim();
        if t.is_empty() {
            continue;
        }
        src.push_str("    ");
        src.push_str(expand_var_decl(t).trim_end_matches(';'));
        src.push_str(";\n");
    }
    if let Some(e) = expr {
        src.push_str(&format!("    System.out.println({e});\n"));
    }
    src.push_str("  }\n}\n");
    src
}

/// `1+2; 3+4` → ([], Some("3+4"))；`var x = 1;` → (["var x = 1"], None)
fn split_last_expr(l: &str) -> (Vec<String>, Option<String>) {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut chars = l.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = in_str {
            cur.push(c);
            if c == q {
                in_str = None;
            } else if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_str = Some(c);
                cur.push(c);
            }
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            ';' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    let tail = cur.trim().to_string();
    if tail.is_empty() {
        (parts, None)
    } else {
        (parts, Some(tail))
    }
}

fn looks_like_decl(l: &str) -> bool {
    let t = l.trim().trim_end_matches(';').trim();
    // 一行里有多条语句（`var x = 1; x+2`）不是纯声明：交给语句/表达式分支
    if t.contains(';') || t.contains('(') {
        return false;
    }
    let Some((lhs, _rhs)) = t.split_once('=') else {
        return false;
    };
    let lhs = lhs.trim();
    let mut it = lhs.split_whitespace();
    let (Some(ty), Some(name), None) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    let ty_ok = ty == "var"
        || matches!(
            ty,
            "int" | "long" | "double" | "float" | "boolean" | "char" | "byte" | "short" | "String"
                | "Object"
        )
        || ty.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false);
    ty_ok && !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn looks_like_method(l: &str) -> bool {
    let t = l.trim();
    if !t.contains('(') || !t.contains(')') {
        return false;
    }
    let head = match t.split_once('(') {
        Some((h, _)) => h.trim(),
        None => return false,
    };
    // `类型 名字` 或 `修饰符 类型 名字`
    let parts: Vec<&str> = head.split_whitespace().collect();
    if parts.len() < 2 {
        return false;
    }
    let name = parts[parts.len() - 1];
    if name.contains('.') || name.contains('=') {
        return false;
    }
    let ty = parts[parts.len() - 2];
    (ty == "void" || ty.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        || matches!(ty, "int" | "long" | "double" | "boolean" | "char" | "float"))
        && parts
            .iter()
            .all(|p| matches!(*p, "public" | "private" | "protected" | "static" | "final") || *p == ty || *p == name)
}

fn var_name(d: &str) -> Option<String> {
    let t = d.trim().trim_end_matches(';');
    let (lhs, _) = t.split_once('=')?;
    lhs.split_whitespace().nth(1).map(|s| s.trim().to_string())
}

fn first_ident_after_space(l: &str) -> String {
    l.split_once('(')
        .and_then(|(h, _)| h.split_whitespace().last().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// classpath 解析：模块树 → 每个模块一个编译目录；同时给出给 VM 的 spec
fn resolve_classpath(args: &Args) -> Result<(Vec<PathBuf>, String), String> {
    if let Ok(Some(v)) = args.value("--class-path") {
        return expand_classpath(&v);
    }
    if let Ok(jdk) = std::env::var("VMLESS_JDK") {
        let jdk = jdk.trim().to_string();
        if !jdk.is_empty() {
            return expand_classpath(&jdk);
        }
    }
    let patch = PathBuf::from("D:/vmless/patch/img");
    if patch.is_dir() {
        return expand_classpath(&patch.to_string_lossy());
    }
    Ok((Vec::new(), String::new()))
}

fn expand_classpath(spec: &str) -> Result<(Vec<PathBuf>, String), String> {
    let mut dirs = Vec::new();
    let mut specs = Vec::new();
    for p in vmless_tools::split_paths(spec) {
        let path = PathBuf::from(&p);
        if !path.exists() {
            return Err(format!("class-path 里的路径不存在: {p}"));
        }
        specs.push(p.clone());
        if path.is_dir() {
            dirs.push(path.clone());
            // 模块树：把每个模块目录也加进来
            if let Ok(rd) = std::fs::read_dir(&path) {
                let mut subs: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|x| x.is_dir() && x.join("module-info.class").is_file())
                    .collect();
                subs.sort();
                dirs.extend(subs);
            }
        }
    }
    if dirs.is_empty() {
        return Err(format!(
            "class-path {spec} 里没有目录：我们没有实现从 jimage 直接给编译器取签名，\
             请指向解包出来的模块目录树（例如 D:/vmless/patch/img）"
        ));
    }
    Ok((dirs, specs.join(";")))
}

// ------------------------------------------------------- var 展开
//
// 我们的编译器/语义层对 `var` 的推断只支撑「声明后直接使用」这一种形态：
// `var x = "hi"; x.length()` 会报「未找到实例方法 length」、`var n = 1; n+1`
// 会报类型转换失败（见回报里的实测）。REPL 里 `var` 是最常用的写法，所以
// 这里在编译前按初始化式把 `var` 展开成具体类型 —— 语义等价（Java 的 `var`
// 就是取初始化式的静态类型），推断不出来时保留 `var` 并让编译器报错。

/// `var NAME = EXPR` → `TYPE NAME = EXPR`（不是 var 声明就原样返回）
fn expand_var_decl(stmt: &str) -> String {
    let trimmed = stmt.trim();
    let no_semi = trimmed.trim_end_matches(';');
    let Some(rest) = no_semi.strip_prefix("var ") else {
        return stmt.to_string();
    };
    let Some((lhs, init)) = rest.split_once('=') else {
        return stmt.to_string();
    };
    let name = lhs.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return stmt.to_string();
    }
    match infer_type(init.trim()) {
        Some(ty) => format!("{ty} {name} = {};", init.trim()),
        None => stmt.to_string(),
    }
}

fn is_type_name(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty()
        && t.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        && t.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '[' || c == ']')
}

/// 从初始化式推断静态类型；认不出来返回 None
fn infer_type(init: &str) -> Option<String> {
    let s = init.trim();
    if s.is_empty() {
        return None;
    }
    // 强制转换：`(Foo) x`
    if let Some(rest) = s.strip_prefix('(') {
        if let Some(end) = rest.find(')') {
            let ty = rest[..end].trim();
            if is_type_name(ty) {
                return Some(ty.to_string());
            }
        }
    }
    if s.starts_with('"') {
        return Some("String".into());
    }
    if s.starts_with('\'') {
        return Some("char".into());
    }
    if s == "true" || s == "false" {
        return Some("boolean".into());
    }
    if let Some(rest) = s.strip_prefix("new ") {
        let head: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '.' || *c == '_')
            .collect();
        if !head.is_empty() {
            return Some(head);
        }
    }
    // 数字字面量
    let core = s.trim_end_matches(|c| c == 'L' || c == 'l' || c == 'F' || c == 'f' || c == 'D' || c == 'd');
    if !core.is_empty()
        && core.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        && core.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
    {
        let ends = s.ends_with('L') || s.ends_with('l');
        let floaty = s.ends_with('F')
            || s.ends_with('f')
            || s.ends_with('D')
            || s.ends_with('d')
            || s.contains('.')
            || s.contains('e')
            || s.contains('E');
        return Some(if ends {
            "long".into()
        } else if s.ends_with('F') || s.ends_with('f') {
            "float".into()
        } else if floaty {
            "double".into()
        } else {
            "int".into()
        });
    }
    // 字符串拼接（右侧出现字符串字面量）——按 Java 语义结果是 String
    if s.contains('"') {
        return Some("String".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_expansion() {
        assert_eq!(expand_var_decl("var x = \"hi\";"), "String x = \"hi\";");
        assert_eq!(expand_var_decl("var n = 1"), "int n = 1;");
        assert_eq!(expand_var_decl("var d = 1.5;"), "double d = 1.5;");
        assert_eq!(expand_var_decl("var b = true;"), "boolean b = true;");
        assert_eq!(expand_var_decl("var s = new StringBuilder();"), "StringBuilder s = new StringBuilder();");
        assert_eq!(expand_var_decl("int x = 1;"), "int x = 1;");
        assert_eq!(expand_var_decl("var y = foo();"), "var y = foo();");
    }
}
