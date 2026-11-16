fn cand(v: f32) -> String {
    if v.is_nan() { return "NaN".into(); }
    if v.is_infinite() { return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }; }
    if v == 0.0 { return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() }; }
    let a = v.abs();
    let mut k = 1usize;
    let (mut digits, mut exp) = (Vec::new(), 0i32);
    while k <= 9 {
        let s = format!("{:.*e}", k - 1, a);
        if s.parse::<f32>().map(|x| x == a).unwrap_or(false) {
            let (m, e) = s.split_once('e').unwrap();
            digits = m.chars().filter(|c| *c != '.').collect();
            exp = e.parse().unwrap();
            break;
        }
        k += 1;
    }
    // subnormal 段：Java 至少给两位（实测 bits=1 → 1.4E-45，不是 1.0E-45）
    if a < f32::MIN_POSITIVE && digits.len() == 1 {
        let s = format!("{:.*e}", 1, a);
        if let Some((m, e)) = s.split_once('e') {
            digits = m.chars().filter(|c| *c != '.').collect();
            exp = e.parse().unwrap();
        }
    }
    while digits.len() > 1 && *digits.last().unwrap() == '0' { digits.pop(); }
    if digits.is_empty() { digits.push('0'); }
    let mut out = String::new();
    if v < 0.0 { out.push('-'); }
    if (-3..7).contains(&exp) {
        let il = exp + 1;
        if il > 0 { for i in 0..il { out.push(*digits.get(i as usize).unwrap_or(&'0')); } } else { out.push('0'); }
        out.push('.');
        if il < 0 { for _ in 0..-il { out.push('0'); } }
        let frac: String = if il >= 0 { digits.iter().skip(il as usize).collect() } else { digits.iter().collect() };
        if frac.is_empty() { out.push('0'); } else { out.push_str(&frac); }
    } else {
        out.push(digits[0]);
        out.push('.');
        if digits.len() > 1 { out.extend(digits.iter().skip(1)); } else { out.push('0'); }
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

fn main() {
    let mut bits: Vec<u32> = (1..=2000).collect();
    let mut i: u64 = 2001;
    while i < 0x7f80_0000u64 { bits.push(i as u32); i += 1301; }
    for e in [0u32, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000,
              0x3f80_0000, 0x3f00_0000, 0x0080_0000, 0x0100_0000, 0x7f7f_ffff,
              0x4b80_0000, 0x4b80_0001, 0x4b7f_ffff, 0x3eaa_aaab] { bits.push(e); }
    let mut out = String::new();
    for b in bits { out.push_str(&format!("{} {}\n", b, cand(f32::from_bits(b)))); }
    print!("{out}");
}
