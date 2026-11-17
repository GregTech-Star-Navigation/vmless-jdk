//! 常量池：线程 / 类 / 方法 / 栈这些**重复出现**的值只存一次，事件里只放 key。
//!
//! # 块布局（`ChunkParser.fillConstantPools`）
//!
//! ```text
//! [varint size][varint 1][varint timestamp][varint duration][varint delta]
//! [1 字节 checkpoint 类型][varint 池个数]
//!   每个池： [varint 类型 id][varint 条目数] { [varint key][值] }*
//! ```
//!
//! `delta` 是**到下一个常量池块的字节距离**，0 表示没有下一块。所以常量池是一串
//! 链，每一块自己带着跟下一块的偏移 —— 不是「一条记录一个 offset 表」。
//!
//! 常量池块在文件里的位置通常是**所有事件之后**（HotSpot 在 flush 时写），
//! 读侧必须先跳过去把链读完，再回来顺序读事件。真 `jfr` 就是这么干的
//! （`ChunkParser` 构造时 `fillConstantPools` 之后才 `position(eventStart)`）。

use std::collections::HashMap;

use super::metadata::StringConsts;
use crate::model::Value;

/// 一个 chunk 的常量池（已解析成值）。
#[derive(Debug, Clone, Default)]
pub struct ConstantPool {
    map: HashMap<(i64, i64), Value>,
    /// `java.lang.String` 的类型 id（每个文件不一样），字符串引用按它查。
    string_type_id: Option<i64>,
}

impl ConstantPool {
    pub fn new() -> Self {
        ConstantPool::default()
    }

    pub fn set_string_type_id(&mut self, id: i64) {
        self.string_type_id = Some(id);
    }

    pub fn insert(&mut self, type_id: i64, key: i64, v: Value) {
        self.map.insert((type_id, key), v);
    }

    pub fn get(&self, type_id: i64, key: i64) -> Option<&Value> {
        self.map.get(&(type_id, key))
    }

    /// 事件里 `constantPool=true` 的字段解析：key 0 表示 null（与 Java 的 `ConstantMap.get` 一致）。
    pub fn resolve(&self, type_id: i64, key: i64) -> Value {
        match self.get(type_id, key) {
            Some(v) => v.clone(),
            None => Value::Null,
        }
    }

    /// 单层解引用：把 `Value::Ref` 换成池里的值的**引用**（不克隆）。
    ///
    /// 为什么不做「解析时就把引用展开进值树」：一次录制里同一个线程/类/方法会被
    /// 几十万条事件引用，展开等于把它们复制几十万遍 —— 54MB 的文件能吃掉 2.5GB 内存。
    /// 真 jfr 也是共享 `RecordedThread` 对象，不复制。
    pub fn deref<'a>(&'a self, v: &'a Value) -> &'a Value {
        match v {
            Value::Ref { type_id, key } => self.get(*type_id, *key).unwrap_or(&NULL_VALUE),
            other => other,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl StringConsts for ConstantPool {
    fn get_string(&self, key: i64) -> Option<String> {
        let tid = self.string_type_id?;
        match self.get(tid, key) {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }
}

/// 把常量池条目（已按类型聚合、并排好序）写成一条链。
///
/// 返回 `(块字节, 每条目在块内的 (类型id, key) 顺序)` —— 顺序不需要返回，
/// 这里只负责序列化，写侧自己知道怎么引用。
pub struct PoolWriter;

impl PoolWriter {
    /// `pools`: `(类型 id, [(key, 值)])`，每个元素是一个池。
    /// 值的序列化由调用方给的回调完成（因为要递归用 CP 引用）。
    pub fn write_block<F>(pools: &[(i64, Vec<(i64, Value)>)], mut write_value: F) -> Vec<u8>
    where
        F: FnMut(&mut Vec<u8>, i64, &Value),
    {
        let mut payload = Vec::new();
        super::write_leb(&mut payload, 0); // timestamp：只在流式录制里有意义
        super::write_leb(&mut payload, 0); // duration
        super::write_leb(&mut payload, 0); // delta：单块链的终点
        payload.push(super::CP_CHUNK_HEADER | super::CP_STATICS);
        super::write_leb(&mut payload, pools.len() as i64);
        for (type_id, entries) in pools {
            super::write_leb(&mut payload, *type_id);
            super::write_leb(&mut payload, entries.len() as i64);
            for (key, v) in entries {
                super::write_leb(&mut payload, *key);
                write_value(&mut payload, *type_id, v);
            }
        }
        let mut out = Vec::with_capacity(payload.len() + 8);
        super::push_block(&mut out, super::BLOCK_CHECKPOINT, &payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_resolve_and_string_lookup() {
        let mut cp = ConstantPool::new();
        cp.set_string_type_id(7);
        cp.insert(7, 1, Value::Str("main".into()));
        cp.insert(20, 1, Value::Int(42));
        assert_eq!(cp.get_string(1), Some("main".to_string()));
        assert_eq!(cp.get_string(2), None);
        assert_eq!(cp.resolve(20, 1), Value::Int(42));
        // key 0 = null
        assert_eq!(cp.resolve(20, 0), Value::Null);
        // 不存在的 key 也当 null，不 panic
        assert_eq!(cp.resolve(20, 99), Value::Null);
    }
}

/// `deref` 找不到目标时返回的空值（静态量，避免每次分配）。
static NULL_VALUE: Value = Value::Null;

impl ConstantPool {
    /// 反查：给定值，找它在池里的键（同类型里取最小键）。
    ///
    /// 用途是 `RecordedClassLoader.getId()`：真文件里 `jdk.types.ClassLoader` 只有
    /// `type` / `name` 两个字段，打印出来的 `(id = N)` 就是**池键**本身。
    /// 值在池里是按内容去重的，所以按内容反查即可（同内容多键时取最小，稳定）。
    pub fn find_key(&self, type_id: i64, v: &Value) -> Option<i64> {
        let mut best: Option<i64> = None;
        for ((t, k), cand) in self.map.iter() {
            if *t != type_id || cand != v {
                continue;
            }
            best = Some(match best {
                Some(b) if b <= *k => b,
                _ => *k,
            });
        }
        best
    }
}
