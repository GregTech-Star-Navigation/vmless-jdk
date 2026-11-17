
// ============================================================ 调用 native（VM → C）

/// 一次 C 调用的参数帧。`gpr` = 整数/指针参数（rcx,rdx,r8,r9），`xmm` = 浮点参数，
/// `stack` = 第 5 个及以后的参数（**Win64 的约定：浮点参数超过 4 个也压栈**）。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[repr(C)]
pub(crate) struct CallFrame {
    pub(crate) fp: usize,
    pub(crate) n: usize,
    pub(crate) gpr: [u64; 4],
    pub(crate) xmm: [u64; 4],
    pub(crate) stack: [u64; 32],
    pub(crate) out: [u64; 2],
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[unsafe(naked)]
unsafe extern "C" fn call_stub() {
    core::arch::naked_asm!(
        "sub rsp, 0x148",
        "mov qword ptr [rsp + 0x120], rcx",
        // 浮点参数：xmm0..3
        "movq xmm0, qword ptr [rcx + 0x30]",
        "movq xmm1, qword ptr [rcx + 0x38]",
        "movq xmm2, qword ptr [rcx + 0x40]",
        "movq xmm3, qword ptr [rcx + 0x48]",
        // 第 5 个起压栈
        "mov r10, qword ptr [rcx + 0x08]",
        "sub r10, 4",
        "jle 1f",
        "xor r11d, r11d",
        "0:",
        "mov rax, qword ptr [rcx + r11*8 + 0x50]",
        "mov qword ptr [rsp + r11*8 + 0x20], rax",
        "inc r11",
        "cmp r11, r10",
        "jl 0b",
        "1:",
        // 整数参数：rcx,rdx,r8,r9（先存 fp，再把 gpr0 放回 rcx）
        "mov rax, qword ptr [rcx]",
        "mov qword ptr [rsp + 0x128], rax",
        "mov rax, qword ptr [rcx + 0x10]",
        "mov rdx, qword ptr [rcx + 0x18]",
        "mov r8,  qword ptr [rcx + 0x20]",
        "mov r9,  qword ptr [rcx + 0x28]",
        "mov rcx, rax",
        "call qword ptr [rsp + 0x128]",
        // 返回值：整数在 rax，浮点在 xmm0
        "mov r11, qword ptr [rsp + 0x120]",
        "mov qword ptr [r11 + 0x150], rax",
        "movq qword ptr [r11 + 0x158], xmm0",
        "add rsp, 0x148",
        "ret",
    )
}

/// 调一个 native 函数指针。`kinds[i]`：0 = 整数/指针，1 = f32，2 = f64。
///
/// 返回 `(整数返回值, 浮点返回值位模式)` —— 调用方按描述符取用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub(crate) unsafe fn call_native_ffi(fp: usize, args: &[u64], kinds: &[u8]) -> (u64, u64) {
    assert!(args.len() <= 36, "native 方法参数超过 36 个（不会发生）");
    let mut f = CallFrame {
        fp,
        n: args.len(),
        gpr: [0; 4],
        xmm: [0; 4],
        stack: [0; 32],
        out: [0; 2],
    };
    for (i, v) in args.iter().enumerate() {
        let k = kinds.get(i).copied().unwrap_or(0);
        let bits = if k == 1 { *v & 0xFFFF_FFFF } else { *v };
        if i < 4 {
            if k == 0 {
                f.gpr[i] = bits;
            } else {
                f.xmm[i] = bits;
            }
        } else {
            f.stack[i - 4] = bits;
        }
    }
    call_stub(&mut f as *mut CallFrame as *mut c_void);
    (f.out[0], f.out[1])
}

/// 其它架构：只能传整数/指针参数（浮点参数的寄存器规则各架构不同，这里不猜）。
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
pub(crate) unsafe fn call_native_ffi(fp: usize, args: &[u64], kinds: &[u8]) -> (u64, u64) {
    if kinds.iter().any(|k| *k != 0) {
        eprintln!("[jni] 这个架构上不支持 native 方法的浮点参数（会传错，已拒绝）");
        return (0, 0);
    }
    let mut a = [0u64; 12];
    for (i, v) in args.iter().take(12).enumerate() {
        a[i] = *v;
    }
    type F = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    let f: F = std::mem::transmute(fp);
    (f(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11]), 0)
}
