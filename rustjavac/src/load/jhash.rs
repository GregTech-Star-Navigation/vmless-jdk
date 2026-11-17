//! 复刻 JDK `HashMap` / `HashSet` 的**迭代顺序**。
//!
//! 为什么需要它：`java --describe-module` 的输出顺序并不是「文件里的顺序」也不是
//! 「字典序」—— `exports` 是显式排序的，而 `requires` / `uses` / `provides` /
//! `qualified exports` / `opens` 打的是 `ModuleDescriptor` 里 `HashSet` 的迭代顺序
//! （见 `sun.launcher.LauncherHelper.describeModule`）。
//!
//! 要做到**逐行一致**，只能把这条构造链也复刻出来：
//!
//! ```text
//! ModuleInfo.read:  builder 的 Map<String, X>（HashMap）按声明顺序 put
//! ModuleDescriptor.Builder.build():  new HashSet<>(map.values())   ← 再散列一次
//! ```
//!
//! 规则（JDK 25 实测源码）：
//! - `new HashMap<>()`：表长 16，阈值 12，`size > threshold` 时翻倍；
//! - `new HashSet<>(Collection c)`：`HashMap.newHashMap(max(c.size(), 12))`
//!   → 容量 = `tableSizeFor(ceil(max(n,12)/0.75))`；
//! - `HashSet.newHashSet(k)`：容量 = `tableSizeFor(ceil(k/0.75))`；
//! - 迭代顺序 = 桶下标升序，桶内按插入顺序（链表尾插）；
//! - 桶下标 = `(cap-1) & (h ^ (h >>> 16))`。
//!
//! 只实现「顺序」需要的部分，不做查找。

/// Java `String.hashCode()`（UTF-16 单元累乘 31）
pub fn string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

/// `HashMap.hash(key)`：高 16 位异或到低位
fn spread(h: i32) -> u32 {
    let u = h as u32;
    u ^ (u >> 16)
}

/// `HashMap.tableSizeFor(cap)`：不小于 cap 的最小 2 的幂
fn table_size_for(cap: usize) -> usize {
    let mut n = 1usize;
    while n < cap {
        n <<= 1;
    }
    n
}

/// 一张「只关心迭代顺序」的 Java 哈希表
#[derive(Debug, Clone)]
pub struct JTable {
    cap: usize,
    size: usize,
    buckets: Vec<Vec<usize>>,
    /// id → hashCode（resize 重挂时要按 hash 重新分桶）
    hashes: Vec<i32>,
}

impl JTable {
    /// `new HashMap<>()`：表长 16、阈值 12
    pub fn hash_map() -> Self {
        JTable { cap: 16, size: 0, buckets: vec![Vec::new(); 16], hashes: Vec::new() }
    }

    /// `new HashSet<>(collection)`：`HashMap.newHashMap(max(n, 12))`
    pub fn from_collection(n: usize) -> Self {
        let hint = ((n.max(12) as f64) / 0.75).ceil() as usize;
        Self::with_capacity(table_size_for(hint))
    }

    /// `HashSet.newHashSet(k)`：`tableSizeFor(ceil(k/0.75))`
    pub fn new_hash_set(k: usize) -> Self {
        let hint = ((k as f64) / 0.75).ceil() as usize;
        Self::with_capacity(table_size_for(hint))
    }

    fn with_capacity(cap: usize) -> Self {
        JTable { cap, size: 0, buckets: vec![Vec::new(); cap], hashes: Vec::new() }
    }

    /// 插入一项（`id` 是调用方自己的下标），`hash` 是该元素的 Java `hashCode()`
    pub fn add(&mut self, hash: i32, id: usize) {
        if self.size + 1 > self.cap * 3 / 4 {
            self.resize();
        }
        if self.hashes.len() <= id { self.hashes.resize(id + 1, 0); }
        self.hashes[id] = hash;
        let b = (spread(hash) as usize) & (self.cap - 1);
        self.buckets[b].push(id);
        self.size += 1;
    }

    fn resize(&mut self) {
        // Java 的 resize：按旧表顺序重挂到新表，桶内相对顺序不变
        let ids: Vec<usize> = self.buckets.iter().flatten().copied().collect();
        self.cap *= 2;
        self.buckets = vec![Vec::new(); self.cap];
        for id in ids {
            // 重挂时 hash 未知：这里由 `add_hashed` 的调用方保存过 hash
            let h = self.hashes[id];
            let b = (spread(h) as usize) & (self.cap - 1);
            self.buckets[b].push(id);
        }
    }

    /// 迭代顺序（桶下标升序、桶内插入顺序）
    pub fn order(&self) -> Vec<usize> {
        self.buckets.iter().flatten().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_hash_matches_java() {
        // java: "java.base".hashCode()
        assert_eq!(string_hash("java.base"), "java.base".chars().fold(0i32, |h, c| {
            h.wrapping_mul(31).wrapping_add(c as i32)
        }));
        // 已知值（真 JDK：`"java.lang".hashCode()` → -888658374，
        // `"java.base".hashCode()` → -888956131）
        assert_eq!(string_hash("java.lang"), -888658374);
        assert_eq!(string_hash("java.base"), -888956131);
    }

    #[test]
    fn table_sizes() {
        assert_eq!(table_size_for(16), 16);
        assert_eq!(table_size_for(17), 32);
        assert_eq!(table_size_for(4), 4);
    }

    #[test]
    fn set_iteration_is_bucket_order() {
        // 插 4 个元素到 newHashSet(4)：容量 8，顺序由 hash 决定
        let names = ["a", "b", "c", "d"];
        let mut t = JTable::new_hash_set(names.len());
        for (i, n) in names.iter().enumerate() {
            t.add(string_hash(n), i);
        }
        let order = t.order();
        let mut want = order.clone();
        want.sort();
        assert_eq!(order, want, "桶下标升序");
    }
}
