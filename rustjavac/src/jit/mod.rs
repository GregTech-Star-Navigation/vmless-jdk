//! 方法级 JIT：JVM 字节码 → Cranelift IR → 本地机器码。
//!
//! 覆盖范围（够跑我们自己编译器产出的 int 方法）：
//! 常量、iload/istore、(iinc)、算术、比较跳转、goto、ireturn/return。
//! 操作数栈用 Cranelift 的 SSA 值表示；局部变量放在一块内存里，
//! 由调用方传入指针（与解释器共用同一份 locals 布局）。
//!
//! 本文件只剩入口与 `Jit` 的定义 —— 具体实现按职责分散在同目录各模块里。

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::default_libcall_names;

use crate::{parse, ClassFile};

mod call;
mod classes;
mod divmod;
mod exc;
mod indy;
mod inline;
mod insn;
mod pure;
mod gcfree;
mod layout;
mod natives;
mod refmap;
pub mod rt;
pub mod strtab;
mod translate;

pub use call::CompiledMethod;
pub(crate) use call::param_descs;
pub(crate) use call::param_slots;
use classes::ClassSet;

pub struct Jit {
    module: JITModule,
    /// (类名, 方法名, 描述符) → 已声明/已编译的函数 id
    compiled: std::collections::HashMap<(String, String, String), (cranelift_module::FuncId, usize, i32)>,
    /// 已加载的类：继承链、字段布局、方法解析
    classes: ClassSet,
    /// 待登记的虚方法实现（函数地址要等 finalize 之后才有）
    pending_v: Vec<(u32, u32, cranelift_module::FuncId, u8)>,
    /// 待登记的 `<clinit>`：(类标签, 函数 id)
    pending_clinit: Vec<(u32, cranelift_module::FuncId)>,
    /// 已经安排编译过 `<clinit>` 的类标签（防递归）
    clinit_started: std::collections::HashSet<u32>,
    /// 编不了、只能**解释执行**的方法 → 原因（方法级回退的账本）
    interpreted: std::collections::HashMap<(String, String, String), String>,
    /// 成功编译的方法个数（= `compiled` 里真正 define 过的那些）
    n_compiled: usize,
    /// 内联掉的调用点个数（`jit::inline` 的账）
    n_inlined: usize,
    /// 方法 → 「会不会触发 GC」（`jit::gcfree` 的判据，见那儿的长注释）
    may_gc: std::collections::HashMap<(String, String, String), bool>,
}

#[derive(Debug)]
pub struct JitError(pub String);

type JitFn = unsafe extern "C" fn(*mut i64) -> i32;

