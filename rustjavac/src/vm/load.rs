//! 多类加载与运行期类元数据：继承链、字段布局、方法表。
//!
//! 类不再靠「扫一个目录」得到，而是沿 `load::ClassPath` **按需查找**：从入口类出发，
//! 顺着它常量池里的 `Class` 项（父类、接口、字段/方法的参数与返回类型都在里面）一个个
//! `find` 回来，形成一个闭包。没被任何已加载类引用到的类不加载 —— 这也是以后跑 jimage
//! 时不会把三万个 java.base 类全解出来的原因。每个名字只查一次，`ClassPath` 内部还有
//! 字节缓存。
//!
//! 字段布局与编译期 `sema::Resolver` 保持一致：**父类字段在前、本类字段在后**。

use crate::load::ClassPath;
use crate::{ClassFile, CpEntry};
use std::collections::HashMap;
use std::sync::Arc;

use super::class::builtin_super;
use super::VmError;

#[derive(Clone)]
pub(crate) struct ClassMeta {
    /// 实例字段名 → 下标（含继承）
    pub(crate) fields: HashMap<String, usize>,
    /// 实例字段总数（含继承），`new` 分配对象时用
    pub(crate) field_count: usize,
}

#[derive(Clone)]
pub(crate) struct ClassStore {
    pub(super) classes: Vec<Arc<ClassFile>>,
    /// 内部类名 → 类下标
    pub(super) by_name: HashMap<String, usize>,
    pub(super) metas: HashMap<String, ClassMeta>,
    /// 类 → 「**本类自己声明**的实例字段」名→下标。
    ///
    /// `metas[..].fields` 是按名字合并过的（父类先插、`or_insert` 保留父类那份），
    /// 所以**子类遮蔽父类同名字段时，`fields` 永远指不到子类那一份**。
    /// JDK 里真有这种：`WindowsWatchService$WindowsWatchKey.events`（`Set`）遮蔽
    /// `AbstractWatchKey.events`（`List`）—— 于是 `signalEvent()` 读到父类那个 List 槽，
    /// `List.get(int)` 打到 HashSet 上，报「内建方法 get 未实现」。
    /// 字段解析按 JVMS 要「从引用类出发、逐层查本类自己的表」，所以这张表是必需的。
    pub(super) own_fields: HashMap<String, HashMap<String, usize>>,
    /// 方法表：`meth_hash(声明类, 方法名, 描述符)` → (类下标, 方法下标)。
    ///
    /// **键是 64 位散列而不是三个 String**：54,615 个方法原本要 16 万个 String 分配
    /// （≈12 MB + 30 ms 建表），改成一个 u64 之后是 1.2 MB。命中后仍然逐字节验一遍
    /// （`verify_method`），碰撞就退化成本类内线性扫 —— 正确性与原表逐字一致。
    pub(super) methods: HashMap<u64, (u32, u32)>,
    /// main 方法所在的类下标
    pub(super) main: usize,
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), exc: None })
}

/// 一个类的字段布局：合并表（父类先插）、**本类自己声明**的表、实例字段总数。
///
/// 语义与 `build_metas()` 里那段逐字一致（`or_insert` 保留父类那份、遮蔽时 `own` 才是
/// 正确的槽），区别只是它**按类独立**，所以能并行算（见 `ClassStore::new`）。
fn layout_of(
    classes: &[Arc<ClassFile>],
    by_name: &HashMap<String, usize>,
    name: &str,
) -> (String, ClassMeta, HashMap<String, usize>) {
    // 本类 → … → 顶层的继承链（未知祖先名也留在链上，与 `chain()` 一致）
    let mut chain: Vec<String> = vec![name.to_string()];
    let mut cur = name.to_string();
    for _ in 0..64 {
        match super_of_of(classes, by_name, &cur) {
            Some(s) if s != cur && !chain.contains(&s) => {
                chain.push(s.clone());
                cur = s;
            }
            _ => break,
        }
    }
    let mut fields = HashMap::new();
    let mut own = HashMap::new();
    let mut count = 0usize;
    for c in chain.iter().rev() {
        let Some(&ci) = by_name.get(c) else { continue };
        let cf = &classes[ci];
        for f in &cf.fields {
            if f.access_flags & 0x0008 != 0 {
                continue; // 静态字段不在对象里
            }
            let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
            fields.entry(n.clone()).or_insert(count);
            if c.as_str() == name {
                own.insert(n, count);
            }
            count += 1;
        }
    }
    (name.to_string(), ClassMeta { fields, field_count: count }, own)
}


/// 类的直接父类：已加载的类看 `super_class`，其余靠 java.* 写死的表
fn super_of_of(classes: &[Arc<ClassFile>], by_name: &HashMap<String, usize>, class: &str) -> Option<String> {
    if let Some(&i) = by_name.get(class) {
        let cf = &classes[i];
        return match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
            Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
    }
    builtin_super(class).map(|s| s.to_string())
}

/// 一个批次的产出：解析好的类（按原顺序）+ 两段耗时（诊断用）。
struct BatchOut {
    files: Vec<ClassFile>,
    read_ms: f64,
    parse_ms: f64,
}

