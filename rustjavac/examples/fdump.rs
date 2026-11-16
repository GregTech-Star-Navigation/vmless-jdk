// 临时工具：把一批 f32 位型的 Java 文本形态打出来（与 java 的 Float.toString 对比用）
fn main() {
    let mut bits: Vec<u32> = (1..=512).collect();
    let mut i: u64 = 513;
    while i < 0x7f80_0000u64 {
        bits.push(i as u32);
        i += 3571;
    }
    for e in [0u32, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000,
              0x3f80_0000, 0x3f00_0000, 0x0080_0000, 0x0100_0000, 0x7f7f_ffff,
              0x4b80_0000, 0x4b80_0001, 0x4b7f_ffff, 0x3eaa_aaab] {
        bits.push(e);
    }
    let mut out = String::new();
    for b in bits {
        out.push_str(&format!("{} {}\n", b, classfile::java_float_to_string(f32::from_bits(b))));
    }
    print!("{out}");
}