impl Jit {
    pub fn new() -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder.set("is_pic", "false").unwrap();
        // Cranelift 的默认 opt_level 是 **none**：不跑 egraph 那一套
        // （GVN/CSE、常量折叠、别名分析、LICM、死代码消除全在里面）。
        // 不设这个等于把生成代码的质量白白交出去 —— 实测把 P2 从 390ms 压到 ~250ms。
        flag_builder.set("opt_level", "speed").unwrap();
        let isa_builder = cranelift_native::builder().expect("native isa");
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .expect("isa");
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        builder.symbol("vmless_new_object", rt::new_object as *const u8);
        builder.symbol("vmless_new_array", rt::new_array as *const u8);
        builder.symbol("vmless_new_multi_array", rt::new_multi_array as *const u8);
        builder.symbol("vmless_set_ref", rt::set_ref as *const u8);
        builder.symbol("vmless_roots_push", rt::roots_push as *const u8);
        builder.symbol("vmless_roots_pop", rt::roots_pop as *const u8);
        builder.symbol("vmless_desc_ptr", strtab::desc_ptr as *const u8);
        builder.symbol("vmless_get_static", rt::get_static as *const u8);
        builder.symbol("vmless_put_static", rt::put_static as *const u8);
        builder.symbol("vmless_clinit", rt::clinit as *const u8);
        builder.symbol("vmless_println_int", rt::println_int as *const u8);
        builder.symbol("vmless_println_bool", rt::println_bool as *const u8);
        builder.symbol("vmless_println_char", rt::println_char as *const u8);
        builder.symbol("vmless_println_long", rt::println_long as *const u8);
        builder.symbol("vmless_println_double", rt::println_double as *const u8);
        builder.symbol("vmless_println_float", rt::println_float as *const u8);
        builder.symbol("vmless_instanceof", rt::instanceof as *const u8);
        builder.symbol("vmless_checkcast", rt::checkcast as *const u8);
        builder.symbol("vmless_println_str", rt::println_str as *const u8);
        builder.symbol("vmless_str_const", strtab::str_const as *const u8);
        builder.symbol("vmless_str_equals", strtab::str_equals as *const u8);
        builder.symbol("vmless_println_obj", strtab::println_str as *const u8);
        builder.symbol("vmless_drem", rt::drem as *const u8);
        builder.symbol("vmless_frem", rt::frem as *const u8);
        builder.symbol("vmless_exc_throw", rt::exc_throw as *const u8);
        builder.symbol("vmless_exc_pending", rt::exc_pending as *const u8);
        builder.symbol("vmless_exc_get", rt::exc_get as *const u8);
        builder.symbol("vmless_exc_clear", rt::exc_clear as *const u8);
        builder.symbol("vmless_exc_set_msg", rt::exc_set_msg as *const u8);
        builder.symbol("vmless_catch_match", rt::catch_match as *const u8);
        builder.symbol("vmless_vcall", rt::vcall as *const u8);
        // 字符串拼接的内建 StringBuilder
        builder.symbol("vmless_sb_new", rt::sb_new as *const u8);
        builder.symbol("vmless_sb_append_int", rt::sb_append_int as *const u8);
        builder.symbol("vmless_sb_append_bool", rt::sb_append_bool as *const u8);
        builder.symbol("vmless_sb_append_long", rt::sb_append_long as *const u8);
        builder.symbol("vmless_sb_append_double", rt::sb_append_double as *const u8);
        builder.symbol("vmless_sb_append_float", rt::sb_append_float as *const u8);
        builder.symbol("vmless_sb_append_obj", rt::sb_append_obj as *const u8);
        builder.symbol("vmless_sb_to_string", rt::sb_to_string as *const u8);
        builder.symbol("vmless_sb_append_char", rt::sb_append_char as *const u8);
        // invokedynamic 的字符串拼接桩（编译期解码 recipe，运行期一次调用拼完）
        builder.symbol("vmless_indy_concat", rt::indy_concat as *const u8);
        // 方法级回退：编不了的方法改走解释器桩
        builder.symbol("vmless_interp_call", rt::interp_call as *const u8);
        // 内建 JDK 类：包装类 / String / Object 协议 / System / Arrays
        builder.symbol("vmless_builtin_vcall", natives::builtin_vcall as *const u8);
        builder.symbol("vmless_builtin_static", natives::builtin_static as *const u8);
        builder.symbol("vmless_builtin_sfield", natives::builtin_sfield as *const u8);
        builder.symbol("vmless_arraycopy", natives::arraycopy as *const u8);
        // 包装类的小整数缓存/布尔单例要在这里建好并登记成永久根：
        // 生成代码每帧都会 push/pop 影子栈，运行期再塞常驻根会截掉帧的根区间
        natives::init_builtin_roots();
        let module = JITModule::new(builder);
        Jit {
            module,
            compiled: std::collections::HashMap::new(),
            classes: ClassSet::new(),
            pending_v: Vec::new(),
            pending_clinit: Vec::new(),
            clinit_started: std::collections::HashSet::new(),
            interpreted: std::collections::HashMap::new(),
            n_compiled: 0,
            n_inlined: 0,
            may_gc: std::collections::HashMap::new(),
        }
    }

    /// 把一个目录里的 .class 都读进来（继承链上的父类必须在这里）
    pub fn add_class_dir(&mut self, dir: &std::path::Path) {
        self.classes.load_dir(dir);
    }

    /// 便捷：从主类文件路径加载它和同包/其它包（同一个包根下）的类
    pub fn load_class_path(&mut self, class_file: &str) {
        let mut name = String::new();
        if let Ok(bytes) = std::fs::read(class_file) {
            if let Ok(cf) = parse(&bytes) {
                name = cf.this_class_name().unwrap_or_default();
                self.classes.insert(cf);
            }
        }
        let dir = std::path::Path::new(class_file)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| ".".into());
        // 主类带包名（`app/X.class`）时从包根开始加载，才能看到 `lib/Y.class` 这种邻居包
        self.add_class_dir(&crate::vm::package_root(&dir, &name));
    }

    pub fn compile_static(
        &mut self,
        cf: &ClassFile,
        name: &str,
        desc: &str,
    ) -> Result<CompiledMethod, JitError> {
        // 主类也进类表（examples 只给一个 cf，同目录的类由 load_class_path/add_class_dir 补）
        let cname = cf
            .this_class_name()
            .ok_or_else(|| JitError("class 没有 this_class".into()))?;
        self.classes.insert(cf.clone());
        // 布局要一次性全部建好：翻译 getfield 时任何类的偏移都可能要查
        for c in self.classes.names() {
            self.classes.build_layout(&c);
        }
        // 入口方法编不了**不算致命**：它自己进「解释执行集合」，调用方仍然是 JIT 的
        // （回退是方法级的）。所以这里先记下结果，finalize 照做 —— 地址不落地的话
        // 已经编好的其它方法就没法被解释器回调。
        let entry = self.compile_into(&cname, name, desc);
        if entry.is_ok() {
            // 入口类的 <clinit> 要在 main 之前跑（JVM 的语义），所以先把它编出来
            self.ensure_clinit(&cname)?;
        }
        self.finalize()?;
        let (func_id, max_locals, _) = entry?;
        // 入口类静态初始化：JVM 在调 main 之前就会做这件事
        rt::clinit(rt::class_tag(&cname));
        let ptr = self.module.get_finalized_function(func_id);
        let code: JitFn = unsafe { std::mem::transmute(ptr) };
        Ok(CompiledMethod {
            name: name.to_string(),
            desc: desc.to_string(),
            code,
            max_locals,
        })
    }

    /// 把定义落地：之后才知道函数地址（虚方法表、`<clinit>` 表、解释器回退表都要它）
    fn finalize(&mut self) -> Result<(), JitError> {
        self.module
            .finalize_definitions()
            .map_err(|e| JitError(e.to_string()))?;
        let pending = std::mem::take(&mut self.pending_v);
        for (tag, sig, fid, ret) in pending {
            let p = self.module.get_finalized_function(fid) as usize;
            rt::register_vmethod(tag, sig, p, ret);
        }
        let pending_clinit = std::mem::take(&mut self.pending_clinit);
        for (tag, fid) in pending_clinit {
            let p = self.module.get_finalized_function(fid) as usize;
            rt::register_clinit(tag, p);
        }
        Ok(())
    }

    /// 成功编译的方法个数
    pub fn compiled_count(&self) -> usize {
        self.n_compiled
    }

    /// 被调方法会不会触发 GC（`jit::gcfree` 的保守判据）。
    ///
    /// 解析不出来就按「会 GC」处理 —— 判反了只会多登记一次根区，
    /// 绝不会漏登记。
    pub(super) fn method_may_gc(&mut self, owner: &str, name: &str, desc: &str) -> bool {
        let key = (owner.to_string(), name.to_string(), desc.to_string());
        if let Some(v) = self.may_gc.get(&key) {
            return *v;
        }
        let v = (|| {
            let cf = self.classes.get(owner)?;
            let m = cf.methods.iter().find(|m| {
                let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
                let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
                n == name && d == desc
            })?;
            let (_, _, body, table) = call::code_of(&cf, m).ok()?;
            Some(gcfree::body_may_gc(&body, !table.is_empty()))
        })()
        .unwrap_or(true);
        self.may_gc.insert(key, v);
        v
    }

    /// **逐方法**试一遍：把类表里每个有字节码的方法都编译一次。
    ///
    /// 这条路径只在入口方法本身编不了时走（见 `examples/run_jit.rs`）：入口要交给
    /// 解释器，但程序里其它的方法不该跟着一起废掉 —— 编得了的都进表，让解释器能回调。
    pub fn compile_all(&mut self) -> Result<(), JitError> {
        let names = self.classes.names();
        for c in names {
            let Some(cf) = self.classes.get(&c) else { continue };
            let cf = cf.clone();
            for m in &cf.methods {
                // native / abstract 没有字节码，跳过（不是「编不了」）
                if m.access_flags & 0x0100 != 0 || m.access_flags & 0x0400 != 0 {
                    continue;
                }
                let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                let _ = self.compile_into(&c, &n, &d);
            }
        }
        self.finalize()
    }

    /// 「解释执行集合」：`(类, 方法, 描述符) → 原因`
    pub fn interpreted(&self) -> &std::collections::HashMap<(String, String, String), String> {
        &self.interpreted
    }

    /// 一行统计（`run_jit` 打到 stderr）：编译 N 个、解释 M 个（按原因归并）
    pub fn stats_line(&self) -> String {
        let mut by_reason: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for why in self.interpreted.values() {
            let key = reason_tag(why);
            *by_reason.entry(key).or_insert(0) += 1;
        }
        let detail = if by_reason.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = by_reason.iter().map(|(k, v)| format!("{k} ×{v}")).collect();
            format!("（原因：{}）", parts.join("、"))
        };
        format!(
            "JIT: 编译 {} 个方法，内联 {} 个调用点，解释 {} 个{}",
            self.n_compiled,
            self.n_inlined,
            self.interpreted.len(),
            detail
        )
    }

    /// 逐个列出被解释的方法（`VMLESS_JIT_VERBOSE=1` 时打）
    pub fn interpreted_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .interpreted
            .iter()
            .map(|(k, why)| format!("  解释执行 {}.{}{}：{}", k.0, k.1, k.2, why))
            .collect();
        out.sort();
        out
    }

    /// 解释器可以回调的 JIT 方法表（纯数值方法，见 `jit::pure`）
    pub fn pure_table(&self) -> crate::vm::JitTable {
        pure::pure_table(self)
    }
}