/// **一批**的「取字节 + 解析」。`ids` 与 `names` 二选一：
/// 前者走类表缓存（`cds`），后者走 classpath。
///
/// 两个反直觉的取舍都在这里：
///
/// 1. **不先把整批字节读进来再解析**。老路径把 5421 个类的字节（27 MB）全读进来再统一解析，
///    于是「字节」与「解析出来的结构」同时存活 —— 那 27 MB 就是启动峰值的一部分。
///    这里每个类**读完立刻解析、解析完立刻丢字节**，任一时刻在飞的字节只有
///    「线程数 × 一个类」（≈ 100 KB）。
/// 2. **不每批重新 spawn 线程**。按批 spawn 看起来更整齐，实测解析段反而慢 10 ms
///    （11 批 × 16 线程 ≈ 176 次线程创建）。线程只开一轮，按类交错读/解析。
///
/// 出错返回 `(批内下标, 原因)` —— 批内下标让调用方补上类名（id 段的名字要问缓存要）。
fn fetch_parse_batch(
    cds: &crate::load::cds::Cds,
    cp: &ClassPath,
    ids: &[u32],
    names: &[String],
) -> Result<BatchOut, (usize, String)> {
    let t = std::time::Instant::now();
    let n = ids.len() + names.len();
    // 缓存里没有字节的（主类 / 用户自己的类）先取回来：`data_len == 0` 是纯内存判断，
    // 不产生 I/O —— 所以这一步多数时候什么也不做，也不占内存（通常 0~1 个类）。
    let mut pre: Vec<Option<Vec<u8>>> = vec![None; n];
    {
        let mut miss_names: Vec<String> = Vec::new();
        let mut miss_at: Vec<usize> = Vec::new();
        for (k, &id) in ids.iter().enumerate() {
            if cds.ents[id as usize].data_len == 0 {
                miss_at.push(k);
                miss_names.push(cds.name_of(id as usize).to_string());
            }
        }
        if !miss_names.is_empty() {
            let got = cp.find_many(&miss_names);
            for (j, &k) in miss_at.iter().enumerate() {
                pre[k] = got[j].clone();
            }
        }
        if !names.is_empty() {
            let got = cp.find_many(names);
            for (j, b) in got.into_iter().enumerate() {
                pre[ids.len() + j] = b;
            }
        }
    }
    let read_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = std::time::Instant::now();
    let nthreads = {
        let hw = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1).min(16);
        (n / 24).min(hw).max(1)
    };
    let mut parsed: Vec<Option<crate::Result<ClassFile>>> = (0..n).map(|_| None).collect();
    let mut panic_err: Option<String> = None;
    {
        let chunk = n.div_ceil(nthreads.max(1)).max(1);
        let pre: &[Option<Vec<u8>>] = &pre;
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for start in (0..n).step_by(chunk) {
                let end = (start + chunk).min(n);
                handles.push(s.spawn(move || {
                    let mut out: Vec<Option<crate::Result<ClassFile>>> = Vec::with_capacity(end - start);
                    // 一块共享一个 Utf8 arena：块内 300+ 个类共用 16 次分配，而不是每类一次
                    let mut pool = crate::classfile::Utf8Builder::with_capacity((end - start) * 1024);
                    for k in start..end {
                        // 读一个、解析一个、立刻放掉字节：任一时刻在飞的字节只有一个类
                        let r = if let Some(b) = &pre[k] {
                            Some(crate::parse_in(b.as_slice(), &mut pool))
                        } else if let Some(&id) = ids.get(k) {
                            cds.bytes(id as usize).as_deref().map(|b| crate::parse_in(b, &mut pool))
                        } else {
                            None
                        };
                        out.push(r);
                    }
                    // 冻结后回填给块内每个类（`Arc` 共享，读路径不变）
                    let arena = std::sync::Arc::new(pool.freeze());
                    for slot in out.iter_mut().flatten() {
                        if let Ok(cf) = slot {
                            cf.utf8 = arena.clone();
                        }
                    }
                    (start, out)
                }));
            }
            for h in handles {
                let got = match h.join() {
                    Ok(v) => v,
                    Err(_) => {
                        panic_err = Some("class 文件解析线程 panic".to_string());
                        return;
                    }
                };
                let (start, rows) = got;
                for (j, r) in rows.into_iter().enumerate() {
                    parsed[start + j] = r;
                }
            }
        });
    }
    let parse_ms = t.elapsed().as_secs_f64() * 1000.0;
    if let Some(msg) = panic_err {
        return Err((0, msg));
    }
    let mut out = Vec::with_capacity(n);
    for (i, slot) in parsed.into_iter().enumerate() {
        match slot {
            // 缓存里没有的类（`None`）跳过，与老路径一致
            None => {}
            Some(Ok(cf)) => out.push(cf),
            Some(Err(e)) => return Err((i, e.to_string())),
        }
    }
    Ok(BatchOut { files: out, read_ms, parse_ms })
}

/// 诊断：类表结构的**精确记账**（容量口径，不是估算）。`VMLESS_MEM_STATS=1` 时打印。
///
/// 为什么按 `capacity()` 记账：`Vec` 的实际占用看的是容量不是长度，而且每个 Utf8 /
/// 属性 info 都是一次**独立的堆分配**（Windows 堆最小块 16 B、块头 8 B），
/// 所以「条目数 × 估算长度」会低估一半 —— 这里把两者都打出来，不靠猜。
fn stats_structures(files: &[ClassFile]) {
    let cp_ent = std::mem::size_of::<Option<CpEntry>>();
    let attr_sz = std::mem::size_of::<crate::classfile::Attribute>();
    let mut cp_slots = 0usize;
    let mut utf8_n = 0usize;
    let mut utf8_bytes = 0usize;
    let mut attr_n = 0usize;
    let mut attr_info = 0usize;
    let mut member_n = 0usize;
    let mut iface_slots = 0usize;
    for cf in files {
        cp_slots += cf.constant_pool.capacity();
        iface_slots += cf.interfaces.capacity();
        for i in 1..cf.constant_pool.len() {
            if let Some(b) = cf.utf8(i as u16) {
                utf8_n += 1;
                utf8_bytes += b.len();
            }
        }
        for m in cf.methods.iter().chain(cf.fields.iter()) {
            member_n += 1;
            for a in &m.attributes {
                attr_n += 1;
                attr_info += a.info.len();
            }
        }
        for a in &cf.attributes {
            attr_n += 1;
            attr_info += a.info.len();
        }
    }
    eprintln!(
        "[mem2] 类 {} | ClassFile {} B | CP 槽 {cp_slots} × {cp_ent} = {} KB | \
         Utf8 {utf8_n} 条 / {utf8_bytes} B（一类一块 arena）| 成员 {member_n} | \
         属性 {attr_n} × {attr_sz} = {} KB + info {attr_info} B | iface 槽 {iface_slots}",
        files.len(),
        files.len() * std::mem::size_of::<ClassFile>(),
        cp_slots * cp_ent / 1024,
        attr_n * attr_sz / 1024,
    );}

