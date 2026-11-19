//! 统一的类来源抽象：目录树 / `.jar`(zip) / jimage（JDK 的 `lib/modules`）。
//!
//! 对外只有一个入口：`ClassPath::find(内部类名)` → 类文件字节。
//! 名字一律是 JVM 内部形式（`java/lang/Object`），带不带 `.class` 后缀都认。
//! 按 classpath 顺序查找，第一个命中的来源胜出；结果（含未命中）缓存在内存里。
//!
//! 只用 std，不加依赖：zip 的中央目录与 deflate 解压都在 `jar.rs` 里自己实现，
//! jimage 的索引解码在 `jimage.rs` 里按 `jdk.internal.jimage.ImageLocation` 的
//! 位压缩格式解（`kind << 3 | (len-1)`）。

pub mod cli;
pub mod dir;
pub mod jar;
pub mod jimage;
pub mod jhash;
pub mod modgraph;
pub mod modules;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 诊断：进程内累计的「文件探测」次数（`VMLESS_CP_STATS=1` 时在析构里打出来）
static PROBES: AtomicU64 = AtomicU64::new(0);

fn pkg_hint_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VMLESS_NO_PKG_HINT").is_none())
}

use modules::{Access, ModuleStore};

/// 一个类来源。三种格式各自独立，`find` 拿的是**条目路径**（`java/lang/Object.class`）。
#[derive(Debug)]
pub enum ClassSource {
    Dir(dir::DirSource),
    Jar(jar::JarSource),
    JImage(jimage::JImageSource),
}

impl ClassSource {
    /// 打开一个来源。按**内容魔数**认类型，不看扩展名：
    /// zip 的 `PK\x03\x04`、jimage 的 `DA DA FE CA`（`0xCAFEDADA` 的小端写法）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let p = path.as_ref();
        if p.is_dir() {
            return Ok(ClassSource::Dir(dir::DirSource::open(p)?));
        }
        let mut f = std::fs::File::open(p).map_err(|e| format!("打开 {} 失败: {e}", p.display()))?;
        let mut magic = [0u8; 4];
        use std::io::Read;
        let n = f.read(&mut magic).map_err(|e| format!("读 {} 失败: {e}", p.display()))?;
        if n >= 4 {
            if magic == [0xDA, 0xDA, 0xFE, 0xCA] {
                return Ok(ClassSource::JImage(jimage::JImageSource::open(p)?));
            }
            if magic[0] == b'P' && magic[1] == b'K' {
                return Ok(ClassSource::Jar(jar::JarSource::open(p)?));
            }
        }
        // 魔数认不出来时退回按扩展名（空文件/畸形输入更容易看懂报错）
        match p.extension().and_then(|s| s.to_str()).unwrap_or("") {
            "jar" | "zip" => Ok(ClassSource::Jar(jar::JarSource::open(p)?)),
            "jimage" => Ok(ClassSource::JImage(jimage::JImageSource::open(p)?)),
            _ => Err(format!(
                "{} 既不是目录，也不是 jar/zip 或 jimage（魔数 {magic:02x?}）",
                p.display()
            )),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ClassSource::Dir(_) => "dir",
            ClassSource::Jar(_) => "jar",
            ClassSource::JImage(_) => "jimage",
        }
    }

    /// 条目路径 → 字节。`Ok(None)` = 这里没有这个条目。
    pub fn find(&self, entry: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            ClassSource::Dir(s) => s.find(entry),
            ClassSource::Jar(s) => s.find(entry),
            ClassSource::JImage(s) => s.find(entry),
        }
    }
}

