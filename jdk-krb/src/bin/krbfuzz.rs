//! 三个解析入口的 fuzz：`kdc-req` / `as-rep` / `ccache`。
//!
//! 目标是「**0 panic**」而不是「找到多少错」：每个入口都喂随机字节和
//! 「真报文 + 随机改写」两种输入，用 `catch_unwind` 兜住 panic 并统计。
//!
//! ```text
//! krbfuzz --iters 50000
//! krbfuzz --iters 50000 --seed 12345 --verbose
//! ```

use std::panic::AssertUnwindSafe;
use std::process::ExitCode;

use vmless_krb::asn1_krb::*;
use vmless_krb::msg::*;
use vmless_krb::ccache::CcCache;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut iters: u64 = 50_000;
    let mut seed: u64 = 0x243f_6a88_85a3_08d3;
    let mut verbose = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--iters" => {
                i += 1;
                iters = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                    eprintln!("krbfuzz: --iters 需要一个数字");
                    std::process::exit(2)
                });
            }
            "--seed" => {
                i += 1;
                seed = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or(seed);
            }
            "-V" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                println!("用法: krbfuzz [--iters N] [--seed S] [-V]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("krbfuzz: 无法识别的选项 {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    // 关掉默认的 panic 打印（否则一个 panic 就刷屏），但保留「有没有」的统计
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        eprintln!("krbfuzz: PANIC {info}");
    }));

    let targets = build_targets();
    let mut total_panics = 0u64;
    let mut total_cases = 0u64;

    for (name, seeds) in &targets {
        let start = std::time::Instant::now();
        let mut x = seed ^ hash_name(name);
        let mut panics = 0u64;
        for k in 0..iters {
            // 一半纯随机，一半「真报文 + 随机改写」
            let input = if k % 2 == 0 {
                let n = (x % 256) as usize;
                random_bytes(&mut x, n)
            } else {
                let idx = (x as usize) % seeds.len();
                let base = &seeds[idx];
                mutate(&mut x, base)
            };
            let r = std::panic::catch_unwind(AssertUnwindSafe(|| run_target(name, &input)));
            if r.is_err() {
                panics += 1;
                if panics <= 5 {
                    eprintln!(
                        "krbfuzz: {name} 在第 {k} 例 panic，输入 {} 字节: {}",
                        input.len(),
                        vmless_crypto::sys::hex(&input[..input.len().min(48)])
                    );
                }
            }
        }
        let ms = start.elapsed().as_millis();
        println!(
            "{name:8} 例数 {iters:>7}  panic {panics}  用时 {ms}ms  ({:.0} 例/秒)",
            iters as f64 / (ms.max(1) as f64 / 1000.0)
        );
        total_panics += panics;
        total_cases += iters;
        if verbose {
            eprintln!("krbfuzz: {name} 完成");
        }
    }
    std::panic::set_hook(prev_hook);
    println!("合计 {total_cases} 例，panic {total_panics}");
    if total_panics > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// 每个入口的种子报文（真报文，保证 fuzz 能走到「解析成功」的深处）。
fn build_targets() -> Vec<(String, Vec<Vec<u8>>)> {
    let realm = "VMKRB.FUZZ";
    let now = 1_800_000_000i64;
    let cname = PrincipalName::parse("alice").unwrap();
    let mut out = Vec::new();

    // kdc-req：AS-REQ（带与不带预认证各一份）+ TGS-REQ
    let mut as_req = AsReqParams::new(realm, cname.clone(), vec![18, 17], now);
    out.push(as_req.to_req().to_der());
    as_req.padata = vec![PaData::new(PA_ENC_TIMESTAMP, vec![0xa0, 0x03, 0x02, 0x01, 0x12])];
    out.push(as_req.to_req().to_der());
    let tgt = Ticket {
        tkt_vno: TKT_VNO,
        realm: realm.to_string(),
        sname: PrincipalName::parse(&format!("krbtgt/{realm}")).unwrap(),
        enc_part: EncryptedData::new(18, Some(1), vec![0xab; 96]),
    };
    let pa = PaData::new(PA_TGS_REQ, {
        let ap = ApReq {
            pvno: PVNO,
            msg_type: MSG_AP_REQ,
            ap_options: 0,
            ticket: tgt.clone(),
            authenticator: EncryptedData::new(18, None, vec![0xcd; 64]),
        };
        ap.to_der()
    });
    let tgs = TgsReqParams {
        realm: realm.to_string(),
        cname: cname.clone(),
        sname: PrincipalName::parse("host/localhost").unwrap(),
        nonce: 7,
        etypes: vec![18],
        kdc_options: 0,
        till: now + 3600,
        rtime: None,
        pa_tgs_req: pa,
    };
    out.push(tgs.to_req().to_der());

    // as-rep：一份完整的 AS-REP + 一份 KRB-ERROR
    let rep = KdcRep {
        pvno: PVNO,
        msg_type: MSG_AS_REP,
        padata: Vec::new(),
        crealm: realm.to_string(),
        cname: cname.clone(),
        ticket: tgt.clone(),
        enc_part: EncryptedData::new(18, None, vec![0xef; 128]),
    };
    out.push(rep.to_der());
    let err = KrbError {
        pvno: PVNO,
        msg_type: MSG_KRB_ERROR,
        ctime: None,
        cusec: None,
        stime: now,
        susec: 0,
        error_code: errc::KDC_ERR_PREAUTH_REQUIRED,
        crealm: Some(realm.to_string()),
        cname: Some(cname.clone()),
        realm: realm.to_string(),
        sname: PrincipalName::parse(&format!("krbtgt/{realm}")).unwrap(),
        e_text: Some("NEED PREAUTH".into()),
        e_data: Some(vec![0x30, 0x00]),
    };
    out.push(err.to_der());
    let cc_targets = ("ccache".to_string(), out.clone());

    // ccache：一份两票的缓存
    let mut cc = CcCache::new(vmless_krb::ccache::CcPrincipal::new(1, realm, vec!["alice".into()]));
    for sname in ["krbtgt", "host/localhost"] {
        let comps: Vec<String> = sname.split('/').map(|s| s.to_string()).collect();
        cc.creds.push(vmless_krb::ccache::Credential::from_tgt(
            vmless_krb::ccache::CcPrincipal::new(1, realm, vec!["alice".into()]),
            vmless_krb::ccache::CcPrincipal::new(2, realm, comps),
            EncryptionKey::new(18, vec![0x11; 32]),
            now,
            now,
            now + 36000,
            now + 604800,
            0x40e0_0000,
            tgt.to_der(),
        ));
    }

    vec![
        ("kdc-req".to_string(), out),
        ("as-rep".to_string(), vec![rep.to_der(), err_der(realm, now)]),
        (cc_targets.0, vec![cc.to_bytes()]),
    ]
}

