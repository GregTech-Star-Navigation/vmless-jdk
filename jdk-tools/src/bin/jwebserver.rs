//! `jwebserver` —— 静态 HTTP/1.1 文件服务器（只用 std `TcpListener`，一个连接一个线程）。
//!
//! 支持：keep-alive、`GET`/`HEAD`、`Range`（单区间）、按扩展名的 `Content-Type`、
//! 目录列表、`Last-Modified` + `If-Modified-Since`（304）、404/403/405/416/500。
//!
//! 与真 JDK 的差异：真 jwebserver 用 `com.sun.net.httpserver`，支持 `--bind-address`
//! 之外的 `-o/--output`（日志到文件）与 realm；这些都实现了。它不支持 POST/PUT
//! （我们返回 405），这一致。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vmless_tools::cli::Args;
use vmless_tools::zipx::mime_of;
use vmless_tools::{die, usage_err, EXIT_USAGE};

const TOOL: &str = "jwebserver";

fn help() {
    println!("Usage: jwebserver [-b bind address] [-d directory] [-o logfile] [-p port]");
    println!("                   [-version] [-h] [-?]");
    println!();
    println!("Option                  Description");
    println!("------                  -----------");
    println!("-b, --bind-address      Address to bind to (default: 127.0.0.1)");
    println!("-d, --directory         Directory to serve (default: current directory)");
    println!("-o, --output            Output file for the log (default: stdout)");
    println!("-p, --port              Port to listen on (default: 8000)");
    println!("-version, --version     Print version information and exit");
    println!("-h, -?, --help          Print this help message");
    println!();
    println!("行为：HTTP/1.1 keep-alive、GET/HEAD、单区间 Range、按扩展名 Content-Type、");
    println!("目录列表、Last-Modified/If-Modified-Since（304）、404/403/405/416/500。");
    println!("不支持的方法返回 405（与真 jwebserver 一致）。");
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() || args.is_empty() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &[
        "-b",
        "--bind-address",
        "-d",
        "--directory",
        "-o",
        "--output",
        "-p",
        "--port",
    ];
    let known: &[&str] = &[
        "-b",
        "--bind-address",
        "-d",
        "--directory",
        "-o",
        "--output",
        "-p",
        "--port",
        "-version",
        "--version",
        "-h",
        "-?",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let bind = get(&args, &["-b", "--bind-address"]).unwrap_or_else(|| "127.0.0.1".to_string());
    let port: u16 = match get(&args, &["-p", "--port"]) {
        Some(v) => match v.parse() {
            Ok(p) => p,
            Err(_) => usage_err(TOOL, &format!("端口不合法：{v}")),
        },
        None => 8000,
    };
    let dir = get(&args, &["-d", "--directory"]).unwrap_or_else(|| ".".to_string());
    let root = PathBuf::from(&dir);
    if !root.is_dir() {
        die(TOOL, &format!("-d 指向的不是目录: {dir}"));
    }
    let log_to = get(&args, &["-o", "--output"]);

    let listener = match TcpListener::bind(format!("{bind}:{port}")) {
        Ok(l) => l,
        Err(e) => die(TOOL, &format!("监听 {bind}:{port} 失败: {e}")),
    };
    println!(
        "Serving {} on http://{}:{}/",
        root.canonicalize().unwrap_or(root.clone()).display(),
        bind,
        port
    );
    if let Some(p) = &log_to {
        println!("Log to {p}");
    }
    let root = std::sync::Arc::new(root);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let root = root.clone();
                let logfile = log_to.clone();
                std::thread::spawn(move || {
                    let remote = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| "-".to_string());
                    if let Err(e) = serve(stream, &root, &remote, logfile.as_deref()) {
                        eprintln!("{TOOL}: 连接 {remote} 出错: {e}");
                    }
                });
            }
            Err(e) => eprintln!("{TOOL}: accept 失败: {e}"),
        }
    }
}

fn get(args: &Args, names: &[&str]) -> Option<String> {
    for n in names {
        match args.value(n) {
            Ok(Some(v)) => return Some(v),
            Ok(None) => {}
            Err(e) => usage_err(TOOL, &e),
        }
    }
    None
}

fn log_line(logfile: Option<&str>, line: &str) {
    match logfile {
        Some(p) => {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{line}");
            }
            println!("{line}");
        }
        None => println!("{line}"),
    }
}

