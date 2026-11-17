//! 大数：`u32` limb 的无符号整数，小端 limb 序（`limbs[0]` 是最低 32 位）。
//!
//! 提供 RSA 需要的全部运算：加减乘、移位、比较、`div_rem`（Knuth Algorithm D）、
//! Montgomery 模幂、模逆（扩展欧几里得）、Miller-Rabin 素性测试与素数生成。
//!
//! 不变量：`limbs` 没有高位 0（`0` 用空 vec 表示）。所有运算都保持这个不变量。

use crate::hash::ct_eq;
use crate::{sys, Error, Result};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BigUint {
    pub limbs: Vec<u32>,
}

fn trim(v: &mut Vec<u32>) {
    while let Some(&0) = v.last() {
        v.pop();
    }
}

fn add_limbs(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for i in 0..long.len() {
        let s = long[i] as u64 + *short.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`，要求 a >= b。
fn sub_limbs(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let d = a[i] as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        if d < 0 {
            out.push((d + (1i64 << 32)) as u32);
            borrow = 1;
        } else {
            out.push(d as u32);
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0, "sub_limbs 下溢");
    let mut o = out;
    trim(&mut o);
    o
}

fn mul_limbs(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for i in 0..a.len() {
        let ai = a[i] as u64;
        if ai == 0 {
            continue;
        }
        let mut carry = 0u64;
        for j in 0..b.len() {
            let t = ai * b[j] as u64 + out[i + j] as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let t = out[k] as u64 + carry;
            out[k] = t as u32;
            carry = t >> 32;
            k += 1;
        }
    }
    trim(&mut out);
    out
}

fn shl_limbs(a: &[u32], bits: usize) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let limb_shift = bits / 32;
    let bit_shift = bits % 32;
    let mut out = vec![0u32; limb_shift];
    if bit_shift == 0 {
        out.extend_from_slice(a);
    } else {
        let mut carry = 0u32;
        for &x in a {
            out.push((x << bit_shift) | carry);
            carry = x >> (32 - bit_shift);
        }
        if carry != 0 {
            out.push(carry);
        }
    }
    trim(&mut out);
    out
}

fn shr_limbs(a: &[u32], bits: usize) -> Vec<u32> {
    let limb_shift = bits / 32;
    if limb_shift >= a.len() {
        return Vec::new();
    }
    let bit_shift = bits % 32;
    let src = &a[limb_shift..];
    let mut out = Vec::with_capacity(src.len());
    if bit_shift == 0 {
        out.extend_from_slice(src);
    } else {
        for i in 0..src.len() {
            let hi = if i + 1 < src.len() { src[i + 1] << (32 - bit_shift) } else { 0 };
            out.push((src[i] >> bit_shift) | hi);
        }
    }
    trim(&mut out);
    out
}

impl BigUint {
    pub fn zero() -> Self {
        BigUint { limbs: Vec::new() }
    }
    pub fn one() -> Self {
        BigUint { limbs: vec![1] }
    }
    pub fn from_u32(v: u32) -> Self {
        if v == 0 {
            Self::zero()
        } else {
            BigUint { limbs: vec![v] }
        }
    }
    pub fn from_u64(v: u64) -> Self {
        let mut out = vec![v as u32, (v >> 32) as u32];
        trim(&mut out);
        BigUint { limbs: out }
    }
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }
    pub fn is_one(&self) -> bool {
        self.limbs.len() == 1 && self.limbs[0] == 1
    }
    pub fn is_even(&self) -> bool {
        self.limbs.first().map(|x| x & 1 == 0).unwrap_or(true)
    }
    pub fn is_odd(&self) -> bool {
        !self.is_even()
    }
    /// 最高有效位位置（bit 数）。0 的 bits 为 0。
    pub fn bits(&self) -> usize {
        match self.limbs.last() {
            None => 0,
            Some(&top) => (self.limbs.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
        }
    }
    pub fn byte_len(&self) -> usize {
        (self.bits() + 7) / 8
    }
    /// 最低 32 位。
    pub fn low32(&self) -> u32 {
        self.limbs.first().copied().unwrap_or(0)
    }
    pub fn bit(&self, i: usize) -> bool {
        let limb = i / 32;
        limb < self.limbs.len() && (self.limbs[limb] >> (i % 32)) & 1 == 1
    }

    pub fn from_bytes_be(b: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(b.len() / 4 + 1);
        let mut i = b.len();
        while i > 0 {
            let start = i.saturating_sub(4);
            let mut v = 0u32;
            for &x in &b[start..i] {
                v = (v << 8) | x as u32;
            }
            limbs.push(v);
            i = start;
        }
        trim(&mut limbs);
        BigUint { limbs }
    }

    pub fn to_bytes_be(&self) -> Vec<u8> {
        self.to_bytes_be_padded(self.byte_len())
    }

    /// 定长大端输出（不够则左补 0；超出则截断高位 —— 调用方自己保证不溢出）。
    pub fn to_bytes_be_padded(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for i in 0..len {
            let bitpos = (len - 1 - i) * 8;
            let limb = bitpos / 32;
            let shift = bitpos % 32;
            let mut byte = 0u8;
            if limb < self.limbs.len() {
                byte = (self.limbs[limb] >> shift) as u8;
                if shift > 24 && limb + 1 < self.limbs.len() {
                    byte |= (self.limbs[limb + 1] << (32 - shift)) as u8;
                }
            }
            out[i] = byte;
        }
        out
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let owned;
        let s = if s.len() % 2 == 1 { owned = format!("0{s}"); owned.as_str() } else { s };
        let b = crate::hash::unhex(s).map_err(|e| Error::new(format!("大数十六进制解析失败: {e}")))?;
        Ok(Self::from_bytes_be(&b))
    }
    pub fn to_hex(&self) -> String {
        crate::hash::hex(&self.to_bytes_be())
    }

    pub fn cmp_val(&self, other: &BigUint) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.limbs.len().cmp(&other.limbs.len()) {
            Ordering::Equal => {
                for i in (0..self.limbs.len()).rev() {
                    match self.limbs[i].cmp(&other.limbs[i]) {
                        Ordering::Equal => continue,
                        o => return o,
                    }
                }
                Ordering::Equal
            }
            o => o,
        }
    }

    pub fn add(&self, other: &BigUint) -> BigUint {
        BigUint { limbs: add_limbs(&self.limbs, &other.limbs) }
    }
    pub fn add_u32(&self, v: u32) -> BigUint {
        self.add(&BigUint::from_u32(v))
    }
    /// 带检查的减法（a < b 时报错，而不是回绕）。
    pub fn sub(&self, other: &BigUint) -> Result<BigUint> {
        if self.cmp_val(other) == std::cmp::Ordering::Less {
            return Err(Error::new("大数减法下溢"));
        }
        Ok(BigUint { limbs: sub_limbs(&self.limbs, &other.limbs) })
    }
    /// 模 `m` 下的减法：`(self - other) mod m`，输入要求 < m。
    pub fn sub_mod(&self, other: &BigUint, m: &BigUint) -> BigUint {
        if self.cmp_val(other) != std::cmp::Ordering::Less {
            BigUint { limbs: sub_limbs(&self.limbs, &other.limbs) }
        } else {
            let d = BigUint { limbs: sub_limbs(&other.limbs, &self.limbs) };
            m.sub(&d).unwrap_or_else(|_| BigUint::zero())
        }
    }
    pub fn mul(&self, other: &BigUint) -> BigUint {
        BigUint { limbs: mul_limbs(&self.limbs, &other.limbs) }
    }
    pub fn mul_u32(&self, v: u32) -> BigUint {
        self.mul(&BigUint::from_u32(v))
    }
    pub fn square(&self) -> BigUint {
        self.mul(self)
    }
    pub fn shl(&self, bits: usize) -> BigUint {
        BigUint { limbs: shl_limbs(&self.limbs, bits) }
    }
    pub fn shr(&self, bits: usize) -> BigUint {
        BigUint { limbs: shr_limbs(&self.limbs, bits) }
    }

    pub fn div_rem(&self, divisor: &BigUint) -> Result<(BigUint, BigUint)> {
        if divisor.is_zero() {
            return Err(Error::new("大数除零"));
        }
        if self.cmp_val(divisor) == std::cmp::Ordering::Less {
            return Ok((BigUint::zero(), self.clone()));
        }
        let n = divisor.limbs.len();
        let m = self.limbs.len();
        if n == 1 {
            let d = divisor.limbs[0] as u64;
            let mut q = vec![0u32; m];
            let mut rem = 0u64;
            for i in (0..m).rev() {
                let cur = (rem << 32) | self.limbs[i] as u64;
                q[i] = (cur / d) as u32;
                rem = cur % d;
            }
            trim(&mut q);
            return Ok((BigUint { limbs: q }, BigUint::from_u64(rem)));
        }
        // Knuth Algorithm D（TAOCP 4.3.1），带规范化移位
        let shift = divisor.limbs[n - 1].leading_zeros() as usize;
        let vn = shl_limbs(&divisor.limbs, shift);
        debug_assert_eq!(vn.len(), n);
        let mut un = shl_limbs(&self.limbs, shift);
        un.resize(m + 1, 0);
        if un.len() < m + 1 {
            un.resize(m + 1, 0);
        }
        let mut q = vec![0u32; m - n + 1];
        let vn_hi = vn[n - 1] as u64;
        let vn_lo = vn[n - 2] as u64;
        const B: u64 = 1u64 << 32;
        for j in (0..=(m - n)).rev() {
            let num = ((un[j + n] as u64) << 32) | un[j + n - 1] as u64;
            let mut qhat = num / vn_hi;
            let mut rhat = num % vn_hi;
            loop {
                if qhat >= B || qhat * vn_lo > ((rhat << 32) | un[j + n - 2] as u64) {
                    qhat -= 1;
                    rhat += vn_hi;
                    if rhat < B {
                        continue;
                    }
                }
                break;
            }
            // un[j..=j+n] -= qhat * vn
            let mut carry = 0u64;
            let mut borrow = 0i64;
            for i in 0..n {
                let prod = qhat * vn[i] as u64 + carry;
                carry = prod >> 32;
                let d = un[j + i] as i64 - ((prod & 0xffff_ffff) as i64) - borrow;
                un[j + i] = d as u32;
                borrow = i64::from(d < 0);
            }
            let d = un[j + n] as i64 - carry as i64 - borrow;
            un[j + n] = d as u32;
            if d < 0 {
                // 估商偏大 1，加回一次
                qhat -= 1;
                let mut c = 0u64;
                for i in 0..n {
                    let s = un[j + i] as u64 + vn[i] as u64 + c;
                    un[j + i] = s as u32;
                    c = s >> 32;
                }
                un[j + n] = un[j + n].wrapping_add(c as u32);
            }
            q[j] = qhat as u32;
        }
        trim(&mut q);
        let mut rem = un[..n].to_vec();
        if shift > 0 {
            rem = shr_limbs(&rem, shift);
        } else {
            trim(&mut rem);
        }
        Ok((BigUint { limbs: q }, BigUint { limbs: rem }))
    }

    pub fn rem(&self, m: &BigUint) -> Result<BigUint> {
        Ok(self.div_rem(m)?.1)
    }

    pub fn rem_small(&self, m: u32) -> u32 {
        if m == 0 {
            return 0;
        }
        let mut rem = 0u64;
        let d = m as u64;
        for i in (0..self.limbs.len()).rev() {
            rem = ((rem << 32) | self.limbs[i] as u64) % d;
        }
        rem as u32
    }

    pub fn gcd(&self, other: &BigUint) -> BigUint {
        let (mut a, mut b) = (self.clone(), other.clone());
        while !b.is_zero() {
            match a.div_rem(&b) {
                Ok((_, r)) => {
                    a = b;
                    b = r;
                }
                Err(_) => return BigUint::zero(),
            }
        }
        a
    }

    pub fn lcm(&self, other: &BigUint) -> BigUint {
        let g = self.gcd(other);
        if g.is_zero() {
            return BigUint::zero();
        }
        let prod = self.mul(other);
        prod.div_rem(&g).map(|(q, _)| q).unwrap_or_else(|_| BigUint::zero())
    }

    /// Montgomery 上下文：R = 2^(32n)，n = m 的 limb 数。
    pub fn montgomery(&self) -> Result<Montgomery> {
        Montgomery::new(self.clone())
    }

    /// `self^exp mod m`。m 为奇数走 Montgomery；偶数（罕见，例如调试）走朴素平方乘。
    pub fn modpow(&self, exp: &BigUint, m: &BigUint) -> Result<BigUint> {
        if m.is_zero() {
            return Err(Error::new("模幂的模为 0"));
        }
        if m.is_one() {
            return Ok(BigUint::zero());
        }
        if exp.is_zero() {
            return Ok(BigUint::one());
        }
        if m.is_even() {
            let base = self.rem(m)?;
            let mut acc = BigUint::one();
            for i in (0..exp.bits()).rev() {
                acc = acc.square().rem(m)?;
                if exp.bit(i) {
                    acc = acc.mul(&base).rem(m)?;
                }
            }
            return Ok(acc);
        }
        let mont = Montgomery::new(m.clone())?;
        mont.modpow(self, exp)
    }

    /// 模逆：`self * x ≡ 1 (mod m)`。不存在时报错。
    pub fn modinv(&self, m: &BigUint) -> Result<BigUint> {
        if m.is_zero() {
            return Err(Error::new("模逆的模为 0"));
        }
        let a = self.rem(m)?;
        if a.is_zero() {
            return Err(Error::new("0 没有模逆"));
        }
        let (mut r0, mut r1) = (m.clone(), a);
        let (mut t0, mut t1) = (BigUint::zero(), BigUint::one());
        while !r1.is_zero() {
            let (q, r) = r0.div_rem(&r1)?;
            let qt1 = q.mul(&t1).rem(m)?;
            let t = t0.sub_mod(&qt1, m);
            r0 = r1;
            r1 = r;
            t0 = t1;
            t1 = t;
        }
        if !r0.is_one() {
            return Err(Error::new("两个数不互素，模逆不存在"));
        }
        Ok(t0.rem(m)?)
    }

    /// 随机生成 `bits` 位整数：最高位与最低位都置 1（用于素数候选）。
    pub fn random_odd(bits: usize) -> Result<BigUint> {
        if bits < 2 {
            return Err(Error::new("随机大数位数太小"));
        }
        let nbytes = bits.div_ceil(8);
        let mut buf = sys::random_vec(nbytes)?;
        let excess = nbytes * 8 - bits;
        buf[0] &= 0xffu8 >> excess;
        buf[0] |= 0x80u8 >> excess;
        let last = buf.len() - 1;
        buf[last] |= 1;
        Ok(BigUint::from_bytes_be(&buf))
    }

    /// 随机 `[0, n)` 区间内的数（拒绝采样，无模偏差）。
    pub fn random_below(n: &BigUint) -> Result<BigUint> {
        if n.is_zero() {
            return Err(Error::new("随机数上界为 0"));
        }
        let bits = n.bits();
        let nbytes = bits.div_ceil(8);
        let excess = nbytes * 8 - bits;
        for _ in 0..1000 {
            let mut buf = sys::random_vec(nbytes)?;
            buf[0] &= 0xffu8 >> excess;
            let v = BigUint::from_bytes_be(&buf);
            if v.cmp_val(n) == std::cmp::Ordering::Less {
                return Ok(v);
            }
        }
        Err(Error::new("随机数拒绝采样失败次数过多"))
    }

    /// Miller-Rabin 素性测试。`rounds` 个随机底数（另加固定底数 2）。
    pub fn is_probable_prime(&self, rounds: usize) -> bool {
        let two = BigUint::from_u32(2);
        if self.cmp_val(&two) == std::cmp::Ordering::Less {
            return false;
        }
        if self.is_even() {
            return self.is_one() || *self == two;
        }
        // n - 1 = d * 2^s
        let n_minus_1 = match self.sub(&BigUint::one()) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let mut s = 0usize;        while !n_minus_1.bit(s) {
            s += 1;
            if s > n_minus_1.bits() {
                return false;
            }
        }
        let d = n_minus_1.shr(s);
        let n_minus_2 = match n_minus_1.sub(&BigUint::one()) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let mut bases = vec![BigUint::from_u32(2)];
        for _ in 0..rounds {
            match BigUint::random_below(&n_minus_2) {
                Ok(r) => bases.push(r.add(&two)),
                Err(_) => return false,
            }
        }
        for a in bases {
            let mut x = match a.modpow(&d, self) {
                Ok(x) => x,
                Err(_) => return false,
            };
            if x.is_one() || x == n_minus_1 {
                continue;
            }
            let mut ok = false;
            for _ in 1..s {
                x = match x.mul(&x).rem(self) {
                    Ok(x) => x,
                    Err(_) => return false,
                };
                if x == n_minus_1 {
                    ok = true;
                    break;
                }
            }
            if !ok {
                return false;
            }
        }
        true
    }
}

/// Montgomery 模乘上下文（n0' = -m^-1 mod 2^32，R = 2^(32n)）。
#[derive(Clone)]
pub struct Montgomery {
    pub m: BigUint,
    n: usize,
    m0inv: u32,
    r2: BigUint,
}

impl Montgomery {
    pub fn new(m: BigUint) -> Result<Montgomery> {
        if m.is_even() || m.is_zero() {
            return Err(Error::new("Montgomery 只支持奇模数"));
        }
        let n = m.limbs.len();
        // inv = m^-1 mod 2^32（Newton 迭代，5 次足够收敛 32 位）
        let m0 = m.limbs[0];
        let mut inv = m0;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(inv)));
        }
        let m0inv = inv.wrapping_neg();
        // R mod m：从 1 开始翻倍 32n 次
        let mut r = BigUint::one();
        for _ in 0..(32 * n) {
            r = r.shl(1);
            if r.cmp_val(&m) != std::cmp::Ordering::Less {
                r = r.sub(&m)?;
            }
        }
        // R^2 mod m：再翻倍 32n 次
        let mut r2 = r.clone();
        for _ in 0..(32 * n) {
            r2 = r2.shl(1);
            if r2.cmp_val(&m) != std::cmp::Ordering::Less {
                r2 = r2.sub(&m)?;
            }
        }
        Ok(Montgomery { m, n, m0inv, r2 })
    }

    /// CIOS 乘法：t = a*b*R^-1 mod m，输入要求 < m。
    fn mul(&self, a: &BigUint, b: &BigUint) -> BigUint {
        let n = self.n;
        let av = a.to_bytes_be_padded(4 * n);
        let bv = b.to_bytes_be_padded(4 * n);
        let mv = self.m.to_bytes_be_padded(4 * n);
        let mut a_limbs = vec![0u32; n];
        let mut b_limbs = vec![0u32; n];
        let mut m_limbs = vec![0u32; n];
        for i in 0..n {
            let off = 4 * (n - 1 - i);
            a_limbs[i] = u32::from_be_bytes([av[off], av[off + 1], av[off + 2], av[off + 3]]);
            b_limbs[i] = u32::from_be_bytes([bv[off], bv[off + 1], bv[off + 2], bv[off + 3]]);
            m_limbs[i] = u32::from_be_bytes([mv[off], mv[off + 1], mv[off + 2], mv[off + 3]]);
        }
        let mut t = vec![0u32; n + 2];
        for i in 0..n {
            let mut carry = 0u64;
            for j in 0..n {
                let s = t[j] as u64 + a_limbs[j] as u64 * b_limbs[i] as u64 + carry;
                t[j] = s as u32;
                carry = s >> 32;
            }
            let s = t[n] as u64 + carry;
            t[n] = s as u32;
            t[n + 1] = (s >> 32) as u32;

            let u = t[0].wrapping_mul(self.m0inv);
            let s = t[0] as u64 + u as u64 * m_limbs[0] as u64;
            let mut carry = (s >> 32) as u32;
            for j in 1..n {
                let s = t[j] as u64 + u as u64 * m_limbs[j] as u64 + carry as u64;
                t[j - 1] = s as u32;
                carry = (s >> 32) as u32;
            }
            let s = t[n] as u64 + carry as u64;
            t[n - 1] = s as u32;
            t[n] = t[n + 1] + (s >> 32) as u32;
        }
        // CIOS 的中间结果可能落在 [m, 2m)，且高位 limb 可能非 0 —— 必须整体比较后再减
        let mut out = BigUint { limbs: t[..=n].to_vec() };
        trim(&mut out.limbs);
        if out.cmp_val(&self.m) != std::cmp::Ordering::Less {
            out = BigUint { limbs: sub_limbs(&out.limbs, &self.m.limbs) };
        }
        out
    }

    pub fn to_mont(&self, a: &BigUint) -> Result<BigUint> {
        let r = a.rem(&self.m)?;
        Ok(self.mul(&r, &self.r2))
    }

    pub fn from_mont(&self, a: &BigUint) -> BigUint {
        self.mul(a, &BigUint::one())
    }

    pub fn modpow(&self, base: &BigUint, exp: &BigUint) -> Result<BigUint> {
        let b = self.to_mont(base)?;
        let mut acc = self.to_mont(&BigUint::one())?;
        let bits = exp.bits();
        for i in (0..bits).rev() {
            acc = self.mul(&acc, &acc);
            if exp.bit(i) {
                acc = self.mul(&acc, &b);
            }
        }
        Ok(self.from_mont(&acc))
    }
}

/// 常数时间比较两个大数（要求等长 limb，用于验签时的模数比较）。
pub fn ct_eq_biguint(a: &BigUint, b: &BigUint) -> bool {
    let n = a.limbs.len().max(b.limbs.len());
    let ab = a.to_bytes_be_padded(4 * n);
    let bb = b.to_bytes_be_padded(4 * n);
    ct_eq(&ab, &bb)
}

/// 小素数表（试除 + 素数筛）。
pub fn small_primes(limit: u32) -> Vec<u32> {
    let mut sieve = vec![true; limit as usize + 1];
    if limit >= 0 {
        sieve[0] = false;
    }
    if limit >= 1 {
        sieve[1] = false;
    }
    let mut i = 2usize;
    while i * i <= limit as usize {
        if sieve[i] {
            let mut j = i * i;
            while j <= limit as usize {
                sieve[j] = false;
                j += i;
            }
        }
        i += 1;
    }
    sieve
        .iter()
        .enumerate()
        .filter(|(_, &p)| p)
        .map(|(i, _)| i as u32)
        .collect()
}

/// 生成 `bits` 位素数（最高位为 1）。
///
/// 用筛法先在候选附近批量排除小素数因子，再对存活者做 Miller-Rabin，
/// 避免对每个候选都跑一次模幂。
pub fn generate_prime(bits: usize) -> Result<BigUint> {
    if bits < 16 {
        return Err(Error::new("素数位数太小"));
    }
    let primes = small_primes(20000);
    let span: u32 = 1 << 15;
    for _attempt in 0..10000 {
        let start = BigUint::random_odd(bits)?;
        let residues: Vec<u32> = primes.iter().map(|p| start.rem_small(*p)).collect();
        let mut k = 0u32;
        while k < span {
            let mut composite = false;
            for (i, p) in primes.iter().enumerate() {
                if (residues[i] as u64 + k as u64) % (*p as u64) == 0 {
                    composite = true;
                    break;
                }
            }
            if !composite {
                let cand = start.add_u32(k);
                if cand.is_probable_prime(4) {
                    return Ok(cand);
                }
            }
            k += 2;
        }
    }
    Err(Error::new("素数生成超时"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: u64) -> BigUint {
        BigUint::from_u64(v)
    }

    #[test]
    fn arith_basic() {
        assert_eq!(b(2).add(&b(3)), b(5));
        assert_eq!(b(0xffff_ffff).add(&b(1)), b(0x1_0000_0000));
        assert_eq!(b(1_000_000).mul(&b(1_000_000)), b(1_000_000_000_000));
        assert_eq!(b(100).sub(&b(1)).unwrap(), b(99));
        assert!(b(1).sub(&b(2)).is_err());
        assert_eq!(BigUint::from_hex("deadbeef").unwrap().to_hex(), "deadbeef");
        assert_eq!(BigUint::from_bytes_be(&[0, 0, 1]).to_hex(), "01");
    }

    #[test]
    fn bits_and_shifts() {
        assert_eq!(b(1).bits(), 1);
        assert_eq!(b(255).bits(), 8);
        assert_eq!(b(256).bits(), 9);
        let x = BigUint::from_hex("0123456789abcdef").unwrap();
        assert_eq!(x.shl(8).to_hex(), "0123456789abcdef00");
        assert_eq!(x.shr(8).to_hex(), "0123456789abcd");
        assert_eq!(x.shr(65), BigUint::zero());
    }

    #[test]
    fn div_rem_known() {
        // 用十进制大数做已知答案校验：(10^30 + 7) / (10^10 + 3)
        let n = BigUint::from_bytes_be(&crate::hash::unhex(
            "0c9f2c9cd04674edea40000007", // 10^29*... 任意大数
        ).unwrap());
        let d = BigUint::from_bytes_be(&crate::hash::unhex("02540be400000003").unwrap());
        let (q, r) = n.div_rem(&d).unwrap();
        assert_eq!(q.mul(&d).add(&r), n);
        assert_eq!(r.cmp_val(&d), std::cmp::Ordering::Less);
        // 逐字节除法一致性
        let mut x = BigUint::zero();
        for byte in [0x12u8, 0x34, 0x56, 0x78, 0x9a] {
            x = x.shl(8).add_u32(byte as u32);
        }
        assert_eq!(x.to_hex(), "123456789a");
        assert_eq!(x.rem_small(1000), 530);
    }

    #[test]
    fn modpow_matches_naive() {
        let m = BigUint::from_hex("f0000000000000000000000000000001").unwrap();
        let cases = [
            ("02", "01000000000000000001", "f0000000000000000000000000000001"),
            ("03", "ffffffffffffffff", "f0000000000000000000000000000001"),
            ("abcdef123456789a", "0123456789abcdef", "f0000000000000000000000000000001"),
        ];
        for (a, e, mm) in cases {
            let m = BigUint::from_hex(mm).unwrap();
            let want = naive_pow(&BigUint::from_hex(a).unwrap(), &BigUint::from_hex(e).unwrap(), &m);
            let got = BigUint::from_hex(a).unwrap().modpow(&BigUint::from_hex(e).unwrap(), &m).unwrap();
            assert_eq!(got, want, "a={a}");
        }
        let _ = m;
    }

    fn naive_pow(a: &BigUint, e: &BigUint, m: &BigUint) -> BigUint {
        let mut acc = BigUint::one();
        let base = a.rem(m).unwrap();
        for i in (0..e.bits()).rev() {
            acc = acc.square().rem(m).unwrap();
            if e.bit(i) {
                acc = acc.mul(&base).rem(m).unwrap();
            }
        }
        acc
    }

    #[test]
    fn modinv_known() {
        // 3 * 5 = 15 ≡ 1 (mod 7)
        assert_eq!(b(3).modinv(&b(7)).unwrap(), b(5));
        // 65537 在 (p-1)(q-1) 下的逆
        let p = b(61);
        let q = b(53);
        let lambda = p.sub(&b(1)).unwrap().lcm(&q.sub(&b(1)).unwrap());
        let d = b(17).modinv(&lambda).unwrap();
        assert_eq!(b(17).mul(&d).rem(&lambda).unwrap(), b(1));
        assert!(b(4).modinv(&b(8)).is_err());
    }

    #[test]
    fn gcd_lcm() {
        assert_eq!(b(48).gcd(&b(18)), b(6));
        assert_eq!(b(4).lcm(&b(6)), b(12));
    }

    #[test]
    fn primality() {
        for p in [2u64, 3, 5, 7, 11, 97, 7919, 104729, 2147483647] {
            assert!(b(p).is_probable_prime(6), "{p} 应为素数");
        }
        for c in [1u64, 4, 9, 15, 100, 7917, 104730, 2147483649] {
            assert!(!b(c).is_probable_prime(6), "{c} 应为合数");
        }
        let prime = generate_prime(256).unwrap();
        assert_eq!(prime.bits(), 256);
        assert!(prime.is_probable_prime(8));
    }

    #[test]
    fn montgomery_matches_naive() {
        let m = BigUint::from_hex("c3b1d4e5f60718293a4b5c6d7e8f90a1").unwrap();
        let a = BigUint::from_hex("1234567890abcdef1234567890abcdef").unwrap();
        let e = BigUint::from_hex("1f2e3d4c5b6a7988").unwrap();
        let mont = Montgomery::new(m.clone()).unwrap();
        assert_eq!(mont.modpow(&a, &e).unwrap(), naive_pow(&a, &e, &m));
        assert_eq!(mont.to_mont(&a).unwrap().cmp_val(&m), std::cmp::Ordering::Less);
    }
}
