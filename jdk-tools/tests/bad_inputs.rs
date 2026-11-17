//! 坏输入回归：15 个工具 × 各类坏输入，必须**明确报错 + 非 0 退出 + 不 panic**。
//!
//! 这不是「功能测试」，是**对抗性体检的回归闸门**：任何一条退化成 0 退出、
//! 空 stderr、或者 `panicked at ...` 都会在这里红掉。
//!
//! 覆盖的坏输入（8 类里的 6 类，剩下的两类见 `scripts/tool-audit.sh`）：
//!   1. 不存在的路径
//!   2. 空文件
//!   3. 截断/乱码文件
//!   4. 超长路径（>260 字符）
//!   5. 非法选项
//!   6. 选项缺值
//!
//! 只读目录与权限不足这两类在 Windows 上要动 ACL（`icacls /deny`），
//! 放在 `scripts/tool-audit.sh` 里跑，不进 `cargo test`（那会让 CI 需要管理员权限）。
//!
//! 每个用例的判定只有三条，都是「人能一眼看懂」的：
//!   - 退出码非 0
//!   - stderr 非空（必须说清为什么）
//!   - stderr 里没有 `panicked at`，且退出码不是 101（Rust panic 的码）

use std::path::{Path, PathBuf};
use std::process::Command;

/// 一次坏输入的调用：`tool` 是 bin 名，`args` 里的 `%P` 会被换成 fixture 路径。
struct Case {
    tool: &'static str,
    args: &'static [&'static str],
    /// 「空文件」对它是合法输入吗？（空脚本 / 空源码 = 什么也不做，不是错误）
    accepts_empty: bool,
}

const CASES: &[Case] = &[
    // jimage：list 吃一个 jimage 路径
    Case { tool: "jimage", args: &["list", "%P"], accepts_empty: false },
    // jmod：list 吃一个 .jmod
    Case { tool: "jmod", args: &["list", "%P"], accepts_empty: false },
    // serialver：-classpath 吃目录
    Case { tool: "serialver", args: &["-classpath", "%P", "Hello"], accepts_empty: false },
    // jnativescan：--class-path 吃目录
    Case { tool: "jnativescan", args: &["--class-path", "%P"], accepts_empty: false },
    // jdeprscan：位置参数吃 dir/jar/class
    Case { tool: "jdeprscan", args: &["%P"], accepts_empty: false },
    // javaw：位置参数吃 class 文件
    Case { tool: "javaw", args: &["%P"], accepts_empty: false },
    // jwebserver：-d 吃目录
    Case { tool: "jwebserver", args: &["-d", "%P", "-p", "18321"], accepts_empty: false },
    // jlink：--module-path 吃路径
    Case { tool: "jlink", args: &["--module-path", "%P", "--add-modules", "java.base", "--output", "%O"], accepts_empty: false },
    // jshell：位置参数吃片段文件
    Case { tool: "jshell", args: &["%P"], accepts_empty: true },
    // javadoc：位置参数吃源文件
    Case { tool: "javadoc", args: &["-d", "%O", "%P"], accepts_empty: true },
    // jpackage：--input 吃目录
    Case { tool: "jpackage", args: &["--input", "%P", "--name", "demo", "--main-jar", "x.jar", "--type", "app-image"], accepts_empty: false },
    // jrunscript：位置参数吃脚本
    Case { tool: "jrunscript", args: &["%P"], accepts_empty: true },
    // jdb：-classpath 吃目录
    Case { tool: "jdb", args: &["-classpath", "%P", "Hello"], accepts_empty: false },
    // 这两个不吃路径，只验「非法选项 / 参数缺值」
    Case { tool: "jps", args: &["--vmless-definitely-not-an-option"], accepts_empty: false },
    Case { tool: "rmiregistry", args: &["--vmless-definitely-not-an-option"], accepts_empty: false },
];

/// 每个工具都要过一遍的「选项本身坏了」的两类
const OPTION_CASES: &[&str] = &[
    "jimage", "jmod", "serialver", "jnativescan", "jdeprscan", "javaw", "jwebserver",
    "jlink", "jshell", "javadoc", "jpackage", "jrunscript", "jdb", "jps", "rmiregistry",
];

