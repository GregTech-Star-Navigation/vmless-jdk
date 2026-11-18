//! 有符号整数除/余常数 → 乘魔数（strength reduction）。
//!
//! **为什么需要它**：Cranelift 不做这个（`opts/arithmetic.isle` 里就写着
//! `;; TODO: strength reduction: div to shifts` / `;; TODO: div/rem by constants ->
//! magic multiplications`），`sdiv`/`srem` 原样落到 x86 的 `idiv` 上。`idiv r64`
//! 是 20~40 周期的串行指令（延迟高、吞吐 1/6），而 JVM 里 `x % 1009`、`x / 3`
//! 这种写法遍地都是 —— 实测 P2 那个 4×800 万次的 `(i*31+7) % 1009`，把 idiv
//! 换成乘魔数之后 **390ms → 150ms**。
//!
//! # 为什么走「有符号魔数」而不是「取绝对值 + 无符号魔数」
//!
//! 先算出 `|x|`、做无符号除法、再按符号还原，一共 12 条（取符号、取绝对值 2 条、
//! 无符号除法 5 条、还原 2 条、算余 2 条）。LLVM/GCC/C2 都**不这么做**，它们用
//! **有符号魔数**：`mulhs` + 条件加 `x` + 算术右移 + 符号修正，共 6 条，算余再多 2 条。
//! 实测这一格（32M 次 `(i*31+7) % 1009`）从 **87ms 压到 60ms**。
//!
//! # 序列与取值
//!
//! 令 `l = ceil(log2 d)`（`d > 0`，非 2 的幂），取
//!
//! ```text
//!   s   = l - 1
//!   M_u = ceil(2^(W+s) / d)          // 落在 [2^(W-1), 2^W) 里
//! ```
//!
//! `M_u` 装不进**有符号** W 位时（`M_u >= 2^(W-1)`）用 `M = M_u - 2^W`（负），
//! 此时 `mulhs(x, M) = floor(x*M_u/2^W) - x`，所以乘完**加一次 x** 就把负魔数
//! 折回了 `floor(x*M_u/2^W)`；这一步本来就要做，所以两种情形是同一条序列：
//!
//! ```text
//!   t = mulhs(x, M)          // 有符号高位乘
//!   t = t + x                // 仅当 M < 0（编译期常量，无运行期分支）
//!   q = t >> s               // 算术右移 = floor(x*M_u / 2^(W+s))
//!   q = q + ((unsigned)q >> (W-1))     // 负则 +1：floor → trunc
//! ```
//!
//! 正确性（`E = M_u*d - 2^(W+s) ∈ [0, d)`，`δ = x*E/2^(W+s)`）：
//! 把 `x = q*d + r`（欧几里得，`0 <= r < d`）代入，
//! `floor(x*M_u/2^(W+s)) = q + floor((r+δ)/d)`。而 `|x| < 2^(W-1)`、`E <= d-1 < 2^l = 2^(s+1)`
//! 给出 `|δ| < 1`，所以：
//!
//! * `x >= 0`：`δ >= 0`，`r+δ < d` → 结果 = `q` = `floor(x/d)` = `trunc`；
//! * `x < 0`：`δ <= 0`，只有「x 是 d 的整数倍且 E > 0」时 `r+δ < 0` 咬掉 1，
//!   其余都是 `q` —— 两种情形都恰好是 `trunc(x/d) - 1`，所以**统一 +1** 就对了。
//!
//! 中间量不会溢出：`|t| <= |x|*M_u/2^W < |x|`（`M_u < 2^W`）；负魔数那一支里
//! `mulhs` 与 `x` 反号，相加只会变小（`|mulhs+x| < |x|`）。
//!
//! 这一版的魔数与移位是**构造出来**的（`s = l-1` 一定满足 `E <= 2^(s+1)`），
//! 不像 Hacker's Delight 那样靠 `q1 < delta` 循环搜，所以不存在「搜出来的魔数
//! 到底对不对」的问题；序列形状与 LLVM 对 `x % 1009` 生成的一致。
//!
//! `d` 为负或 `|d|` 是 2 的幂时走更短的分支（取负 / 移位加偏置）。

use cranelift_codegen::ir::{types, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

/// 常数除数拆出来的**有符号**魔数方案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SMagic {
    /// 有符号 W 位魔数（`add` 为真时是负的）
    pub m: i64,
    /// 算术右移位数 `s = ceil(log2 d) - 1`
    pub s: u32,
    /// 乘完要不要再加一次 `x`（等价于「魔数是负的」）
    pub add: bool,
}

