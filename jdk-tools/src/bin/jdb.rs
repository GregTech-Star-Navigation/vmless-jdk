//! `jdb` —— 我们自己的命令行调试器（不做 JDWP；直接驱动我们自己的编译器 + 解释器）。
//!
//! **模型：记录-回放（record & replay）**。我们的解释器没有暴露「暂停/恢复」原语
//! （`Vm::run_main` 一次跑到结束），所以调试是这样工作的：
//!
//! 1. 把源码按「语句行」插桩：每行前插一条
//!    `System.out.println("@@DBG@@<行号>|<方法>|<var>=<值>…")`，变量取该行**之前**
//!    已经声明的全部局部变量；
//! 2. 用我们的编译器编译、用我们的解释器执行，边执行边把这批标记行收进 trace；
//! 3. 之后的 `break/step/next/cont/print/locals` 都在 trace 上走 —— 显示的每个值
//!    都是解释器**真算出来的**，断点也是真停在那一行。
//!
//! 与真 jdb 的差别（`--help` 里写明）：断点在程序跑完之后才「停」（回放），程序输出
//! 统一在 run 结束后打印；没有 JDWP/远程 attach；`catch` 靠解释器报告的未捕获异常定位。

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use vmless_tools::cli::Args;
use vmless_tools::{die, EXIT_USAGE};

const TOOL: &str = "jdb";
/// 目标类名在运行时从源码里取（见 `pick_main_class`）；这里只留一个兜底默认值
const DEFAULT_MAIN: &str = "DbgTarget";

fn help() {
    println!("用法: jdb [options] <source-file>");
    println!();
    println!("选项:");
    println!("  -classpath <path>   编译/运行用的类路径（默认取 $VMLESS_JDK 或 D:/vmless/patch/img）");
    println!("  -q                  安静模式");
    println!("  -h, -?, --help      打印这段帮助");
    println!("  --version           版本信息");
    println!();
    println!("命令:");
    println!("  run | r             插桩、编译并执行，收集执行轨迹");
    println!("  break <line> | stop at <line>   在源码第 line 行下断点");
    println!("  clear <line>        清掉该行断点      breakpoints  列出断点");
    println!("  step | s            前进到下一条语句（含进入方法）");
    println!("  next | n            前进到同一个方法里的下一条语句");
    println!("  cont | c            继续到下一个断点");
    println!("  print <var> | p     打印当前语句处该局部变量的值");
    println!("  locals              打印当前语句处全部局部变量");
    println!("  where               当前方法 + 行号");
    println!("  list                打印当前语句的源码行");
    println!("  catch <Exception>   指定「异常断点」的异常简单名（运行后若抛了它，报告抛出位置）");
    println!("  exit | quit         退出");
    println!();
    println!("边界：这是**记录-回放**式调试器。解释器不暴露暂停原语，所以程序在 run 里一次跑完，");
    println!("断点/单步是在收集到的轨迹上回放；显示的变量值都是解释器真算出来的。");
    println!("没有 JDWP / 远程 attach / 跨进程调试。");
}

#[derive(Clone)]
struct TraceEntry {
    line: usize,
    method: String,
    vars: Vec<(String, String)>,
}

