// 候选实现：最小位数 + 正确舍入（ties-to-even），比对 Java
fn cand(v: f32) -> String {
    if v.is_nan() { return "NaN".into(); }
    if v.is_infinite() { return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }; }
    if v == 0.0 { return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() }; }
    let a = v.abs();
    let mut k = 1usize;
    let (mut mant, mut exp) = (String::new(), 0i32);
    while k <= 9 {
        let s = format!("{:.*e}", k - 1, a); // 形如 "3.40e38"
        if s.parse::<f32>().map(|x| x == a).unwrap_or(false) {
            let (m, e) = s.split_once('e').unwrap();
            mant = m.to_string();
            exp = e.parse().unwrap();
            break;
        }
        k += 1;
    }
    if mant.is_empty() { return format!("{:e}", a); }
    let digits: Vec<char> = mant.chars().filter(|c| *c != '.').collect();
    // 去掉尾随 0（"1.40e-45" → digits "14"）
    let mut d = digits.clone();
    while d.len() > 1 && *d.last().unwrap() == '0' { d.pop(); }
    let mut out = String::new();
    if v < 0.0 { out.push('-'); }
    if (-3..7).contains(&exp) {
        let il = exp + 1;
        if il > 0 { for i in 0..il { out.push(*d.get(i as usize).unwrap_or(&'0')); } } else { out.push('0'); }
        out.push('.');
        if il < 0 { for _ in 0..-il { out.push('0'); } }
        let frac: String = if il >= 0 { d.iter().skip(il as usize).collect() } else { d.iter().collect() };
        if frac.is_empty() { out.push('0'); } else { out.push_str(&frac); }
    } else {
        out.push(d[0]);
        out.push('.');
        if d.len() > 1 { out.extend(d.iter().skip(1)); } else { out.push('0'); }
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

fn main() {
    let mut bits: Vec<u32> = (1..=512).collect();
    let mut i: u64 = 513;
    while i < 0x7f80_0000u64 { bits.push(i as u32); i += 3571; }
    for e in [0u32, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000,
              0x3f80_0000, 0x3f00_0000, 0x0080_0000, 0x0100_0000, 0x7f7f_ffff,
              0x4b80_0000, 0x4b80_0001, 0x4b7f_ffff, 0x3eaa_aaab] { bits.push(e); }
    let mut out = String::new();
    for b in bits { out.push_str(&format!("{} {}\n", b, cand(f32::from_bits(b)))); }
    print!("{out}");
}