/// 把一条失败原因归并成统计用的短标签
fn reason_tag(why: &str) -> String {
    let pick = |s: &str| why.starts_with(s);
    if pick("JIT 不支持 invokedynamic") {
        if why.contains("LambdaMetafactory") {
            "invokedynamic/LambdaMetafactory".into()
        } else if why.contains("StringConcatFactory") {
            "invokedynamic/StringConcatFactory".into()
        } else {
            "invokedynamic".into()
        }
    } else if pick("JIT 不支持 synchronized") || why.contains("monitor") {
        "synchronized/monitor".into()
    } else if pick("JIT 不接管 java.lang.invoke") {
        "java.lang.invoke".into()
    } else if pick("JIT 暂不支持字节码") {
        why.to_string()
    } else if why.contains("不能跨 JIT/解释器边界") {
        "引用跨边界".into()
    } else {
        "其它编译失败".into()
    }
}

/// 从文件编译并调用（测试/基准用）
pub fn compile_file(path: &str, name: &str, desc: &str) -> Result<(Jit, CompiledMethod), JitError> {
    let bytes = std::fs::read(path).map_err(|e| JitError(e.to_string()))?;
    let cf = parse(&bytes).map_err(|e| JitError(e.to_string()))?;
    let mut jit = Jit::new();
    let m = jit.compile_static(&cf, name, desc)?;
    Ok((jit, m))
}