struct Session {
    src_lines: Vec<String>,
    /// 每个方法名 → 该方法的局部变量声明 [(行号, 变量名)]
    declared: HashMap<String, Vec<(usize, String)>>,
    trace: Vec<TraceEntry>,
    user_output: Vec<String>,
    cursor: usize,
    breakpoints: Vec<usize>,
    exception: Option<String>,
    catch_filter: Option<String>,
    cp_dirs: Vec<PathBuf>,
    vm_spec: String,
    tmp: PathBuf,
    ran: bool,
    /// 目标类名（从源码里取，用于编译产物与 VM 入口）
    main_class: String,
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
    let takes: &[&str] = &["-classpath"];
    let known: &[&str] = &["-classpath", "-q", "-h", "-?", "--help", "--version"];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let file = match args.positionals(takes) {
        Ok(p) if !p.is_empty() => p[0].clone(),
        Ok(_) => {
            eprintln!("{TOOL}: 需要一个 .java 源文件");
            std::process::exit(EXIT_USAGE);
        }
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            std::process::exit(EXIT_USAGE);
        }
    };
    let src = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => die(TOOL, &format!("读 {file} 失败: {e}")),
    };
    let (cp_dirs, vm_spec) = match resolve_cp(&args) {
        Ok(v) => v,
        Err(e) => die(TOOL, &e),
    };
    let tmp = std::env::temp_dir().join(format!("vmless_jdb_{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        die(TOOL, &format!("建临时目录失败: {e}"));
    }
    let main_class = pick_main_class(&src).unwrap_or_else(|| DEFAULT_MAIN.to_string());
    let (instrumented, declared) = instrument(&src, &file);
    let mut s = Session {
        src_lines: src.lines().map(|x| x.to_string()).collect(),
        declared,
        trace: Vec::new(),
        user_output: Vec::new(),
        cursor: 0,
        breakpoints: Vec::new(),
        exception: None,
        catch_filter: None,
        cp_dirs,
        vm_spec,
        tmp: tmp.clone(),
        main_class,
        ran: false,
    };
    // 把插桩后的源码写到临时目录
    let target = s.tmp.join(format!("{}.java", s.main_class));
    if let Err(e) = std::fs::write(&target, &instrumented) {
        die(TOOL, &format!("写插桩源码失败: {e}"));
    }
    println!("{TOOL}: {file} 已插桩（{} 行）", instrumented.lines().count());
    println!("输入 help 看命令；先 run。");

    let tty = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    loop {
        if tty {
            print!("jdb> ");
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let l = line.trim().to_string();
        if l.is_empty() {
            continue;
        }
        if !dispatch(&mut s, &l, &file) {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

fn dispatch(s: &mut Session, l: &str, file: &str) -> bool {
    let mut it = l.split_whitespace();
    let cmd = it.next().unwrap_or("");
    let rest: Vec<&str> = it.collect();
    match cmd {
        "exit" | "quit" => return false,
        "help" | "?" => help(),
        "run" | "r" => do_run(s, file),
        "break" | "stop" => {
            let n = match rest.last().and_then(|x| x.parse::<usize>().ok()) {
                Some(n) => n,
                None => {
                    println!("|  用法: break <line>");
                    return true;
                }
            };
            if !s.breakpoints.contains(&n) {
                s.breakpoints.push(n);
                s.breakpoints.sort();
            }
            println!("|  断点已设置: 行 {n}");
        }
        "clear" => {
            if let Some(n) = rest.first().and_then(|x| x.parse::<usize>().ok()) {
                s.breakpoints.retain(|b| *b != n);
                println!("|  已清除行 {n}");
            }
        }
        "breakpoints" => println!("|  断点: {:?}", s.breakpoints),
        "catch" => {
            let name = rest.first().map(|x| x.to_string()).unwrap_or_default();
            if name.is_empty() {
                println!("|  用法: catch <ExceptionClass>");
            } else {
                s.catch_filter = Some(name.clone());
                println!("|  异常断点: {name}");
            }
        }
        "step" | "s" => advance(s, Advance::Step),
        "next" | "n" => advance(s, Advance::Next),
        "cont" | "c" => advance(s, Advance::Cont),
        "print" | "p" => {
            let Some(name) = rest.first() else {
                println!("|  用法: print <var>");
                return true;
            };
            let Some(e) = s.trace.get(s.cursor) else {
                println!("|  还没有停在任何语句（先 run）");
                return true;
            };
            match e.vars.iter().find(|(k, _)| k == name) {
                Some((_, v)) => println!("{name} = {v}"),
                None => println!("|  当前语句处没有变量 {name}"),
            }
        }
        "locals" => {
            let Some(e) = s.trace.get(s.cursor) else {
                println!("|  还没有停在任何语句（先 run）");
                return true;
            };
            if e.vars.is_empty() {
                println!("|  （无局部变量）");
            } else {
                for (k, v) in &e.vars {
                    println!("{k} = {v}");
                }
            }
        }
        "where" => match s.trace.get(s.cursor) {
            Some(e) => println!("  [1] {}:{}, line {}", s.main_class, e.method, e.line),
            None => println!("|  还没有停在任何语句（先 run）"),
        },
        "list" => match s.trace.get(s.cursor) {
            Some(e) => match s.src_lines.get(e.line.saturating_sub(1)) {
                Some(text) => println!("{}: {}", e.line, text),
                None => println!("|  行号超出源码范围"),
            },
            None => println!("|  还没有停在任何语句（先 run）"),
        },
        other => println!("|  不认识的命令 {other}（用 help）"),
    }
    true
}

enum Advance {
    Step,
    Next,
    Cont,
}

fn advance(s: &mut Session, how: Advance) {
    if s.trace.is_empty() {
        println!("|  还没有执行轨迹（先 run）");
        return;
    }
    let cur = s.cursor.min(s.trace.len().saturating_sub(1));
    let cur_method = s.trace.get(cur).map(|e| e.method.clone()).unwrap_or_default();
    match how {
        Advance::Step => {
            if cur + 1 < s.trace.len() {
                s.cursor = cur + 1;
                show_stop(s);
            } else {
                println!("|  已经到轨迹末尾");
            }
        }
        Advance::Next => {
            match s.trace[cur + 1..]
                .iter()
                .position(|e| e.method == cur_method)
                .map(|p| cur + 1 + p)
            {
                Some(n) => {
                    s.cursor = n;
                    show_stop(s);
                }
                None => println!("|  方法 {} 已经没有下一条语句", cur_method),
            }
        }
        Advance::Cont => {
            if s.breakpoints.is_empty() {
                println!("|  没有断点（用 break <line>）");
                return;
            }
            match s.trace[cur + 1..]
                .iter()
                .position(|e| s.breakpoints.contains(&e.line))
                .map(|p| cur + 1 + p)
            {
                Some(n) => {
                    s.cursor = n;
                    show_stop(s);
                }
                None => println!("|  后面没有命中断点"),
            }
        }
    }
}

fn show_stop(s: &Session) {
    if let Some(e) = s.trace.get(s.cursor) {
        println!(
            "断点命中：{}:{}，行 {}",
            s.main_class, e.method, e.line
        );
        if let Some(text) = s.src_lines.get(e.line.saturating_sub(1)) {
            println!("  {}", text.trim());
        }
    }
}

fn do_run(s: &mut Session, file: &str) {
    let source = match std::fs::read_to_string(s.tmp.join(format!("{}.java", s.main_class))) {
        Ok(x) => x,
        Err(e) => {
            println!("|  读插桩源码失败: {e}");
            return;
        }
    };
    let cu = match classfile::parser::parse_compilation_unit(&source) {
        Ok(c) => c,
        Err(e) => {
            println!("|  插桩后解析失败: {e:?}");
            println!("|  （插桩可能不该落在这一行：{}）", file);
            return;
        }
    };
    let classes = match classfile::compiler::compile_with_path(&cu, &s.cp_dirs) {
        Ok(c) => c,
        Err(e) => {
            println!("|  编译失败: {}", e.msg);
            return;
        }
    };
    let dir = s.tmp.join("classes");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        println!("|  建目录失败: {e}");
        return;
    }
    for (name, bytes) in &classes {
        let mut p = dir.clone();
        for seg in name.split('/') {
            p.push(seg);
        }
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(p.with_extension("class"), bytes) {
            println!("|  写 class 失败: {e}");
            return;
        }
    }
    let spec = if s.vm_spec.is_empty() {
        dir.to_string_lossy().to_string()
    } else {
        format!("{};{}", dir.to_string_lossy(), s.vm_spec)
    };
    let r = match classfile::vm::Vm::open(&spec, &s.main_class).and_then(|mut vm| vm.run_main()) {
        Ok(r) => r,
        Err(e) => {
            println!("|  执行失败: {}", e.msg);
            return;
        }
    };
    // 拆出标记行与用户输出
    s.trace.clear();
    s.user_output.clear();
    for line in &r.output {
        if let Some(rest) = line.strip_prefix("@@DBG@@") {
            let mut parts = rest.split('|');
            let lno: usize = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let method = parts.next().unwrap_or("").to_string();
            let mut vars = Vec::new();
            for kv in parts {
                if kv.is_empty() {
                    continue;
                }
                match kv.split_once('=') {
                    Some((k, v)) => vars.push((k.to_string(), v.to_string())),
                    None => vars.push((kv.to_string(), String::new())),
                }
            }
            s.trace.push(TraceEntry { line: lno, method, vars });
        } else {
            s.user_output.push(line.clone());
        }
    }
    s.exception = r.exception.clone();
    s.ran = true;
    s.cursor = 0;
    if !s.user_output.is_empty() {
        println!("--- 程序输出 ---");
        for l in &s.user_output {
            println!("{l}");
        }
        println!("--- 输出结束 ---");
    }
    if let Some(exc) = &s.exception {
        println!("未捕获异常: {exc}");
        if let Some(e) = s.trace.last() {
            println!("  抛出位置（最后执行到的语句）：{}:{}，行 {}", s.main_class, e.method, e.line);
        }
        if let Some(f) = &s.catch_filter {
            let simple = exc.rsplit('.').next().unwrap_or(exc);
            if exc.contains(f) || simple.contains(f.as_str()) {
                println!("  命中断点：exception {f}");
            }
        }
    }
    match s.breakpoints.first() {
        Some(b) => {
            if let Some(n) = s.trace.iter().position(|e| e.line == *b) {
                s.cursor = n;
                show_stop(s);
            } else {
                println!("|  轨迹里没有命中第一个断点（行 {b}）");
            }
        }
        None => {
            if let Some(e) = s.trace.first() {
                println!(
                    "轨迹共 {} 条语句，第一条是 {}:{}（行 {}）—— 用 step/next/cont 走",
                    s.trace.len(),
                    s.main_class,
                    e.method,
                    e.line
                );
            }
        }
    }
    println!("|  已停在第一条语句（可用 where / locals / list 查看）");
}

// ---------------------------------------------------------------- 插桩

/// 返回 (插桩后的源码, 每个方法的局部变量声明表)
fn instrument(src: &str, file: &str) -> (String, HashMap<String, Vec<(usize, String)>>) {
    let lines: Vec<&str> = src.lines().collect();
    // 先找出方法名与它的行范围：`[修饰符] 返回类型 名字(参数) {`
    let mut methods: Vec<(String, usize, usize)> = Vec::new(); // (名, 起行, 止行)
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.contains('(') && (t.ends_with('{') || t.ends_with(')')) && !t.starts_with("//") {
            let head = t.split('(').next().unwrap_or("");
            let name = head.split_whitespace().last().unwrap_or("").to_string();
            if !name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !matches!(
                    name.as_str(),
                    "if" | "for" | "while" | "switch" | "catch" | "class" | "new"
                )
            {
                // 找方法体的闭合括号
                let mut depth = 0i32;
                let mut j = i;
                let mut started = false;
                let mut end = lines.len();
                while j < lines.len() {
                    for ch in lines[j].chars() {
                        if ch == '{' {
                            depth += 1;
                            started = true;
                        } else if ch == '}' {
                            depth -= 1;
                        }
                    }
                    if started && depth <= 0 {
                        end = j;
                        break;
                    }
                    j += 1;
                }
                methods.push((name, i + 1, end + 1));
                i = end;
            }
        }
        i += 1;
    }
    // 每个方法的局部变量声明（行号 → 名字）
    let mut declared: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (name, start, end) in &methods {
        let mut vars: Vec<(usize, String)> = Vec::new();
        for (k, line) in lines.iter().enumerate().take(*end).skip(*start - 1) {
            if let Some(v) = decl_name(line) {
                vars.push((k + 1, v));
            }
        }
        declared.insert(name.clone(), vars);
    }
    // 生成：在「语句行」前插一条标记
    let mut out = String::new();
    out.push_str(&format!("// instrumented from {file}\n"));
    for (idx, line) in lines.iter().enumerate() {
        let lno = idx + 1;
        let t = line.trim();
        let method = methods
            .iter()
            .find(|(_, s, e)| lno >= *s && lno <= *e)
            .map(|(n, _, _)| n.clone());
        let is_stmt = t.ends_with(';')
            && !t.starts_with("//")
            && !t.starts_with('*')
            && !t.starts_with("import")
            && !t.starts_with("package")
            && !t.contains("class ");
        if let Some(m) = method {
            if is_stmt && !t.starts_with("for") {
                let vars: Vec<String> = declared
                    .get(&m)
                    .map(|v| {
                        v.iter()
                            .filter(|(ln, _)| *ln < lno)
                            .map(|(_, n)| n.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let indent = line.len() - line.trim_start().len();
                let pad = " ".repeat(indent);
                let mut print = format!(
                    "{pad}System.out.println(\"@@DBG@@{lno}|{m}|\""
                );
                for v in &vars {
                    print.push_str(&format!(" + \"{v}=\" + {v} + \"|\""));
                }
                print.push_str(");");
                out.push_str(&print);
                out.push('\n');
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, declared)
}

/// `int x = 1;` / `String s = ...;` / `var v = ...;` → Some("x")
fn decl_name(line: &str) -> Option<String> {
    let t = line.trim();
    if t.starts_with("//") || t.starts_with("return") || t.starts_with("if") || t.starts_with("for") {
        return None;
    }
    let eq = t.find('=')?;
    let lhs = t[..eq].trim();
    let mut parts = lhs.split_whitespace();
    let ty = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None; // `int a = 1, b = 2` 之类不处理
    }
    let ty_ok = ty == "var"
        || matches!(
            ty,
            "int" | "long" | "double" | "float" | "boolean" | "char" | "byte" | "short" | "String"
                | "Object"
        )
        || ty.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false);
    if ty_ok && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

fn resolve_cp(args: &Args) -> Result<(Vec<PathBuf>, String), String> {
    let spec = match args.value("-classpath") {
        Ok(Some(v)) => v,
        _ => {
            if let Ok(j) = std::env::var("VMLESS_JDK") {
                if !j.trim().is_empty() {
                    j.trim().to_string()
                } else {
                    "D:/vmless/patch/img".into()
                }
            } else {
                "D:/vmless/patch/img".into()
            }
        }
    };
    let mut dirs = Vec::new();
    let mut specs = Vec::new();
    for p in vmless_tools::split_paths(&spec) {
        let path = PathBuf::from(&p);
        if path.is_dir() {
            dirs.push(path.clone());
            if let Ok(rd) = std::fs::read_dir(&path) {
                let mut subs: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|x| x.is_dir() && x.join("module-info.class").is_file())
                    .collect();
                subs.sort();
                dirs.extend(subs);
            }
            specs.push(p);
        }
    }
    Ok((dirs, specs.join(";")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_statement_lines() {
        let src = "public class T {\n  public static void main(String[] a) {\n    int x = 1;\n    System.out.println(x);\n  }\n}\n";
        let (out, decl) = instrument(src, "T.java");
        assert!(out.contains("@@DBG@@3|main|"));
        assert!(out.contains("@@DBG@@4|main|x=\" + x"));
        assert_eq!(decl.get("main").map(|v| v.len()), Some(1));
    }

    #[test]
    fn decl_parse() {
        assert_eq!(decl_name("int x = 1;"), Some("x".into()));
        assert_eq!(decl_name("String s = \"a\";"), Some("s".into()));
        assert_eq!(decl_name("return 1;"), None);
    }
}

/// 从源码里挑出要调试的类：优先「有 main 方法的类型」，否则第一个类型
fn pick_main_class(src: &str) -> Option<String> {
    let cu = classfile::parser::parse_compilation_unit(src).ok()?;
    let mut fallback = None;
    for t in &cu.types {
        if fallback.is_none() {
            fallback = Some(t.name.clone());
        }
        let has_main = t.members.iter().any(|m| match m {
            classfile::ast::Member::Method(md) => md.name == "main",
            _ => false,
        });
        if has_main {
            return Some(t.name.clone());
        }
    }
    fallback
}
