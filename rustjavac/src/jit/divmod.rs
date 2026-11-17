//! 有符号整数除/余常数 → 乘魔数（strength reduction）。
//!
//! **为什么需要它**：Cranelift 不做这个（`opts/arithmetic.isle` 里就写着
//! `;; TODO: strength reduction: div to shifts` / `;; TODO: div/rem by constants ->
//! magic multiplications`），`sdiv`/`srem` 原样落到 x86 的 `idiv` 上。`idiv r64`
//! 是 20~40 周期的串行指令（延迟高、吞吐 1/6），而 JVM 里 `x % 1009`、`x / 3`
//! 这种写法遍地都是 —— 实测 P2 那个 4×800 万次的 `(i*31+7) % 1009`，把 idiv
//! 换成乘魔数之后 **390ms → 150ms**。
//!
//! **做法**：Granlund–Montgomery 定理。对 `0 <= n < 2^W`、常数 `d >= 2`，取
//! `l = ceil(log2 d)`、`M = ceil(2^(W+l) / d)`，则
//! `floor(n*M / 2^(W+l)) == floor(n/d)`。
//!
//! 定理的前提 `2^(W+l) <= M*d <= 2^(W+l) + 2^l` 由 `M = ceil(...)` 与
//! `d - 1 <= 2^l` 直接给出 —— **不用像 Hacker's Delight 那样靠循环搜魔数**，
//! 也就没有「搜出来的魔数到底对不对」的疑问。
//!
//! 因为 `d > 2^(l-1)` 必然成立，`M` 一定 `>= 2^W`，写成 `M = 2^W + M'` 之后
//! （`0 <= M' < 2^W`，正好是一个字）：
//!
//! ```text
//!   t = umulhi(n, M')                    // n*M' 的高 W 位
//!   q = (t + ((n - t) >> 1)) >> (l - 1)  // 恒等于 floor(n*M / 2^(W+l)) = n/d
//! ```
//!
//! **有符号**：先取绝对值做无符号除法，再按符号还原 ——
//! `sgn = x >> (W-1)`（算术右移）、`ax = (x ^ sgn) - sgn`（= |x|，最小值也成立）、
//! `q = (aq ^ sign) - sign`、`r = x - q*d`（数学上 `|q*d| <= |x|`，所以回绕是安全的；
//! `MIN / -1` 恰好回绕成 `MIN`、余 0，与 JVM 规范一致）。

use cranelift_codegen::ir::{types, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

/// 常数除数拆出来的乘魔数方案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Magic {
    /// 魔数的高位部分 `M' = M - 2^W`（一定 `< 2^W`，一个字装得下）
    pub m: u64,
    /// 右移位数 `l = ceil(log2 d)`，`l >= 1`
    pub s: u32,
}

/// 对 `0 <= n < 2^W` 算 `(M', l)`；`d` 必须 >= 2。
pub(super) fn magic(width: u32, d: u64) -> Magic {
    assert!(d >= 2, "magic 只处理 d >= 2");
    assert!(width == 32 || width == 64);
    let w = width as u32;
    let l = 64 - (d - 1).leading_zeros(); // ceil(log2 d)，d>=2 时 >=1
    let num: u128 = 1u128 << (w + l);
    let m = (num + d as u128 - 1) / d as u128;
    // 定理前提：2^(W+l) <= M*d <= 2^(W+l) + 2^l
    let md = m * d as u128;
    debug_assert!(md >= num && md - num <= (1u128 << l), "magic 前提不成立 d={d}");
    // d >= 2^(l-1) 保证 M >= 2^W，所以 M' 一定落在 W 位里
    debug_assert!(m >= (1u128 << w), "magic 的 M 该 >= 2^W，d={d}");
    Magic { m: (m - (1u128 << w)) as u64, s: l }
}

/// `q = n / d`（无符号，`n` 的位宽 = `width`）
pub(super) fn emit_udiv_const(fb: &mut FunctionBuilder, n: Value, g: &Magic, width: u32) -> Value {
    let ty = if width == 32 { types::I32 } else { types::I64 };
    let m = fb.ins().iconst(ty, g.m as i64);
    let t = fb.ins().umulhi(n, m);
    let diff = fb.ins().isub(n, t);
    let half = fb.ins().ushr_imm(diff, 1);
    let sum = fb.ins().iadd(t, half);
    fb.ins().ushr_imm(sum, (g.s - 1) as i64)
}

