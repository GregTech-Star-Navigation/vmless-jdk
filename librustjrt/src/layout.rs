//! 类布局表：`class_id → 字段位图 / 大小 / 哪些槽是引用`。
//!
//! 这是修掉「全局只有一个类布局」的东西：对象头里存 `class_id`，
//! GC 追引用时**按对象自己的类**取位图（见 `young::scan_slots` / `old::major`）。

use crate::object::{Object, HEADER};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotKind {
    Prim,
    Ref,
}

const WORD: usize = 64;

/// 一个类的布局。
pub struct ClassLayout {
    pub id: u32,
    pub name: String,
    /// 字段槽数（数组类为 0）
    pub field_count: u32,
    /// 固定大小的普通对象字节数（数组类为 0）
    pub size: u32,
    pub is_array: bool,
    /// 数组元素字节数
    pub elem_size: u32,
    /// 数组元素是否全是引用
    pub ref_elem: bool,
    /// 哪些字段槽是引用（位图，可支持任意多个字段）
    refs: Box<[u64]>,
}

impl ClassLayout {
    fn object(id: u32, name: &str, fields: &[SlotKind]) -> Self {
        let mut refs = vec![0u64; fields.len().div_ceil(WORD)].into_boxed_slice();
        for (i, f) in fields.iter().enumerate() {
            if *f == SlotKind::Ref {
                refs[i / WORD] |= 1 << (i % WORD);
            }
        }
        ClassLayout {
            id,
            name: name.to_string(),
            field_count: fields.len() as u32,
            size: Object::size_for_fields(fields.len()) as u32,
            is_array: false,
            elem_size: 0,
            ref_elem: false,
            refs,
        }
    }

    fn array(id: u32, name: &str, elem_size: u32, ref_elem: bool) -> Self {
        ClassLayout {
            id,
            name: name.to_string(),
            field_count: 0,
            size: 0,
            is_array: true,
            elem_size,
            ref_elem,
            refs: Box::new([]),
        }
    }

    /// 第 `i` 个字段是否是引用槽（数组类返回 false，数组用 `ref_elem`）
    #[inline]
    pub fn is_ref_slot(&self, i: u32) -> bool {
        let w = i as usize / WORD;
        match self.refs.get(w) {
            Some(bits) => bits & (1u64 << (i as usize % WORD)) != 0,
            None => false,
        }
    }

    /// 遍历所有引用槽下标（普通对象）。位图遍历，跳过非引用槽。
    #[inline]
    pub fn for_each_ref(&self, mut f: impl FnMut(u32)) {
        for (w, &bits) in self.refs.iter().enumerate() {
            let mut b = bits;
            while b != 0 {
                let t = b.trailing_zeros();
                f((w * WORD) as u32 + t);
                b &= b - 1;
            }
        }
    }

    #[inline]
    pub fn array_size(&self, len: u32) -> u32 {
        Object::size_for_array(len as usize, self.elem_size as usize) as u32
    }

    /// 该类的实例占多少字节（数组按 `length` 算）
    #[inline]
    pub fn total_size(&self, length: u32) -> u32 {
        if self.is_array {
            self.array_size(length)
        } else {
            self.size
        }
    }
}

/// 类布局表。`class_id` 就是下标，`0` 号是保留的 `<opaque>`：
/// 没有任何引用槽 —— 用 `alloc_sized` 给未注册的类分配时用它，GC 不会追踪其字段。
pub struct LayoutTable {
    classes: Vec<ClassLayout>,
}

impl Default for LayoutTable {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutTable {
    pub fn new() -> Self {
        let opaque = ClassLayout {
            id: 0,
            name: "<opaque>".to_string(),
            field_count: 0,
            size: HEADER as u32,
            is_array: false,
            elem_size: 0,
            ref_elem: false,
            refs: Box::new([]),
        };
        LayoutTable { classes: vec![opaque] }
    }

    /// 注册一个普通类，返回 `class_id`
    pub fn register_object(&mut self, name: &str, fields: &[SlotKind]) -> u32 {
        let id = self.classes.len() as u32;
        self.classes.push(ClassLayout::object(id, name, fields));
        id
    }

    /// 注册一个数组类：元素按 `elem_size` 字节排列，`ref_elem` 表示元素是引用
    pub fn register_array(&mut self, name: &str, elem_size: u32, ref_elem: bool) -> u32 {
        let id = self.classes.len() as u32;
        self.classes.push(ClassLayout::array(id, name, elem_size, ref_elem));
        id
    }

    /// 取类布局；未知 `class_id` 退化成 `<opaque>`（不追踪引用，宁可漏活不可炸）
    #[inline]
    pub fn class(&self, id: u32) -> &ClassLayout {
        match self.classes.get(id as usize) {
            Some(c) => c,
            None => &self.classes[0],
        }
    }

    pub fn try_class(&self, id: u32) -> Option<&ClassLayout> {
        self.classes.get(id as usize)
    }

    #[inline]
    pub fn total_size(&self, class_id: u32, length: u32) -> u32 {
        self.class(class_id).total_size(length)
    }

    pub fn len(&self) -> usize {
        self.classes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.classes.iter().map(|c| c.name.as_str())
    }
}