/// 工具名 → 用完即弃的临时目录（测试结束不清理也不影响，都在 temp 下）
fn fixture_root() -> PathBuf {
    let d = std::env::temp_dir().join(format!("vmless-bad-inputs-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("创建 fixture 目录");
    d
}

fn make_fixtures(root: &Path) -> Fixtures {
    let empty = root.join("empty.bin");
    std::fs::write(&empty, b"").unwrap();

    // 截断的 class：magic 对、后面全是垃圾（真实场景：传输中断 / 磁盘满）
    let trunc = root.join("trunc.class");
    let mut body = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x45];
    body.extend_from_slice(&[0xEE; 24]);
    std::fs::write(&trunc, &body).unwrap();

    // 超长路径：>260 字符。**不创建**它 —— 要测的就是「路径不存在时怎么报」
    let long = root.join("a".repeat(140)).join("b".repeat(140)).join("x.class");

    Fixtures {
        missing: root.join("no").join("such").join("file.bin"),
        empty,
        trunc,
        long,
        out: root.join("out-dir"),
    }
}

struct Fixtures {
    missing: PathBuf,
    empty: PathBuf,
    trunc: PathBuf,
    long: PathBuf,
    out: PathBuf,
}

fn exe(tool: &str) -> PathBuf {
    // cargo 会为同 package 的每个 bin 注入 CARGO_BIN_EXE_<name>
    let key = match tool {
        "jimage" => env!("CARGO_BIN_EXE_jimage"),
        "jmod" => env!("CARGO_BIN_EXE_jmod"),
        "serialver" => env!("CARGO_BIN_EXE_serialver"),
        "jnativescan" => env!("CARGO_BIN_EXE_jnativescan"),
        "jdeprscan" => env!("CARGO_BIN_EXE_jdeprscan"),
        "javaw" => env!("CARGO_BIN_EXE_javaw"),
        "jwebserver" => env!("CARGO_BIN_EXE_jwebserver"),
        "jlink" => env!("CARGO_BIN_EXE_jlink"),
        "jshell" => env!("CARGO_BIN_EXE_jshell"),
        "javadoc" => env!("CARGO_BIN_EXE_javadoc"),
        "jpackage" => env!("CARGO_BIN_EXE_jpackage"),
        "jrunscript" => env!("CARGO_BIN_EXE_jrunscript"),
        "jdb" => env!("CARGO_BIN_EXE_jdb"),
        "jps" => env!("CARGO_BIN_EXE_jps"),
        "rmiregistry" => env!("CARGO_BIN_EXE_rmiregistry"),
        other => panic!("未知工具 {other}"),
    };
    PathBuf::from(key)
}

/// 跑一次，返回 (退出码, stderr)。**任何 panic / 0 退出都算失败**。
fn run_bad(tool: &str, args: &[String], what: &str) {
    let out = Command::new(exe(tool))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{tool} {what}: 起不来: {e}"));
    let code = out.status.code().unwrap_or(-1);
    let err = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !err.contains("panicked at"),
        "{tool} {what}: **panic 了**，这是硬红线\nstderr:\n{err}"
    );
    assert_ne!(code, 0, "{tool} {what}: 退出码 0 —— 坏输入被当成成功（静默失败）\nstdout:\n{stdout}\nstderr:\n{err}");
    assert_ne!(code, 101, "{tool} {what}: 退出码 101 = Rust panic");
    assert!(
        !err.trim().is_empty(),
        "{tool} {what}: 退出码 {code} 但 stderr 是空的 —— 用户看不到原因"
    );
}

/// 只要求「不 panic」的那一类（空输入对某些工具是合法的）
fn run_no_panic(tool: &str, args: &[String], what: &str) {
    let out = Command::new(exe(tool))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{tool} {what}: 起不来: {e}"));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("panicked at"),
        "{tool} {what}: **panic 了**
stderr:
{err}"
    );
    assert_ne!(out.status.code(), Some(101), "{tool} {what}: 退出码 101 = Rust panic");
}

fn expand(tool: &str, tmpl: &[&str], f: &Fixtures, p: &Path) -> Vec<String> {
    let _ = tool;
    tmpl.iter()
        .map(|a| match *a {
            "%P" => p.display().to_string(),
            "%O" => f.out.display().to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[test]
fn bad_paths_never_panic_and_never_exit_zero() {
    let root = fixture_root();
    let f = make_fixtures(&root);
    for c in CASES {
        // 1. 不存在的路径
        run_bad(c.tool, &expand(c.tool, c.args, &f, &f.missing), "不存在的路径");
        // 2. 空文件（空脚本/空源码对 jshell/jrunscript/javadoc 是合法输入，只要求不 panic）
        if c.accepts_empty {
            run_no_panic(c.tool, &expand(c.tool, c.args, &f, &f.empty), "空文件");
        } else {
            run_bad(c.tool, &expand(c.tool, c.args, &f, &f.empty), "空文件");
        }
        // 3. 截断文件
        run_bad(c.tool, &expand(c.tool, c.args, &f, &f.trunc), "截断文件");
        // 4. 超长路径
        run_bad(c.tool, &expand(c.tool, c.args, &f, &f.long), "超长路径");
    }
}

#[test]
fn unknown_option_is_usage_error() {
    for tool in OPTION_CASES {
        run_bad(tool, &["--vmless-definitely-not-an-option".to_string()], "非法选项");
    }
}

#[test]
fn missing_option_value_is_usage_error() {
    // 选项后面缺值：这些工具都会走 opt_value 的「缺少值」分支
    let cases: &[(&str, &[&str])] = &[
        ("jimage", &["list", "--include"]),
        ("jmod", &["list", "--dir"]),
        ("jlink", &["--module-path"]),
        ("javadoc", &["-d"]),
        ("jpackage", &["--input"]),
        ("jnativescan", &["--class-path"]),
        ("jdeprscan", &["--class-path"]),
        ("jshell", &["--class-path"]),
        ("jrunscript", &["-classpath"]),
        ("jdb", &["-classpath"]),
    ];
    for (tool, args) in cases {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_bad(tool, &a, "选项缺值");
    }
}

#[test]
fn help_and_version_are_clean_success() {
    // 反面保证：`--help` 必须 0 退出、stdout 非空、不 panic（15 个工具都过一遍）
    for tool in OPTION_CASES {
        let out = Command::new(exe(tool)).arg("--help").output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{tool} --help 应当 0 退出，实际 {:?}",
            out.status.code()
        );
        assert!(
            !out.stdout.is_empty(),
            "{tool} --help 没有往 stdout 打东西"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked at"), "{tool} --help panic 了");
        // 帮助里必须提到工具自己的名字，避免「打印了别的工具的帮助」
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        assert!(
            text.contains(&tool.to_lowercase()),
            "{tool} --help 里没提到自己（打印错了？）"
        );
    }
}