/// 并行读一批 (下标, 路径)，成功的结果直接写进 `out[下标]`。
///
/// 读不到（不存在 / 权限 / 名字非法）就留 `None` —— 调用方自己决定是重试还是当「没有」。
fn read_parallel(jobs: &[(usize, PathBuf)], out: &mut [Option<Vec<u8>>], threads: usize) {
    if jobs.is_empty() {
        return;
    }
    let chunk = jobs.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        let mut hs = Vec::new();
        for part in jobs.chunks(chunk) {
            hs.push(s.spawn(move || {
                part.iter().map(|(i, p)| (*i, std::fs::read(p).ok())).collect::<Vec<_>>()
            }));
        }
        for h in hs {
            for (i, bytes) in h.join().unwrap_or_default() {
                if let Some(b) = bytes {
                    out[i] = Some(b);
                }
            }
        }
    });
}


#[derive(Debug, Default)]
pub struct ClassPath {
    entries: Vec<(PathBuf, ClassSource)>,
    /// 内部类名 → 字节（未命中记 `None`，避免同一个缺类反复查盘）
    cache: RefCell<HashMap<String, Option<Vec<u8>>>>,
    /// 模块模式：模块 → 包 → 来源（`None` = 旧的扁平 classpath）
    modules: Option<Box<ModuleStore>>,
    /// 模块访问规则（与 `modules` 一起设置；反射层通过 `current_access` 拿它）
    access: Option<Arc<Access>>,
    /// 是不是 `push_module_tree` 挂进来的模块镜像（多来源、包不重叠）
    module_tree: bool,
    /// 包 → 上次命中的来源下标（只在 `module_tree` 下用，省掉冷启动的文件探测）
    pkg_hint: RefCell<HashMap<String, usize>>,
}

/// 内部类名 → 条目路径：`java/lang/Object` / `java/lang/Object.class` → `java/lang/Object.class`
fn to_entry(class: &str) -> String {
    let n = class.trim_start_matches('/');
    if n.ends_with(".class") {
        n.to_string()
    } else {
        format!("{n}.class")
    }
}

impl ClassPath {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个来源（目录 / jar / jimage，自动识别）
    pub fn push(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let p = path.as_ref();
        let src = ClassSource::open(p)?;
        self.entries.push((p.to_path_buf(), src));
        self.cache.borrow_mut().clear();
        self.pkg_hint.borrow_mut().clear();
        Ok(())
    }

