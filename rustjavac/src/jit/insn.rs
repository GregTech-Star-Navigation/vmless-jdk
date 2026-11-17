//! 指令长度表：线性扫描跳转/调用目标时唯一可用的一张表。

/// 指令长度（含操作码自身）。扫描跳转目标 / 调用目标时用，
/// 任何带操作数的指令漏算都会让线性扫描错位，所以集中在一处。
/// 不支持变长的 wide(0xc4)。
    pub(super) fn insn_len(op: u8) -> usize {
    match op {
        // 1 字节操作数
        0x10 | 0x12 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x36 | 0x37 | 0x38 | 0x39 | 0x3a | 0xa9
        | 0xbc => 2,
        // 2 字节操作数
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2 | 0xb3 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8
        | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        // 3 字节操作数
        0xc5 => 4,
        // invokeinterface / invokedynamic
        0xb9 | 0xba => 5,
        _ => 1,
    }
}

/// 指令长度（变长指令要按操作数算：`tableswitch` / `lookupswitch`）。
///
/// `pc` 指向操作码。线性扫描（跳转目标收集、引用分析、异常扫描）必须用这个版本，
/// 否则 switch 后面的字节会被当成指令解析，扫出来的「目标」全是垃圾。
    pub(super) fn insn_len_at(body: &[u8], pc: usize) -> usize {
    let op = body[pc];
    match op {
        0xaa => {
            // tableswitch：opcode + padding + default + low + high + 表项
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let rd = |i: usize| -> i64 {
                let b = |k: usize| body.get(k).copied().unwrap_or(0) as i64;
                (b(i) << 24) | (b(i + 1) << 16) | (b(i + 2) << 8) | b(i + 3)
            };
            let low = rd(q + 4) as i32;
            let high = rd(q + 8) as i32;
            let n = (high as i64 - low as i64 + 1).max(0) as usize;
            (q + 12 + n * 4) - pc
        }
        0xab => {
            // lookupswitch：opcode + padding + default + npairs + npairs*(4+4)
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let npairs = i32::from_be_bytes([
                body.get(q + 4).copied().unwrap_or(0),
                body.get(q + 5).copied().unwrap_or(0),
                body.get(q + 6).copied().unwrap_or(0),
                body.get(q + 7).copied().unwrap_or(0),
            ])
            .max(0) as usize;
            (q + 8 + npairs * 8) - pc
        }
        _ => insn_len(op),
    }
}