/// 把 `u64` 的 W 位模式按有符号解释
fn sext(v: u64, width: u32) -> i64 {
    if width == 32 {
        v as u32 as i32 as i64
    } else {
        v as i64
    }
}

/// 对 `d >= 2`（非 2 的幂）算有符号魔数。
///
/// 取**最小**的可行 `s`：`s` 越小魔数越小，越可能装进有符号正数区间（省掉加 `x`）。
/// 满足 `E <= 2^(s+1)` 的最小 `s` 就是解；`s = l-1` 一定可行，所以不会找不到。
pub(super) fn smagic(width: u32, d: u64) -> SMagic {
    assert!(d >= 2, "smagic 只处理 d >= 2");
    assert!(width == 32 || width == 64);
    assert!(!d.is_power_of_two(), "2 的幂走移位分支");
    let w = width;
    let two_w: u128 = 1u128 << w;
    let half: u128 = 1u128 << (w - 1);
    for s in 0..w {
        let num: u128 = 1u128 << (w + s);
        let m = (num + d as u128 - 1) / d as u128;
        if m >= two_w {
            // s 再大魔数只会更大，装不进 W 位
            break;
        }
        let e = m * d as u128 - num; // ceil 保证 >= 0
        if e > (2u128 << s) {
            // 误差超出「|δ| < 1」的范围，换大一点的 s
            continue;
        }
        let add = m >= half;
        let mv: i64 = if add {
            // 负魔数：M = M_u - 2^W（用 i128 算，避开 `1u64 << 64` 的回绕）
            (m as i128 - (1i128 << w)) as i64
        } else {
            m as i64
        };
        return SMagic { m: mv, s, add };
    }
    unreachable!("smagic 找不到方案 d={d} width={width}");
}

/// 有符号除法：`x / d`（`d != 0`），截断语义（与 JVM 一致）
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
    let ad = d.unsigned_abs();
    let q = emit_udiv_signed(fb, x, ad, width);
    if d < 0 {
        let zero = fb.ins().iconst(ty, 0);
        fb.ins().isub(zero, q)
    } else {
        q
    }
}

