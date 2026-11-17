//! `jpackage` —— 把 jar/类打成本地应用镜像。
//!
//! 真实现：`--type app-image` 生成一个可运行的目录（launcher + app 目录 + cfg + release）。
//! `--type exe` / `msi` 需要 WiX 工具链（candle/light）与 MSI 表，本 crate 不生成安装器，
//! 会**明确报错并说明原因**（不是静默降级）。

use std::path::{Path, PathBuf};

use vmless_tools::cli::Args;
use vmless_tools::{die, mkdirs, usage_err, EXIT_USAGE};

const TOOL: &str = "jpackage";

fn help() {
    println!("用法: jpackage <options>");
    println!();
    println!("输入:");
    println!("  --input <dir>            应用文件所在目录（jar 或类）");
    println!("  --main-jar <jar>         主 jar（相对 --input）");
    println!("  --main-class <class>     main 所在的类");
    println!("  --name <name>            应用名（也是 launcher 名与输出目录名）");
    println!("  --app-version <ver>      应用版本（写入 release 与 cfg）");
    println!("  --java-options <opt>     启动参数（可多次）");
    println!("  --icon <file>            图标（本实现不处理，接受但忽略）");
    println!();
    println!("输出:");
    println!("  --type <type>            app-image（默认，真实现）| exe | msi（需要 WiX，会报错）");
    println!("  --dest <dir>             输出目录（默认 .）");
    println!("  -d, --verbose            打印进度");
    println!("  -h, -?, --help           打印这段帮助");
    println!("  --version                版本信息");
    println!();
    println!("产物布局（与真 jpackage 的 app-image 对齐）：");
    println!("  <dest>/<name>/app/<main-jar>");
    println!("  <dest>/<name>/bin/<name>.exe        （我们自己的 javaw.exe 复制而来）");
    println!("  <dest>/<name>/lib/app/<name>.cfg    （[Application] 段：classpath/mainclass/java-options）");
    println!("  <dest>/<name>/release");
    println!();
    println!("边界：`--type exe|msi` 要 WiX 的 candle/light 与 MSI 表；本 crate 只做 app-image。");
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
    let takes: &[&str] = &[
        "--input",
        "--main-jar",
        "--main-class",
        "--name",
        "--dest",
        "--type",
        "--app-version",
        "--java-options",
        "--icon",
        "--vendor",
        "--description",
    ];
    let known: &[&str] = &[
        "--input",
        "--main-jar",
        "--main-class",
        "--name",
        "--dest",
        "--type",
        "--app-version",
        "--java-options",
        "--icon",
        "--vendor",
        "--description",
        "--verbose",
        "-d",
        "-h",
        "-?",
        "--help",
        "--version",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let ty = match args.value("--type") {
        Ok(Some(v)) => v,
        _ => "app-image".to_string(),
    };
    if ty != "app-image" {
        eprintln!("{TOOL}: --type {ty} 不支持");
        eprintln!("原因：exe/msi 安装器需要 WiX 工具链（candle/light）与 MSI 表定义，本 crate 不生成安装器。");
        eprintln!("可用：--type app-image（真实现，产出可直接运行的目录）");
        std::process::exit(1);
    }
    let name = match args.value("--name") {
        Ok(Some(v)) if !v.trim().is_empty() => v,
        _ => usage_err(TOOL, "缺少 --name"),
    };
    let dest = match args.value("--dest") {
        Ok(Some(v)) => PathBuf::from(v),
        _ => PathBuf::from("."),
    };
    let input = match args.value("--input") {
        Ok(Some(v)) => PathBuf::from(v),
        Ok(None) => usage_err(TOOL, "缺少 --input（应用文件所在目录）"),
        Err(e) => usage_err(TOOL, &e),
    };
    if !input.is_dir() {
        die(TOOL, &format!("--input 不是目录: {}", input.display()));
    }
    let main_jar = match args.value("--main-jar") {
        Ok(Some(v)) => v,
        _ => usage_err(TOOL, "缺少 --main-jar"),
    };
    if !input.join(&main_jar).is_file() {
        die(
            TOOL,
            &format!("--main-jar 指向的文件不存在: {}", input.join(&main_jar).display()),
        );
    }
    let main_class = match args.value("--main-class") {
        Ok(Some(v)) => v,
        Ok(None) => usage_err(TOOL, "缺少 --main-class"),
        Err(e) => usage_err(TOOL, &e),
    };
    let version = match args.value("--app-version") {
        Ok(Some(v)) => v,
        _ => "1.0".to_string(),
    };
    let java_options: Vec<String> = args
        .raw
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            if a == "--java-options" {
                args.raw.get(i + 1).cloned()
            } else {
                a.strip_prefix("--java-options=").map(|s| s.to_string())
            }
        })
        .collect();

    let app_dir = dest.join(&name);
    let app_files = app_dir.join("app");
    let lib_app = app_dir.join("lib").join("app");
    let bin = app_dir.join("bin");
    for d in [&app_files, &lib_app, &bin] {
        if let Err(e) = mkdirs(d) {
            die(TOOL, &e);
        }
    }
    // 1) 拷应用文件
    for f in vmless_tools::walk_files(&input, false).unwrap_or_default() {
        let rel = match f.strip_prefix(&input) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let target = app_files.join(&rel);
        if let Some(p) = target.parent() {
            if let Err(e) = mkdirs(p) {
                die(TOOL, &e);
            }
        }
        if let Err(e) = std::fs::copy(&f, &target) {
            die(TOOL, &format!("复制 {} 失败: {e}", f.display()));
        }
    }
    // 2) cfg
    let cfg = format!(
        "[Application]\napp.classpath=$APPDIR/{main_jar}\napp.mainclass={main_class}\napp.java-options={}\napp.version={version}\n",
        java_options.join(" ")
    );
    if let Err(e) = std::fs::write(lib_app.join(format!("{name}.cfg")), cfg) {
        die(TOOL, &format!("写 cfg 失败: {e}"));
    }
    // 3) launcher：复制我们自己的 javaw.exe（GUI 子系统），再配一个 .bat 包上 classpath。
    //    javaw 本身不读 cfg，所以真正决定「跑什么」的是这个 .bat；exe 只是执行引擎。
    let exe_name = format!("{name}.exe");
    let self_exe = std::env::current_exe().ok();
    let javaw = self_exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.join("javaw.exe"))
        .filter(|p| p.is_file());
    let mut launcher_desc = String::new();
    if let Some(j) = &javaw {
        if let Err(e) = std::fs::copy(j, bin.join(&exe_name)) {
            die(TOOL, &format!("复制 launcher 失败: {e}"));
        }
        launcher_desc = format!("bin/{exe_name}");
    }
    let bat = format!(
        "@echo off\r\nrem {name} launcher —— 由 jpackage --type app-image 生成\r\n\
         rem 真正决定 classpath 与主类的是本文件（cfg 只是给外部工具看的）\r\n\
         set APPDIR=%~dp0..\r\n\
         \"%~dp0{engine}\" -cp \"%APPDIR%\\app\\{main_jar}\" {main_class} %*\r\n",
        engine = if javaw.is_some() { exe_name.clone() } else { "javaw.exe".to_string() }
    );
    let bat_path = bin.join(format!("{name}.bat"));
    if let Err(e) = std::fs::write(&bat_path, bat) {
        die(TOOL, &format!("写 launcher 失败: {e}"));
    }
    if javaw.is_none() {
        launcher_desc = format!("bin/{name}.bat（没找到 javaw.exe，靠 PATH 上的 javaw）");
    }
    let release = format!(
        "JAVA_VERSION=\"25\"\nAPP_NAME=\"{name}\"\nAPP_VERSION=\"{version}\"\nVMLESS_PACKAGE=\"app-image\"\n"
    );
    if let Err(e) = std::fs::write(app_dir.join("release"), release) {
        die(TOOL, &format!("写 release 失败: {e}"));
    }
    println!(
        "{TOOL}: app-image 已生成 {}{}",
        app_dir.display(),
        if args.flag("-d") || args.flag("--verbose") {
            format!(
                "\n  app/{}   {}   lib/app/{}.cfg",
                main_jar,
                if launcher_desc.is_empty() { format!("bin/{name}.bat") } else { launcher_desc.clone() },
                name
            )
        } else {
            String::new()
        }
    );
    let _ = Path::new(&main_jar);
}