fn err_der(realm: &str, now: i64) -> Vec<u8> {
    KrbError {
        pvno: PVNO,
        msg_type: MSG_KRB_ERROR,
        ctime: None,
        cusec: None,
        stime: now,
        susec: 0,
        error_code: 6,
        crealm: None,
        cname: None,
        realm: realm.to_string(),
        sname: PrincipalName::parse(&format!("krbtgt/{realm}")).unwrap(),
        e_text: None,
        e_data: None,
    }
    .to_der()
}

fn run_target(name: &str, data: &[u8]) {
    match name {
        "kdc-req" => {
            if let Ok(req) = KdcReq::from_bytes(data) {
                // 解析成功后再走一遍「按字段取值」，覆盖更深的分支
                let _ = req.body.cname.as_ref().map(|c| c.display());
                let _ = req.body.etypes.len();
                let _ = req.pa(PA_ENC_TIMESTAMP);
            }
            let _ = KdcRep::from_bytes(data);
            let _ = Ticket::from_bytes(data);
            let _ = ApReq::from_bytes(data);
            let _ = EncTicketPart::from_bytes(data);
            let _ = Authenticator::from_bytes(data);
        }
        "as-rep" => {
            if let Ok(rep) = KdcRep::from_bytes(data) {
                let _ = rep.ticket.sname.display();
                let _ = rep.enc_part.cipher.len();
            }
            if let Some(e) = vmless_krb::msg::parse_krb_error(data) {
                let _ = vmless_krb::msg::etype_hints(&e);
                let _ = vmless_krb::msg::describe_error(&e);
            }
            let _ = EncKdcRepPart::from_bytes(data, false);
            let _ = EncKdcRepPart::from_bytes(data, true);
            let _ = EncryptedData::from_bytes(data);
        }
        "ccache" => {
            if let Ok(cc) = CcCache::parse(data) {
                let _ = cc.tgt().map(|c| c.server.display());
                let _ = cc.to_bytes();
                let _ = vmless_krb::client::format_klist(&cc, "x", true, true, 0);
            }
        }
        other => panic!("未知目标 {other}"),
    }
}

fn random_bytes(x: &mut u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    for b in v.iter_mut() {
        *b = next_u8(x);
    }
    v
}

fn next_u8(x: &mut u64) -> u8 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    (*x >> 24) as u8
}

/// 真报文 + 随机改写（改字节 / 截断 / 插入），比纯随机更能打进深层分支。
fn mutate(x: &mut u64, base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    if v.is_empty() {
        return v;
    }
    match next_u8(x) % 4 {
        0 => {
            let n = 1 + (next_u8(x) as usize % 4);
            for _ in 0..n {
                let idx = (next_u8(x) as usize) % v.len();
                v[idx] = next_u8(x);
            }
        }
        1 => {
            let cut = (next_u8(x) as usize) % v.len();
            v.truncate(cut);
        }
        2 => {
            let idx = (next_u8(x) as usize) % v.len();
            v.insert(idx, next_u8(x));
        }
        _ => {
            let idx = (next_u8(x) as usize) % v.len();
            v.remove(idx);
        }
    }
    v
}

fn hash_name(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
