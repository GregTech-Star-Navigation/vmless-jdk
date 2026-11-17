//! SHA-1（FIPS 180-4）。仅用于兼容旧格式（PKCS#12 旧 MAC / SHA1withRSA 验签），不用于新签名。

fn compress(h: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let mut a = h[0];
    let mut b = h[1];
    let mut c = h[2];
    let mut d = h[3];
    let mut e = h[4];
    for i in 0..80 {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
            20..=39 => (b ^ c ^ d, 0x6ed9eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
            _ => (b ^ c ^ d, 0xca62c1d6),
        };
        let tmp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(w[i]);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = tmp;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

pub fn digest(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut off = 0;
    while off + 64 <= data.len() {
        compress(&mut h, &data[off..off + 64]);
        off += 64;
    }
    let rem = &data[off..];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut last = [0u8; 64];
    last[..rem.len()].copy_from_slice(rem);
    last[rem.len()] = 0x80;
    if rem.len() < 56 {
        last[56..64].copy_from_slice(&bits.to_be_bytes());
        compress(&mut h, &last);
    } else {
        compress(&mut h, &last);
        let mut last2 = [0u8; 64];
        last2[56..64].copy_from_slice(&bits.to_be_bytes());
        compress(&mut h, &last2);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}
