//! 「这个方法不会触发 GC」判据。
//!
//! **为什么需要它**：生成代码在**每个调用点**都要 `roots_push`/`roots_pop` 一次
//! （把本帧根区推给堆的根表、调用回来再把可能被搬过的偏移写回）。这两次调用
//! 实测 **73ns/对**（`with_heap` 两次 TLS + `Rc`/`Vec` 的 8 次 push/pop + 两次
//! ABI 穿越 + 调用点寄存器被打散）。16M 次虚调用里它占 1125ms/1358ms —— 也就是
//! 说，分派表根本不是瓶颈，根区登记才是。
//!
//! 只要被调方**不可能触发 GC**，这对登记就是纯浪费：没有 GC 就没有搬动，
//! 也就没有「回来要刷新」这回事。所以这里的判据是**语义充分条件**：
//!
//! > 方法体里没有任何能让 GC 跑起来的操作 —— 不分配、不 `athrow`、
//! > 不碰 `getstatic`/`putstatic`（可能触发 `<clinit>`）、不 `ldc` 字符串/类
//! > （首次使用要分配）、不加锁、不做会陷入异常的 `checkcast`/`instanceof`/
//! > 除零，**并且一次方法调用都没有**（任何调用都可能间接分配）。
//!
//! 判据刻意取**保守方向**：拿不准就算「会 GC」。算错的代价只是少省一次登记，
//! 绝不会让活对象被回收。因此它是「叶子 + 纯数据流」的集合：取值器/写值器/
//! 算术/比较/分支 —— 真实代码里这类方法很多，虚调用里占比更高。
//!
//! 注意 `getfield`/`putfield` 在本 JIT 里**不发非空检查**（直接 `基址+偏移` 读），
//! 所以它们不产生任何 GC 点，可以留在判据的「安全」一侧。

use super::insn::insn_len_at;

/// 这个字节码里有没有「可能让 GC 跑起来」的指令。
///
/// `has_handler` = 方法带异常表（保守起见直接判为会 GC：handler 里的代码
/// 与主流程共享块结构，判据不去逐块区分）。
pub(super) fn body_may_gc(body: &[u8], has_handler: bool) -> bool {
    if has_handler {
        return true;
    }
    let mut pc = 0usize;
    while pc < body.len() {
        let op = body[pc];
        if may_gc_opcode(op) {
            return true;
        }
        pc += insn_len_at(body, pc).max(1);
    }
    false
}

/// 单条指令会不会引入 GC 点
fn may_gc_opcode(op: u8) -> bool {
    match op {
        // invoke*：任何调用都可能间接分配（本判据不做传递闭包，保守判为会 GC）
        0xb6..=0xba => true,
        // 分配
        0xbb | 0xbc | 0xbd | 0xc5 => true,
        // 抛异常
        0xbf => true,
        // 静态字段：可能触发 <clinit>
        0xb2 | 0xb3 => true,
        // ldc / ldc_w：字符串与 Class 常量首次使用要分配（ldc2_w 只有 long/double）
        0x12 | 0x13 => true,
        // 监视器
        0xc2 | 0xc3 => true,
        // checkcast 会分配 ClassCastException；instanceof/length 走运行时调用
        0xc0 | 0xc1 | 0xbe => true,
        // 除/余：除零要陷入异常（0x72/0x73 的 drem/frem 走运行时调用，一并按会 GC 处理）
        0x6c | 0x6d | 0x70 | 0x71 | 0x72 | 0x73 => true,
        // 老式子程序
        0xa8 | 0xa9 => true,
        // 多数组分配已覆盖；arraylength(0xbe) 已覆盖
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_arithmetic_is_gc_free() {
        // iconst_2; ireturn  （`int v() { return 2; }`）
        assert!(!body_may_gc(&[0x05, 0xac], false));
        // iload_0; getfield; ireturn
        assert!(!body_may_gc(&[0x1a, 0xb4, 0x00, 0x01, 0xac], false));
        // ireturn 之前 iconst_1; iadd
        assert!(!body_may_gc(&[0x1a, 0x04, 0x60, 0xac], false));
    }

    #[test]
    fn allocation_and_calls_may_gc() {
        assert!(body_may_gc(&[0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0xb0], false)); // new
        assert!(body_may_gc(&[0xb8, 0x00, 0x01, 0xac], false)); // invokestatic
        assert!(body_may_gc(&[0xb2, 0x00, 0x01, 0xb0], false)); // getstatic
        assert!(body_may_gc(&[0x12, 0x05, 0xb0], false)); // ldc
        assert!(body_may_gc(&[0xbb, 0x00, 0x01, 0xbf], true)); // 有异常表
    }
}