fn pull_jdk(jdk_present: bool, name: &str) -> bool {
    jdk_present || !name.starts_with("java/")
}

/// 方法表的键：`hash(声明类, 方法名, 描述符)`。
///
/// 用散列而不是三个 String 当键：54,615 个方法原本要 16 万次 String 分配（≈12 MB、
/// 建表 30 ms 的一半），换成一个 u64 只要 1.2 MB。碰撞由 `lookup_method` 的逐字节验证兜住。
#[inline]
fn meth_hash(owner: &str, name: &str, desc: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in [owner, name, desc] {
        for &b in s.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // 分段：否则 ("ab","c") 与 ("a","bc") 同散列（本来也该靠验证兜，但少撞一点更好）
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
// ------------------------------------------------------- 类表缓存的「建」

/// 建缓存：**冷启动**（还没有缓存）时边加载边把类字节写进临时文件，不攒内存 ——
/// 5421 个类 22 MB，攒起来就是启动峰值多 22 MB。
///
/// 只收「模块镜像里的类」：用户自己的类随程序变，收进来缓存就永远失效。
/// 建缓存永远不会让加载失败：`Writer::new` 失败就退化成不建。
struct CdsBuild {
    w: Option<crate::load::cds::Writer>,
}

impl CdsBuild {
    fn new(cp: &ClassPath) -> Self {
        if !cp.wants_cds() {
            return CdsBuild { w: None };
        }
        CdsBuild { w: crate::load::cds::Writer::new(&cp.fingerprint()) }
    }

    /// 收一个类（不是模块类就丢掉）
    fn add_class(&mut self, cp: &ClassPath, name: &str, bytes: &[u8], refs: &[String]) {
        if bytes.is_empty() || !cp.is_module_class(name) {
            return;
        }
        if let Some(w) = self.w.as_mut() {
            w.add(name, bytes, refs);
        }
    }

    /// 收工（返回一句给 prof 的注记）。任何失败都只是「没建成」，不影响本次运行。
    fn finish(&mut self) -> String {
        let Some(w) = self.w.take() else { return String::new() };
        let total = w.len();
        match w.finish() {
            Some(p) => format!("，缓存新建 {total} 个类 → {}", p.display()),
            None => String::new(),
        }
    }
}

impl ClassStore {
    pub(super) fn load_path(cp: &ClassPath, main_class: &str) -> Result<Self, VmError> {
        // java.base 在不在 classpath 上 —— 一次探测，决定整条闭包要不要拉 `java.*`
        let jdk_present = cp.find("java/lang/Object").ok().flatten().is_some();
        super::prof(&format!("探测 java/lang/Object（jdk_present={jdk_present}）"));
        if let Some(cds) = cp.cds().cloned() {
            return Self::load_path_cds(cp, &cds, jdk_present, main_class);
        }
        let t_all = std::time::Instant::now();
        let mut find_ms = 0f64;
        let mut parse_ms = 0f64;
        let mut files: Vec<ClassFile> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut layer: Vec<String> = vec![main_class.to_string()];
        let mut build = CdsBuild::new(cp);
        while !layer.is_empty() {
            // 去重 + 过滤（空名 / 数组描述符）；同层内保持顺序
            let mut cur: Vec<String> = Vec::new();
            cur.reserve(layer.len());
            for n in layer.drain(..) {
                if n.is_empty() || n.starts_with('[') || !seen.insert(n.clone()) {
                    continue;
                }
                cur.push(n);
            }
            if cur.is_empty() {
                break;
            }
            // ---- 取字节（并行） ----
            let t = std::time::Instant::now();
            let mut bytes = cp.find_many(&cur);
            find_ms += t.elapsed().as_secs_f64() * 1000.0;
            // 主类必须存在（别的类找不到就跳过：常量池里的 Class 项只是一笔引用，
            // JVM 也是用到才加载，扫到解不开的条目不能整个执行失败）
            if cur.iter().any(|n| n == main_class) {
                let i = cur.iter().position(|n| n == main_class).unwrap();
                if bytes[i].is_none() {
                    // `find_many` 把出错当「没有」；主类要给出真原因
                    return match cp.find(main_class) {
                        Ok(Some(_)) => err(format!("classpath 里找不到主类 {main_class}")),
                        Ok(None) => err(format!("classpath 里找不到主类 {main_class}")),
                        Err(e) => Err(VmError { msg: e, exc: None }),
                    };
                }
            }
            // ---- 解析（并行，按块切） ----
            let t = std::time::Instant::now();
            let nthreads = {
                let hw = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1).min(16);
                if cur.len() < 16 { 1 } else { (cur.len() / 16).min(hw).max(1) }
            };
            let mut parsed: Vec<Option<crate::Result<ClassFile>>> = (0..cur.len()).map(|_| None).collect();
            // 线程 panic 的口子：scope 的闭包只能返回 ()，所以先记下来，出 scope 再报。
            let mut panic_err: Option<String> = None;
            {
                let chunk = cur.len().div_ceil(nthreads.max(1)).max(1);
                std::thread::scope(|s| {
                    let mut handles = Vec::new();
                    for (base, part) in cur.chunks(chunk).enumerate() {
                        let start = base * chunk;
                        let slice = &bytes[start..start + part.len()];
                        handles.push(s.spawn(move || {
                            slice
                                .iter()
                                .map(|b| b.as_deref().map(crate::parse))
                                .collect::<Vec<_>>()
                        }));
                    }
                    for (k, h) in handles.into_iter().enumerate() {
                        // **不能静默吞掉**：少解析一批类 = 闭包里少一堆类，症状会散落成
                        // 后面某处的「找不到方法/字段」，极难定位。宁可当场报错。
                        let got = match h.join() {
                            Ok(v) => v,
                            Err(_) => {
                                panic_err = Some(format!("class 文件解析线程 panic（第 {k} 块）"));
                                return;
                            }
                        };
                        for (j, r) in got.into_iter().enumerate() {
                            parsed[k * chunk + j] = r;
                        }
                    }
                });
            }
            if let Some(msg) = panic_err {
                return Err(VmError { msg, exc: None });
            }
            parse_ms += t.elapsed().as_secs_f64() * 1000.0;
            // ---- 收集本层 + 展开下一层 ----
            let mut next: Vec<String> = Vec::new();
            for (ci, (name, slot)) in cur.iter().zip(parsed.into_iter()).enumerate() {
                let Some(r) = slot else { continue };
                let cf = match r {
                    Ok(cf) => cf,
                    Err(e) => {
                        return Err(VmError { msg: format!("解析 {name} 失败: {e}"), exc: None })
                    }
                };
                {
                    // 这个类自己属于哪个模块（模块模式才有；否则是未命名模块 = classpath）
                    let cur_module = cp.module_of_class(name).map(|s| s.to_string());
                    // 引用到的类型都在 `Class` 常量池项里（含 super_class 与 interfaces 指过去的那两个）。
                    // `<SELF>` 是我们编译器写常量池时用的占位符（字节码生成完会被改写成真实类名），
                    // 它不是类，别拿它去 classpath 里找。
                    let mut mine: Vec<String> = Vec::new();
                    for e in &cf.constant_pool {
                        if let Some(CpEntry::Class(ix)) = e {
                            if let Some(raw) = cf.utf8(*ix) {
                                let n = String::from_utf8_lossy(raw);
                                if n.starts_with('[') || n.contains('<') || n.contains(';') {
                                    continue;
                                }
                                // **模块访问检查**：引用者模块 → 被引用类所在模块的包。
                                // 没有模块模式时 `check_reference` 是 no-op（旧行为不变）；
                                // 有模块模式时**默认拒绝**，不许静默放行。
                                if let Err(msg) = cp.check_reference(cur_module.as_deref(), name, &n) {
                                    return Err(VmError { msg, exc: None });
                                }
                                mine.push(n.clone().into_owned());
                                if pull_jdk(jdk_present, &n) {
                                    next.push(n.into_owned());
                                }
                            }
                        }
                    }
                    build.add_class(cp, name, bytes.get(ci).and_then(|b| b.as_deref()).unwrap_or(&[]), &mine);
                }
                files.push(cf);
            }
            layer = next;
        }
        let build_note = build.finish();
        super::prof(&format!(
            "常量池闭包 BFS：{} 个类（find {find_ms:.1} ms，parse {parse_ms:.1} ms，总 {:.1} ms{build_note}）",
            files.len(),
            t_all.elapsed().as_secs_f64() * 1000.0
        ));
        if std::env::var_os("VMLESS_CP_STATS").is_some() {
            eprintln!(
                "[cp] 装载 {} 个类，文件探测 {} 次",
                files.len(),
                crate::load::probe_count()
            );
        }
        if std::env::var_os("VMLESS_MEM_STATS").is_some() {
            let mut cp_slots = 0usize;
            let mut utf8s = 0usize;
            let mut utf8_bytes = 0usize;
            let mut attrs = 0usize;
            let mut attr_bytes = 0usize;
            let mut methods = 0usize;
            let mut fields = 0usize;
            let mut code_bytes = 0usize;
            for cf in &files {
                cp_slots += cf.constant_pool.len();
                for i in 1..cf.constant_pool.len() {
                    if let Some(b) = cf.utf8(i as u16) {
                        utf8s += 1;
                        utf8_bytes += b.len();
                    }
                }
                methods += cf.methods.len();
                fields += cf.fields.len();
                for m in cf.methods.iter().chain(cf.fields.iter()) {
                    attrs += m.attributes.len();
                    for a in &m.attributes {
                        attr_bytes += a.info.len();
                        if cf.utf8(a.name_index) == Some(b"Code") {
                            code_bytes += a.info.len();
                        }
                    }
                }
                attrs += cf.attributes.len();
                for a in &cf.attributes {
                    attr_bytes += a.info.len();
                }
            }
            eprintln!(
                "[mem] 类 {} | CP 槽 {cp_slots}（Utf8 {utf8s} 条 / {utf8_bytes} B）| 方法 {methods} 字段 {fields} \
                 | 属性 {attrs} 个 / {attr_bytes} B（其中 Code {code_bytes} B）",
                files.len()
            );
        }
        if files.is_empty() {
            return err("没有可执行的 class");
        }
        if std::env::var_os("VMLESS_MEM_STATS").is_some() {
            stats_structures(&files);
        }
        let store = Self::new(files, Some(main_class))?;
        super::prof("ClassStore::new（by_name/methods/build_metas）");
        Ok(store)
    }

    /// **有类表缓存时的快速装载**：先把闭包算出来（纯 id 图），再一次性取字节 + 解析。
    ///
    /// 与老路径的三处差别，就是那 150 ms 的来源：
    /// 1. 引用表来自缓存 → 闭包展开**不扫常量池**（762,850 槽 = 24 MB 随机访存）、
    ///    也不为每条引用分配字符串；
    /// 2. 取字节与解析各只做**一批** —— 老路径按 BFS 层做，14 层里每层都要重开线程；
    /// 3. 字节来自缓存文件（同一句柄定位读），5421 次文件探测变成 0 次。
    ///
    /// 语义与老路径逐条对齐：闭包 = 同一个 BFS（层序一致）、`files` 顺序一致、
    /// `pull_jdk` / 引用过滤 / 模块访问检查（无模块模式时是 no-op）都照旧。
    /// **有类表缓存时的快速装载**：闭包先按「缓存 id 图」走完，再一次性取字节 + 解析。
    ///
    /// 与老路径的三处差别，就是那 150 ms 的来源：
    /// 1. 引用表来自缓存 → 闭包展开**不扫常量池**（762,850 槽 = 24 MB 随机访存），
    ///    也不再为每条引用分配字符串（缓存里连引用目标的下标都是现成的）；
    /// 2. 取字节与解析各只做**一批** —— 老路径按 BFS 层做，14 层里每层都要重开线程；
    /// 3. 字节来自缓存文件（同一句柄定位读），5421 次文件探测变成 0 次。
    ///
    /// 语义与老路径对齐：闭包 = 同一个传递闭包（`pull_jdk` / 引用过滤 / 模块访问检查
    /// 都照旧），`files` 里一个类不少。**类的先后顺序**与老路径不同（这里是工作队列序），
    /// 但顺序不参与任何语义：`main` 按名字找、`by_name` 按名字建、静态初始化按执行序。
    fn load_path_cds(
        cp: &ClassPath,
        cds: &std::sync::Arc<crate::load::cds::Cds>,
        jdk_present: bool,
        main_class: &str,
    ) -> Result<Self, VmError> {
        const NO: u32 = u32::MAX;
        let t_all = std::time::Instant::now();
        let mut seen_id = vec![false; cds.ents.len()];
        let mut seen_str: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut order_id: Vec<u32> = Vec::new();
        let mut order_str: Vec<String> = Vec::new();
        let mut queue: Vec<u32> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        match cds.idx(main_class) {
            Some(k) => {
                seen_id[k] = true;
                queue.push(k as u32);
            }
            None => {
                seen_str.insert(main_class.to_string());
                pending.push(main_class.to_string());
            }
        }
        let bad = |rn: &str| {
            rn.is_empty() || rn.starts_with('[') || rn.contains('<') || rn.contains(';')
        };
        let mut parse_ms = 0f64;
        let mut n_str = 0usize;
        loop {
            // ---- 展开缓存里的类：纯 id 图，一个字符串都不分配 ----
            while let Some(k) = queue.pop() {
                order_id.push(k);
                for (rid, rn) in cds.ref_ids(k as usize) {
                    // 缓存里的引用在写缓存时就按同一套规则过滤过了（见 cds::Writer::add），
                    // 这里只需要过 jdk 策略这一道
                    if !pull_jdk(jdk_present, rn) {
                        continue;
                    }
                    if rid != NO {
                        let rk = rid as usize;
                        if !seen_id[rk] {
                            seen_id[rk] = true;
                            queue.push(rid);
                        }
                    } else if seen_str.insert(rn.to_string()) {
                        pending.push(rn.to_string());
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            // ---- 不在缓存里的类（主类 + 用户自己的类）：要解析才知道它们引用谁 ----
            let names: Vec<String> = std::mem::take(&mut pending);
            let got = cp.find_many(&names);
            if let Some(k) = names.iter().position(|n| n == main_class) {
                if got[k].is_none() {
                    return match cp.find(main_class) {
                        Ok(_) => err(format!("classpath 里找不到主类 {main_class}")),
                        Err(e) => Err(VmError { msg: e, exc: None }),
                    };
                }
            }
            let t = std::time::Instant::now();
            for (i, n) in names.iter().enumerate() {
                let Some(b) = got[i].as_deref() else { continue };
                let Ok(cf) = crate::parse(b) else { continue };
                parse_ms += t.elapsed().as_secs_f64() * 1000.0 * 0.0; // 计入下面统一报
                order_str.push(n.clone());
                n_str += 1;
                let cur_module = cp.module_of_class(n).map(|s| s.to_string());
                for e in cf.constant_pool.iter() {
                    if let Some(CpEntry::Class(ix)) = e {
                        if let Some(raw) = cf.utf8(*ix) {
                            let rn = String::from_utf8_lossy(raw);
                            if let Err(msg) = cp.check_reference(cur_module.as_deref(), n, &rn) {
                                return Err(VmError { msg, exc: None });
                            }
                            if bad(&rn) || !pull_jdk(jdk_present, &rn) {
                                continue;
                            }
                            match cds.idx(&rn) {
                                Some(rk) => {
                                    if !seen_id[rk] {
                                        seen_id[rk] = true;
                                        queue.push(rk as u32);
                                    }
                                }
                                None => {
                                    if seen_str.insert(rn.to_string()) {
                                        pending.push(rn.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            parse_ms += t.elapsed().as_secs_f64() * 1000.0;
        }
        super::prof(&format!(
            "闭包（缓存图）：{} 个类 + {n_str} 个不在缓存，总 {:.1} ms",
            order_id.len(),
            t_all.elapsed().as_secs_f64() * 1000.0
        ));

        // ---- 分批取字节 + 解析 ----
        //
        // **别一次性把 5421 个类的字节全读进来再解析**：那 27 MB 与解析出来的结构
        // 同时存活，峰值 RSS 就多一大截（实测该步到 85.9 MB）。按批「取 → 解析 → 丢字节」
        // 让堆复用同一批块，峰值只剩一批的字节。
        const BATCH: usize = 512;
        let t = std::time::Instant::now();
        let mut read_ms = 0f64;
        let mut batch_parse_ms = 0f64;
        // 顺序与老路径逐条一致：id 段（缓存内）在前、str 段（缓存外）在后
        let mut files: Vec<ClassFile> = Vec::with_capacity(order_id.len() + order_str.len());
        for chunk in order_id.chunks(BATCH) {
            match fetch_parse_batch(cds, cp, chunk, &[]) {
                Ok(v) => {
                    read_ms += v.read_ms;
                    batch_parse_ms += v.parse_ms;
                    files.extend(v.files);
                }
                Err((i, e)) => {
                    let name = cds.name_of(chunk[i] as usize);
                    return Err(VmError { msg: format!("解析 {name} 失败: {e}"), exc: None });
                }
            }
        }
        for chunk in order_str.chunks(BATCH) {
            match fetch_parse_batch(cds, cp, &[], chunk) {
                Ok(v) => {
                    read_ms += v.read_ms;
                    batch_parse_ms += v.parse_ms;
                    files.extend(v.files);
                }
                Err((i, e)) => {
                    return Err(VmError { msg: format!("解析 {} 失败: {e}", chunk[i]), exc: None });
                }
            }
        }
        let wait_ms = read_ms;
        let parse_ms = batch_parse_ms;
        if files.is_empty() {
            return err("没有可执行的 class");
        }
        super::prof(&format!(
            "取字节 + 解析：{} 个类（取字节 {wait_ms:.1} ms，解析 {parse_ms:.1} ms，总 {:.1} ms）",
            files.len(),
            t_all.elapsed().as_secs_f64() * 1000.0
        ));
        if std::env::var_os("VMLESS_MEM_STATS").is_some() {
            stats_structures(&files);
        }
        let store = Self::new(files, Some(main_class))?;
        super::prof("ClassStore::new（by_name/methods/build_metas）");
        Ok(store)
    }

    /// 运行期加入一个类（`ClassLoader.defineClass` 的雏形）：同名的换掉，然后重建
    /// 方法表与字段布局（父类新加字段会移动子类字段的偏移，所以只能整体重建）。
    pub(super) fn add_class(&mut self, cf: ClassFile) -> usize {
        let name = cf.this_class_name().unwrap_or_default();
        let idx = match self.by_name.get(&name) {
            Some(&i) => {
                self.classes[i] = Arc::new(cf);
                i
            }
            None => {
                self.classes.push(Arc::new(cf));
                self.classes.len() - 1
            }
        };
        self.by_name.insert(name.clone(), idx);
        // [dsk-scratch] 增量维护 methods / metas：整体重建是 O(类总数)，
        // 每次 add_class 都是 80ms 级（AWT 一个闭包几百个类 = 几十秒）。
        let existed = !self.metas.contains_key(&name) || self.metas.get(&name).is_some();
        {
            let cf = self.classes[idx].clone();
            for (mi, m) in cf.methods.iter().enumerate() {
                let mn = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                let md = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                let k = meth_hash(&name, &mn, &md);
                self.methods.insert(k, (idx as u32, mi as u32));
            }
        }
        self.build_meta_one(&name);
        let _ = existed;
        idx
    }

    /// 只重建**一个类**的字段元数据（`add_class` 用；语义与 `build_metas` 里同名那段一致）
    fn build_meta_one(&mut self, name: &str) {
        let chain = self.chain(name);
        let mut fields = HashMap::new();
        let mut own = HashMap::new();
        let mut count = 0usize;
        for c in chain.iter().rev() {
            let Some(&ci) = self.by_name.get(c) else { continue };
            let cf = self.classes[ci].clone();
            for f in &cf.fields {
                if f.access_flags & 0x0008 != 0 {
                    continue; // 静态字段不在对象里
                }
                let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
                fields.entry(n.clone()).or_insert(count);
                // 只记「本类自己声明」的那一份 —— 遮蔽时它才是正确的槽
                if c.as_str() == name {
                    own.insert(n, count);
                }
                count += 1;
            }
        }
        self.own_fields.insert(name.to_string(), own);
        self.metas.insert(name.to_string(), ClassMeta { fields, field_count: count });
    }

    fn rebuild_methods(&mut self) {
        self.methods.clear();
        for (ci, cf) in self.classes.iter().enumerate() {
            for (mi, m) in cf.methods.iter().enumerate() {
                let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                let owner = cf.this_class_name().unwrap_or_default();
                let k = meth_hash(&owner, &name, &desc);
                self.methods.insert(k, (ci as u32, mi as u32));
            }
        }
    }

    /// 用一批已解析的类建表
    pub(super) fn new(files: Vec<ClassFile>, main_class: Option<&str>) -> Result<Self, VmError> {
        let classes: Vec<Arc<ClassFile>> = files.into_iter().map(Arc::new).collect();
        if classes.is_empty() {
            return err("没有可执行的 class");
        }
        // 入口类必须排在最前面？不必 —— 按名字找即可
        let mut by_name = HashMap::with_capacity(classes.len());
        for (i, cf) in classes.iter().enumerate() {
            if let Some(n) = cf.this_class_name() {
                by_name.insert(n, i);
            }
        }
        let main = match main_class {
            Some(n) => match by_name.get(n) {
                Some(&i) => i,
                None => 0,
            },
            None => 0,
        };
        // ---- 方法表 / 字段布局 ----
        //
        // 这两张表**按类独立**，所以按类并行算、再顺序插进 HashMap（HashMap 不能并发写）。
        // 串行版这两步在 java.base 的 5421 个类上是 80 ms，并行后 ~20 ms —— 而内容逐字
        // 一致（每个类的条目只依赖它自己的 ClassFile 与父类链）。
        let nthreads = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1).min(8);
        let chunk = classes.len().div_ceil(nthreads.max(1)).max(1);
        let cls: &[Arc<ClassFile>] = &classes;
        let names: &HashMap<String, usize> = &by_name;
        let mut per_class: Vec<Vec<(u64, (u32, u32))>> = Vec::with_capacity(classes.len());
        let mut layouts: Vec<(String, ClassMeta, HashMap<String, usize>)> = Vec::with_capacity(classes.len());
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for (base, part) in cls.chunks(chunk).enumerate() {
                let start = base * chunk;
                hs.push(s.spawn(move || {
                    let mut m: Vec<Vec<(u64, (u32, u32))>> = Vec::with_capacity(part.len());
                    let mut l: Vec<(String, ClassMeta, HashMap<String, usize>)> = Vec::with_capacity(part.len());
                    for (k, cf) in part.iter().enumerate() {
                        let ci = start + k;
                        let owner = cf.this_class_name().unwrap_or_default();
                        let mut rows = Vec::with_capacity(cf.methods.len());
                        for (mi, me) in cf.methods.iter().enumerate() {
                            let name = String::from_utf8_lossy(cf.utf8(me.name_index).unwrap_or_default()).into_owned();
                            let desc = String::from_utf8_lossy(cf.utf8(me.descriptor_index).unwrap_or_default()).into_owned();
                            rows.push((meth_hash(&owner, &name, &desc), (ci as u32, mi as u32)));
                        }
                        m.push(rows);
                        l.push(layout_of(cls, names, &owner));
                    }
                    (m, l)
                }));
            }
            for h in hs {
                // panic 说明我们自己的 bug（布局计算是纯函数，不该炸）。
                // **不能静默跳过**：少一个类的字段布局就是对象布局错，会静默改语义。
                let (m, l) = match h.join() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("[vm] 类表构建线程 panic（字段布局可能不完整）");
                        continue;
                    }
                };
                per_class.extend(m);
                layouts.extend(l);
            }
        });
        let mut methods = HashMap::with_capacity(per_class.iter().map(|v| v.len()).sum::<usize>());
        for rows in per_class {
            for (k, v) in rows {
                methods.insert(k, v);
            }
        }
        let mut store = ClassStore {
            classes,
            by_name,
            metas: HashMap::with_capacity(layouts.len()),
            own_fields: HashMap::new(),
            methods,
            main,
        };
        for (name, meta, own) in layouts {
            store.own_fields.insert(name.clone(), own);
            store.metas.insert(name, meta);
        }
        Ok(store)
    }

    /// 类名 → 继承链（本类 → … → 顶层；未知的祖先名也留在链上）
    pub(super) fn chain(&self, class: &str) -> Vec<String> {
        let mut out = vec![class.to_string()];
        let mut cur = class.to_string();
        for _ in 0..64 {
            match super_of_of(&self.classes, &self.by_name, &cur) {
                Some(s) if s != cur && !out.contains(&s) => {
                    out.push(s.clone());
                    cur = s;
                }
                _ => break,
            }
        }
        out
    }

    /// 一个类**直接**实现的接口（class 文件的 `interfaces` 表）
    pub(super) fn interfaces_of(&self, class: &str) -> Vec<String> {
        let Some(&i) = self.by_name.get(class) else { return Vec::new() };
        let cf = &self.classes[i];
        cf.interfaces
            .iter()
            .filter_map(|ix| match cf.constant_pool.get(*ix as usize).and_then(|e| e.as_ref()) {
                Some(crate::CpEntry::Class(n)) => {
                    cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// 类（含继承链上每个类）实现的全部接口，传递闭包（接口 extends 接口也算）
    pub(super) fn iface_closure(&self, class: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = self.chain(class);
        while let Some(c) = queue.pop() {
            for i in self.interfaces_of(&c) {
                if seen.insert(i.clone()) {
                    out.push(i.clone());
                    queue.push(i);
                }
            }
        }
        out
    }

    fn build_metas(&mut self) {
        let names: Vec<String> = self.by_name.keys().cloned().collect();
        self.own_fields.clear();
        for name in names {
            let chain = self.chain(&name);
            let mut fields = HashMap::new();
            let mut own = HashMap::new();
            let mut count = 0usize;
            for c in chain.iter().rev() {
                let Some(&ci) = self.by_name.get(c) else { continue };
                let cf = self.classes[ci].clone();
                for f in &cf.fields {
                    if f.access_flags & 0x0008 != 0 {
                        continue; // 静态字段不在对象里
                    }
                    let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
                    fields.entry(n.clone()).or_insert(count);
                    if c.as_str() == name.as_str() {
                        own.insert(n, count);
                    }
                    count += 1;
                }
            }
            self.own_fields.insert(name.clone(), own);
            self.metas.insert(name.clone(), ClassMeta { fields, field_count: count });
        }
    }

    pub(crate) fn meta(&self, class: &str) -> Option<&ClassMeta> {
        self.metas.get(class)
    }

    /// 已加载的类名（诊断用）
    pub(super) fn class_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().cloned().collect();
        v.sort();
        v
    }

    /// 这个类的字节码在不在类表里？在就必须执行它，不许拿内建实现顶替
    pub(super) fn has_class(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// 类内部名 → 已解析的 class 文件
    pub(super) fn class_file(&self, name: &str) -> Option<Arc<ClassFile>> {
        self.by_name.get(name).map(|&i| self.classes[i].clone())
    }

    /// 类下标 → 内部名（反射的「调用者是谁」用；越界返回 None）
    pub(super) fn name_of_index(&self, idx: usize) -> Option<String> {
        if idx >= self.classes.len() {
            return None;
        }
        self.classes[idx].this_class_name()
    }

    /// 运行期按需加载一个类**及其引用的闭包**（`Class.forName` 用）。
    ///
    /// 与 `load_path` 是同一套闭包规则，区别是它发生在运行中途：新类直接 `add_class`
    /// 进类表（会顺带重建方法表与字段布局）。已经加载的类不重复加载，所以对
    /// `forName("java.lang.String")` 这类调用是空操作。
    ///
    /// 返回**新加入**的类名。找不到 `root` 自己就返回错误（这是 `ClassNotFoundException`
    /// 的来源），闭包里别的类找不到只跳过（JVMS 允许链接期再报）。
    pub(super) fn load_closure(
        &mut self,
        cp: &ClassPath,
        root: &str,
    ) -> Result<Vec<String>, VmError> {
        let mut added = Vec::new();
        let mut queue: Vec<String> = vec![root.to_string()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let jdk_present = self.has_class("java/lang/Object");
        while let Some(name) = queue.pop() {
            if name.is_empty() || name.starts_with('[') || !seen.insert(name.clone()) {
                continue;
            }
            if self.has_class(&name) {
                continue;
            }
            let bytes = match cp.find(&name) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    if name == root {
                        return err(format!("classpath 里找不到 {root}"));
                    }
                    continue;
                }
                Err(e) => return err(e),
            };
            let cf = crate::parse(&bytes)
                .map_err(|e| VmError { msg: format!("解析 {name} 失败: {e}"), exc: None })?;
            for e in &cf.constant_pool {
                if let Some(CpEntry::Class(ix)) = e {
                    if let Some(raw) = cf.utf8(*ix) {
                        let n = String::from_utf8_lossy(raw).into_owned();
                        if !n.starts_with('[')
                            && !n.contains('<')
                            && !n.contains(';')
                            && pull_jdk(jdk_present, &n)
                        {
                            queue.push(n);
                        }
                    }
                }
            }
            self.add_class(cf);
            added.push(name);
        }
        Ok(added)
    }

    /// 从 `from` 开始沿继承链解析方法；类链上找不到再找接口的 **default 实现**。
    ///
    /// **抽象方法不算实现**：它们没有 Code 属性，解析到了也执行不了。跳过它们才能让
    /// 「接口方法调用」正确落到 default 实现（或落到调用点的 lambda/内建分流）。
    pub(super) fn resolve_method(&self, from: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        for c in self.chain(from) {
            if let Some(v) = self.lookup_method(&c, name, desc) {
                if !self.is_abstract(v) {
                    return Some(v);
                }
            }
        }
        // 接口里的 default 方法：实现类没覆写时落在这里
        for i in self.iface_closure(from) {
            if let Some(v) = self.lookup_method(&i, name, desc) {
                // 抽象声明（没有 Code）不能当实现用
                let (ci, mi) = v;
                if self.classes[ci].methods[mi].attributes.is_empty() || self.is_abstract(v) {
                    continue;
                }
                return Some(v);
            }
        }
        None
    }

    /// 方法表查一次：散列命中后**逐字节验名字**，碰撞就退化成本类内线性扫。
    ///
    /// 为什么要验：`HashMap<u64, _>` 的键是散列，两条不同的 (类,名,描述符) 撞同一个 u64
    /// 时后写的会顶掉前一条。验一遍（以及验不过时线性扫本类）把「概率」变成「不可能错」，
    /// 代价是每次解析多两次 `utf8` 比较。
    fn lookup_method(&self, owner: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        let k = meth_hash(owner, name, desc);
        match self.methods.get(&k) {
            Some(&(ci, mi)) => {
                if self.method_matches(ci, mi, owner, name, desc) {
                    Some((ci as usize, mi as usize))
                } else {
                    self.scan_method(owner, name, desc)
                }
            }
            None => None,
        }
    }

    fn method_matches(&self, ci: u32, mi: u32, owner: &str, name: &str, desc: &str) -> bool {
        let Some(cf) = self.classes.get(ci as usize) else { return false };
        if cf.this_class_name().as_deref() != Some(owner) {
            return false;
        }
        match cf.methods.get(mi as usize) {
            Some(m) => {
                cf.utf8(m.name_index) == Some(name.as_bytes())
                    && cf.utf8(m.descriptor_index) == Some(desc.as_bytes())
            }
            None => false,
        }
    }

    /// 兜底：在这个类自己的方法表里线性找（碰撞时用；类没有几个方法，代价可忽略）
    fn scan_method(&self, owner: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        let &ci = self.by_name.get(owner)?;
        let cf = &self.classes[ci];
        for (mi, m) in cf.methods.iter().enumerate() {
            if cf.utf8(m.name_index) == Some(name.as_bytes())
                && cf.utf8(m.descriptor_index) == Some(desc.as_bytes())
            {
                return Some((ci, mi));
            }
        }
        None
    }

    /// 这个方法有没有实现体（抽象方法没有）
    fn is_abstract(&self, (ci, mi): (usize, usize)) -> bool {
        const ACC_ABSTRACT: u16 = 0x0400;
        self.classes[ci].methods[mi].access_flags & ACC_ABSTRACT != 0
    }

    /// 只在这个类**自己**里找方法（`<clinit>` 不继承，不能用 resolve_method）
    pub(super) fn method_in_class(&self, class: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        self.lookup_method(class, name, desc)
    }

    /// 直接父类名（已加载的类看 super_class，java.* 靠内置表）
    pub(super) fn super_of(&self, class: &str) -> Option<String> {
        super_of_of(&self.classes, &self.by_name, class)
    }

    /// 从 `from` 开始的继承链上，字段名 → 下标
    /// 字段解析（JVMS 5.4.3.2）：从 `from` 出发沿继承链**逐层查该层自己声明的表**。
    ///
    /// 不能查合并过的 `metas[..].fields` —— 那张表用 `or_insert` 保留的是**父类**那份，
    /// 子类遮蔽父类同名字段时永远指不到子类的槽。
    pub(super) fn field_index(&self, from: &str, name: &str) -> Option<usize> {
        for c in self.chain(from) {
            if let Some(m) = self.own_fields.get(&c) {
                if let Some(i) = m.get(name) {
                    return Some(*i);
                }
            }
        }
        // 兜底：链上某个类没进 `own_fields`（内置元数据 / 未加载的父类）时用合并表
        self.metas.get(from).and_then(|m| m.fields.get(name)).copied()
    }

    pub(super) fn field_count(&self, class: &str) -> usize {
        self.metas.get(class).map(|m| m.field_count).unwrap_or(0)
    }

    /// 字段下标 → 字段名（只在这个类**自己**的元数据里找）。
    ///
    /// `Unsafe.staticFieldOffset` 给出「字段下标」，而 `staticFieldBase` 给出的是哨兵对象 ——
    /// 于是 `Unsafe.putInt(base, off)` 落到这里时只剩一个下标，必须能反查名字才能定位静态字段。
    pub(super) fn field_name_at(&self, class: &str, idx: usize) -> Option<String> {
        let m = self.metas.get(class)?;
        m.fields.iter().find(|(_, &i)| i == idx).map(|(n, _)| n.clone())
    }

    /// main 方法
    pub(super) fn main_method(&self) -> Option<usize> {
        let cf = &self.classes[self.main];
        cf.methods.iter().position(|m| {
            let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
            let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
            n == "main" && d == "([Ljava/lang/String;)V"
        })
    }
}