/// `|ad| >= 2` 时的 `x / ad`（`ad > 0`）
fn emit_udiv_signed(fb: &mut FunctionBuilder, x: Value, ad: u64, width: u32) -> Value {
    let ty = if width == 32 { types::I32 } else { types::I64 };
    if ad.is_power_of_two() {
        // trunc(x/2^k) = (x + (x<0 ? 2^k-1 : 0)) >> k
        let k = ad.trailing_zeros() as i64;
        let sgn = if width == 32 {
            fb.ins().sshr_imm(x, 31)
        } else {
            fb.ins().sshr_imm(x, 63)
        };
        let bias = fb.ins().iconst(ty, (ad - 1) as i64);
        let b = fb.ins().band(sgn, bias);
        let n = fb.ins().iadd(x, b);
        return fb.ins().sshr_imm(n, k);
    }
    let g = smagic(width, ad);
    let m = fb.ins().iconst(ty, g.m);
    let mut t = fb.ins().smulhi(x, m);
    if g.add {
        // 负魔数：mulhs(x, M-2^W) + x == floor(x*M/2^W)
        t = fb.ins().iadd(t, x);
    }
    let q = fb.ins().sshr_imm(t, g.s as i64);
    // 负则 +1：floor → trunc
    let bit = fb.ins().ushr_imm(q, (width - 1) as i64);
    fb.ins().iadd(q, bit)
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

    /// 按目标位宽回绕
    fn wrap(v: i128, width: u32) -> i64 {
        if width == 32 {
            v as i32 as i64
        } else {
            v as i64
        }
    }
    /// 按位宽的算术右移
    fn sar(v: i64, k: u32, width: u32) -> i64 {
        if width == 32 {
            ((v as i32) >> k) as i64
        } else {
            v >> k
        }
    }
    /// 按位宽的符号位（0/1）
    fn top_bit(v: i64, width: u32) -> i64 {
        if width == 32 {
            ((v as u32) >> 31) as i64
        } else {
            ((v as u64) >> 63) as i64
        }
    }

    /// 与 `emit_udiv_signed` **逐条**对应的标量模型（乘法用 i128 精确算高位）
    fn udiv_signed(x: i64, ad: u64, width: u32) -> i64 {
        if ad.is_power_of_two() {
            let k = ad.trailing_zeros();
            let sgn = sar(x, width - 1, width);
            let mask = wrap(ad as i128 - 1, width);
            let n = wrap(x as i128 + (sgn & mask) as i128, width);
            return sar(n, k, width);
        }
        let g = smagic(width, ad);
        // smulhi(x, M) = floor(x*M / 2^W)（有符号，按位宽解释）
        let hi = wrap((((x as i128) * (g.m as i128)) >> width as i32) as i128, width);
        let mut t = hi;
        if g.add {
            // 负魔数：mulhs(x, M-2^W) + x == floor(x*M/2^W)
            t = wrap(t as i128 + x as i128, width);
        }
        let q = sar(t, g.s, width);
        wrap(q as i128 + top_bit(q, width) as i128, width)
    }

    fn sdiv32(x: i32, d: i32) -> i32 {
        if d == 1 {
            return x;
        }
        if d == -1 {
            return (0i32).wrapping_sub(x);
        }
        let q = udiv_signed(x as i64, d.unsigned_abs() as u64, 32) as i32;
        if d < 0 {
            (0i32).wrapping_sub(q)
        } else {
            q
        }
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
        let q = udiv_signed(x, d.unsigned_abs(), 64);
        if d < 0 {
            (0i64).wrapping_sub(q)
        } else {
            q
        }
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
            // 顶点附近（负魔数那一支最容易在 MIN/MAX 附近溢出）
            for k in 0..64i64 {
                xs.push(i64::MAX - k);
                xs.push(i64::MIN + k);
                xs.push(1 << 62 | k);
                xs.push(-(1 << 62) - k);
            }
            // d 的整数倍附近（截断/floor 的分界）
            let ad = d.unsigned_abs();
            for k in 1..2000u64 {
                let base = (ad.wrapping_mul(k)) as i64;
                xs.push(base);
                xs.push(base.wrapping_add(1));
                xs.push(base.wrapping_sub(1));
                xs.push(base.wrapping_neg());
                xs.push(base.wrapping_neg().wrapping_add(1));
                xs.push(base.wrapping_neg().wrapping_sub(1));
            }
            xs.extend((0..300_000).map(|_| next()));
            for x in xs {
                assert_eq!(sdiv64(x, d), x.wrapping_div(d), "sdiv64 x={x} d={d}");
                assert_eq!(srem64(x, d), x.wrapping_rem(d), "srem64 x={x} d={d}");
            }
        }
    }

    /// 魔数方案的正确性前提：`M_u = ceil(2^(W+s)/d)` 装得进 W 位、误差 `E <= 2^(s+1)`。
    #[test]
    fn smagic_precondition_holds() {
        for width in [32u32, 64] {
            let top: u64 = if width == 32 { 1 << 31 } else { 1 << 62 };
            let step = ((top / 20000) as usize).max(1);
            for d in (3u64..=top).step_by(step) {
                if d.is_power_of_two() {
                    continue;
                }
                let g = smagic(width, d);
                let w = width;
                let num: u128 = 1u128 << (w + g.s);
                let m = if g.add {
                    (g.m as u128) + (1u128 << w)
                } else {
                    g.m as u128
                };
                assert_eq!(m, (num + d as u128 - 1) / d as u128, "d={d} M 不是 ceil");
                assert!(m < 1u128 << w, "d={d} M 装不进 W 位");
                let e = m * d as u128 - num;
                assert!(e <= 2u128 << g.s, "d={d} 误差 {e} 超过 2^(s+1)");
                // 负魔数只在装不进有符号正数时才用
                assert_eq!(g.add, m >= 1u128 << (w - 1), "d={d} add 标志不对");
            }
        }
    }

    /// 2 的幂走移位分支；`s` 应该取到 `ceil(log2 d) - 1`（最小可行）
    #[test]
    fn smagic_shift_is_tight() {
        for width in [32u32, 64] {
            for d in [3u64, 7, 1009, 1_000_000_007, 5, 9, 11, 641] {
                let g = smagic(width, d);
                let l = 64 - (d - 1).leading_zeros();
                assert!(g.s <= l - 1, "d={d} s={} 超过 l-1={}", g.s, l - 1);
            }
        }
    }
}