/// 该位宽的符号掩码（全 1 = 负数）
fn sign_mask(fb: &mut FunctionBuilder, x: Value, width: u32) -> Value {
    if width == 32 {
        fb.ins().sshr_imm(x, 31)
    } else {
        fb.ins().sshr_imm(x, 63)
    }
}

/// 有符号除法：`x / d`（`d != 0`）
pub(super) fn emit_sdiv_const(fb: &mut FunctionBuilder, x: Value, d: i64, width: u32) -> Value {
    let ty = if width == 32 { types::I32 } else { types::I64 };
    if d == 1 {
        return x;
    }
    if d == -1 {
        // MIN / -1 回绕成 MIN，与 JVM 一致
        let zero = fb.ins().iconst(ty, 0);
        return fb.ins().isub(zero, x);
    }
    let ad = if d < 0 { (d as i128).unsigned_abs() as u64 } else { d as u64 };
    let g = magic(width, ad);
    let sgn = sign_mask(fb, x, width);
    let ax = {
        let xa = fb.ins().bxor(x, sgn);
        fb.ins().isub(xa, sgn)
    };
    let aq = emit_udiv_const(fb, ax, &g, width);
    // 结果的符号 = x 的符号 ^ d 的符号
    let sign = if d < 0 {
        let all = fb.ins().iconst(ty, -1);
        fb.ins().bxor(sgn, all)
    } else {
        sgn
    };
    let qa = fb.ins().bxor(aq, sign);
    fb.ins().isub(qa, sign)
}

