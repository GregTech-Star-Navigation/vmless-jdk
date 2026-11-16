//! JIT 分配接进 librustjrt 分代 GC 之后的基准：分配大量**短命**对象。
//!
//! 接 GC 之前 JIT 的 `new_object` 走的是自己的裸 bump 区（`std::alloc::alloc_zeroed`），
//! 对象永不回收 —— 堆用量随分配次数单调上涨，到 256 MB 直接 panic("JIT heap full")。
//! 现在对象落在 librustjrt 的分代堆里：eden 满了就 minor，活对象搬走、垃圾回收。
//!
//! 跑两轮（n 和 4n）：第二轮分配数是第一轮的四倍，但跑完堆占用应该几乎不变 ——
//! 这才是「GC 真的在回收」，而不只是「堆够大没炸」。
//!
//! 输出：
//!   ALLOC_NS   —— 每次迭代的耗时（含分配、字段写、GC 摊销）
//!   GC_COUNT   —— 这一轮触发了多少次 GC
//!   LIVE_AFTER —— 跑完之后堆里还活着的字节数
//!   HEAP_MB    —— 堆占用（MB）
//!
//! 跑：`cargo run --release --example bench_jit_gc`（可带一个次数参数）

use classfile::jit::rt;
use classfile::jit::Jit;
use classfile::{compiler, parser};

/// 每次迭代 new 一个 16 字节对象、写两个字段、立刻丢掉 —— 典型的短命对象模式
const SRC: &str = r#"
class GcNode {
    int v;
    GcNode next;
    static int run(int n) {
        int acc = 0;
        int i = 0;
        while (i < n) {
            GcNode a = new GcNode();
            a.v = i;
            a.next = null;
            acc = acc + (a.v & 1);
            i = i + 1;
        }
        return acc;
    }
}
"#;

/// 一轮：跑 n 次分配，返回 (耗时, GC 次数, 跑完的堆占用)
fn round(m: &classfile::jit::CompiledMethod, n: i64) -> (std::time::Duration, u64, usize) {
    let gc0 = rt::gc_count();
    let t = std::time::Instant::now();
    let r = m.call(&[n]);
    let dt = t.elapsed();
    let expect = ((n + 1) / 2) as i32;
    assert_eq!(r, expect, "JIT 计算结果不对（{n} 次分配）：分配/引用路径坏了");
    (dt, rt::gc_count() - gc0, rt::heap_live_bytes())
}

fn main() {
    // 小堆：让 eden 很快填满，GC 才跑得起来（默认 64MB 对 100 万个小对象来说太大）
    std::env::set_var("VMLESS_HEAP_MB", "8");
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);

    let cu = parser::parse_compilation_unit(SRC).expect("解析 Java 源码");
    let classes = compiler::compile_with_path(&cu, &[]).expect("编译");
    let (_, bytes) = classes.iter().find(|(name, _)| name == "GcNode").expect("GcNode");
    let cf = classfile::parse(bytes).expect("解析 class");

    let mut jit = Jit::new();
    let m = jit.compile_static(&cf, "run", "(I)I").expect("JIT run(I)I");

    // 第一轮：n 次
    let (dt1, gc1, live1) = round(&m, n);
    let total1 = (n as usize) * 16; // 8 字节头 + 2 个 8 字节字段
    println!("RESULT={}", (n + 1) / 2);
    println!("ALLOC_NS={:.1}", dt1.as_nanos() as f64 / n as f64);
    println!("GC_COUNT={gc1}");
    println!("LIVE_AFTER={live1}");
    println!("HEAP_MB={:.2}", live1 as f64 / (1024.0 * 1024.0));
    println!(
        "（共分配 {:.1} MB，跑完堆里只剩 {:.2} MB）",
        total1 as f64 / (1024.0 * 1024.0),
        live1 as f64 / (1024.0 * 1024.0)
    );

    // 第二轮：4n 次。分配量翻四倍，堆占用必须基本不变
    let (dt2, gc2, live2) = round(&m, n * 4);
    println!("ROUND2_ALLOC_NS={:.1}", dt2.as_nanos() as f64 / (n * 4) as f64);
    println!("ROUND2_GC_COUNT={gc2}");
    println!("ROUND2_LIVE_AFTER={live2}");

    assert!(gc1 > 0, "一次 GC 都没触发：分配没接进分代堆？");
    assert!(
        live1 < 8 * 1024 * 1024,
        "跑完堆里还剩 {live1} 字节：短命对象没被回收（接 GC 前就是这种单调上涨）"
    );
    assert!(
        live2 <= live1 + 1024 * 1024,
        "分配量翻四倍后堆占用从 {live1} 涨到 {live2}：回收跟不上分配"
    );
    println!(
        "OK：{n} 次 → {gc1} 次 GC、{:.2} MB；{} 次 → {gc2} 次 GC、{:.2} MB（堆占用不随分配次数增长）",
        live1 as f64 / (1024.0 * 1024.0),
        n * 4,
        live2 as f64 / (1024.0 * 1024.0)
    );
}
