//! 常量池构建器：自动去重，返回从 1 开始的索引。

use crate::CpEntry;
use std::collections::HashMap;

/// 常量池构建器：自动去重，返回从 1 开始的索引。
#[derive(Default)]
pub struct CpBuilder {
    entries: Vec<Option<CpEntry>>,
    utf8_cache: HashMap<String, u16>,
    class_cache: HashMap<u16, u16>,
    nat_cache: HashMap<(u16, u16), u16>,
    field_cache: HashMap<(u16, u16), u16>,
    method_cache: HashMap<(u16, u16), u16>,
    iface_method_cache: HashMap<(u16, u16), u16>,
    string_cache: HashMap<u16, u16>,
    method_type_cache: HashMap<u16, u16>,
    /// `BootstrapMethods` 属性：每项 `(bootstrap method_ref, 静态参数...)`。
    /// `invokedynamic` 的操作数是这张表的下标，所以它必须在**整份类**里唯一编号。
    pub(crate) bsm: Vec<(u16, Vec<u16>)>,
}

impl CpBuilder {
    pub fn new() -> Self {
        CpBuilder { entries: vec![None], ..Default::default() }
    }

    pub(crate) fn push(&mut self, e: CpEntry) -> u16 {
        let idx = self.entries.len() as u16;
        let slots = e.slots();
        self.entries.push(Some(e));
        for _ in 1..slots {
            self.entries.push(None);
        }
        idx
    }

    pub fn utf8(&mut self, s: &str) -> u16 {
        if let Some(i) = self.utf8_cache.get(s) {
            return *i;
        }
        let i = self.push(CpEntry::Utf8(s.as_bytes().to_vec()));
        self.utf8_cache.insert(s.to_string(), i);
        i
    }

    pub fn class(&mut self, name: &str) -> u16 {
        let n = self.utf8(name);
        if let Some(i) = self.class_cache.get(&n) {
            return *i;
        }
        let i = self.push(CpEntry::Class(n));
        self.class_cache.insert(n, i);
        i
    }

    pub fn name_and_type(&mut self, name: &str, desc: &str) -> u16 {
        let n = self.utf8(name);
        let d = self.utf8(desc);
        if let Some(i) = self.nat_cache.get(&(n, d)) {
            return *i;
        }
        let i = self.push(CpEntry::NameAndType(n, d));
        self.nat_cache.insert((n, d), i);
        i
    }

    pub fn field_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, desc);
        if let Some(i) = self.field_cache.get(&(c, nt)) {
            return *i;
        }
        let i = self.push(CpEntry::FieldRef(c, nt));
        self.field_cache.insert((c, nt), i);
        i
    }

    pub fn method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, desc);
        if let Some(i) = self.method_cache.get(&(c, nt)) {
            return *i;
        }
        let i = self.push(CpEntry::MethodRef(c, nt));
        self.method_cache.insert((c, nt), i);
        i
    }

    /// 接口方法引用：invokeinterface 的操作数**必须**是 InterfaceMethodref 项，
    /// 用 MethodRef 会被 HotSpot 判成 `Illegal type at constant pool entry`
    pub fn interface_method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, desc);
        let key = (c, nt);
        if let Some(i) = self.iface_method_cache.get(&key) {
            return *i;
        }
        let i = self.push(CpEntry::InterfaceMethodRef(c, nt));
        self.iface_method_cache.insert(key, i);
        i
    }

    pub fn string(&mut self, s: &str) -> u16 {
        let u = self.utf8(s);
        if let Some(i) = self.string_cache.get(&u) {
            return *i;
        }
        let i = self.push(CpEntry::String(u));
        self.string_cache.insert(u, i);
        i
    }

    pub fn int(&mut self, v: i32) -> u16 {
        self.push(CpEntry::Integer(v as u32))
    }

    /// long 常量（常量池里占两个槽）
    pub fn long(&mut self, v: i64) -> u16 {
        self.push(CpEntry::Long(v as u64))
    }

    /// double 常量（常量池里占两个槽，ldc2_w 取用）
    pub fn double(&mut self, v: f64) -> u16 {
        self.push(CpEntry::Double(v.to_bits()))
    }

    /// float 常量（占一个槽，ldc/ldc_w 取用）
    pub fn float(&mut self, v: f32) -> u16 {
        self.push(CpEntry::Float(v.to_bits()))
    }

    pub fn entries(self) -> Vec<Option<CpEntry>> {
        self.entries
    }

    // ---------------------------------------------------------------- invokedynamic

    /// `CONSTANT_MethodType`（一个方法描述符）。
    pub fn method_type(&mut self, desc: &str) -> u16 {
        let u = self.utf8(desc);
        if let Some(i) = self.method_type_cache.get(&u) {
            return *i;
        }
        let i = self.push(CpEntry::MethodType(u));
        self.method_type_cache.insert(u, i);
        i
    }

    /// `CONSTANT_MethodHandle`。`kind` 用 JVMS 的引用种类
    /// （5 = invokeVirtual，6 = invokeStatic，7 = invokeSpecial，9 = invokeInterface）。
    pub fn method_handle(&mut self, kind: u8, owner: &str, name: &str, desc: &str) -> u16 {
        let r = if kind == 9 {
            self.interface_method_ref(owner, name, desc)
        } else {
            self.method_ref(owner, name, desc)
        };
        self.push(CpEntry::MethodHandle(kind, r))
    }

    /// 注册一条 `BootstrapMethods` 项，返回它的下标。
    pub fn bootstrap(&mut self, method_ref: u16, args: Vec<u16>) -> u16 {
        if let Some(i) = self.bsm.iter().position(|(m, a)| *m == method_ref && *a == args) {
            return i as u16;
        }
        self.bsm.push((method_ref, args));
        (self.bsm.len() - 1) as u16
    }

    /// `CONSTANT_InvokeDynamic`：`(引导方法下标, 名字+描述符)`。
    pub fn invoke_dynamic(&mut self, name: &str, desc: &str, bsm_index: u16) -> u16 {
        let nt = self.name_and_type(name, desc);
        self.push(CpEntry::InvokeDynamic(bsm_index, nt))
    }

    /// 取走 `BootstrapMethods`（写属性用），其余照常交出去
    pub fn take_bootstrap(&mut self) -> Vec<(u16, Vec<u16>)> {
        std::mem::take(&mut self.bsm)
    }

    /// 克隆一份常量池（用于先算索引、后组装）
    pub fn snapshot(&self) -> Vec<Option<CpEntry>> {
        self.entries.clone()
    }
}