    /// `a.jar:b.jar:dir`（Windows 上 `;` 也认）→ ClassPath
    ///
    /// **Windows 上只能按 `;` 切**：`D:/x/classes` 里的 `:` 是盘符，按 `:` 切会把
    /// 绝对路径切成 `D` + `/x/classes`（`打开 D 失败`）。
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        let mut cp = ClassPath::new();
        let seps: &[char] = if cfg!(windows) { &[';'] } else { &[':', ';'] };
        for part in spec.split(seps) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            cp.push(part)?;
        }
        if cp.is_empty() {
            return Err(format!("classpath 为空: {spec:?}"));
        }
        Ok(cp)
    }

    /// 把「jimage 解包出来的目录」（`img/java.base/java/lang/Object.class`）当来源：
    /// 每个模块子目录各算一层，根目录本身也算一层（没有模块层的普通目录树直接可用）。
    pub fn push_module_tree(&mut self, root: impl AsRef<Path>) -> Result<(), String> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(format!("{} 不是目录", root.display()));
        }
        self.push(root)?;
        let mut mods: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|e| format!("读目录 {} 失败: {e}", root.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        mods.sort();
        for m in mods {
            self.push(m)?;
        }
        self.module_tree = true;
        self.pkg_hint.borrow_mut().clear();
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn sources(&self) -> impl Iterator<Item = (&Path, &'static str)> {
        self.entries.iter().map(|(p, s)| (p.as_path(), s.kind()))
    }

    /// 内部类名 → 类文件字节（第一个命中的来源胜出）
    pub fn find(&self, class: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(hit) = self.cache.borrow().get(class) {
            return Ok(hit.clone());
        }
        let entry = to_entry(class);
        let mut got = None;
        // 模块模式：先问模块路径（模块里的类是「模块私有」的，不该被扁平层盖掉）
        if let Some(store) = &self.modules {
            got = store.find(class)?;
        }
        // 「包 → 上次命中的来源」提示：只对 `push_module_tree` 挂进来的模块镜像生效。
        // 动机是冷启动的 syscall 数：一个 80 来源的镜像上，加载 N 个类要探 80×N 次文件，
        // 冷缓存（Defender 逐个扫）时 5421 个类的闭包要 40 秒。同一个包的类几乎总在
        // 同一个来源里，先试那一个能把探测数压到「包数×来源数」。
        // 语义不变：模块镜像里一个包只属于一个模块，不存在「前面的来源也有一份」。
        let pkg = match class.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        };
        let hint = if self.module_tree && pkg_hint_enabled() {
            self.pkg_hint.borrow().get(pkg).copied()
        } else {
            None
        };
        if got.is_none() {
            if let Some(h) = hint {
                if let Some((path, src)) = self.entries.get(h) {
                    match src.find(&entry) {
                        Ok(Some(bytes)) => got = Some(bytes),
                        Ok(None) => {}
                        Err(e) => return Err(format!("{} 里的 {entry}: {e}", path.display())),
                    }
                }
            }
        }
        if got.is_none() {
            for (i, (path, src)) in self.entries.iter().enumerate() {
                if Some(i) == hint {
                    continue;
                }
                PROBES.fetch_add(1, Ordering::Relaxed);
                match src.find(&entry) {
                    Ok(Some(bytes)) => {
                        got = Some(bytes);
                        if self.module_tree && pkg_hint_enabled() {
                            self.pkg_hint.borrow_mut().insert(pkg.to_string(), i);
                        }
                        break;
                    }
                    Ok(None) => {}
                    // 找到了但解不出来（例如 deflate）：直接报出来，不要静默跳过
                    Err(e) => return Err(format!("{} 里的 {entry}: {e}", path.display())),
                }
            }
        }
        // **只缓存未命中**。命中的字节一旦被 `crate::parse` 吃掉就没用了，把它留在
        // 缓存里等于把整个 java.base 的原始字节再存一份（实测 5421 个类的闭包
        // = 20.5 MB，而 hello 的峰值只有 116 MB —— 这一项就占了 1/6）。
        // 重复 `find` 同一个类的路径几乎不存在（调用方都先 `has_class` 挡一道），
        // 真重读一次盘也比常驻 20 MB 划算。
        if got.is_none() {
            self.cache.borrow_mut().insert(class.to_string(), None);
        }
        Ok(got)
    }

    // ------------------------------------------------------ 模块模式（JPMS）

    /// 挂上模块路径：从此类查找先走「模块 → 包」，并启用**访问检查**。
    ///
    /// 访问规则同时发布到进程级（`current_access`），反射层（`setAccessible`）
    /// 与这里用的是同一份 —— 两处的判定必须一致，不能各说各话。
    pub fn set_module_store(&mut self, store: ModuleStore) {
        let access = Arc::new(store.access());
        self.modules = Some(Box::new(store));
        self.access = Some(access.clone());
        self.cache.borrow_mut().clear();
        set_current_access(Some(access));
    }

    pub fn modules(&self) -> Option<&ModuleStore> {
        self.modules.as_deref()
    }

    pub fn access(&self) -> Option<&Arc<Access>> {
        self.access.as_ref()
    }

    /// 内部类名 → 模块名（`None` = 未命名模块 = classpath 上的类）
    pub fn module_of_class(&self, class: &str) -> Option<&str> {
        self.modules.as_ref().and_then(|s| s.module_of_class(class))
    }

    /// 字节码引用的访问检查。没有模块模式时是 **no-op**（旧行为不变）。
    ///
    /// `requester`：引用者所在模块（`None` = 未命名模块）
    /// `requester_class`：引用者的内部类名（只用来写错误消息）
    /// `target`：被引用类的内部类名
    pub fn check_reference(
        &self,
        requester: Option<&str>,
        requester_class: &str,
        target: &str,
    ) -> Result<(), String> {
        let Some(access) = &self.access else { return Ok(()) };
        access.check_class_access(
            requester.unwrap_or(modules::UNNAMED),
            requester_class,
            target,
        )
    }

    /// 批量取字节：返回顺序与入参一致，`None` = classpath 里没有这个类。
    ///
    /// 为什么要批量：启动时闭包是**按层**展开的（见 `vm::ClassStore::load_path`），
    /// 一层的名字一次性拿到。单个文件的打开成本在这台机上有 **57 µs**（带外监视器扫
    /// 每个 open），5421 个类串行读就是 300 ms —— 而 8 线程能把同样的读压到 ~8 µs/文件。
    /// 所以只要「同层」的读并行起来，启动的 I/O 就从 O(类数) 掉到 O(层数) 量级。
    ///
    /// 语义与 `find` 一致，只有两点不同：
    /// - **不写 `cache`**：预取的字节马上被 `parse` 吃掉，留着只是白占几十 MB 内存；
    /// - 只有「全是目录来源」时才并行（目录来源是无状态的读盘）；jar/jimage 来源有
    ///   内部状态，退回逐个 `find`。
    ///
    /// 批量取字节：返回顺序与入参一致，`None` = classpath 里没有这个类。
    ///
    /// 为什么要批量：启动时闭包是**按层**展开的（见 `vm::ClassStore::load_path`），
    /// 一层的名字一次性拿到。单个文件的打开成本在这台机上 **~57 µs**（带外监视器扫每个
    /// open），5421 个类串行读就是 300 ms —— 而并行后同样的读是 ~8 µs/文件。
    /// 于是启动的 I/O 从 O(类数) 掉到 O(类数/线程数)。
    ///
    /// 语义与 `find` 一致，只有两点不同：
    /// - **不写 `cache`**：预取的字节马上被 `parse` 吃掉，留着只是白占几十 MB 内存；
    /// - 只有「全是目录来源」时才并行（目录来源是无状态的读盘）；jar/jimage 来源有
    ///   内部状态，退回逐个 `find`。
    ///
    /// 分四步（每一步都是并行的）：
    /// 1. 包提示（`pkg_hint`）命中的名字 → 直接算路径并行读；
    /// 2. 没命中的 → **按包去重**，每个包只派一个代表去探「这个包在哪个来源里」；
    ///    去重很关键：一层的 143 个新名字可能只属于 40 个包，不去重就是 3 倍白探；
    /// 3. 包定位到了的名字 → 再并行读一轮；
    /// 4. 剩下的（包根本不存在 / jar、jimage 来源）→ 逐个 `find`。
    pub fn find_many(&self, names: &[String]) -> Vec<Option<Vec<u8>>> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; names.len()];
        if names.is_empty() {
            return out;
        }
        let all_dir = self.entries.iter().all(|(_, s)| matches!(s, ClassSource::Dir(_)));
        if !all_dir {
            for (i, n) in names.iter().enumerate() {
                out[i] = self.find(n).ok().flatten();
            }
            return out;
        }
        // 线程数按**工作量**给：小层（<16 个名字）单线程，省掉线程创建
        let threads_for = |n: usize| -> usize {
            let hw = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1).min(16);
            if n < 16 {
                1
            } else {
                (n / 16).min(hw).max(1)
            }
        };
        let use_hint = self.module_tree && pkg_hint_enabled();
        fn pkg_of(n: &str) -> &str {
            n.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
        }
        let path_in = |si: usize, name: &str| -> PathBuf {
            self.entries[si].0.join(to_entry(name).replace('/', std::path::MAIN_SEPARATOR_STR))
        };

        // ---- 1) 有提示的：并行读 ----
        let mut unknown: Vec<usize> = Vec::new();
        let mut jobs: Vec<(usize, PathBuf)> = Vec::new();
        {
            let hints = self.pkg_hint.borrow();
            for (i, name) in names.iter().enumerate() {
                match hints.get(pkg_of(name)).copied().filter(|_| use_hint) {
                    Some(h) => jobs.push((i, path_in(h, name))),
                    None => unknown.push(i),
                }
            }
        }
        read_parallel(&jobs, &mut out, threads_for(jobs.len()));

        // ---- 2) 没命中的：按包去重，每个包探一次 ----
        let mut pending: Vec<usize> = Vec::new();
        if !unknown.is_empty() {
            let mut rep_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for &i in &unknown {
                rep_of.entry(pkg_of(&names[i])).or_insert(i);
            }
            let reps: Vec<usize> = rep_of.values().copied().collect();
            let nsrc = self.entries.len();
            let roots: Vec<&Path> = self.entries.iter().map(|(p, _)| p.as_path()).collect();
            let chunk = reps.len().div_ceil(threads_for(reps.len())).max(1);
            let mut hit: Vec<(usize, usize)> = Vec::new(); // (name 下标, 来源下标)
            std::thread::scope(|s| {
                let mut hs = Vec::new();
                for part in reps.chunks(chunk) {
                    let roots = &roots;
                    hs.push(s.spawn(move || {
                        let mut got: Vec<(usize, usize)> = Vec::new();
                        for &i in part {
                            let ent = to_entry(&names[i]).replace('/', std::path::MAIN_SEPARATOR_STR);
                            for si in 0..nsrc {
                                PROBES.fetch_add(1, Ordering::Relaxed);
                                match std::fs::metadata(roots[si].join(&ent)) {
                                    Ok(m) if m.is_file() => {
                                        got.push((i, si));
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        got
                    }));
                }
                for h in hs {
                    if let Ok(got) = h.join() {
                        hit.extend(got);
                    }
                }
            });
            // 回填提示 + 把这些包里剩下的名字排进第二轮读
            {
                let mut hints = self.pkg_hint.borrow_mut();
                for (i, si) in &hit {
                    hints.insert(pkg_of(&names[*i]).to_string(), *si);
                }
            }
            let mut second: Vec<(usize, PathBuf)> = Vec::new();
            for &i in &unknown {
                match self.pkg_hint.borrow().get(pkg_of(&names[i])).copied() {
                    Some(h) => second.push((i, path_in(h, &names[i]))),
                    None => pending.push(i), // 包不存在（或非目录来源语义）→ 交给 `find`
                }
            }
            read_parallel(&second, &mut out, threads_for(second.len()));
        }

        // ---- 3) 剩下的走正常查找（会建包提示 / 处理 jar、jimage 与多来源）----
        for i in pending {
            out[i] = self.find(&names[i]).ok().flatten();
        }
        out
    }

    /// 哪个来源提供的这个类（诊断用）
    pub fn which(&self, class: &str) -> Option<&Path> {
        let entry = to_entry(class);
        self.entries
            .iter()
            .find(|(_, s)| matches!(s.find(&entry), Ok(Some(_))))
            .map(|(p, _)| p.as_path())
    }
}

// ------------------------------------------------------- 进程级访问规则（反射层用）

/// 反射层（`vm/reflect.rs` 的 `setAccessible` / 成员访问）要用的模块访问规则。
///
/// 放在进程级而不是穿参数传下去：反射路径在 `vm/` 深处，调用方拿不到 `ClassPath`；
/// 而一个进程只跑一次模块解析，规则是启动时定死的。没进模块模式时是 `None`
/// （= 旧行为：不做任何模块检查）。
static CURRENT_ACCESS: Mutex<Option<Arc<Access>>> = Mutex::new(None);

pub fn set_current_access(access: Option<Arc<Access>>) {
    *CURRENT_ACCESS.lock().unwrap_or_else(|e| e.into_inner()) = access;
}

pub fn current_access() -> Option<Arc<Access>> {
    CURRENT_ACCESS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 模块模式是否开着（反射层用它决定要不要做检查）
pub fn module_mode() -> bool {
    current_access().is_some()
}


/// 进程内累计的「文件探测」次数（诊断用，见 `VMLESS_CP_STATS`）
pub fn probe_count() -> u64 {
    PROBES.load(Ordering::Relaxed)
}