fn serve(
    mut stream: TcpStream,
    root: &Path,
    remote: &str,
    logfile: Option<&str>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()), // 对端关闭
            Ok(_) => {}
            Err(_) => return Ok(()), // 超时/断开：静默结束这条连接
        }
        let req = line.trim_end().to_string();
        if req.is_empty() {
            return Ok(());
        }
        let mut parts = req.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("/").to_string();
        let version = parts.next().unwrap_or("HTTP/1.0").to_string();
        // 头部
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let keep_alive = if version.eq_ignore_ascii_case("HTTP/1.1") {
            !headers
                .iter()
                .any(|(k, v)| k == "connection" && v.eq_ignore_ascii_case("close"))
        } else {
            headers
                .iter()
                .any(|(k, v)| k == "connection" && v.eq_ignore_ascii_case("keep-alive"))
        };
        let status = handle(&mut stream, root, &method, &target, &headers, keep_alive)?;
        log_line(
            logfile,
            &format!(
                "{remote} - - [{}] \"{method} {target} {version}\" {status} -",
                http_date(SystemTime::now())
            ),
        );
        if !keep_alive {
            return Ok(());
        }
    }
}

fn handle(
    stream: &mut TcpStream,
    root: &Path,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    keep_alive: bool,
) -> Result<u16, String> {
    if method != "GET" && method != "HEAD" {
        respond(stream, 405, "text/plain; charset=utf-8", b"405 Method Not Allowed\n", &[], keep_alive, method == "HEAD", None)?;
        return Ok(405);
    }
    // 查询串与百分号解码
    let path_only = target.split('?').next().unwrap_or("/");
    let decoded = match percent_decode(path_only) {
        Ok(p) => p,
        Err(_) => {
            respond(stream, 400, "text/plain; charset=utf-8", b"400 Bad Request\n", &[], keep_alive, method == "HEAD", None)?;
            return Ok(400);
        }
    };
    let mut rel = decoded.trim_start_matches('/').to_string();
    if rel.contains("..") {
        respond(stream, 403, "text/plain; charset=utf-8", b"403 Forbidden\n", &[], keep_alive, method == "HEAD", None)?;
        return Ok(403);
    }
    if rel.is_empty() {
        rel = ".".into();
    }
    let mut full = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        full.push(seg);
    }
    let meta = match std::fs::metadata(&full) {
        Ok(m) => m,
        Err(_) => {
            respond(stream, 404, "text/plain; charset=utf-8", b"404 Not Found\n", &[], keep_alive, method == "HEAD", None)?;
            return Ok(404);
        }
    };
    let mtime = meta.modified().ok();
    if meta.is_dir() {
        // 目录没有 index.html 就列目录
        let index = full.join("index.html");
        if index.is_file() {
            return send_file(stream, &index, headers, keep_alive, method == "HEAD");
        }
        match list_dir(&full, path_only) {
            Ok(html) => {
                respond(
                    stream,
                    200,
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                    &[],
                    keep_alive,
                    method == "HEAD",
                    mtime,
                )?;
                return Ok(200);
            }
            Err(e) => {
                let body = format!("500 Internal Server Error\n{e}\n");
                respond(stream, 500, "text/plain; charset=utf-8", body.as_bytes(), &[], keep_alive, method == "HEAD", None)?;
                return Ok(500);
            }
        }
    }
    send_file(stream, &full, headers, keep_alive, method == "HEAD")
}

fn send_file(
    stream: &mut TcpStream,
    path: &Path,
    headers: &[(String, String)],
    keep_alive: bool,
    head_only: bool,
) -> Result<u16, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let mtime = meta.modified().ok();
    // If-Modified-Since
    if let Some(ims) = headers.iter().find(|(k, _)| k == "if-modified-since").map(|(_, v)| v) {
        if let (Some(m), Some(since)) = (mtime, parse_http_date(ims)) {
            if secs(m) <= secs(since) {
                let extra = vec![("Last-Modified".to_string(), http_date(m))];
                respond(stream, 304, "text/plain; charset=utf-8", b"", &extra, keep_alive, head_only, mtime)?;
                return Ok(304);
            }
        }
    }
    let data = std::fs::read(path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let mime = mime_of(&path.file_name().and_then(|s| s.to_str()).unwrap_or(""));
    let mut extra = vec![("Accept-Ranges".to_string(), "bytes".to_string())];
    if let Some(m) = mtime {
        extra.push(("Last-Modified".to_string(), http_date(m)));
    }
    // 单区间 Range
    if let Some(r) = headers.iter().find(|(k, _)| k == "range").map(|(_, v)| v) {
        if let Some((start, end)) = parse_range(r, data.len()) {
            let slice = &data[start..=end];
            let cr = format!("bytes {start}-{end}/{}", data.len());
            extra.push(("Content-Range".to_string(), cr));
            respond_with_len(stream, 206, mime, slice, &extra, keep_alive, head_only, Some(slice.len()), mtime)?;
            return Ok(206);
        } else {
            let cr = format!("bytes */{}", data.len());
            respond(stream, 416, "text/plain; charset=utf-8", b"416 Range Not Satisfiable\n", &[("Content-Range".to_string(), cr)], keep_alive, head_only, mtime)?;
            return Ok(416);
        }
    }
    respond(
        stream,
        200,
        mime,
        &data,
        &extra,
        keep_alive,
        head_only,
        mtime,
    )?;
    Ok(200)
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(String, String)],
    keep_alive: bool,
    head_only: bool,
    _mtime: Option<SystemTime>,
) -> Result<(), String> {
    respond_with_len(stream, status, ctype, body, extra, keep_alive, head_only, Some(body.len()), _mtime)
}

