
// ---- 系统属性：JNI（以及整个真 java.base 路径）的前置条件 ----
//
// 真 java.base 模式下 `System.getProperties()` 是 null —— 因为 `System.initPhase1()`
// 本该由 **VM 启动时**调用，而我们之前没调。它的第一步是问 native 要属性：
//
//   * `jdk.internal.util.SystemProps$Raw.platformProperties()` → **按位置**的 String[40]
//     （下标常量 `_os_name_NDX = 21` 之类是编译进 class 文件的，所以位置不能错）
//   * `jdk.internal.util.SystemProps$Raw.vmProperties()` → 键/值**成对**交替的 String[]
//
// 缺了它们：`System.loadLibrary` 在 `InternalError: null property: java.home` 上死，
// LWJGL 的 `Library` / `Configuration` 也拿不到 `os.name`。

fn jdk_home() -> String {
    if let Ok(p) = std::env::var("VMLESS_JDK") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            let path = std::path::Path::new(&p);
            if path.join("lib").is_dir() && path.join("bin").is_dir() {
                return p;
            }
            // `VMLESS_JDK` 也可能指向 `lib/modules` 或解包目录：往上找带 bin+lib 的那层
            let mut cur = Some(path);
            for _ in 0..3 {
                cur = cur.and_then(|c| c.parent());
                if let Some(c) = cur {
                    if c.join("lib").is_dir() && c.join("bin").is_dir() {
                        return c.to_string_lossy().into_owned();
                    }
                }
            }
            return p;
        }
    }
    for cand in [
        r"C:\Program Files\Eclipse Adoptium\jdk-25.0.4.101-hotspot",
        r"C:\Program Files\Java\jdk-25",
    ] {
        if std::path::Path::new(cand).is_dir() {
            return cand.to_string();
        }
    }
    ".".to_string()
}

fn env_or(name: &str, alt: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| alt.to_string())
}

fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into())
}

/// `platformProperties()`：**位置**固定，见 `SystemProps$Raw` 的 `_xxx_NDX` 常量。
fn platform_props() -> Vec<Option<String>> {
    let win = cfg!(windows);
    let mut v: Vec<Option<String>> = vec![None; 40];
    let mut set = |i: usize, s: String| v[i] = Some(s);
    // 0..3 display.*，5..8 format.*（本地化信息没有就用空串）
    for i in [0usize, 1, 2, 3, 5, 6, 7, 8] {
        set(i, String::new());
    }
    set(4, if win { "\\".into() } else { "/".into() }); // file.separator
    // 9..16 代理相关：不设（null = 属性不存在），和没配代理的 JVM 一致
    set(17, std::env::temp_dir().to_string_lossy().into_owned()); // java.io.tmpdir
    set(18, if win { "\r\n".into() } else { "\n".into() }); // line.separator
    set(19, if win { "GBK".into() } else { "UTF-8".into() }); // native.encoding
    set(20, if cfg!(target_arch = "x86_64") { "amd64".into() } else { "x86".into() }); // os.arch
    set(
        21,
        if win {
            "Windows 11".into()
        } else if cfg!(target_os = "macos") {
            "Mac OS X".into()
        } else {
            "Linux".into()
        },
    ); // os.name
    set(22, "10.0".into()); // os.version
    set(23, if win { ";".into() } else { ":".into() }); // path.separator
    set(27, "utf-8".into()); // stderr.encoding
    set(28, "utf-8".into()); // stdin.encoding
    set(29, "utf-8".into()); // stdout.encoding
    set(30, String::new()); // sun.arch.abi
    set(31, "64".into()); // sun.arch.data.model
    set(32, "little".into()); // sun.cpu.endian
    set(33, if cfg!(target_arch = "x86_64") { "amd64".into() } else { "x86".into() }); // sun.cpu.isalist
    set(34, "UnicodeLittle".into()); // sun.io.unicode.encoding
    set(35, if win { "GBK".into() } else { "UTF-8".into() }); // sun.jnu.encoding
    set(36, String::new()); // sun.os.patch.level
    set(37, cwd()); // user.dir
    set(38, env_or("USERPROFILE", &env_or("HOME", "."))); // user.home
    set(39, env_or("USERNAME", &env_or("USER", "user"))); // user.name
    v
}

/// `vmProperties()`：键/值成对的列表（`java.home` 等 VM 侧属性 + 命令行 `-D`）
fn vm_props() -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| p.push((k.to_string(), v));
    push("java.home", jdk_home());
    push("java.vendor", "vmless".into());
    push("java.vendor.url", "https://example.invalid/vmless".into());
    push("java.version", "25.0.4".into());
    push("java.version.date", "2026-01-20".into());
    push("java.runtime.name", "vmless Java Runtime".into());
    push("java.runtime.version", "25.0.4".into());
    push("java.vm.name", "vmless".into());
    push("java.vm.version", env!("CARGO_PKG_VERSION").into());
    push("java.vm.vendor", "vmless".into());
    push("java.vm.info", "interpreter".into());
    push("java.specification.name", "Java Platform API Specification".into());
    push("java.specification.vendor", "Oracle Corporation".into());
    push("java.specification.version", "25".into());
    push("java.class.version", "69.0".into());
    push("java.class.path", ".".into());
    push("java.library.path", cwd());
    push("jdk.debug", "release".into());
    push("file.encoding", "UTF-8".into());
    push("stdout.encoding", "utf-8".into());
    push("stderr.encoding", "utf-8".into());
    // 命令行 `-Dk=v` 最后并进来（后写的赢）
    if let Ok(g) = crate::vm::PROPS.lock() {
        for (k, v) in g.iter() {
            p.push((k.clone(), v.clone()));
        }
    }
    p
}

/// 把 `Vec<Option<String>>` 变成 `String[]`（`None` → null）
fn positional_array(vm: &mut Vm, vals: Vec<Option<String>>) -> Slot {
    let mut slots = Vec::with_capacity(vals.len());
    for v in vals {
        slots.push(Slot::I(match v {
            Some(s) => vm.str_new(s),
            None => 0,
        }));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/String;").unwrap_or(0);
    Slot::I(h)
}

fn sysprops_platform(vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    Ok(positional_array(vm, platform_props()))
}

fn sysprops_vm(vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    let pairs = vm_props();
    let mut slots = Vec::with_capacity(pairs.len() * 2);
    for (k, v) in pairs {
        let kh = vm.str_new(k);
        let vh = vm.str_new(v);
        slots.push(Slot::I(kh));
        slots.push(Slot::I(vh));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/String;").unwrap_or(0);
    Ok(Slot::I(h))
}