/// 有符号取余：`x % d`（`d != 0`）。按 JVM 语义 —— 符号跟被除数走。
pub(super) fn emit_srem_const(fb: &mut FunctionBuilder, x: Value, d: i64, width: u32) -> Value {
    let ty = if width == 32 { types::I32 } else { types::I64 };
    if d == 1 || d == -1 {
        return fb.ins().iconst(ty, 0);
    }
    let q = emit_sdiv_const(fb, x, d, width);
    let dd = fb.ins().iconst(ty, d);
    // |q*d| <= |x|，所以 i32/i64 回绕是安全的（MIN/-1 也成立）
    let qd = fb.ins().imul(q, dd);
    fb.ins().isub(x, qd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全量 2^32 穷举很慢（分钟级），默认只跑抽样；`VMLESS_DIVMOD_FULL=1` 时跑全量。
    fn full() -> bool {
        std::env::var_os("VMLESS_DIVMOD_FULL").is_some()
    }

    fn udiv32(g: &Magic, n: u32) -> u32 {
        let hi = ((n as u64 * g.m) >> 32) as u32;
        let diff = n.wrapping_sub(hi);
        hi.wrapping_add(diff >> 1) >> (g.s - 1)
    }
    fn udiv64(g: &Magic, n: u64) -> u64 {
        let hi = ((n as u128 * g.m as u128) >> 64) as u64;
        let diff = n.wrapping_sub(hi);
        hi.wrapping_add(diff >> 1) >> (g.s - 1)
    }
    fn sdiv32(x: i32, d: i32) -> i32 {
        if d == 1 {
            return x;
        }
        if d == -1 {
            return (0i32).wrapping_sub(x);
        }
        let g = magic(32, d.unsigned_abs() as u64);
        let sgn = x >> 31;
        let ax = ((x ^ sgn).wrapping_sub(sgn)) as u32;
        let aq = udiv32(&g, ax) as i32;
        let sign = if d < 0 { sgn ^ -1 } else { sgn };
        (aq ^ sign).wrapping_sub(sign)
    }
    fn srem32(x: i32, d: i32) -> i32 {
        if d == 1 || d == -1 {
            return 0;
        }
        x.wrapping_sub(sdiv32(x, d).wrapping_mul(d))
    }
    fn sdiv64(x: i64, d: i64) -> i64 {
        if d == 1 {
            return x;
        }
        if d == -1 {
            return (0i64).wrapping_sub(x);
        }
        let g = magic(64, d.unsigned_abs());
        let sgn = x >> 63;
        let ax = ((x ^ sgn).wrapping_sub(sgn)) as u64;
        let aq = udiv64(&g, ax) as i64;
        let sign = if d < 0 { sgn ^ -1 } else { sgn };
        (aq ^ sign).wrapping_sub(sign)
    }
    fn srem64(x: i64, d: i64) -> i64 {
        if d == 1 || d == -1 {
            return 0;
        }
        x.wrapping_sub(sdiv64(x, d).wrapping_mul(d))
    }

    const D32: [i32; 12] = [
        2, 3, 7, 10, 1009, 1000_000_007, 65536, 1 << 30, i32::MIN, -3, -1009, -65536,
    ];

    /// 一批最能压出边界错的被除数
    fn edge32() -> Vec<i32> {
        let mut v = vec![
            0, 1, -1, 2, -2, i32::MAX, i32::MIN, i32::MIN + 1, i32::MAX - 1, 1 << 30, -(1 << 30),
        ];
        for k in 0..600i64 {
            v.push(k as i32);
            v.push(-(k as i32));
        }
        v
    }

    #[test]
    fn divmod_const_i32() {
        let mut xs = edge32();
        // 抽样：2^22 个伪随机 + 0..2^17 稠密段
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..(1 << 22) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            xs.push(seed as u32 as i32);
        }
        for i in 0..(1 << 17) {
            xs.push(i);
            xs.push(-i);
        }
        for d in D32 {
            for &x in &xs {
                assert_eq!(sdiv32(x, d), x.wrapping_div(d), "sdiv x={x} d={d}");
                assert_eq!(srem32(x, d), x.wrapping_rem(d), "srem x={x} d={d}");
            }
        }
        if full() {
            // 全量 2^32 穷举：默认 12 个除数 × 2 个运算要十几分钟，所以允许
            // 用 VMLESS_DIVMOD_DIVISORS=1009（逗号分隔）只穷举关心的那几个。
            let want: Vec<i32> = std::env::var("VMLESS_DIVMOD_DIVISORS")
                .map(|s| s.split(0x2c as char).filter_map(|t| t.trim().parse().ok()).collect())
                .unwrap_or_else(|_| D32.to_vec());
            for d in want {
                for x in i32::MIN..=i32::MAX {
                    assert_eq!(sdiv32(x, d), x.wrapping_div(d), "sdiv x={x} d={d}");
                    assert_eq!(srem32(x, d), x.wrapping_rem(d), "srem x={x} d={d}");
                }
            }
        }
    }

    const D64: [i64; 10] = [
        2, 3, 1009, 1_000_000_007, 65536, i64::MIN, i64::MAX, -1009, -7, 1 << 40,
    ];

    #[test]
    fn divmod_const_i64() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as i64
        };
        for d in D64 {
            let mut xs: Vec<i64> = vec![
                0,
                1,
                -1,
                i64::MAX,
                i64::MIN,
                i64::MIN + 1,
                i64::MAX - 1,
                d,
                d - 1,
                d + 1,
                -d,
                -d + 1,
                -d - 1,
                1 << 40,
                -(1 << 40),
            ];
            xs.extend((0..300_000).map(|_| next()));
            for k in 0..4000i64 {
                xs.push(d.saturating_mul(k));
                xs.push(d.saturating_mul(k).wrapping_add(1));
                xs.push(d.saturating_mul(k).wrapping_sub(1));
            }
            for x in xs {
                assert_eq!(sdiv64(x, d), x.wrapping_div(d), "sdiv64 x={x} d={d}");
                assert_eq!(srem64(x, d), x.wrapping_rem(d), "srem64 x={x} d={d}");
            }
        }
    }

    /// 定理前提：魔数必须落在 `[2^(W+l), 2^(W+l) + 2^l]` 里，否则乘魔数不再等价于除法。
    #[test]
    fn magic_precondition_holds() {
        for width in [32u32, 64] {
            let top: u64 = if width == 32 { 1 << 31 } else { 1 << 62 };
            let step = ((top / 8192) as usize).max(1);
            for d in (2u64..=top).step_by(step) {
                let l = 64 - (d - 1).leading_zeros();
                let num = 1u128 << (width + l);
                let m = (num + d as u128 - 1) / d as u128;
                let md = m * d as u128;
                assert!(md >= num, "d={d} width={width} 下界不成立");
                assert!(md - num <= 1u128 << l, "d={d} width={width} 上界不成立");
                // M 落在 [2^W, 2^(W+1)) 里 → M' = M - 2^W 一定装得进一个字
                assert!(m >= 1u128 << width && m < 1u128 << (width + 1), "d={d} M 越界");
                let g = magic(width, d);
                assert_eq!(g.m as u128, m - (1u128 << width), "d={d} 魔数截断");
                assert_eq!(g.s, l);
            }
        }
    }
}