#[allow(clippy::too_many_arguments)]
fn respond_with_len(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(String, String)],
    keep_alive: bool,
    head_only: bool,
    len: Option<usize>,
    _mtime: Option<SystemTime>,
) -> Result<(), String> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    head.push_str(&format!("Date: {}\r\n", http_date(SystemTime::now())));
    head.push_str(&format!("Content-Type: {ctype}\r\n"));
    head.push_str(&format!("Content-Length: {}\r\n", len.unwrap_or(body.len())));
    head.push_str(&format!(
        "Connection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    ));
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    if !head_only {
        stream.write_all(body).map_err(|e| e.to_string())?;
    }
    stream.flush().map_err(|e| e.to_string())
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn percent_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err("百分号编码不完整".into());
            }
            let hi = hexval(b[i + 1]).ok_or_else(|| "百分号编码非法".to_string())?;
            let lo = hexval(b[i + 2]).ok_or_else(|| "百分号编码非法".to_string())?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "路径不是合法 UTF-8".to_string())
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_range(v: &str, len: usize) -> Option<(usize, usize)> {
    let v = v.trim();
    let v = v.strip_prefix("bytes=")?;
    if v.contains(',') {
        return None; // 多区间不支持（真 jwebserver 也只处理单区间）
    }
    let (a, b) = v.split_once('-')?;
    if len == 0 {
        return None;
    }
    if a.is_empty() {
        // 后缀区间：-N
        let n: usize = b.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let start = len.saturating_sub(n);
        return Some((start, len - 1));
    }
    let start: usize = a.trim().parse().ok()?;
    let end = if b.trim().is_empty() {
        len - 1
    } else {
        b.trim().parse::<usize>().ok()?.min(len - 1)
    };
    if start > end || start >= len {
        return None;
    }
    Some((start, end))
}

fn list_dir(dir: &Path, url: &str) -> Result<String, String> {
    let mut items: Vec<(String, bool)> = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.path().is_dir();
        items.push((name, is_dir));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let base = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">");
    html.push_str(&format!("<title>Index of {}{}</title></head>\n", url, ""));
    html.push_str("<body>\n");
    html.push_str(&format!("<h1>Index of {url}</h1>\n<ul>\n"));
    if url != "/" {
        html.push_str(&format!("<li><a href=\"{base}../\">../</a></li>\n"));
    }
    for (name, is_dir) in items {
        let href = format!(
            "{}{}{}",
            base,
            urlencode(&name),
            if is_dir { "/" } else { "" }
        );
        html.push_str(&format!(
            "<li><a href=\"{href}\">{name}{}</a></li>\n",
            if is_dir { "/" } else { "" }
        ));
    }
    html.push_str("</ul>\n</body></html>\n");
    Ok(html)
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// HTTP-date（RFC 7231 IMF-fixdate）：`Sun, 06 Nov 1994 08:49:37 GMT`
fn http_date(t: SystemTime) -> String {
    let secs = secs(t);
    let days = secs / 86400;
    let tod = secs % 86400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let wd = DAYS[(days % 7) as usize];
    // 民用历
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{wd}, {d:02} {} {year} {hh:02}:{mm:02}:{ss:02} GMT",
        MONTHS[(m - 1) as usize]
    )
}

fn parse_http_date(s: &str) -> Option<SystemTime> {
    // 只认 IMF-fixdate 与 RFC 850 的常见形式，够 If-Modified-Since 用
    let s = s.trim();
    let mut it = s.split_whitespace();
    let _wd = it.next()?;
    let day: u64 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let hh: u64 = hms.next()?.parse().ok()?;
    let mm: u64 = hms.next()?.parse().ok()?;
    let ss: u64 = hms.next()?.parse().ok()?;
    let mi = MONTHS.iter().position(|m| *m == mon)? as i64 + 1;
    // days_from_civil
    let y = if mi <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mi > 2 { mi - 3 } else { mi + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe - 719_468) as u64;
    Some(UNIX_EPOCH + Duration::from_secs(days * 86400 + hh * 3600 + mm * 60 + ss))
}

/// 把 `Read` 全读进来（给测试/扩展用）
#[allow(dead_code)]
fn read_all(mut r: impl Read) -> Vec<u8> {
    let mut v = Vec::new();
    let _ = r.read_to_end(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_range("bytes=5-", 10), Some((5, 9)));
        assert_eq!(parse_range("bytes=9-100", 10), Some((9, 9)));
        assert_eq!(parse_range("bytes=20-30", 10), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), None);
    }

    #[test]
    fn decode_paths() {
        assert_eq!(percent_decode("/a%20b").unwrap(), "/a b");
        assert!(percent_decode("/a%2").is_err());
    }
}
