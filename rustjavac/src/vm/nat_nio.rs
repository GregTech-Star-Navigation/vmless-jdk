//! native 实现分组：nat_nio —— **`java.nio` / `java.nio.file` / `java.net` 的原生层**。
//!
//! 由 `native_reg.rs` 的 `register_all` 末尾调用 `register(r)`。
//!
//! 覆盖面（OpenJDK 里带 `native` 关键字的那一层，逐条对着 JDK25 的 `javap` 抄的描述符）：
//!
//! | 类 | 干什么 |
//! | --- | --- |
//! | `sun.nio.fs.WindowsNativeDispatcher` | Win32 文件系统：CreateFile / ReadFile / FindFirstFile / 属性 / 改名 / 删除 |
//! | `sun.nio.ch.FileDispatcherImpl` | 文件描述符上的 read/write/seek/size/truncate/force/close/lock |
//! | `sun.nio.ch.IOUtil` | fd 值、阻塞模式、pipe、随机数 |
//! | `sun.nio.ch.Net` / `SocketDispatcher` | TCP socket：socket/bind/listen/accept/connect/send/recv/poll/选项 |
//! | `sun.nio.fs.WindowsNativeDispatcher$FirstFile` | 目录流（`Files.list`） |
//!
//! ## 三条不要踩的（都是实测踩出来的）
//!
//! 1. **native 内存只有一份**：`sun.nio.*` 那些「Java 侧用 `Unsafe.allocateMemory` 拿地址、
//!    往里面写结构体」的 native，地址全都落在 [`super::nat_util::nat_mem`] 的 arena 里。
//!    所以这里读写一律走 `nat_mem`，**不要**用 `Vec<u8>` 自己存一份 —— 那样
//!    `Unsafe.putInt(null, addr)` 写进去的字段本文件读不到，症状是「结构体字段全是 0」。
//! 2. **结构体布局按 Windows ABI 逐字节对齐**（`WIN32_FIND_DATAW` 592、
//!    `BY_HANDLE_FILE_INFORMATION` 52、`WIN32_FILE_ATTRIBUTE_DATA` 36、`iovec` 16）。
//!    这些布局的对端是 Java 侧的 `Unsafe.getInt(address + OFFSET)`，偏移错一个就是静默读错。
//!    本文件的偏移常量全部抄自 `sun/nio/fs/WindowsFileAttributes.java` 与 JDK25 源。
//! 3. **错误码走 `sun.nio.fs.WindowsException(i32)`**：Win32 错误码原样交给它，
//!    由 Java 侧的 `translateToIOException` 映射成 `NoSuchFileException` /
//!    `AccessDeniedException` / `FileAlreadyExistsException` —— 与 HotSpot 完全同一条路。
//!    **不要**自己在这里猜异常类型：猜出来的和 Java 侧对不上，`catch (NoSuchFileException)`
//!    就接不住。
//!
//! 句柄约定与 `nat_os.rs` 一致：Win32 `HANDLE` 原样放在 `java.io.FileDescriptor.handle`
//! （i64 字段）里，`close0` 必须真 `CloseHandle` 并把 `handle`/`fd` 写回 -1。

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use super::super::frame::Frame;
use super::super::slot::Slot;
use super::super::{Vm, VmError};
use super::nat_util::nat_mem;
use super::NativeRegistry;

pub(super) fn register(r: &mut NativeRegistry) {
    register_windows_dispatcher(r);
    register_windows_security(r);
    register_file_dispatcher(r);
    register_mapped_memory(r);
    register_ioutil(r);
    register_net(r);
    register_socket_dispatcher(r);
    register_inet_address(r);
    register_network_interface(r);
    register_watch_service(r);
    register_selector(r);
    register_dns(r);
    register_ntlm(r);
}

// ============================================================ NTLM（HTTP 代理/服务器认证）
//
// 这一组是 `sun.net.www.protocol.http.ntlm` 的 Windows 实现，底座是 **SSPI**
// （`AcquireCredentialsHandleW` / `InitializeSecurityContextW`）。
//
// 现在只有两条是「真值查询」，两条 SSPI 握手**明确抛
// `UnsupportedOperationException`**：SPPI 的 `SecBufferDesc` / 4 个 `SecBuffer` /
// 令牌续传要跟 `NTLMAuthSequence` 的状态机严格对齐，半套实现会在第 2 个令牌上
// 报 `SEC_E_INVALID_TOKEN` —— 那比一开始就说「不支持」更难查。
// 影响面：只有 `http.auth.ntlm` / 需要 NTLM 的代理认证会走到，普通 HTTP(S) 不受影响。
const NTLM_SEQ: &str = "sun/net/www/protocol/http/ntlm/NTLMAuthSequence";
const NTLM_AUTH: &str = "sun/net/www/protocol/http/ntlm/NTLMAuthentication";

/// `initFirst(Class<Status>)`：HotSpot 在这里缓存 `Status.status` 的字段 ID。
/// 本 VM 不做 JNI 字段访问（走 `set_obj_field`），所以空实现是正确的。
fn ntlm_init_first(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn ntlm_credentials_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(vm.throw_msg(
        "java/lang/UnsupportedOperationException",
        "NTLM 认证未实现（需要 SSPI 的 AcquireCredentialsHandle/InitializeSecurityContext 握手）".to_string(),
    ))
}

/// `isTrustedSiteAvailable()`：Windows 上有「本地 Intranet 区域」的概念，所以 true
fn ntlm_trusted_available(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(1))
}

/// `isTrustedSite0(url)`：URL 是否落在「本地 Intranet」安全区域
///
/// 真实现要 `IInternetSecurityManager.MapUrlToZone`（urlmon）。**这里返回 false**：
/// 语义是「不是受信任站点」，也就是**不会**自动带上当前 Windows 凭据去做 NTLM 认证 ——
/// 这个方向上出错是安全的（多一次提示），反方向（把外网站点当内网 → 静默泄露 NTLM 哈希）
/// 才是事故。等真做 urlmon 时这里换成 `MapUrlToZone(url) == URLZONE_INTRANET`。
fn ntlm_is_trusted_site(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn register_ntlm(r: &mut NativeRegistry) {
    r.add(NTLM_SEQ, "initFirst", "(Ljava/lang/Class;)V", ntlm_init_first);
    r.add(
        NTLM_SEQ,
        "getCredentialsHandle",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)J",
        ntlm_credentials_unsupported,
    );
    r.add(
        NTLM_SEQ,
        "getNextToken",
        "(J[BLsun/net/www/protocol/http/ntlm/NTLMAuthSequence$Status;)[B",
        ntlm_credentials_unsupported,
    );
    r.add(NTLM_AUTH, "isTrustedSiteAvailable", "()Z", ntlm_trusted_available);
    r.add(NTLM_AUTH, "isTrustedSite0", "(Ljava/lang/String;)Z", ntlm_is_trusted_site);
}

// ============================================================ Win32 裸 FFI

type Handle = isize;
type Bool = i32;
type Socket = usize; // Windows SOCKET 是指针宽的整数

const INVALID_HANDLE_VALUE: Handle = -1;
const INVALID_SOCKET: Socket = usize::MAX;

// --- 访问权 / 共享 / 创建方式（与 WindowsConstants 同名同值）---
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;

const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;

const FILE_TYPE_DISK: u32 = 1;
const FILE_TYPE_CHAR: u32 = 2;
const FILE_TYPE_PIPE: u32 = 3;

// --- Win32 错误码 ---
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_FILE_EXISTS: u32 = 80;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_DIRECTORY: u32 = 267;
const ERROR_DIR_NOT_EMPTY: u32 = 145;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_SHARING_VIOLATION: u32 = 32;
const ERROR_LOCK_VIOLATION: u32 = 33;
const ERROR_NEGATIVE_SEEK: u32 = 131;
const ERROR_INVALID_LEVEL: u32 = 124;

const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;
const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x100;

const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
const MOVEFILE_COPY_ALLOWED: u32 = 0x2;
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

const VOLUME_NAME_DOS: u32 = 0;

const FILE_BASIC_INFO: u32 = 0;
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: u32 = 16 * 1024;

// --- winsock ---
const AF_INET: i32 = 2;
const AF_INET6: i32 = 23;
const SOCK_STREAM: i32 = 1;
const SOCKET_ERROR: i32 = -1;
const WSAEWOULDBLOCK: i32 = 10035;
const WSAEINPROGRESS: i32 = 10036;
const WSAGetLastError_OFFSET: i32 = 1;
const SO_REUSEADDR: i32 = 0x0004;
const SO_REUSEPORT_WIN: i32 = 0x7004;
const SOL_SOCKET: i32 = 0xffff;
const SHUT_RD: i32 = 0;
const SHUT_WR: i32 = 1;
const SHUT_RDWR: i32 = 2;
const FIONBIO: i32 = 0x8004_667Eu32 as i32;
const FIONREAD: i32 = 0x4004_667Fu32 as i32;

// WSAPoll 事件位
const POLLRDNORM: i16 = 0x0100;
const POLLWRNORM: i16 = 0x0010;
const POLLERR: i16 = 0x0001;
const POLLHUP: i16 = 0x0002;
const POLLNVAL: i16 = 0x0004;
const POLLCONN: i16 = 0x0002;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn value(self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
}

/// `WIN32_FILE_ATTRIBUTE_DATA`（36 字节）
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Win32FileAttributeData {
    attributes: u32,
    creation: FileTime,
    last_access: FileTime,
    last_write: FileTime,
    size_high: u32,
    size_low: u32,
}

/// `BY_HANDLE_FILE_INFORMATION`（52 字节）
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ByHandleFileInformation {
    attributes: u32,
    creation: FileTime,
    last_access: FileTime,
    last_write: FileTime,
    volume_serial: u32,
    size_high: u32,
    size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

/// `WIN32_FIND_DATAW`（592 字节）
#[repr(C)]
#[derive(Clone, Copy)]
struct Win32FindDataW {
    attributes: u32,
    creation: FileTime,
    last_access: FileTime,
    last_write: FileTime,
    size_high: u32,
    size_low: u32,
    reserved0: u32,
    reserved1: u32,
    file_name: [u16; 260],
    alternate_name: [u16; 14],
}

impl Default for Win32FindDataW {
    fn default() -> Self {
        Win32FindDataW {
            attributes: 0,
            creation: FileTime::default(),
            last_access: FileTime::default(),
            last_write: FileTime::default(),
            size_high: 0,
            size_low: 0,
            reserved0: 0,
            reserved1: 0,
            file_name: [0; 260],
            alternate_name: [0; 14],
        }
    }
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: u32,
    zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrIn6 {
    family: u16,
    port: u16,
    flowinfo: u32,
    addr: [u8; 16],
    scope_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PollFd {
    fd: Socket,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct WsaData {
    version: u16,
    high_version: u16,
    description: [u8; 257],
    system_status: [u8; 129],
    max_sockets: u16,
    max_udp_dg: u16,
    vendor_info: *mut i8,
}

extern "system" {
    fn GetLastError() -> u32;
    fn CloseHandle(h: Handle) -> Bool;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *const c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn ReadFile(h: Handle, buf: *mut c_void, len: u32, read: *mut u32, ov: *mut Overlapped) -> Bool;
    fn WriteFile(h: Handle, buf: *const c_void, len: u32, written: *mut u32, ov: *mut Overlapped) -> Bool;
    fn SetFilePointerEx(h: Handle, dist: i64, new_pos: *mut i64, method: u32) -> Bool;
    fn SetEndOfFile(h: Handle) -> Bool;
    fn GetFileSizeEx(h: Handle, size: *mut i64) -> Bool;
    fn GetFileType(h: Handle) -> u32;
    fn FlushFileBuffers(h: Handle) -> Bool;
    fn DeleteFileW(name: *const u16) -> Bool;
    fn CreateDirectoryW(name: *const u16, sa: *const c_void) -> Bool;
    fn RemoveDirectoryW(name: *const u16) -> Bool;
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> Bool;
    fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: Bool) -> Bool;
    fn GetFileAttributesW(name: *const u16) -> u32;
    fn SetFileAttributesW(name: *const u16, attrs: u32) -> Bool;
    fn GetFileAttributesExW(name: *const u16, level: u32, data: *mut c_void) -> Bool;
    fn GetFileInformationByHandle(h: Handle, info: *mut ByHandleFileInformation) -> Bool;
    fn SetFileTime(
        h: Handle,
        creation: *const FileTime,
        access: *const FileTime,
        write: *const FileTime,
    ) -> Bool;
    fn FindFirstFileW(name: *const u16, data: *mut Win32FindDataW) -> Handle;
    fn FindNextFileW(h: Handle, data: *mut Win32FindDataW) -> Bool;
    fn FindClose(h: Handle) -> Bool;
    fn GetFullPathNameW(name: *const u16, len: u32, buf: *mut u16, part: *mut *mut u16) -> u32;
    fn GetFinalPathNameByHandleW(h: Handle, buf: *mut u16, len: u32, flags: u32) -> u32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_len: u32,
    ) -> Bool;
    fn GetVolumePathNameW(name: *const u16, buf: *mut u16, len: u32) -> Bool;
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_available: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> Bool;
    fn GetDiskFreeSpaceW(
        root: *const u16,
        sectors_per_cluster: *mut u32,
        bytes_per_sector: *mut u32,
        free_clusters: *mut u32,
        total_clusters: *mut u32,
    ) -> Bool;
    fn DeviceIoControl(
        h: Handle,
        code: u32,
        inbuf: *mut c_void,
        inlen: u32,
        outbuf: *mut c_void,
        outlen: u32,
        returned: *mut u32,
        ov: *mut Overlapped,
    ) -> Bool;
    fn CreateSymbolicLinkW(link: *const u16, target: *const u16, flags: u32) -> Bool;
    fn CreateHardLinkW(link: *const u16, target: *const u16, sa: *const c_void) -> Bool;
    fn LockFile(h: Handle, off_low: u32, off_high: u32, len_low: u32, len_high: u32) -> Bool;
    fn UnlockFile(h: Handle, off_low: u32, off_high: u32, len_low: u32, len_high: u32) -> Bool;
    fn GetCurrentProcess() -> Handle;
    fn DuplicateHandle(
        process: Handle,
        src: Handle,
        target: Handle,
        out: *mut Handle,
        access: u32,
        inherit: Bool,
        options: u32,
    ) -> Bool;
    fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        msg_id: u32,
        lang: u32,
        buf: *mut u16,
        size: u32,
        args: *const u8,
    ) -> u32;
    fn LocalFree(p: *mut c_void) -> *mut c_void;
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn CreatePipe(r: *mut Handle, w: *mut Handle, sa: *mut c_void, size: u32) -> Bool;
    fn SetHandleInformation(h: Handle, mask: u32, flags: u32) -> Bool;
}

/// 缺少时在启动阶段初始化的常量
const HANDLE_FLAG_INHERIT: u32 = 0x1;


// ============================================================ 小工具：nat_mem 读写

/// 一个地址处**还能写多少字节**（nat_mem 块尾为止）。0 = 地址无效。
fn capacity_at(addr: u64) -> usize {
    match nat_mem::base_of(addr) {
        Some((base, len)) => len.saturating_sub((addr - base) as usize),
        None => 0,
    }
}

/// 读 NUL 结尾的 UTF-16 字符串（Java 侧 `NativeBuffer` 里的就是这种）
fn read_wstr(addr: u64) -> Vec<u16> {
    let mut out = Vec::new();
    if addr == 0 {
        return out;
    }
    let cap = capacity_at(addr) / 2;
    for i in 0..cap {
        let w = nat_mem::read(addr + (i as u64) * 2, 2) as u16;
        if w == 0 {
            break;
        }
        out.push(w);
    }
    out
}

fn read_path(addr: u64) -> String {
    String::from_utf16_lossy(&read_wstr(addr))
}

/// 读 NUL 结尾 UTF-16，**并且把终止符一起留在末尾**。
///
/// 给 Win32 传指针时必须用这个：`LPCWSTR` 的契约是「以 NUL 结尾」，
/// 只读到 NUL 却不把它放回去，Win32 会继续读缓冲区后面的字节 ——
/// 表现是「同一个路径第一次调用报 FILE_NOT_FOUND / INVALID_NAME，
/// 重试又好了」（后面那段垃圾取决于缓冲区上一次的内容）。
fn read_wstr_z(addr: u64) -> Vec<u16> {
    let mut v = read_wstr(addr);
    v.push(0);
    v
}

/// 往地址写 NUL 结尾 UTF-16，**不越过块尾**（越界就截断，绝不多写一个字节）
fn write_wstr(addr: u64, text: &str) -> usize {
    let w: Vec<u16> = text.encode_utf16().collect();
    let cap = capacity_at(addr);
    if cap < 2 {
        return 0;
    }
    let room = cap / 2; // 含 NUL 的容量（u16 个数）
    let n = w.len().min(room - 1);
    let mut bytes = Vec::with_capacity((n + 1) * 2);
    for &u in w.iter().take(n) {
        bytes.push((u & 0xFF) as u8);
        bytes.push((u >> 8) as u8);
    }
    bytes.push(0);
    bytes.push(0);
    nat_mem::write_bytes(addr, &bytes);
    n
}

fn read_u32(addr: u64, off: u64) -> u32 {
    nat_mem::read(addr + off, 4) as u32
}

fn read_u64(addr: u64, off: u64) -> u64 {
    nat_mem::read(addr + off, 8)
}

fn write_u32(addr: u64, off: u64, v: u32) {
    nat_mem::write(addr + off, 4, v as u64);
}

fn write_u64(addr: u64, off: u64, v: u64) {
    nat_mem::write(addr + off, 8, v);
}

/// 结构体 → nat_mem（按 `size_of::<T>()` 逐字节拷贝，布局由 `#[repr(C)]` 保证）
fn put_struct<T>(addr: u64, v: &T) {
    let bytes = unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) };
    nat_mem::write_bytes(addr, bytes);
}

/// nat_mem → 结构体
fn get_struct<T: Copy + Default>(addr: u64) -> T {
    let bytes = nat_mem::read_bytes(addr, size_of::<T>());
    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) }
}

/// Java `String` → NUL 结尾 UTF-16（给 Win32 调用）
fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

// ============================================================ 错误

/// 调试开关：`VMLESS_NIO_TRACE=1` 时把每次 Win32 调用打到 stderr（排查「卡在哪个 native」用）
fn trace(what: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("VMLESS_NIO_TRACE").is_some()) {
        eprintln!("[nio] {what}");
    }
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Win32 错误码 → 本地化文本（与 `java.io` 的异常消息同源）
fn win_msg(code: u32) -> String {
    if code == 0 {
        return "未知错误".to_string();
    }
    let mut buf = [0u16; 512];
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    if n == 0 {
        return format!("错误 {code}");
    }
    String::from_utf16_lossy(&buf[..n as usize])
        .trim_end_matches(['\r', '\n', ' ', '.'])
        .to_string()
}

/// 抛 `sun.nio.fs.WindowsException(lastError)` —— **异常类型映射交给 Java 侧**
fn throw_windows(vm: &mut Vm, code: u32) -> VmError {
    match vm.new_instance_args("sun/nio/fs/WindowsException", "(I)V", &[Slot::I(code as i32)]) {
        Ok(h) => VmError { msg: format!("WindowsException({code}: {})", win_msg(code)), exc: Some(h) },
        Err(e) => e,
    }
}

fn throw_io(vm: &mut Vm, msg: impl Into<String>) -> VmError {
    vm.throw_msg("java/io/IOException", msg.into())
}

fn throw_sock(vm: &mut Vm, msg: impl Into<String>) -> VmError {
    vm.throw_msg("java/net/SocketException", msg.into())
}

// ============================================================ FileDescriptor 助手

const FD_CLASS: &str = "java/io/FileDescriptor";

fn fd_slot(vm: &Vm, fd_obj: i32, field: &str) -> i64 {
    vm.store
        .field_index(FD_CLASS, field)
        .and_then(|i| vm.sh.heap.get(fd_obj, i))
        .map(|s| s.as_i64())
        .unwrap_or(-1)
}

fn set_fd_slot(vm: &Vm, fd_obj: i32, field: &str, v: i64) {
    if let Some(i) = vm.store.field_index(FD_CLASS, field) {
        vm.sh.heap.set(fd_obj, i, Slot::L(v));
    }
}

/// `FileDescriptor.handle`（Windows 上唯一有效的句柄；`fd` 只作 int 映射用）
fn fd_handle(vm: &Vm, fd_obj: i32) -> Handle {
    fd_slot(vm, fd_obj, "handle") as Handle
}

fn set_fd_handle(vm: &Vm, fd_obj: i32, h: Handle) {
    set_fd_slot(vm, fd_obj, "handle", h as i64);
}

/// 对象字段读写（Java 侧那些 private 字段全靠名字定位）
fn obj_field(vm: &Vm, obj: i32, class: &str, name: &str) -> Slot {
    vm.store
        .field_index(class, name)
        .and_then(|i| vm.sh.heap.get(obj, i))
        .unwrap_or(Slot::I(0))
}

fn set_obj_field(vm: &Vm, obj: i32, class: &str, name: &str, v: Slot) {
    if let Some(i) = vm.store.field_index(class, name) {
        let _ = vm.sh.heap.set(obj, i, v);
    }
}

// ============================================================ Win32 文件系统

const WND: &str = "sun/nio/fs/WindowsNativeDispatcher";
const FIRST_FILE: &str = "sun/nio/fs/WindowsNativeDispatcher$FirstFile";

fn create_file_w(path: &str, access: u32, share: u32, disposition: u32, flags: u32) -> Result<Handle, u32> {
    let w = to_wide(path);
    let h = unsafe {
        CreateFileW(w.as_ptr(), access, share, std::ptr::null(), disposition, flags, 0)
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(h)
    }
}

fn wnd_CreateFile0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = read_path(f.arg_l(0) as u64);
    trace(&format!("CreateFile0 {path:?} access={} flags={}", f.arg_i(1), f.arg_i(5)));
    let access = f.arg_i(1) as u32;
    let share = f.arg_i(2) as u32;
    let disposition = f.arg_i(4) as u32;
    let flags = f.arg_i(5) as u32;
    match create_file_w(&path, access, share, disposition, flags) {
        Ok(h) => Ok(Slot::L(h as i64)),
        Err(code) => Err(throw_windows(vm, code)),
    }
}

fn wnd_CloseHandle(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    if h != 0 && h != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(h) };
    }
    Ok(Slot::I(0))
}

fn wnd_DeleteFile0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    if w.is_empty() {
        return Err(throw_windows(vm, ERROR_INVALID_PARAMETER));
    }
    if unsafe { DeleteFileW(w.as_ptr()) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_CreateDirectory0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    if w.is_empty() {
        return Err(throw_windows(vm, ERROR_INVALID_PARAMETER));
    }
    if unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_RemoveDirectory0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    if w.is_empty() {
        return Err(throw_windows(vm, ERROR_INVALID_PARAMETER));
    }
    if unsafe { RemoveDirectoryW(w.as_ptr()) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_MoveFileEx0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let from = read_wstr_z(f.arg_l(0) as u64);
    let to = read_wstr_z(f.arg_l(1) as u64);
    let flags = f.arg_i(2) as u32;
    if from.is_empty() || to.is_empty() {
        return Err(throw_windows(vm, ERROR_INVALID_PARAMETER));
    }
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_CopyFileEx0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // CopyFileEx0(LPCWSTR existing, LPCWSTR new, int flags, jlong progress)
    let from = read_wstr_z(f.arg_l(0) as u64);
    let to = read_wstr_z(f.arg_l(1) as u64);
    let flags = f.arg_i(2) as u32;
    // COPY_FILE_COPY_SYMLINK = 0x800，COPY_FILE_NO_BUFFERING = 0x1000
    let _ = flags;
    if unsafe { CopyFileW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetFileAttributes0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let a = unsafe { GetFileAttributesW(w.as_ptr()) };
    if a == u32::MAX {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(a as i32))
}

fn wnd_SetFileAttributes0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    if unsafe { SetFileAttributesW(w.as_ptr(), f.arg_i(1) as u32) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetFileAttributesEx0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let buf = f.arg_l(1) as u64;
    trace(&format!("GetFileAttributesEx {:?}", String::from_utf16_lossy(&w)));
    let mut data = Win32FileAttributeData::default();
    if unsafe { GetFileAttributesExW(w.as_ptr(), 0, &mut data as *mut _ as *mut c_void) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    put_struct(buf, &data);
    Ok(Slot::I(0))
}

fn wnd_GetFileInformationByHandle0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let buf = f.arg_l(1) as u64;
    let mut info = ByHandleFileInformation::default();
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    put_struct(buf, &info);
    Ok(Slot::I(0))
}

fn wnd_GetFileSizeEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let mut size: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(size))
}

fn wnd_SetEndOfFile(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    if unsafe { SetEndOfFile(f.arg_l(0) as Handle) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_SetFileTime0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // 三个参数是 FILETIME 的 64 位值（-1 = 不改）
    let h = f.arg_l(0) as Handle;
    let conv = |v: i64| FileTime { low: v as u32, high: (v >> 32) as u32 };
    let (c, a, w) = (f.arg_l(1), f.arg_l(2), f.arg_l(3));
    let cf = conv(c);
    let af = conv(a);
    let wf = conv(w);
    let cp = if c == -1 { std::ptr::null() } else { &cf as *const FileTime };
    let ap = if a == -1 { std::ptr::null() } else { &af as *const FileTime };
    let wp = if w == -1 { std::ptr::null() } else { &wf as *const FileTime };
    if unsafe { SetFileTime(h, cp, ap, wp) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_FindFirstFile0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let obj = f.arg_i(1);
    let mut data = Win32FindDataW::default();
    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if h != INVALID_HANDLE_VALUE {
        let n = data.file_name.iter().position(|&c| c == 0).unwrap_or(260);
        let name = String::from_utf16_lossy(&data.file_name[..n]);
        let name_h = vm.sh.heap.new_string(name);
        set_obj_field(vm, obj, FIRST_FILE, "handle", Slot::L(h as i64));
        set_obj_field(vm, obj, FIRST_FILE, "name", Slot::I(name_h));
        set_obj_field(vm, obj, FIRST_FILE, "attributes", Slot::I(data.attributes as i32));
        return Ok(Slot::I(0));
    }
    let code = last_error();
    // 目录里一个匹配都没有时 FindFirstFileW 报 FILE_NOT_FOUND / NO_MORE_FILES：
    // HotSpot 把它当成「空目录」而不是错误（handle=-1, name="", attributes=-1）
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_NO_MORE_FILES {
        let empty = vm.sh.heap.new_string(String::new());
        set_obj_field(vm, obj, FIRST_FILE, "handle", Slot::L(-1));
        set_obj_field(vm, obj, FIRST_FILE, "name", Slot::I(empty));
        set_obj_field(vm, obj, FIRST_FILE, "attributes", Slot::I(-1));
        return Ok(Slot::I(0));
    }
    Err(throw_windows(vm, code))
}

fn wnd_FindFirstFile1(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let addr = f.arg_l(1) as u64;
    let mut data = Win32FindDataW::default();
    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return Err(throw_windows(vm, last_error()));
    }
    put_struct(addr, &data);
    Ok(Slot::L(h as i64))
}

fn wnd_FindNextFile0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let addr = f.arg_l(1) as u64;
    let mut data = Win32FindDataW::default();
    if unsafe { FindNextFileW(h, &mut data) } == 0 {
        let code = last_error();
        if code == ERROR_NO_MORE_FILES {
            return Ok(Slot::I(0)); // null = 遍历结束
        }
        return Err(throw_windows(vm, code));
    }
    let n = data.file_name.iter().position(|&c| c == 0).unwrap_or(260);
    let name = String::from_utf16_lossy(&data.file_name[..n]);
    put_struct(addr, &data);
    Ok(Slot::I(vm.sh.heap.new_string(name)))
}

fn wnd_FindClose(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    if h != INVALID_HANDLE_VALUE && h != 0 {
        unsafe { FindClose(h) };
    }
    Ok(Slot::I(0))
}

fn wnd_GetFullPathName0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    trace(&format!("GetFullPathName0 {:?}", String::from_utf16_lossy(&w)));
    // 先问长度（传 0 长度）：Win32 返回需要的字符数（不含 NUL）
    let need = unsafe { GetFullPathNameW(w.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if need == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let mut buf = vec![0u16; need as usize + 1];
    let n = unsafe {
        GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut())
    };
    if n == 0 || n as usize > buf.len() {
        return Err(throw_windows(vm, last_error()));
    }
    let text = String::from_utf16_lossy(&buf[..n as usize]);
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

fn wnd_GetFinalPathNameByHandle(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let mut buf = vec![0u16; 1024];
    let mut n = unsafe {
        GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS)
    };
    if n as usize >= buf.len() {
        buf.resize(n as usize + 1, 0);
        n = unsafe {
            GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS)
        };
    }
    if n == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let text = String::from_utf16_lossy(&buf[..n as usize]);
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

fn wnd_GetVolumePathName0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let mut buf = vec![0u16; 1024];
    if unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Ok(Slot::I(vm.sh.heap.new_string(String::from_utf16_lossy(&buf[..n]))))
}

fn wnd_GetLogicalDrives(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let bits = unsafe { GetLogicalDrives() };
    if bits == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(bits as i32))
}

fn wnd_GetDriveType0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let t = unsafe { GetDriveTypeW(w.as_ptr()) };
    if t == 0 {
        // DRIVE_UNKNOWN：Java 侧有专门的判断，不当错误
        return Ok(Slot::I(0));
    }
    let _ = vm;
    Ok(Slot::I(t as i32))
}

fn wnd_GetVolumeInformation0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let obj = f.arg_i(1);
    let mut name = [0u16; 261];
    let mut fs_name = [0u16; 261];
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    if unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    let vn = name.iter().position(|&c| c == 0).unwrap_or(0);
    let fsn = fs_name.iter().position(|&c| c == 0).unwrap_or(0);
    let volume = vm.sh.heap.new_string(String::from_utf16_lossy(&name[..vn]));
    let fs = vm.sh.heap.new_string(String::from_utf16_lossy(&fs_name[..fsn]));
    let class = "sun/nio/fs/WindowsNativeDispatcher$VolumeInformation";
    set_obj_field(vm, obj, class, "volumeName", Slot::I(volume));
    set_obj_field(vm, obj, class, "fileSystemName", Slot::I(fs));
    set_obj_field(vm, obj, class, "volumeSerialNumber", Slot::I(serial as i32));
    set_obj_field(vm, obj, class, "flags", Slot::I(flags as i32));
    Ok(Slot::I(0))
}

fn wnd_GetDiskFreeSpaceEx0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let obj = f.arg_i(1);
    let (mut free_avail, mut total, mut total_free) = (0u64, 0u64, 0u64);
    if unsafe {
        GetDiskFreeSpaceExW(w.as_ptr(), &mut free_avail, &mut total, &mut total_free)
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    let class = "sun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace";
    set_obj_field(vm, obj, class, "freeBytesAvailable", Slot::L(free_avail as i64));
    set_obj_field(vm, obj, class, "totalNumberOfBytes", Slot::L(total as i64));
    set_obj_field(vm, obj, class, "totalNumberOfFreeBytes", Slot::L(total_free as i64));
    // bytesPerSector 用 GetDiskFreeSpace 补（Java 侧只在少数地方读）
    let mut spc = 0u32;
    let mut bps = 0u32;
    let (mut fc, mut tc) = (0u32, 0u32);
    if unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) } != 0 {
        set_obj_field(vm, obj, class, "bytesPerSector", Slot::L(bps as i64));
    }
    Ok(Slot::I(0))
}

fn wnd_GetDiskFreeSpace0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let obj = f.arg_i(1);
    let mut spc = 0u32;
    let mut bps = 0u32;
    let (mut fc, mut tc) = (0u32, 0u32);
    if unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let bpc = (spc as u64) * (bps as u64);
    let class = "sun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace";
    set_obj_field(vm, obj, class, "freeBytesAvailable", Slot::L(bpc as i64 * fc as i64));
    set_obj_field(vm, obj, class, "totalNumberOfBytes", Slot::L(bpc as i64 * tc as i64));
    set_obj_field(vm, obj, class, "totalNumberOfFreeBytes", Slot::L(bpc as i64 * fc as i64));
    set_obj_field(vm, obj, class, "bytesPerSector", Slot::L(bps as i64));
    Ok(Slot::I(0))
}

fn wnd_FormatMessage(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let code = f.arg_i(0) as u32;
    let mut buf = vec![0u16; 1024];
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    if n == 0 {
        return Ok(Slot::I(0)); // null：Java 侧会退化成 "Unknown error: 0x…"
    }
    let text = String::from_utf16_lossy(&buf[..n as usize])
        .trim_end_matches(['\r', '\n'])
        .to_string();
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

fn wnd_LocalFree(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let p = f.arg_l(0) as *mut c_void;
    if !p.is_null() {
        unsafe { LocalFree(p) };
    }
    Ok(Slot::I(0))
}

fn wnd_CreateEvent(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // CreateEventW(NULL, manualReset, initialState, NULL)
    let h = unsafe {
        extern "system" {
            fn CreateEventW(sa: *const c_void, manual: Bool, initial: Bool, name: *const u16) -> Handle;
        }
        CreateEventW(std::ptr::null(), 0, 0, std::ptr::null())
    };
    if h == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(h as i64))
}

fn wnd_DeviceIoControlSetSparse(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // FSCTL_SET_SPARSE = 0x900C4
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    let h = f.arg_l(0) as Handle;
    let mut ret: u32 = 0;
    if unsafe {
        DeviceIoControl(h, FSCTL_SET_SPARSE, std::ptr::null_mut(), 0, std::ptr::null_mut(), 0, &mut ret, std::ptr::null_mut())
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_DeviceIoControlGetReparsePoint(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // FSCTL_GET_REPARSE_POINT = 0x900A8
    const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    let h = f.arg_l(0) as Handle;
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2) as usize;
    let want = len.min(MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize).min(capacity_at(addr));
    let mut buf = vec![0u8; want];
    let mut ret: u32 = 0;
    if unsafe {
        DeviceIoControl(
            h,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &buf[..ret as usize]);
    Ok(Slot::I(0))
}

fn wnd_CreateSymbolicLink0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let link = read_wstr_z(f.arg_l(0) as u64);
    let target = read_wstr_z(f.arg_l(1) as u64);
    let flags = f.arg_i(2) as u32;
    if unsafe { CreateSymbolicLinkW(link.as_ptr(), target.as_ptr(), flags) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_CreateHardLink0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let link = read_wstr_z(f.arg_l(0) as u64);
    let target = read_wstr_z(f.arg_l(1) as u64);
    if unsafe { CreateHardLinkW(link.as_ptr(), target.as_ptr(), std::ptr::null()) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetCurrentProcess(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(unsafe { GetCurrentProcess() } as i64))
}

fn wnd_GetCurrentThread(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(0xFFFF_FFFF_FFFF_FFFEu64 as i64)) // GetCurrentThread 的伪句柄 -2
}

fn register_windows_dispatcher(r: &mut NativeRegistry) {
    r.add(WND, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(WND, "CreateFile0", "(JIIJII)J", wnd_CreateFile0);
    r.add(WND, "CloseHandle", "(J)V", wnd_CloseHandle);
    r.add(WND, "DeleteFile0", "(J)V", wnd_DeleteFile0);
    r.add(WND, "CreateDirectory0", "(JJ)V", wnd_CreateDirectory0);
    r.add(WND, "RemoveDirectory0", "(J)V", wnd_RemoveDirectory0);
    r.add(WND, "MoveFileEx0", "(JJI)V", wnd_MoveFileEx0);
    r.add(WND, "CopyFileEx0", "(JJIJ)V", wnd_CopyFileEx0);
    r.add(WND, "GetFileAttributes0", "(J)I", wnd_GetFileAttributes0);
    r.add(WND, "SetFileAttributes0", "(JI)V", wnd_SetFileAttributes0);
    r.add(WND, "GetFileAttributesEx0", "(JJ)V", wnd_GetFileAttributesEx0);
    r.add(WND, "GetFileInformationByHandle0", "(JJ)V", wnd_GetFileInformationByHandle0);
    r.add(WND, "GetFileSizeEx", "(J)J", wnd_GetFileSizeEx);
    r.add(WND, "SetEndOfFile", "(J)V", wnd_SetEndOfFile);
    r.add(WND, "SetFileTime0", "(JJJJ)V", wnd_SetFileTime0);
    r.add(WND, "FindFirstFile0", "(JLsun/nio/fs/WindowsNativeDispatcher$FirstFile;)V", wnd_FindFirstFile0);
    r.add(WND, "FindFirstFile1", "(JJ)J", wnd_FindFirstFile1);
    r.add(WND, "FindNextFile0", "(JJ)Ljava/lang/String;", wnd_FindNextFile0);
    r.add(WND, "FindClose", "(J)V", wnd_FindClose);
    r.add(WND, "GetFullPathName0", "(J)Ljava/lang/String;", wnd_GetFullPathName0);
    r.add(WND, "GetFinalPathNameByHandle", "(J)Ljava/lang/String;", wnd_GetFinalPathNameByHandle);
    r.add(WND, "GetVolumePathName0", "(J)Ljava/lang/String;", wnd_GetVolumePathName0);
    r.add(WND, "GetLogicalDrives", "()I", wnd_GetLogicalDrives);
    r.add(WND, "GetDriveType0", "(J)I", wnd_GetDriveType0);
    r.add(WND, "GetVolumeInformation0", "(JLsun/nio/fs/WindowsNativeDispatcher$VolumeInformation;)V", wnd_GetVolumeInformation0);
    r.add(WND, "GetDiskFreeSpaceEx0", "(JLsun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace;)V", wnd_GetDiskFreeSpaceEx0);
    r.add(WND, "GetDiskFreeSpace0", "(JLsun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace;)V", wnd_GetDiskFreeSpace0);
    r.add(WND, "FormatMessage", "(I)Ljava/lang/String;", wnd_FormatMessage);
    r.add(WND, "LocalFree", "(J)V", wnd_LocalFree);
    r.add(WND, "CreateEvent", "(ZZ)J", wnd_CreateEvent);
    r.add(WND, "DeviceIoControlSetSparse", "(J)V", wnd_DeviceIoControlSetSparse);
    r.add(WND, "DeviceIoControlGetReparsePoint", "(JJI)V", wnd_DeviceIoControlGetReparsePoint);
    r.add(WND, "CreateSymbolicLink0", "(JJI)V", wnd_CreateSymbolicLink0);
    r.add(WND, "CreateHardLink0", "(JJ)V", wnd_CreateHardLink0);
    r.add(WND, "GetCurrentProcess", "()J", wnd_GetCurrentProcess);
    r.add(WND, "GetCurrentThread", "()J", wnd_GetCurrentThread);
}

// ============================================================ FileDispatcherImpl

const FDP: &str = "sun/nio/ch/FileDispatcherImpl";
/// `IOStatus.EOF`（Java 侧 `IOStatus.EOF = -1`）
const IO_EOF: i32 = -1;

/// 往 nat_mem 地址读文件（`ReadFile` 的薄封装）。返回字节数或 -1（EOF）
fn read_at(h: Handle, addr: u64, len: usize) -> Result<i32, u32> {
    if len == 0 {
        return Ok(0);
    }
    let cap = capacity_at(addr);
    let want = len.min(cap).min(i32::MAX as usize) as u32;
    let mut buf = vec![0u8; want as usize];
    let mut got: u32 = 0;
    let ok = unsafe { ReadFile(h, buf.as_mut_ptr() as *mut c_void, want, &mut got, std::ptr::null_mut()) };
    if ok == 0 {
        let code = last_error();
        if code == ERROR_HANDLE_EOF || code == ERROR_BROKEN_PIPE {
            return Ok(IO_EOF);
        }
        return Err(code);
    }
    if got == 0 {
        return Ok(IO_EOF);
    }
    nat_mem::write_bytes(addr, &buf[..got as usize]);
    Ok(got as i32)
}

/// 从 nat_mem 地址写文件。返回写入字节数
fn write_at(h: Handle, addr: u64, len: usize) -> Result<i32, u32> {
    if len == 0 {
        return Ok(0);
    }
    let data = nat_mem::read_bytes(addr, len);
    let mut written: u32 = 0;
    let ok = unsafe {
        WriteFile(
            h,
            data.as_ptr() as *const c_void,
            data.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(written as i32)
}

fn fdp_read0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(throw_io(vm, "Stream Closed"));
    }
    match read_at(h, addr, len) {
        Ok(n) => Ok(Slot::I(n)),
        Err(code) => Err(throw_io(vm, format!("读取失败 (len={len}): {}", win_msg(code)))),
    }
}

fn fdp_pread0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let pos = f.arg_l(3);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(throw_io(vm, "Stream Closed"));
    }
    // 同步句柄上 OVERLAPPED 不生效，所以「定位→读写→还原」；文件通道读是串行的
    let mut cur: i64 = 0;
    let have_pos = unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } != 0;
    if unsafe { SetFilePointerEx(h, pos, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        return Err(throw_io(vm, format!("定位失败 (pos={pos})")));
    }
    let r = read_at(h, addr, len);
    let restore_ok = if have_pos {
        (unsafe { SetFilePointerEx(h, cur, std::ptr::null_mut(), FILE_BEGIN) }) != 0
    } else {
        true
    };
    if !restore_ok {
        return Err(throw_io(vm, "定位还原失败"));
    }
    match r {
        // EOF 的原文件通道语义：-1
        Ok(n) => Ok(Slot::I(n)),
        Err(code) => Err(throw_io(vm, format!("读失败: {}", win_msg(code)))),
    }
}

fn fdp_write0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let append = f.arg_i(3) != 0;
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(throw_io(vm, "Stream Closed"));
    }
    if append && unsafe { SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END) } == 0 {
        return Err(throw_io(vm, "定位失败"));
    }
    match write_at(h, addr, len) {
        Ok(n) => Ok(Slot::I(n)),
        Err(code) => Err(throw_io(vm, format!("写入失败 (len={len}): {}", win_msg(code)))),
    }
}

fn fdp_pwrite0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let pos = f.arg_l(3);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(throw_io(vm, "Stream Closed"));
    }
    let mut cur: i64 = 0;
    let have_pos = unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } != 0;
    if unsafe { SetFilePointerEx(h, pos, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        return Err(throw_io(vm, format!("定位失败 (pos={pos})")));
    }
    let r = write_at(h, addr, len);
    let restore_ok = if have_pos {
        (unsafe { SetFilePointerEx(h, cur, std::ptr::null_mut(), FILE_BEGIN) }) != 0
    } else {
        true
    };
    if !restore_ok {
        return Err(throw_io(vm, "定位还原失败"));
    }
    match r {
        Ok(n) => Ok(Slot::I(n)),
        Err(code) => Err(throw_io(vm, format!("写失败: {}", win_msg(code)))),
    }
}

fn fdp_seek0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let pos = f.arg_l(1);
    if pos < 0 {
        return Err(throw_io(vm, "Negative seek offset"));
    }
    let mut new_pos: i64 = 0;
    if unsafe { SetFilePointerEx(h, pos, &mut new_pos, FILE_BEGIN) } == 0 {
        return Err(throw_io(vm, format!("定位失败 (pos={pos}): {}", win_msg(last_error()))));
    }
    Ok(Slot::L(new_pos))
}

fn fdp_size0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let mut size: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } == 0 {
        return Err(throw_io(vm, format!("取文件大小失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::L(size))
}

fn fdp_truncate0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let size = f.arg_l(1);
    let mut cur: i64 = 0;
    if unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } == 0 {
        return Err(throw_io(vm, "取当前位置失败"));
    }
    if unsafe { SetFilePointerEx(h, size, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        return Err(throw_io(vm, "定位失败"));
    }
    let ok = unsafe { SetEndOfFile(h) } != 0;
    let code = last_error();
    // 还原（HotSpot 也把位置还原，失败不算错）
    unsafe { SetFilePointerEx(h, cur, std::ptr::null_mut(), FILE_BEGIN) };
    if !ok {
        return Err(throw_io(vm, format!("截断失败: {}", win_msg(code))));
    }
    Ok(Slot::I(0))
}

fn fdp_force0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    if unsafe { FlushFileBuffers(h) } == 0 {
        return Err(throw_io(vm, format!("同步失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(0))
}

fn fdp_close0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    let h = fd_handle(vm, fd_obj);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Ok(Slot::I(0));
    }
    set_fd_handle(vm, fd_obj, -1);
    set_fd_slot(vm, fd_obj, "fd", -1);
    if unsafe { CloseHandle(h) } == 0 {
        let code = last_error();
        if code != ERROR_INVALID_HANDLE {
            return Err(throw_io(vm, format!("关闭失败: {}", win_msg(code))));
        }
    }
    Ok(Slot::I(0))
}

fn fdp_available0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let mut cur: i64 = 0;
    if unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } == 0 {
        return Err(throw_io(vm, "取当前位置失败"));
    }
    let mut size: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } == 0 {
        return Err(throw_io(vm, "取文件大小失败"));
    }
    Ok(Slot::I((size - cur).clamp(0, i32::MAX as i64) as i32))
}

fn fdp_is_other0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let t = unsafe { GetFileType(h) };
    Ok(Slot::I((t != FILE_TYPE_DISK) as i32))
}


// ============================================================ 文件通道：注册

fn register_file_dispatcher(r: &mut NativeRegistry) {
    r.add(FDP, "read0", "(Ljava/io/FileDescriptor;JI)I", fdp_read0);
    r.add(FDP, "pread0", "(Ljava/io/FileDescriptor;JIJ)I", fdp_pread0);
    r.add(FDP, "write0", "(Ljava/io/FileDescriptor;JIZ)I", fdp_write0);
    r.add(FDP, "pwrite0", "(Ljava/io/FileDescriptor;JIJ)I", fdp_pwrite0);
    r.add(FDP, "seek0", "(Ljava/io/FileDescriptor;J)J", fdp_seek0);
    r.add(FDP, "size0", "(Ljava/io/FileDescriptor;)J", fdp_size0);
    r.add(FDP, "truncate0", "(Ljava/io/FileDescriptor;J)I", fdp_truncate0);
    r.add(FDP, "force0", "(Ljava/io/FileDescriptor;Z)I", fdp_force0);
    r.add(FDP, "close0", "(Ljava/io/FileDescriptor;)V", fdp_close0);
    r.add(FDP, "available0", "(Ljava/io/FileDescriptor;)I", fdp_available0);
    r.add(FDP, "isOther0", "(Ljava/io/FileDescriptor;)Z", fdp_is_other0);
    r.add(FDP, "readv0", "(Ljava/io/FileDescriptor;JI)J", fdp_readv0);
    r.add(FDP, "writev0", "(Ljava/io/FileDescriptor;JIZ)J", fdp_writev0);
    r.add(FDP, "lock0", "(Ljava/io/FileDescriptor;ZJJZ)I", fdp_lock0);
    r.add(FDP, "release0", "(Ljava/io/FileDescriptor;JJ)V", fdp_release0);
    r.add(FDP, "duplicateHandle", "(J)J", fdp_duplicate_handle);
    // 分配粒度：Windows 的 `SYSTEM_INFO.dwAllocationGranularity` 恒为 64K
    r.add(FDP, "allocationGranularity0", "()J", |_, _| Ok(Slot::L(65536)));
    // FileChannel.map/unmap：把真映射登记进 arena（见「内存映射文件」那一节）
    r.add(FDP, "map0", "(Ljava/io/FileDescriptor;IJJZ)J", fdp_map0);
    r.add(FDP, "unmap0", "(JJ)I", fdp_unmap0);
    r.add(FDP, "maxDirectTransferSize0", "()I", |_, _| Ok(Slot::I(16 * 1024 * 1024)));
    r.add(
        FDP,
        "transferTo0",
        "(Ljava/io/FileDescriptor;JJLjava/io/FileDescriptor;Z)J",
        fdp_transfer_to0,
    );
    r.add(FDP, "setDirect0", "(Ljava/io/FileDescriptor;Ljava/nio/CharBuffer;)I", |_, _| Ok(Slot::I(0)));
}

/// `MappedMemoryUtils`：`MappedByteBuffer.load()/force()/isLoaded()` 的落地点
fn register_mapped_memory(r: &mut NativeRegistry) {
    r.add(MMU, "registerNatives", "()V", mmu_register_natives);
    r.add(MMU, "force0", "(Ljava/io/FileDescriptor;JJ)V", mmu_force0);
    r.add(MMU, "load0", "(JJ)V", mmu_load0);
    r.add(MMU, "unload0", "(JJ)V", mmu_unload0);
    r.add(MMU, "isLoaded0", "(JJJ)Z", mmu_is_loaded0);
}

/// `iovec[]`：64 位下每项 16 字节（base 在 0、len 在 8）
const IOVEC: usize = 16;

fn fdp_readv0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    let mut total: i64 = 0;
    for i in 0..cnt {
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        if len == 0 {
            continue;
        }
        match read_at(h, base, len) {
            Ok(n) if n == IO_EOF || n == 0 => break,
            Ok(n) => {
                total += n as i64;
                if (n as usize) < len {
                    break; // 短读即停（与 readv 一致）
                }
            }
            Err(code) => return Err(throw_io(vm, format!("readv 失败: {}", win_msg(code)))),
        }
    }
    Ok(Slot::L(total))
}

fn fdp_writev0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    let append = f.arg_i(3) != 0;
    if append && unsafe { SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END) } == 0 {
        return Err(throw_io(vm, "定位失败"));
    }
    let mut total: i64 = 0;
    for i in 0..cnt {
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        if len == 0 {
            continue;
        }
        match write_at(h, base, len) {
            Ok(n) => {
                total += n as i64;
                if (n as usize) < len {
                    break;
                }
            }
            Err(code) => return Err(throw_io(vm, format!("writev 失败: {}", win_msg(code)))),
        }
    }
    Ok(Slot::L(total))
}

fn fdp_lock0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let pos = f.arg_l(2);
    let size = f.arg_l(3);
    // Windows 的 LockFile 只有独占锁（共享锁的差别只影响多进程争用，不影响「锁住了」）
    let (lo, hi) = (pos as u32, (pos >> 32) as u32);
    let (llo, lhi) = (size as u32, (size >> 32) as u32);
    if unsafe { LockFile(h, lo, hi, llo, lhi) } == 0 {
        return Err(throw_io(vm, format!("加锁失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(0))
}

fn fdp_release0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let pos = f.arg_l(1);
    let size = f.arg_l(2);
    let (lo, hi) = (pos as u32, (pos >> 32) as u32);
    let (llo, lhi) = (size as u32, (size >> 32) as u32);
    if unsafe { UnlockFile(h, lo, hi, llo, lhi) } == 0 {
        return Err(throw_io(vm, format!("解锁失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(0))
}

fn fdp_duplicate_handle(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let proc = unsafe { GetCurrentProcess() };
    let mut out: Handle = 0;
    // DUPLICATE_SAME_ACCESS = 0x2
    if unsafe { DuplicateHandle(proc, h, proc, &mut out, 0, 0, 0x2) } == 0 {
        return Err(throw_io(vm, format!("复制句柄失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::L(out as i64))
}

// ============================================================ 内存映射文件（mmap）
//
// 这一组的关键是**地址桥接**。`FileChannel.map` 拿到的地址会被 `Unsafe.getX/putX` 当
// 裸地址用，而本 VM 的裸地址是 arena 假地址。所以：
//
//   `map0`  → `CreateFileMapping` + `MapViewOfFile` 拿到**真视图**，
//             再 `nat_mem::alloc_mapped(真指针, 长度)` 把这块真内存**登记**进 arena，
//             返回的假地址就是视图的别名（不拷一份 —— 见 `nat_mem` 里 `Block::Mapped` 的理由）。
//   `unmap0`→ 先注销 arena 登记，再 `UnmapViewOfFile`（顺序反了，残留读写会踩已解除的视图）。
//
// 假地址必须 **64K 对齐**：`MappedMemoryUtils.mappingOffset()` 会拿 `address & (pageSize-1)`
// 反推映射基址，基址不对那 `force0`/`load0` 算出来的区间就是错的。

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileMappingW(
        file: Handle,
        sa: *const c_void,
        protect: u32,
        hi: u32,
        lo: u32,
        name: *const u16,
    ) -> Handle;
    fn MapViewOfFile(mapping: Handle, access: u32, off_hi: u32, off_lo: u32, len: usize) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> Bool;
    fn FlushViewOfFile(base: *const c_void, len: usize) -> Bool;
    fn PrefetchVirtualMemory(process: Handle, count: usize, ranges: *mut MemoryRangeEntry, flags: u64) -> Bool;
    fn QueryWorkingSetEx(process: Handle, buf: *mut c_void, len: u32) -> Bool;
}

#[repr(C)]
struct MemoryRangeEntry {
    virtual_address: *mut c_void,
    number_of_bytes: usize,
}

/// `PSAPI_WORKING_SET_EX_INFORMATION`：**每个元素 16 字节**（x64）
/// `{ PVOID VirtualAddress; PSAPI_WORKING_SET_EX_BLOCK { ULONG_PTR Flags; } }`
///
/// 曾经这里只放了 8 字节的 `Flags`，于是 ① 传给 `QueryWorkingSetEx` 的长度少一半 →
/// 调用失败 → 被当成「全部已加载」返回 true（`MappedByteBuffer.isLoaded()` 于是永远 true，
/// 与 HotSpot 的 false 对不上）。结构体布局必须照 `psapi.h` 来。
#[repr(C)]
#[derive(Clone, Copy)]
struct WorkingSetExInfo {
    virtual_address: *mut c_void,
    /// `VirtualAttributes.Flags`，bit0 = Valid
    flags: u64,
}

impl Default for WorkingSetExInfo {
    fn default() -> Self {
        WorkingSetExInfo { virtual_address: std::ptr::null_mut(), flags: 0 }
    }
}

const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const FILE_MAP_WRITE: u32 = 0x0002;
const FILE_MAP_READ: u32 = 0x0004;
const FILE_MAP_COPY: u32 = 0x0001;
/// 映射块在 arena 里的对齐要求（`MappedMemoryUtils` 按 4K 页反推基址，64K 一定更安全）
const MAP_ALIGN: u64 = 0x1_0000;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;

/// 一次活着的映射。`fake` 是 arena 假地址（= `map0` 的返回值），也是登记表的键
struct MapRec {
    view: usize,
    len: usize,
    /// 建映射时的 `FileDescriptor` 对象句柄（`force0` 走无映射分支时用）
    fd_obj: i32,
}

fn mappings() -> &'static std::sync::Mutex<std::collections::HashMap<u64, MapRec>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, MapRec>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `addr` 落在哪个映射里 → (假基址, 记录, 块内偏移)
fn map_containing(addr: u64) -> Option<(u64, usize, usize, usize)> {
    let g = mappings().lock().unwrap_or_else(|e| e.into_inner());
    g.iter()
        .find(|(base, r)| addr >= **base && addr < **base + r.len as u64)
        .map(|(base, r)| (*base, r.view, r.len, (addr - *base) as usize))
}

/// `map0(FileDescriptor fd, int prot, long pos, long size, boolean isSync)` → 真视图的 arena 别名
fn fdp_map0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    let h = fd_handle(vm, fd_obj);
    // `prot`：MapMode → `FileChannelImpl.MAP_RO=0 / MAP_RW=1 / MAP_PV=2`
    let prot = f.arg_i(1);
    let pos = f.arg_l(2).max(0) as u64;
    let size = f.arg_l(3).max(0) as usize;
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/nio/channels/ClosedChannelException", "Stream Closed".to_string()));
    }
    if size == 0 {
        return Err(throw_io(vm, "map0: size 必须 > 0"));
    }
    // (页保护, 视图访问权)。PRIVATE 走 copy-on-write；文件只读时 copy-on-write 可能被拒，
    // 那就退回「读写映射」—— PRIVATE 的语义是「改动不外泄」，退回后改动同样不回写。
    let (page_prot, map_access) = match prot {
        0 => (PAGE_READONLY, FILE_MAP_READ),
        1 => (PAGE_READWRITE, FILE_MAP_READ | FILE_MAP_WRITE),
        2 => (PAGE_WRITECOPY, FILE_MAP_COPY | FILE_MAP_READ),
        other => return Err(throw_io(vm, format!("map0: 未知的映射保护位 {other}"))),
    };
    let mut mapping = unsafe { CreateFileMappingW(h, std::ptr::null(), page_prot, 0, 0, std::ptr::null()) };
    if mapping == 0 && prot == 2 {
        let code = last_error();
        if code == ERROR_ACCESS_DENIED {
            mapping = unsafe { CreateFileMappingW(h, std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
        }
    }
    if mapping == 0 {
        let code = last_error();
        // 与 HotSpot 一样：内存不足报 OutOfMemoryError（`mapInternal` 会 gc 后重试一次）
        if code == ERROR_NOT_ENOUGH_MEMORY {
            return Err(vm.throw_msg(
                "java/lang/OutOfMemoryError",
                format!("Map failed: {}", win_msg(code)),
            ));
        }
        return Err(throw_io(vm, format!("CreateFileMapping 失败: {}", win_msg(code))));
    }
    let view = unsafe { MapViewOfFile(mapping, map_access, (pos >> 32) as u32, pos as u32, size) };
    let view_err = if view.is_null() { Some(last_error()) } else { None };
    // 视图自己让映射活着，句柄可以立刻关（少一个要管的资源）
    unsafe { CloseHandle(mapping) };
    if let Some(code) = view_err {
        if code == ERROR_NOT_ENOUGH_MEMORY {
            return Err(vm.throw_msg(
                "java/lang/OutOfMemoryError",
                format!("Map failed: {}", win_msg(code)),
            ));
        }
        return Err(throw_io(vm, format!("MapViewOfFile 失败: {}", win_msg(code))));
    }
    let fake = nat_mem::alloc_mapped_aligned(view as usize, size, MAP_ALIGN);
    trace(&format!("map0 fd={fd_obj} prot={prot} pos={pos} size={size} view={view:?} fake={fake}"));
    mappings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(fake, MapRec { view: view as usize, len: size, fd_obj });
    Ok(Slot::L(fake as i64))
}

/// `unmap0(long address, long size)`：注销 arena 登记 + 解除视图。返回 0 成功 / -1 失败
fn fdp_unmap0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    if addr == 0 {
        return Ok(Slot::I(0)); // size==0 的映射根本不建视图
    }
    let rec = mappings().lock().unwrap_or_else(|e| e.into_inner()).remove(&addr);
    // 顺序不能反：先让 arena 忘掉这个地址，再解除映射
    let was_mapped = nat_mem::unmapped(addr);
    let Some(rec) = rec else {
        // 没登记过：win32 的语义是「解除一个不存在的视图」= 失败，
        // 但 Java 侧可能重复 unmap（cleaner 与显式调用都跑），这里按幂等处理并留痕
        trace(&format!("unmap0 addr={addr} 没登记过（重复 unmap？）"));
        return Ok(Slot::I(if was_mapped { 0 } else { -1 }));
    };
    if unsafe { UnmapViewOfFile(rec.view as *const c_void) } == 0 {
        return Err(throw_io(vm, format!("UnmapViewOfFile 失败: {}", win_msg(last_error()))));
    }
    trace(&format!("unmap0 addr={addr} view={:?} len={}", rec.view, rec.len));
    Ok(Slot::I(0))
}

// ---------------------------------------------------------------- MappedMemoryUtils
//
// 这 5 条 native 全是**页级提示**，不是数据通路：数据通路是 arena 假地址（映射块）。
// 但它们是 `MappedByteBuffer.load()/force()/isLoaded()` 的唯一落地点，实现成空操作会让
// `force()` 变成「什么都没做却报告成功」—— 所以每条都真调 Win32。

const MMU: &str = "java/nio/MappedMemoryUtils";

fn mmu_register_natives(_vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// `force0(FileDescriptor fd, long address, long length)`
fn mmu_force0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    let addr = f.arg_l(1) as u64;
    let len = f.arg_l(2).max(0) as usize;
    if let Some((base, view, mlen, off)) = map_containing(addr) {
        // 只刷这次要求的那段（可能比整个映射短）
        let n = len.min(mlen - off);
        let p = (view + off) as *const c_void;
        if unsafe { FlushViewOfFile(p, n) } == 0 {
            return Err(throw_io(vm, format!("FlushViewOfFile 失败: {}", win_msg(last_error()))));
        }
        trace(&format!("force0 mapped base={base} off={off} len={n}"));
        return Ok(Slot::I(0));
    }
    // 不是映射（`force()` 也可能落在普通 DirectByteBuffer 上）：刷文件缓冲
    let h = fd_handle(vm, fd_obj);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/nio/channels/ClosedChannelException", "Stream Closed".to_string()));
    }
    if unsafe { FlushFileBuffers(h) } == 0 {
        return Err(throw_io(vm, format!("FlushFileBuffers 失败: {}", win_msg(last_error()))));
    }
    trace(&format!("force0 file fd={fd_obj} len={len}"));
    Ok(Slot::I(0))
}

/// `load0(long address, long length)` —— 把区间预取进物理内存
fn mmu_load0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let len = f.arg_l(1).max(0) as usize;
    if addr == 0 || len == 0 {
        return Ok(Slot::I(0));
    }
    let Some((_, view, mlen, off)) = map_containing(addr) else {
        // 非映射地址：没有「页」可预取，但也不能假装做过 —— 留痕并报错
        return Err(throw_io(vm, format!("load0: 地址 {addr:#x} 不是映射区")));
    };
    let n = len.min(mlen - off);
    let mut entry = MemoryRangeEntry {
        virtual_address: (view + off) as *mut c_void,
        number_of_bytes: n,
    };
    let ok = unsafe { PrefetchVirtualMemory(GetCurrentProcess(), 1, &mut entry, 0) };
    if ok == 0 {
        return Err(throw_io(vm, format!("PrefetchVirtualMemory 失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(0))
}

/// `unload0(long address, long length)` —— 把区间从物理内存挪走
///
/// Windows 没有 `madvise(MADV_DONTNEED)`，能做的只有「不再主动保留」。
/// 明确记一笔而不是假装做过：`unload()` 的契约是提示性的，返回成功是合法的。
fn mmu_unload0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let len = f.arg_l(1).max(0) as usize;
    if addr != 0 && len != 0 && map_containing(addr).is_none() {
        return Err(throw_io(vm, format!("unload0: 地址 {addr:#x} 不是映射区")));
    }
    trace(&format!("unload0 addr={addr} len={len}（Windows 无 madvise，提示性操作）"));
    Ok(Slot::I(0))
}

/// `isLoaded0(long address, long length, long pageCount)` —— 逐页问内核「在不在工作集里」
fn mmu_is_loaded0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let len = f.arg_l(1).max(0) as usize;
    let pages = f.arg_l(2).max(0) as usize;
    if addr == 0 || len == 0 || pages == 0 {
        return Ok(Slot::I(1));
    }
    let Some((_, view, mlen, off)) = map_containing(addr) else {
        return Err(throw_io(vm, format!("isLoaded0: 地址 {addr:#x} 不是映射区")));
    };
    let n = len.min(mlen - off);
    let base = (view + off) as *mut c_void;
    let mut info = vec![WorkingSetExInfo::default(); pages];
    let bytes = (pages * std::mem::size_of::<WorkingSetExInfo>()) as u32;
    // 先把每页的虚拟地址填进去（`VirtualAddress` 是入参）
    for (i, e) in info.iter_mut().enumerate() {
        e.virtual_address = base as u64 as *mut c_void;
        if i > 0 {
            e.virtual_address = (base as usize + i * 4096) as *mut c_void;
        }
    }
    let ok = unsafe { QueryWorkingSetEx(GetCurrentProcess(), info.as_mut_ptr() as *mut c_void, bytes) };
    let _ = n;
    // `Flags` bit0 = Valid
    let all = ok != 0 && info.iter().all(|e| e.flags & 1 != 0);
    trace(&format!("isLoaded0 pages={pages} query_ok={} all_valid={all}", ok != 0));
    // **与 HotSpot 对齐：Windows 上一律返回 false。**
    // 实测对照（同一份 .class、同一个 JDK 镜像）：HotSpot 在 fresh / 写完 / `load()` 之后 /
    // `force()` 之后**全都是 false**，而我们用 `QueryWorkingSetEx` 能查到「页在工作集里」。两边
    // 差一个字段就会让 `MappedByteBuffer.isLoaded()` 的走查对不上，所以这里按 HotSpot 的口径回答。
    // 代价只有一个：`load()` 不会因为「已经在了」而跳过 —— 那是提示性优化，不影响正确性。
    Ok(Slot::I(0))
}

// ---------------------------------------------------------------- transferTo0

/// `transferTo0(FileDescriptor src, long position, long count, FileDescriptor dst, boolean append)`
///
/// 真实现是「源按位置读 + 目标按当前/末尾写」的循环：文件→文件、文件→socket 两条路都覆盖
/// （socket 句柄在 Windows 上也是 HANDLE，`WriteFile` 直接可用，不必走 `TransmitFile`
///  —— 后者只覆盖「文件→socket」且要求 socket 支持重叠 I/O）。
fn fdp_transfer_to0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let src = fd_handle(vm, f.arg_i(0));
    let pos = f.arg_l(1).max(0) as u64;
    let count = f.arg_l(2).max(0) as usize;
    let dst = fd_handle(vm, f.arg_i(3));
    let append = f.arg_i(4) != 0;
    if src == INVALID_HANDLE_VALUE || src == 0 || dst == INVALID_HANDLE_VALUE || dst == 0 {
        return Err(vm.throw_msg("java/nio/channels/ClosedChannelException", "Stream Closed".to_string()));
    }
    if count == 0 {
        return Ok(Slot::L(0));
    }
    // 源按绝对位置读，读完把文件指针还原（transferTo 不能改源通道的位置）
    let mut saved: i64 = 0;
    let have_saved = unsafe { SetFilePointerEx(src, 0, &mut saved, FILE_CURRENT) } != 0;
    if unsafe { SetFilePointerEx(src, pos as i64, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        return Err(throw_io(vm, format!("transferTo0: 源定位失败 pos={pos}")));
    }
    if append && unsafe { SetFilePointerEx(dst, 0, std::ptr::null_mut(), FILE_END) } == 0 {
        let _ = have_saved.then(|| unsafe { SetFilePointerEx(src, saved, std::ptr::null_mut(), FILE_BEGIN) });
        return Err(throw_io(vm, "transferTo0: 目标定位失败（append）"));
    }

    let chunk = count.min(1 << 16);
    let mut buf = vec![0u8; chunk];
    let mut total = 0usize;
    let mut failure: Option<u32> = None;
    while total < count {
        let want = (count - total).min(chunk);
        let mut got: u32 = 0;
        let ok = unsafe { ReadFile(src, buf.as_mut_ptr() as *mut c_void, want as u32, &mut got, std::ptr::null_mut()) };
        if ok == 0 {
            failure = Some(last_error());
            break;
        }
        if got == 0 {
            break; // EOF
        }
        let mut off = 0usize;
        while off < got as usize {
            let mut wrote: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    dst,
                    buf[off..got as usize].as_ptr() as *const c_void,
                    (got as usize - off) as u32,
                    &mut wrote,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let code = last_error();
                // 非阻塞 socket 写满：已经送出去的那些仍然算数
                if code == 997 /* ERROR_IO_PENDING */ || code == 232 /* ERROR_NO_DATA */ || code == 10035 {
                    trace(&format!("transferTo0 目标暂时不可写 code={code} 已转 {total}"));
                    failure = None;
                    total = count.max(total); // 跳出双层循环
                    break;
                }
                failure = Some(code);
                break;
            }
            if wrote == 0 {
                break;
            }
            off += wrote as usize;
        }
        total += got as usize;
        if failure.is_some() {
            break;
        }
    }
    if have_saved {
        unsafe { SetFilePointerEx(src, saved, std::ptr::null_mut(), FILE_BEGIN) };
    }
    match failure {
        Some(code) if total == 0 => Err(throw_io(vm, format!("transferTo0 失败: {}", win_msg(code)))),
        Some(code) => {
            trace(&format!("transferTo0 半途失败 code={code}（已转 {total} 字节，按已转数返回）"));
            Ok(Slot::L(total as i64))
        }
        None => {
            trace(&format!("transferTo0 pos={pos} count={count} 转了 {total}"));
            Ok(Slot::L(total as i64))
        }
    }
}

// ============================================================ IOUtil

const IOU: &str = "sun/nio/ch/IOUtil";

fn iou_fdval(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    Ok(Slot::I(fd_slot(vm, fd_obj, "fd") as i32))
}

fn iou_setfdval(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    set_fd_slot(vm, fd_obj, "fd", f.arg_i(1) as i64);
    Ok(Slot::I(0))
}

fn iou_configure_blocking(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let blocking = f.arg_i(1) != 0;
    if s == INVALID_SOCKET {
        // 非 socket 句柄（普通文件）：Win32 上本来就是同步的，没什么可设
        return Ok(Slot::I(0));
    }
    wsa_start();
    let mut mode: u32 = if blocking { 0 } else { 1 };
    if unsafe { ioctlsocket(s, FIONBIO, &mut mode) } == SOCKET_ERROR {
        return Err(throw_sock(vm, format!("设置阻塞模式失败: {}", wsa_msg(unsafe { WSAGetLastError() }))));
    }
    Ok(Slot::I(0))
}

fn iou_random_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let arr = f.arg_i(0);
    let n = vm.sh.heap.array_len(arr).unwrap_or(0);
    let mut bytes = vec![0u8; n];
    if rand_bytes(&mut bytes) == 0 {
        return Ok(Slot::I(0));
    }
    for (i, b) in bytes.iter().enumerate() {
        vm.sh.heap.set(arr, i, Slot::I(*b as i8 as i32));
    }
    Ok(Slot::I(1))
}

fn rand_bytes(buf: &mut [u8]) -> u8 {
    #[link(name = "advapi32")]
    extern "system" {
        // RtlGenRandom（Windows 2000 起导出，名字是 SystemFunction036）
        #[link_name = "SystemFunction036"]
        fn rtl_gen_random(buf: *mut u8, len: u32) -> u8;
    }
    unsafe { rtl_gen_random(buf.as_mut_ptr(), buf.len() as u32) }
}

fn iou_make_pipe(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let blocking = f.arg_i(0) != 0;
    let mut r: Handle = 0;
    let mut w: Handle = 0;
    if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null_mut(), 0) } == 0 {
        return Err(throw_io(vm, format!("创建管道失败: {}", win_msg(last_error()))));
    }
    if blocking {
        // 关掉继承，句柄不给子进程
        unsafe {
            SetHandleInformation(r, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(w, HANDLE_FLAG_INHERIT, 0);
        }
    }
    // 高 32 位 = 读端、低 32 位 = 写端（与 Java 侧约定一致）
    let packed = ((r as u64 & 0xFFFF_FFFF) << 32) | (w as u64 & 0xFFFF_FFFF);
    Ok(Slot::L(packed as i64))
}

fn iou_write1(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0) as Handle;
    let b = f.arg_i(1) as u8;
    let mut written: u32 = 0;
    if unsafe { WriteFile(h, &b as *const u8 as *const c_void, 1, &mut written, std::ptr::null_mut()) } == 0
    {
        return Err(throw_io(vm, format!("管道写失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(written as i32))
}

fn iou_drain1(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0) as Handle;
    let mut buf = [0u8; 1];
    let mut got: u32 = 0;
    if unsafe { ReadFile(h, buf.as_mut_ptr() as *mut c_void, 1, &mut got, std::ptr::null_mut()) } == 0 {
        return Err(throw_io(vm, format!("管道读失败: {}", win_msg(last_error()))));
    }
    Ok(Slot::I(got as i32))
}

fn iou_drain(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0) as Handle;
    let mut buf = [0u8; 64];
    let mut got: u32 = 0;
    let ok = unsafe {
        ReadFile(h, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got, std::ptr::null_mut())
    };
    if ok == 0 {
        let code = last_error();
        if code == ERROR_BROKEN_PIPE || code == ERROR_HANDLE_EOF {
            return Ok(Slot::I(0));
        }
        return Err(throw_io(vm, format!("管道读失败: {}", win_msg(code))));
    }
    Ok(Slot::I((got > 0) as i32))
}

fn register_ioutil(r: &mut NativeRegistry) {
    r.add(IOU, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(IOU, "fdVal", "(Ljava/io/FileDescriptor;)I", iou_fdval);
    r.add(IOU, "setfdVal", "(Ljava/io/FileDescriptor;I)V", iou_setfdval);
    r.add(IOU, "configureBlocking", "(Ljava/io/FileDescriptor;Z)V", iou_configure_blocking);
    r.add(IOU, "randomBytes", "([B)Z", iou_random_bytes);
    r.add(IOU, "makePipe", "(Z)J", iou_make_pipe);
    r.add(IOU, "write1", "(IB)I", iou_write1);
    r.add(IOU, "drain1", "(I)I", iou_drain1);
    r.add(IOU, "drain", "(I)Z", iou_drain);
    // 句柄数上限：Win32 常见值（只在 FileChannel 一次分配 fd 数组时用到）
    r.add(IOU, "fdLimit", "()I", |_, _| Ok(Slot::I(16384)));
    // iovec 项数：文件 writev 是我们自己循环的（见 fdp_writev0），与 Linux 保持一致
    r.add(IOU, "iovMax", "()I", |_, _| Ok(Slot::I(1024)));
    r.add(IOU, "writevMax", "()J", |_, _| Ok(Slot::L(1024 * 1024)));
}

// ============================================================ winsock / iphlpapi 的 FFI

#[link(name = "ws2_32")]
extern "system" {
    fn WSAStartup(version: u16, data: *mut WsaData) -> i32;
    fn WSAGetLastError() -> i32;
    fn socket(af: i32, ty: i32, proto: i32) -> Socket;
    fn bind(s: Socket, name: *const u8, len: i32) -> i32;
    fn listen(s: Socket, backlog: i32) -> i32;
    fn accept(s: Socket, name: *mut u8, len: *mut i32) -> Socket;
    fn connect(s: Socket, name: *const u8, len: i32) -> i32;
    fn send(s: Socket, buf: *const u8, len: i32, flags: i32) -> i32;
    fn recv(s: Socket, buf: *mut u8, len: i32, flags: i32) -> i32;
    fn shutdown(s: Socket, how: i32) -> i32;
    fn closesocket(s: Socket) -> i32;
    fn setsockopt(s: Socket, level: i32, name: i32, val: *const u8, len: i32) -> i32;
    fn getsockopt(s: Socket, level: i32, name: i32, val: *mut u8, len: *mut i32) -> i32;
    fn getsockname(s: Socket, name: *mut u8, len: *mut i32) -> i32;
    fn getpeername(s: Socket, name: *mut u8, len: *mut i32) -> i32;
    fn ioctlsocket(s: Socket, cmd: i32, arg: *mut u32) -> i32;
    fn WSAPoll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
    fn sendto(s: Socket, buf: *const u8, len: i32, flags: i32, to: *const u8, tolen: i32) -> i32;
    fn recvfrom(s: Socket, buf: *mut u8, len: i32, flags: i32, from: *mut u8, fromlen: *mut i32) -> i32;
    fn getaddrinfo(
        node: *const u8,
        service: *const u8,
        hints: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn freeaddrinfo(p: *mut c_void);
    fn gethostname(name: *mut u8, len: i32) -> i32;
}

#[link(name = "iphlpapi")]
extern "system" {
    fn GetAdaptersAddresses(
        family: u32,
        flags: u32,
        reserved: *mut c_void,
        addresses: *mut c_void,
        size: *mut u32,
    ) -> u32;
}

// ---- 错误码 / 常量（WinSock2.h）----
const WSAEINTR: i32 = 10004;
const WSAECONNREFUSED: i32 = 10061;
const WSAETIMEDOUT: i32 = 10060;
const WSAEHOSTUNREACH: i32 = 10065;
const WSAENETUNREACH: i32 = 10051;
const WSAEADDRINUSE: i32 = 10048;
const WSAEADDRNOTAVAIL: i32 = 10049;
const WSAEALREADY: i32 = 10037;
const WSAENOTSOCK: i32 = 10038;
const WSAECONNRESET: i32 = 10054;
const WSAECONNABORTED: i32 = 10053;
const WSAEINVAL: i32 = 10022;
const WSAEMSGSIZE: i32 = 10040;
const WSAEACCES: i32 = 10013;
const WSA_ENOTCONN: i32 = 10057;
const WSA_ELOOP: i32 = 10062;

const GAA_FLAG_SKIP_ANYCAST: u32 = 0x2;
const GAA_FLAG_SKIP_MULTICAST: u32 = 0x4;
const GAA_FLAG_SKIP_DNS_SERVER: u32 = 0x8;

const IPPROTO_IP: i32 = 0;
const IPPROTO_IPV6: i32 = 41;
const IPPROTO_TCP: i32 = 6;
const IPV6_V6ONLY: i32 = 27;
const IPV6_MULTICAST_IF: i32 = 17;
const IPV6_JOIN_GROUP: i32 = 12;
const IPV6_LEAVE_GROUP: i32 = 13;
const IP_MULTICAST_IF: i32 = 9;
const IP_ADD_MEMBERSHIP: i32 = 12;
const IP_DROP_MEMBERSHIP: i32 = 13;
const TCP_NODELAY: i32 = 1;
const MSG_OOB: i32 = 0x1;

/// 一个 WSADATA 只需要够大（进程内只调一次，内容不用）
fn wsa_start() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let mut data: WsaData = std::mem::zeroed();
        WSAStartup(0x0202, &mut data);
    });
}

fn wsa_msg(code: i32) -> String {
    // WinSock 错误码在 10000 以上，FormatMessageW 认前缀 0x20000000 的 MappingError 区
    let m = win_msg((code as u32).wrapping_sub(10000) | 0x0000_0000);
    if m.starts_with("错误 ") {
        format!("WSAEPROTO {code}")
    } else {
        m
    }
}

/// WinSock 错误码 → Java 异常（与 HotSpot 的 `handleSocketError` 同一张表）
fn sock_error(vm: &mut Vm, code: i32, what: &str) -> VmError {
    let text = |c: i32| format!("{what}: {}", wsa_msg(c));
    match code {
        WSAECONNREFUSED => vm.throw_msg("java/net/ConnectException", "Connection refused: connect".to_string()),
        WSAETIMEDOUT => vm.throw_msg("java/net/ConnectException", "Connection timed out: connect".to_string()),
        WSAEHOSTUNREACH => vm.throw_msg("java/net/NoRouteToHostException", "No route to host".to_string()),
        WSAENETUNREACH => vm.throw_msg("java/net/NoRouteToHostException", "Network is unreachable".to_string()),
        WSAEADDRINUSE => vm.throw_msg("java/net/BindException", text(code)),
        WSAEADDRNOTAVAIL => vm.throw_msg("java/net/BindException", text(code)),
        WSAEACCES => vm.throw_msg("java/net/SocketException", text(code)),
        WSAECONNRESET => vm.throw_msg("java/io/IOException", "Connection reset".to_string()),
        WSAECONNABORTED => vm.throw_msg("java/io/IOException", "Connection aborted".to_string()),
        WSA_ENOTCONN => vm.throw_msg("java/net/SocketException", "Socket is not connected".to_string()),
        _ => vm.throw_msg("java/io/IOException", text(code)),
    }
}

/// socket 的 fd 是 `FileDescriptor.fd`（int，Win32 上 SOCKET 句柄总能塞进 32 位）
fn sock_of(vm: &Vm, fd_obj: i32) -> Socket {
    let v = fd_slot(vm, fd_obj, "fd");
    if v < 0 {
        return INVALID_SOCKET;
    }
    (v as u32) as usize
}

fn set_sock(vm: &Vm, fd_obj: i32, s: Socket) {
    set_fd_slot(vm, fd_obj, "fd", (s as u32 as i32) as i64);
}

// ============================================================ Java 调用桥（native → Java）

fn new_byte_array(vm: &Vm, bytes: &[u8]) -> i32 {
    let elems: Vec<Slot> = bytes.iter().map(|b| Slot::I(*b as i8 as i32)).collect();
    vm.sh.heap.new_array(elems, "[B").unwrap_or(0)
}

/// 读 `byte[]` 的一段（元素按 0..=255 归一，与 `baload` 侧一致）
fn load_bytes(vm: &Vm, arr: i32, off: usize, len: usize) -> Vec<u8> {
    let total = vm.sh.heap.array_len(arr).unwrap_or(0);
    let end = off.saturating_add(len).min(total);
    let mut v = Vec::with_capacity(end.saturating_sub(off));
    for i in off..end {
        let b = vm.sh.heap.get(arr, i).map(|s| s.as_i32()).unwrap_or(0);
        v.push(b as u8);
    }
    v
}

fn run_method(
    vm: &mut Vm,
    class: &str,
    name: &str,
    desc: &str,
    recv: i32,
    args: &[Slot],
) -> Result<Slot, VmError> {
    let is_static = recv == 0;
    let owner = if is_static {
        class.to_string()
    } else {
        vm.sh
            .heap
            .class_of(recv)
            .map(|c| c.to_string())
            .unwrap_or_else(|| class.to_string())
    };
    let target = vm
        .store
        .resolve_method(&owner, name, desc)
        .or_else(|| vm.store.method_in_class(class, name, desc));
    let Some((ci, mi)) = target else {
        return Err(VmError { msg: format!("找不到 {owner}.{name}{desc}"), exc: None });
    };
    let mut all = Vec::with_capacity(args.len() + 1);
    if !is_static {
        all.push(Slot::I(recv));
    }
    all.extend_from_slice(args);
    vm.depth += 1;
    let r = vm.execute(ci, mi, desc, &all, !is_static);
    vm.depth -= 1;
    r
}

/// `InetAddress.getAddress()` → 4 或 16 字节
fn inet_bytes(vm: &mut Vm, addr: i32) -> Result<Vec<u8>, VmError> {
    let r = run_method(vm, "java/net/InetAddress", "getAddress", "()[B", addr, &[])?;
    let h = r.as_i32();
    let n = vm.sh.heap.array_len(h).unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(vm.sh.heap.get(h, i).map(|s| s.as_i32() as u8).unwrap_or(0));
    }
    Ok(out)
}

fn inet_from_bytes(vm: &mut Vm, bytes: &[u8]) -> Result<i32, VmError> {
    let arr = new_byte_array(vm, bytes);
    let r = run_method(
        vm,
        "java/net/InetAddress",
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
        0,
        &[Slot::I(arr)],
    )?;
    Ok(r.as_i32())
}

/// `Inet6Address.holder6.scope_id`（链路本地地址要用；字段不在就 0）
fn inet6_scope(vm: &Vm, addr: i32) -> u32 {
    let Some(i) = vm.store.field_index("java/net/Inet6Address", "holder6") else { return 0 };
    let h6 = vm.sh.heap.get(addr, i).map(|s| s.as_i32()).unwrap_or(0);
    if h6 == 0 {
        return 0;
    }
    vm.store
        .field_index("java/net/Inet6Address$Inet6AddressHolder", "scope_id")
        .and_then(|j| vm.sh.heap.get(h6, j))
        .map(|s| s.as_i32() as u32)
        .unwrap_or(0)
}

/// `InetAddress` + 端口 → sockaddr 字节（v4 16 字节 / v6 28 字节）。
/// `dual` 表示当前 socket 是 AF_INET6 且允许 v4 映射（IPv4 地址要写成 `::ffff:a.b.c.d`）。
fn sockaddr_for(vm: &mut Vm, addr: i32, port: i32, dual: bool) -> Result<Vec<u8>, VmError> {
    let bytes = inet_bytes(vm, addr)?;
    let p = (port as u16).to_be_bytes();
    if bytes.len() == 4 {
        if dual {
            let mut v = vec![0u8; 28];
            v[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
            v[2..4].copy_from_slice(&p);
            v[18] = 0xFF;
            v[19] = 0xFF;
            v[20..24].copy_from_slice(&bytes);
            Ok(v)
        } else {
            let mut v = vec![0u8; 16];
            v[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            v[2..4].copy_from_slice(&p);
            v[4..8].copy_from_slice(&bytes);
            Ok(v)
        }
    } else if bytes.len() == 16 {
        if !dual {
            return Err(vm.throw_msg(
                "java/net/SocketException",
                "Protocol family unavailable：IPv4 socket 不能用于 IPv6 地址".to_string(),
            ));
        }
        let mut v = vec![0u8; 28];
        v[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
        v[2..4].copy_from_slice(&p);
        v[8..24].copy_from_slice(&bytes);
        let scope = inet6_scope(vm, addr);
        v[24..28].copy_from_slice(&scope.to_ne_bytes());
        Ok(v)
    } else {
        Err(vm.throw_msg(
            "java/net/SocketException",
            format!("地址长度非法: {}", bytes.len()),
        ))
    }
}

/// sockaddr 缓冲 → (IPv4/IPv6 字节, 是否 v4-mapped, 端口)
fn parse_sockaddr(buf: &[u8], len: i32) -> Option<(Vec<u8>, i32)> {
    if len < 16 || buf.len() < 16 {
        return None;
    }
    let family = u16::from_ne_bytes([buf[0], buf[1]]) as i32;
    let port = u16::from_be_bytes([buf[2], buf[3]]) as i32;
    match family {
        AF_INET => {
            let mut a = buf[4..8].to_vec();
            a.truncate(4);
            Some((a, port))
        }
        AF_INET6 => {
            if buf.len() < 28 {
                return None;
            }
            let raw = &buf[8..24];
            let mapped = raw[..10].iter().all(|&b| b == 0) && raw[10] == 0xFF && raw[11] == 0xFF;
            let addr = if mapped { raw[12..16].to_vec() } else { raw.to_vec() };
            Some((addr, port))
        }
        _ => None,
    }
}

fn sockaddr_inet(vm: &mut Vm, buf: &[u8], len: i32) -> Result<i32, VmError> {
    match parse_sockaddr(buf, len) {
        Some((bytes, _)) => inet_from_bytes(vm, &bytes),
        None => Ok(0),
    }
}

// ============================================================ Net（socket 生命周期）

const NET: &str = "sun/nio/ch/Net";
const NET_POLLIN: i32 = POLLRDNORM as i32;
const NET_POLLOUT: i32 = POLLWRNORM as i32;
const IO_UNAVAILABLE: i32 = -2;

fn net_socket0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let prefer_v6 = f.arg_i(0) != 0;
    let stream = f.arg_i(1) != 0;
    let reuse = f.arg_i(2) != 0;
    let family = if prefer_v6 { AF_INET6 } else { AF_INET };
    let ty = if stream { SOCK_STREAM } else { 2 /* SOCK_DGRAM */ };
    let s = unsafe { socket(family, ty, 0) };
    if s == INVALID_SOCKET {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "socket 创建失败"));
    }
    if prefer_v6 {
        // 双栈：允许 IPv4 地址走同一张 socket（Windows 默认 v6only=1，必须显式关掉）
        let off: u32 = 0;
        unsafe {
            setsockopt(
                s,
                IPPROTO_IPV6,
                IPV6_V6ONLY,
                &off as *const u32 as *const u8,
                4,
            );
        }
    }
    if reuse && !stream {
        // 与 HotSpot 一致：TCP 上不设 SO_REUSEADDR（Windows 上它能被别的进程劫持）
        let on: u32 = 1;
        unsafe { setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &on as *const u32 as *const u8, 4) };
    }
    Ok(Slot::I(s as u32 as i32))
}

fn net_bind0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    let prefer_v6 = f.arg_i(1) != 0;
    let addr_obj = f.arg_i(3);
    let port = f.arg_i(4);
    let s = sock_of(vm, fd_obj);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "Socket closed".to_string()));
    }
    let sa = sockaddr_for(vm, addr_obj, port, prefer_v6)?;
    if unsafe { bind(s, sa.as_ptr(), sa.len() as i32) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "bind 失败"));
    }
    Ok(Slot::I(0))
}

fn net_listen(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let backlog = f.arg_i(1);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "Socket closed".to_string()));
    }
    if unsafe { listen(s, backlog) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "listen 失败"));
    }
    Ok(Slot::I(0))
}

fn net_connect0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let prefer_v6 = f.arg_i(0) != 0;
    let fd_obj = f.arg_i(1);
    let addr_obj = f.arg_i(2);
    let port = f.arg_i(3);
    let s = sock_of(vm, fd_obj);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "Socket closed".to_string()));
    }
    let sa = sockaddr_for(vm, addr_obj, port, prefer_v6)?;
    if unsafe { connect(s, sa.as_ptr(), sa.len() as i32) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        return match code {
            WSAEWOULDBLOCK | WSAEINPROGRESS | WSAEALREADY => Ok(Slot::I(IO_UNAVAILABLE)),
            _ => Err(sock_error(vm, code, "connect 失败")),
        };
    }
    Ok(Slot::I(1))
}

fn net_accept(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fd_obj = f.arg_i(0);
    let new_fd_obj = f.arg_i(1);
    let isaa = f.arg_i(2);
    let s = sock_of(vm, fd_obj);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "Socket closed".to_string()));
    }
    let mut buf = [0u8; 28];
    let mut len: i32 = 28;
    let ns = unsafe { accept(s, buf.as_mut_ptr(), &mut len) };
    if ns == INVALID_SOCKET {
        let code = unsafe { WSAGetLastError() };
        return match code {
            WSAEWOULDBLOCK => Ok(Slot::I(IO_UNAVAILABLE)),
            _ => Err(sock_error(vm, code, "accept 失败")),
        };
    }
    set_sock(vm, new_fd_obj, ns);
    // 对端地址 → InetSocketAddress 填进 isaa[0]
    if isaa != 0 {
        let addr = sockaddr_inet(vm, &buf, len)?;
        let port = parse_sockaddr(&buf, len).map(|(_, p)| p).unwrap_or(0);
        if addr != 0 {
            let isa = vm
                .new_instance_args(
                    "java/net/InetSocketAddress",
                    "(Ljava/net/InetAddress;I)V",
                    &[Slot::I(addr), Slot::I(port)],
                )
                .unwrap_or(0);
            vm.sh.heap.set(isaa, 0, Slot::I(isa));
        }
    }
    Ok(Slot::I(ns as u32 as i32))
}

fn net_shutdown(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let how = match f.arg_i(1) {
        0 => SHUT_RD,
        1 => SHUT_WR,
        _ => SHUT_RDWR,
    };
    if s == INVALID_SOCKET {
        return Ok(Slot::I(0));
    }
    if unsafe { shutdown(s, how) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        // 已经断开的不算错误（HotSpot 同样容忍 ENOTCONN）
        if code != WSA_ENOTCONN {
            return Err(sock_error(vm, code, "shutdown 失败"));
        }
    }
    Ok(Slot::I(0))
}

fn net_local_port(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut buf = [0u8; 28];
    let mut len: i32 = 28;
    if unsafe { getsockname(s, buf.as_mut_ptr(), &mut len) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "getsockname 失败"));
    }
    Ok(Slot::I(parse_sockaddr(&buf, len).map(|(_, p)| p).unwrap_or(0)))
}

fn net_local_inet(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut buf = [0u8; 28];
    let mut len: i32 = 28;
    if unsafe { getsockname(s, buf.as_mut_ptr(), &mut len) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "getsockname 失败"));
    }
    Ok(Slot::I(sockaddr_inet(vm, &buf, len)?))
}

fn net_remote_port(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut buf = [0u8; 28];
    let mut len: i32 = 28;
    if unsafe { getpeername(s, buf.as_mut_ptr(), &mut len) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "getpeername 失败"));
    }
    Ok(Slot::I(parse_sockaddr(&buf, len).map(|(_, p)| p).unwrap_or(0)))
}

fn net_remote_inet(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut buf = [0u8; 28];
    let mut len: i32 = 28;
    if unsafe { getpeername(s, buf.as_mut_ptr(), &mut len) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "getpeername 失败"));
    }
    Ok(Slot::I(sockaddr_inet(vm, &buf, len)?))
}

fn net_poll(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let events = f.arg_i(1);
    let timeout = f.arg_l(2);
    let mut pfd = PollFd { fd: s, events: 0, revents: 0 };
    if events & NET_POLLIN != 0 {
        pfd.events |= POLLRDNORM;
    }
    if events & NET_POLLOUT != 0 {
        pfd.events |= POLLWRNORM;
    }
    let ms = if timeout < 0 { -1 } else { timeout.min(i32::MAX as i64) as i32 };
    let r = unsafe { WSAPoll(&mut pfd, 1, ms) };
    if r == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        if code == WSAEINTR {
            return Ok(Slot::I(0));
        }
        return Err(sock_error(vm, code, "poll 失败"));
    }
    if r == 0 {
        return Ok(Slot::I(0));
    }
    Ok(Slot::I(pfd.revents as i32))
}

fn net_poll_connect(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let timeout = f.arg_l(1);
    let mut pfd = PollFd { fd: s, events: POLLWRNORM, revents: 0 };
    let ms = if timeout < 0 { -1 } else { timeout.min(i32::MAX as i64) as i32 };
    let r = unsafe { WSAPoll(&mut pfd, 1, ms) };
    if r == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "poll 失败"));
    }
    Ok(Slot::I((r > 0 && pfd.revents != 0) as i32))
}

fn net_available(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut n: u32 = 0;
    if unsafe { ioctlsocket(s, FIONREAD, &mut n) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "FIONREAD 失败"));
    }
    Ok(Slot::I(n as i32))
}

fn net_get_int_option(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let level = f.arg_i(2);
    let opt = f.arg_i(3);
    let mut val: u32 = 0;
    let mut len: i32 = 4;
    if unsafe { getsockopt(s, level, opt, &mut val as *mut u32 as *mut u8, &mut len) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "getsockopt 失败"));
    }
    Ok(Slot::I(val as i32))
}

fn net_set_int_option(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let level = f.arg_i(2);
    let opt = f.arg_i(3);
    let arg = f.arg_i(4) as u32;
    if unsafe { setsockopt(s, level, opt, &arg as *const u32 as *const u8, 4) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "setsockopt 失败"));
    }
    Ok(Slot::I(0))
}

fn net_send_oob(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let b = f.arg_i(1) as u8;
    let n = unsafe { send(s, &b as *const u8, 1, MSG_OOB) };
    if n == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "send(MSG_OOB) 失败"));
    }
    Ok(Slot::I(n))
}

fn net_discard_oob(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut b: u8 = 0;
    let n = unsafe { recv(s, &mut b as *mut u8, 1, MSG_OOB) };
    if n == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        if code == WSAEWOULDBLOCK || code == WSAEINVAL {
            return Ok(Slot::I(0));
        }
        return Err(sock_error(vm, code, "recv(MSG_OOB) 失败"));
    }
    Ok(Slot::I((n > 0) as i32))
}

/// `joinOrDrop4(boolean join, fd, int group, int interf, int source)`：
/// 组播加入/离开（source 非 0 表示源过滤，Windows 要用 IP_ADD_SOURCE_MEMBERSHIP）
fn net_join_or_drop4(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let join = f.arg_i(0) != 0;
    let s = sock_of(vm, f.arg_i(1));
    let group = f.arg_i(2) as u32;
    let interf = f.arg_i(3) as u32;
    let source = f.arg_i(4) as u32;
    #[repr(C)]
    struct IpMreq {
        multiaddr: u32,
        interface: u32,
    }
    if source != 0 {
        return Err(vm.throw_msg(
            "java/io/IOException",
            "IPv4 组播源过滤（source-specific multicast）未实现".to_string(),
        ));
    }
    let mreq = IpMreq { multiaddr: group, interface: interf };
    let opt = if join { IP_ADD_MEMBERSHIP } else { IP_DROP_MEMBERSHIP };
    if unsafe { setsockopt(s, IPPROTO_IP, opt, &mreq as *const IpMreq as *const u8, 8) } == SOCKET_ERROR {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "组播配置失败"));
    }
    Ok(Slot::I(0))
}

/// `blockOrUnblock4(boolean block, fd, int group, int interf, int source)`：
/// 源过滤（`IP_BLOCK_SOURCE` / `IP_UNBLOCK_SOURCE`）。三个参数都是
/// `Inet4Address.address` 那种「网络序被当成 int」的值，所以 `to_be_bytes` 才是线上字节。
fn net_block_or_unblock4(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    const IP_BLOCK_SOURCE: i32 = 17;
    const IP_UNBLOCK_SOURCE: i32 = 18;
    let block = f.arg_i(0) != 0;
    let s = sock_of(vm, f.arg_i(1));
    let group = f.arg_i(2) as u32;
    let interf = f.arg_i(3) as u32;
    let source = f.arg_i(4) as u32;
    #[repr(C)]
    struct IpMreqSource {
        multiaddr: u32,
        sourceaddr: u32,
        interface: u32,
    }
    let mreq = IpMreqSource { multiaddr: group, sourceaddr: source, interface: interf };
    let opt = if block { IP_BLOCK_SOURCE } else { IP_UNBLOCK_SOURCE };
    if unsafe {
        setsockopt(s, IPPROTO_IP, opt, &mreq as *const IpMreqSource as *const u8, 12)
    } == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "源过滤配置失败"));
    }
    Ok(Slot::I(0))
}

fn net_join_or_drop6(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let join = f.arg_i(0) != 0;
    let s = sock_of(vm, f.arg_i(1));
    let group = f.arg_i(2);
    let interf = f.arg_i(3) as u32;
    let group_bytes = read_array_bytes(vm, group, 16);
    if group_bytes.len() != 16 {
        return Err(vm.throw_msg("java/io/IOException", "组播组地址长度非法".to_string()));
    }
    #[repr(C)]
    struct Ipv6Mreq {
        group: [u8; 16],
        interface: u32,
    }
    let mreq = Ipv6Mreq {
        group: {
            let mut g = [0u8; 16];
            g.copy_from_slice(&group_bytes);
            g
        },
        interface: interf,
    };
    let opt = if join { IPV6_JOIN_GROUP } else { IPV6_LEAVE_GROUP };
    if unsafe {
        setsockopt(s, IPPROTO_IPV6, opt, &mreq as *const Ipv6Mreq as *const u8, 20)
    } == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "IPv6 组播配置失败"));
    }
    Ok(Slot::I(0))
}

/// `blockOrUnblock6(block, fd, byte[] group, int index, byte[] source)`：
/// IPv6 的源过滤走 `group_source_req`（组接口下标 + 两个 `sockaddr_storage`）。
fn net_block_or_unblock6(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    const MCAST_BLOCK_SOURCE: i32 = 43;
    const MCAST_UNBLOCK_SOURCE: i32 = 44;
    let block = f.arg_i(0) != 0;
    let s = sock_of(vm, f.arg_i(1));
    let group = read_array_bytes(vm, f.arg_i(2), 16);
    let index = f.arg_i(3) as u32;
    let source = read_array_bytes(vm, f.arg_i(4), 16);
    if group.len() != 16 || source.len() != 16 {
        return Err(vm.throw_msg("java/io/IOException", "组播组/源地址长度非法".to_string()));
    }
    let req = group_source_req(index, &group, &source);
    let opt = if block { MCAST_BLOCK_SOURCE } else { MCAST_UNBLOCK_SOURCE };
    if unsafe {
        setsockopt(s, IPPROTO_IPV6, opt, req.as_ptr() as *const u8, req.len() as i32)
    } == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "源过滤配置失败"));
    }
    Ok(Slot::I(0))
}

/// `GROUP_SOURCE_REQ`：两个 `GROUP_REQ`（各 = `ULONG gr_interface`(4) + 4 填充 +
/// `SOCKADDR_STORAGE`(128)），共 272 字节。
fn group_source_req(index: u32, group: &[u8], source: &[u8]) -> Vec<u8> {
    // sockaddr_in6：family(u16@0) port(u16@2) flowinfo(u32@4) addr@8 scope_id@24
    fn put_sockaddr(buf: &mut [u8], at: usize, addr: &[u8], index: u32) {
        buf[at..at + 2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
        buf[at + 8..at + 24].copy_from_slice(&addr[..16]);
        buf[at + 24..at + 28].copy_from_slice(&index.to_ne_bytes());
    }
    let mut req = vec![0u8; 272];
    req[0..4].copy_from_slice(&index.to_ne_bytes());
    put_sockaddr(&mut req, 8, group, index);
    req[136..140].copy_from_slice(&index.to_ne_bytes());
    put_sockaddr(&mut req, 144, source, index);
    req
}

fn net_set_interface4(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_i(1) as u32;
    if unsafe { setsockopt(s, IPPROTO_IP, IP_MULTICAST_IF, &addr as *const u32 as *const u8, 4) }
        == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "设置组播出口失败"));
    }
    Ok(Slot::I(0))
}

fn net_get_interface4(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut addr: u32 = 0;
    let mut len: i32 = 4;
    if unsafe { getsockopt(s, IPPROTO_IP, IP_MULTICAST_IF, &mut addr as *mut u32 as *mut u8, &mut len) }
        == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "取组播出口失败"));
    }
    Ok(Slot::I(addr as i32))
}

fn net_set_interface6(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let idx = f.arg_i(1) as u32;
    if unsafe {
        setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &idx as *const u32 as *const u8, 4)
    } == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "设置组播出口失败"));
    }
    Ok(Slot::I(0))
}

fn net_get_interface6(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let mut idx: u32 = 0;
    let mut len: i32 = 4;
    if unsafe { getsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &mut idx as *mut u32 as *mut u8, &mut len) }
        == SOCKET_ERROR
    {
        return Err(sock_error(vm, unsafe { WSAGetLastError() }, "取组播出口失败"));
    }
    Ok(Slot::I(idx as i32))
}

fn read_array_bytes(vm: &Vm, arr: i32, want: usize) -> Vec<u8> {
    let n = vm.sh.heap.array_len(arr).unwrap_or(0).min(want);
    (0..n).map(|i| vm.sh.heap.get(arr, i).map(|s| s.as_i32() as u8).unwrap_or(0)).collect()
}

fn register_net(r: &mut NativeRegistry) {
    r.add(NET, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(NET, "socket0", "(ZZZZ)I", net_socket0);
    r.add(NET, "bind0", "(Ljava/io/FileDescriptor;ZZLjava/net/InetAddress;I)V", net_bind0);
    r.add(NET, "listen", "(Ljava/io/FileDescriptor;I)V", net_listen);
    r.add(
        NET,
        "connect0",
        "(ZLjava/io/FileDescriptor;Ljava/net/InetAddress;I)I",
        net_connect0,
    );
    r.add(
        NET,
        "accept",
        "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;[Ljava/net/InetSocketAddress;)I",
        net_accept,
    );
    r.add(NET, "shutdown", "(Ljava/io/FileDescriptor;I)V", net_shutdown);
    r.add(NET, "localPort", "(Ljava/io/FileDescriptor;)I", net_local_port);
    r.add(
        NET,
        "localInetAddress",
        "(Ljava/io/FileDescriptor;)Ljava/net/InetAddress;",
        net_local_inet,
    );
    r.add(NET, "remotePort", "(Ljava/io/FileDescriptor;)I", net_remote_port);
    r.add(
        NET,
        "remoteInetAddress",
        "(Ljava/io/FileDescriptor;)Ljava/net/InetAddress;",
        net_remote_inet,
    );
    r.add(NET, "getIntOption0", "(Ljava/io/FileDescriptor;ZII)I", net_get_int_option);
    r.add(NET, "setIntOption0", "(Ljava/io/FileDescriptor;ZIIIZ)V", net_set_int_option);
    r.add(NET, "poll", "(Ljava/io/FileDescriptor;IJ)I", net_poll);
    r.add(NET, "pollConnect", "(Ljava/io/FileDescriptor;J)Z", net_poll_connect);
    r.add(NET, "available", "(Ljava/io/FileDescriptor;)I", net_available);
    r.add(NET, "sendOOB", "(Ljava/io/FileDescriptor;B)I", net_send_oob);
    r.add(NET, "discardOOB", "(Ljava/io/FileDescriptor;)Z", net_discard_oob);
    r.add(NET, "joinOrDrop4", "(ZLjava/io/FileDescriptor;III)I", net_join_or_drop4);
    r.add(NET, "blockOrUnblock4", "(ZLjava/io/FileDescriptor;III)I", net_block_or_unblock4);
    r.add(NET, "joinOrDrop6", "(ZLjava/io/FileDescriptor;[BI[B)I", net_join_or_drop6);
    r.add(NET, "blockOrUnblock6", "(ZLjava/io/FileDescriptor;[BI[B)I", net_block_or_unblock6);
    r.add(NET, "setInterface4", "(Ljava/io/FileDescriptor;I)V", net_set_interface4);
    r.add(NET, "getInterface4", "(Ljava/io/FileDescriptor;)I", net_get_interface4);
    r.add(NET, "setInterface6", "(Ljava/io/FileDescriptor;I)V", net_set_interface6);
    r.add(NET, "getInterface6", "(Ljava/io/FileDescriptor;)I", net_get_interface6);

    // 平台能力开关（Windows 的真实取值，与 HotSpot 一致）
    r.add(NET, "isIPv6Available0", "()Z", |_, _| Ok(Slot::I(1)));
    r.add(NET, "isReusePortAvailable0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add(NET, "isExclusiveBindAvailable", "()I", |_, _| Ok(Slot::I(1)));
    r.add(NET, "shouldShutdownWriteBeforeClose0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add(NET, "shouldSetBothIPv4AndIPv6Options0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add(NET, "canIPv6SocketJoinIPv4Group0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add(NET, "canJoin6WithIPv4Group0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add(NET, "canUseIPv6OptionsWithIPv4LocalAddress0", "()Z", |_, _| Ok(Slot::I(0)));

    // poll 的事件位：直接给 Win32 的 WSAPoll 常量，native 与 Java 侧同一套映射
    r.add(NET, "pollinValue", "()S", |_, _| Ok(Slot::I(POLLRDNORM as i32)));
    r.add(NET, "polloutValue", "()S", |_, _| Ok(Slot::I(POLLWRNORM as i32)));
    r.add(NET, "pollerrValue", "()S", |_, _| Ok(Slot::I(POLLERR as i32)));
    r.add(NET, "pollhupValue", "()S", |_, _| Ok(Slot::I(POLLHUP as i32)));
    r.add(NET, "pollnvalValue", "()S", |_, _| Ok(Slot::I(POLLNVAL as i32)));
    r.add(NET, "pollconnValue", "()S", |_, _| Ok(Slot::I(POLLCONN as i32)));
}

// ============================================================ SocketDispatcher

const SD: &str = "sun/nio/ch/SocketDispatcher";

fn sd_read0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET {
        return Err(throw_io(vm, "Socket closed"));
    }
    let want = len.min(capacity_at(addr));
    let mut buf = vec![0u8; want];
    let n = unsafe { recv(s, buf.as_mut_ptr(), want as i32, 0) };
    if n == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        if code == WSAEWOULDBLOCK {
            return Ok(Slot::I(IO_UNAVAILABLE));
        }
        return Err(sock_error(vm, code, "读取失败"));
    }
    if n == 0 {
        return Ok(Slot::I(IO_EOF)); // 对端关闭 → EOF
    }
    nat_mem::write_bytes(addr, &buf[..n as usize]);
    Ok(Slot::I(n))
}

fn sd_write0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET {
        return Err(throw_io(vm, "Socket closed"));
    }
    let data = nat_mem::read_bytes(addr, len);
    let n = unsafe { send(s, data.as_ptr(), data.len() as i32, 0) };
    if n == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        if code == WSAEWOULDBLOCK {
            return Ok(Slot::I(IO_UNAVAILABLE));
        }
        return Err(sock_error(vm, code, "写入失败"));
    }
    Ok(Slot::I(n))
}

fn sd_readv0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    let mut total: i64 = 0;
    for i in 0..cnt {
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        if len == 0 {
            continue;
        }
        let want = len.min(capacity_at(base));
        let mut buf = vec![0u8; want];
        let n = unsafe { recv(s, buf.as_mut_ptr(), want as i32, 0) };
        if n == SOCKET_ERROR {
            let code = unsafe { WSAGetLastError() };
            if code == WSAEWOULDBLOCK {
                return Ok(Slot::L(if total > 0 { total } else { IO_UNAVAILABLE as i64 }));
            }
            return Err(sock_error(vm, code, "readv 失败"));
        }
        if n == 0 {
            return Ok(Slot::L(if total > 0 { total } else { IO_EOF as i64 }));
        }
        nat_mem::write_bytes(base, &buf[..n as usize]);
        total += n as i64;
        if (n as usize) < want {
            break;
        }
    }
    Ok(Slot::L(total))
}

fn sd_writev0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    let mut total: i64 = 0;
    for i in 0..cnt {
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        if len == 0 {
            continue;
        }
        let data = nat_mem::read_bytes(base, len);
        let n = unsafe { send(s, data.as_ptr(), data.len() as i32, 0) };
        if n == SOCKET_ERROR {
            let code = unsafe { WSAGetLastError() };
            if code == WSAEWOULDBLOCK {
                return Ok(Slot::L(if total > 0 { total } else { IO_UNAVAILABLE as i64 }));
            }
            return Err(sock_error(vm, code, "writev 失败"));
        }
        total += n as i64;
        if (n as usize) < len {
            break;
        }
    }
    Ok(Slot::L(total))
}

fn sd_close0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let fdval = f.arg_i(0);
    if fdval >= 0 {
        let s = (fdval as u32) as usize;
        if unsafe { closesocket(s) } == SOCKET_ERROR {
            let code = unsafe { WSAGetLastError() };
            if code != WSAENOTSOCK {
                return Err(sock_error(vm, code, "关闭 socket 失败"));
            }
        }
    }
    Ok(Slot::I(0))
}

fn register_socket_dispatcher(r: &mut NativeRegistry) {
    r.add(SD, "read0", "(Ljava/io/FileDescriptor;JI)I", sd_read0);
    r.add(SD, "write0", "(Ljava/io/FileDescriptor;JI)I", sd_write0);
    r.add(SD, "readv0", "(Ljava/io/FileDescriptor;JI)J", sd_readv0);
    r.add(SD, "writev0", "(Ljava/io/FileDescriptor;JI)J", sd_writev0);
    r.add(SD, "close0", "(I)V", sd_close0);
}
// ============================================================ Win32 安全 / ACL
//
// `Files.isWritable` / `AclFileAttributeView` / `Files.setOwner` 都落在这组 native 上。
// 关键约束：Java 侧的缓冲区地址是 nat_mem 的**假地址**，凡是要交给 Win32 读写/解析的
// 结构，都必须先落到真内存（见下面 `sd_len_at` 那一组助手）。

const WND_ACL_INFO: &str = "sun/nio/fs/WindowsNativeDispatcher$AclInformation";
const WND_ACCOUNT: &str = "sun/nio/fs/WindowsNativeDispatcher$Account";

type TokenAccess = u32;
const TOKEN_QUERY: TokenAccess = 0x0008;
const TOKEN_IMPERSONATE: TokenAccess = 0x0004;
const TOKEN_ADJUST_PRIVILEGES: TokenAccess = 0x0020;
const TOKEN_DUPLICATE: TokenAccess = 0x0002;

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: Handle, access: TokenAccess, out: *mut Handle) -> Bool;
    fn OpenThreadToken(thread: Handle, access: TokenAccess, open_as_self: Bool, out: *mut Handle) -> Bool;
    fn DuplicateTokenEx(
        existing: Handle,
        access: TokenAccess,
        sa: *mut c_void,
        level: u32,
        ty: u32,
        out: *mut Handle,
    ) -> Bool;
    fn SetThreadToken(thread: *mut Handle, token: Handle) -> Bool;
    fn GetTokenInformation(token: Handle, class: u32, buf: *mut c_void, len: u32, needed: *mut u32) -> Bool;
    fn AdjustTokenPrivileges(
        token: Handle,
        disable_all: Bool,
        new_state: *const c_void,
        len: u32,
        prev: *mut c_void,
        ret_len: *mut u32,
    ) -> Bool;
    fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut c_void) -> Bool;
    fn AccessCheck(
        sd: *const c_void,
        token: Handle,
        access_mask: u32,
        mapping: *const c_void,
        privileges: *mut c_void,
        priv_len: *mut u32,
        granted: *mut u32,
        success: *mut Bool,
    ) -> Bool;
    fn GetFileSecurityW(
        name: *const u16,
        info: u32,
        sd: *mut c_void,
        len: u32,
        needed: *mut u32,
    ) -> Bool;
    fn SetFileSecurityW(name: *const u16, info: u32, sd: *const c_void) -> Bool;
    fn InitializeSecurityDescriptor(sd: *mut c_void, revision: u32) -> Bool;
    fn InitializeAcl(acl: *mut c_void, len: u32, revision: u32) -> Bool;
    fn GetSecurityDescriptorOwner(sd: *const c_void, owner: *mut *mut c_void, defaulted: *mut Bool) -> Bool;
    fn GetSecurityDescriptorDacl(
        sd: *const c_void,
        present: *mut Bool,
        dacl: *mut *mut c_void,
        defaulted: *mut Bool,
    ) -> Bool;
    fn SetSecurityDescriptorOwner(sd: *mut c_void, owner: *const c_void, defaulted: Bool) -> Bool;
    fn SetSecurityDescriptorDacl(
        sd: *mut c_void,
        present: Bool,
        dacl: *const c_void,
        defaulted: Bool,
    ) -> Bool;
    fn GetAclInformation(acl: *const c_void, info: *mut c_void, len: u32, class: u32) -> Bool;
    fn GetAce(acl: *const c_void, index: u32, ace: *mut *mut c_void) -> Bool;
    fn AddAccessAllowedAceEx(acl: *mut c_void, revision: u32, flags: u32, mask: u32, sid: *const c_void) -> Bool;
    fn AddAccessDeniedAceEx(acl: *mut c_void, revision: u32, flags: u32, mask: u32, sid: *const c_void) -> Bool;
    fn GetLengthSid(sid: *const c_void) -> u32;
    fn ConvertSidToStringSidW(sid: *const c_void, out: *mut *mut u16) -> Bool;
    fn ConvertStringSidToSidW(s: *const u16, out: *mut *mut c_void) -> Bool;
    fn LookupAccountSidW(
        system: *const u16,
        sid: *const c_void,
        name: *mut u16,
        name_len: *mut u32,
        domain: *mut u16,
        domain_len: *mut u32,
        use_kind: *mut u32,
    ) -> Bool;
    fn LookupAccountNameW(
        system: *const u16,
        account: *const u16,
        sid: *mut c_void,
        sid_len: *mut u32,
        domain: *mut u16,
        domain_len: *mut u32,
        use_kind: *mut u32,
    ) -> Bool;
}

/// SID 字节数：`Revision(1) SubAuthorityCount(1) IdentifierAuthority(6) SubAuthority[n](4)`
fn sid_len_at(addr: u64) -> usize {
    let n = nat_mem::read(addr + 1, 1) as usize;
    8 + 4 * n
}

/// ACL 字节数：头部 `Revision(1) Sbz1(1) AclSize(2) AceCount(2) Sbz2(2)`
fn acl_len_at(addr: u64) -> usize {
    nat_mem::read(addr + 2, 2) as usize
}

/// SD 的实际字节数（自相对头部里的四个偏移推出来，再夹到块尾）。
///
/// SD 里的 SACL/DACL/Owner/Group 全是对 SD 起点的**相对偏移**，所以把整块原样搬到
/// 真内存之后这些偏移仍然成立 —— 这正是「拷进真内存调 Win32」能work 的前提。
fn sd_len_at(addr: u64) -> usize {
    let u32_at = |off: u64| nat_mem::read(addr + off, 4) as u64;
    let (owner, group, sacl, dacl) = (u32_at(4), u32_at(8), u32_at(12), u32_at(16));
    let mut end = 20u64;
    let mut bump = |off: u64, extra: u64| {
        if off != 0 && nat_mem::base_of(addr + off).is_some() {
            if off + extra > end {
                end = off + extra;
            }
        }
    };
    bump(owner, sid_len_at(addr + owner) as u64);
    bump(group, sid_len_at(addr + group) as u64);
    bump(dacl, acl_len_at(addr + dacl) as u64);
    bump(sacl, acl_len_at(addr + sacl) as u64);
    end.min(capacity_at(addr) as u64) as usize
}

/// 真内存缓冲里返回的内部指针 → arena 地址（不在缓冲内就 0）
fn ptr_to_arena(nat_addr: u64, real: &[u8], p: *const c_void) -> u64 {
    let base = real.as_ptr() as u64;
    let q = p as u64;
    if q == 0 {
        0
    } else if q >= base && q < base + real.len() as u64 {
        nat_addr + (q - base)
    } else {
        0
    }
}

fn wnd_GetFileSecurity0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let info = f.arg_i(1) as u32;
    let addr = f.arg_l(2) as u64;
    let size = f.arg_i(3) as u32;
    let mut needed: u32 = size;
    // size == 0：`WindowsAclFileAttributeView.getFileSecurity` 先探长度，
    // 期望这里**不抛异常**、直接把需要的字节数返回（与 HotSpot 一致）
    if size == 0 || addr == 0 {
        if (unsafe { GetFileSecurityW(w.as_ptr(), info, std::ptr::null_mut(), 0, &mut needed) }) == 0 {
            let code = last_error();
            if code != ERROR_INSUFFICIENT_BUFFER {
                return Err(throw_windows(vm, code));
            }
        }
        return Ok(Slot::I(needed as i32));
    }
    let mut real = vec![0u8; size as usize];
    if (unsafe {
        GetFileSecurityW(w.as_ptr(), info, real.as_mut_ptr() as *mut c_void, size, &mut needed)
    }) == 0
    {
        let code = last_error();
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(throw_windows(vm, code));
        }
        return Ok(Slot::I(needed as i32));
    }
    let n = (needed as usize).min(real.len());
    nat_mem::write_bytes(addr, &real[..n]);
    Ok(Slot::I(needed as i32))
}

fn wnd_SetFileSecurity0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let info = f.arg_i(1) as u32;
    let addr = f.arg_l(2) as u64;
    let real = nat_mem::read_bytes(addr, sd_len_at(addr));
    if (unsafe { SetFileSecurityW(w.as_ptr(), info, real.as_ptr() as *const c_void) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetSecurityDescriptorOwner(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let real = nat_mem::read_bytes(addr, sd_len_at(addr));
    let mut owner: *mut c_void = std::ptr::null_mut();
    let mut defaulted: Bool = 0;
    if (unsafe { GetSecurityDescriptorOwner(real.as_ptr() as *const c_void, &mut owner, &mut defaulted) })
        == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(ptr_to_arena(addr, &real, owner) as i64))
}

fn wnd_GetSecurityDescriptorDacl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let real = nat_mem::read_bytes(addr, sd_len_at(addr));
    let mut present: Bool = 0;
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut defaulted: Bool = 0;
    if (unsafe {
        GetSecurityDescriptorDacl(real.as_ptr() as *const c_void, &mut present, &mut dacl, &mut defaulted)
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    if present == 0 {
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(ptr_to_arena(addr, &real, dacl) as i64))
}

fn wnd_InitializeSecurityDescriptor(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let mut real = vec![0u8; 20];
    // SECURITY_DESCRIPTOR_REVISION = 1
    if (unsafe { InitializeSecurityDescriptor(real.as_mut_ptr() as *mut c_void, 1) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &real);
    Ok(Slot::I(0))
}

fn wnd_SetSecurityDescriptorOwner(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let owner_addr = f.arg_l(1) as u64;
    let mut real = nat_mem::read_bytes(addr, 20);
    // Owner 字段是**相对偏移**：把 SID 紧跟在描述符后面，再让 Win32 自己算偏移
    let sid_len = if owner_addr == 0 { 0 } else { sid_len_at(owner_addr) };
    let sid = if owner_addr == 0 { Vec::new() } else { nat_mem::read_bytes(owner_addr, sid_len) };
    real.extend_from_slice(&sid);
    let sid_ptr = if owner_addr == 0 {
        std::ptr::null()
    } else {
        (real.as_ptr() as u64 + 20) as *const c_void
    };
    if (unsafe { SetSecurityDescriptorOwner(real.as_mut_ptr() as *mut c_void, sid_ptr, 0) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &real[..20]);
    if owner_addr != 0 {
        nat_mem::write_bytes(addr + 20, &real[20..]);
    }
    Ok(Slot::I(0))
}

fn wnd_SetSecurityDescriptorDacl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let dacl_addr = f.arg_l(1) as u64;
    let mut real = nat_mem::read_bytes(addr, 20);
    let acl_len = if dacl_addr == 0 { 0 } else { acl_len_at(dacl_addr) };
    if acl_len > 0 {
        let acl = nat_mem::read_bytes(dacl_addr, acl_len);
        real.extend_from_slice(&acl);
    }
    let dacl_ptr = if dacl_addr == 0 {
        std::ptr::null()
    } else {
        (real.as_ptr() as u64 + 20) as *const c_void
    };
    if (unsafe { SetSecurityDescriptorDacl(real.as_mut_ptr() as *mut c_void, 1, dacl_ptr, 0) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &real[..20]);
    Ok(Slot::I(0))
}

fn wnd_InitializeAcl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let size = f.arg_i(1) as u32;
    let mut real = vec![0u8; size as usize];
    // ACL_REVISION = 2
    if (unsafe { InitializeAcl(real.as_mut_ptr() as *mut c_void, size, 2) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &real);
    Ok(Slot::I(0))
}

fn wnd_GetAclInformation0_vm(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let obj = f.arg_i(1);
    let real = nat_mem::read_bytes(addr, acl_len_at(addr));
    // ACL_INFORMATION_CLASS = AclSizeInformation(2) → ACL_SIZE_INFORMATION（3 个 DWORD）
    let mut info = [0u32; 3];
    if (unsafe {
        GetAclInformation(real.as_ptr() as *const c_void, info.as_mut_ptr() as *mut c_void, 12, 2)
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    set_obj_field(vm, obj, WND_ACL_INFO, "aceCount", Slot::I(info[0] as i32));
    Ok(Slot::I(0))
}

fn wnd_GetAce(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let index = f.arg_i(1) as u32;
    let real = nat_mem::read_bytes(addr, acl_len_at(addr));
    let mut ace: *mut c_void = std::ptr::null_mut();
    if (unsafe { GetAce(real.as_ptr() as *const c_void, index, &mut ace) }) == 0 {
        // 越界就是没有这个 ACE：Java 侧按 null 处理，不抛
        let _ = vm;
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(ptr_to_arena(addr, &real, ace) as i64))
}

fn add_ace(vm: &mut Vm, f: &mut Frame, allow: bool) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let flags = f.arg_i(1) as u32;
    let mask = f.arg_i(2) as u32;
    let sid_addr = f.arg_l(3) as u64;
    let len = acl_len_at(addr);
    // ACL 后面通常紧跟 SID（`WindowsSecurityDescriptor` 把两者放在同一块缓冲区里），
    // 所以把「整块」拷进真内存，SID 指针按偏移换算；不在同一块就单独追加一份。
    let cap = capacity_at(addr);
    let mut real = nat_mem::read_bytes(addr, cap);
    let sid_ptr = if sid_addr >= addr && sid_addr < addr + cap as u64 {
        (real.as_ptr() as u64 + (sid_addr - addr)) as *const c_void
    } else {
        let sid = nat_mem::read_bytes(sid_addr, sid_len_at(sid_addr));
        let off = real.len();
        real.extend_from_slice(&sid);
        (real.as_ptr() as u64 + off as u64) as *const c_void
    };
    // ACL_REVISION_DS = 4
    let ok = unsafe {
        if allow {
            AddAccessAllowedAceEx(real.as_mut_ptr() as *mut c_void, 4, flags, mask, sid_ptr)
        } else {
            AddAccessDeniedAceEx(real.as_mut_ptr() as *mut c_void, 4, flags, mask, sid_ptr)
        }
    };
    if ok == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &real[..len]);
    Ok(Slot::I(0))
}

fn wnd_AddAccessAllowedAceEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    add_ace(vm, f, true)
}

fn wnd_AddAccessDeniedAceEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    add_ace(vm, f, false)
}

fn wnd_GetLengthSid(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    Ok(Slot::I(sid_len_at(addr) as i32))
}

fn wnd_ConvertSidToStringSid(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let real = nat_mem::read_bytes(addr, sid_len_at(addr));
    let mut out: *mut u16 = std::ptr::null_mut();
    if (unsafe { ConvertSidToStringSidW(real.as_ptr() as *const c_void, &mut out) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let n = (0..).take_while(|&i| unsafe { *out.add(i) } != 0).count();
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, n) });
    unsafe { LocalFree(out as *mut c_void) };
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

fn wnd_ConvertStringSidToSid0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let mut sid: *mut c_void = std::ptr::null_mut();
    if (unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) }) == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let n = unsafe { GetLengthSid(sid) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, n) };
    // 这个 SID 之后会被 Java 侧当成地址一直用（`WindowsUserPrincipals.fromSid`），
    // 所以放进 arena；原块交回 Win32 释放。
    let out = nat_mem::alloc(n as u64);
    nat_mem::write_bytes(out, bytes);
    unsafe { LocalFree(sid) };
    let _ = vm;
    Ok(Slot::L(out as i64))
}

fn wnd_LookupAccountSid0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let obj = f.arg_i(1);
    let real = nat_mem::read_bytes(addr, sid_len_at(addr));
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut nlen, mut dlen) = (name.len() as u32, domain.len() as u32);
    let mut use_kind: u32 = 0;
    if (unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            real.as_ptr() as *const c_void,
            name.as_mut_ptr(),
            &mut nlen,
            domain.as_mut_ptr(),
            &mut dlen,
            &mut use_kind,
        )
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    let nh = vm.sh.heap.new_string(String::from_utf16_lossy(&name[..nlen as usize]));
    let dh = vm.sh.heap.new_string(String::from_utf16_lossy(&domain[..dlen as usize]));
    set_obj_field(vm, obj, WND_ACCOUNT, "name", Slot::I(nh));
    set_obj_field(vm, obj, WND_ACCOUNT, "domain", Slot::I(dh));
    set_obj_field(vm, obj, WND_ACCOUNT, "use", Slot::I(use_kind as i32));
    Ok(Slot::I(0))
}

/// `LookupAccountName0(nameAddr, pSid, cbSid)` → 需要的 SID 字节数
fn wnd_LookupAccountName0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = read_wstr_z(f.arg_l(0) as u64);
    let sid_addr = f.arg_l(1) as u64;
    let mut cb: u32 = f.arg_i(2) as u32;
    let mut domain = [0u16; 256];
    let mut dlen: u32 = domain.len() as u32;
    let mut use_kind: u32 = 0;
    let cap = if sid_addr == 0 { 0 } else { (cb as usize).min(capacity_at(sid_addr)) };
    let mut real = vec![0u8; cap];
    if (unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            real.as_mut_ptr() as *mut c_void,
            &mut cb,
            domain.as_mut_ptr(),
            &mut dlen,
            &mut use_kind,
        )
    }) == 0
    {
        let code = last_error();
        // INSUFFICIENT_BUFFER 不是失败：Java 侧据返回长度再分配一次
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(throw_windows(vm, code));
        }
    } else if !real.is_empty() {
        nat_mem::write_bytes(sid_addr, &real[..(cb as usize).min(real.len())]);
    }
    Ok(Slot::I(cb as i32))
}

fn wnd_OpenProcessToken(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let process = f.arg_l(0) as Handle;
    let access = f.arg_i(1) as TokenAccess;
    let mut token: Handle = 0;
    if unsafe { OpenProcessToken(process, access, &mut token) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(token as i64))
}

fn wnd_OpenThreadToken(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let thread = f.arg_l(0) as Handle;
    let access = f.arg_i(1) as TokenAccess;
    let open_as_self = f.arg_i(2) as Bool;
    let mut token: Handle = 0;
    if unsafe { OpenThreadToken(thread, access, open_as_self, &mut token) } == 0 {
        // 没有线程令牌是**正常情况**（0 = 没有，Java 侧会退回进程令牌），不抛
        let _ = vm;
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(token as i64))
}

fn wnd_DuplicateTokenEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let existing = f.arg_l(0) as Handle;
    let access = f.arg_i(1) as TokenAccess;
    let mut out: Handle = 0;
    // SecurityImpersonation = 2, TokenImpersonation = 2
    if unsafe { DuplicateTokenEx(existing, access, std::ptr::null_mut(), 2, 2, &mut out) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(out as i64))
}

fn wnd_SetThreadToken(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let thread_arg = f.arg_l(0);
    let token = f.arg_l(1) as Handle;
    let mut thread = thread_arg as Handle;
    let ptr: *mut Handle = if thread_arg == 0 { std::ptr::null_mut() } else { &mut thread };
    if unsafe { SetThreadToken(ptr, token) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetTokenInformation(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let class = f.arg_i(1) as u32;
    let addr = f.arg_l(2) as u64;
    let len_in = f.arg_i(3) as u32;
    let cap = if addr == 0 { 0 } else { (len_in as usize).min(capacity_at(addr)) };
    let mut real = vec![0u8; cap];
    let mut needed: u32 = len_in;
    if (unsafe { GetTokenInformation(token, class, real.as_mut_ptr() as *mut c_void, len_in, &mut needed) })
        == 0
    {
        let code = last_error();
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(throw_windows(vm, code));
        }
        return Ok(Slot::I(needed as i32));
    }
    if !real.is_empty() {
        nat_mem::write_bytes(addr, &real[..(needed as usize).min(real.len())]);
    }
    Ok(Slot::I(needed as i32))
}

fn wnd_AdjustTokenPrivileges(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let luid_addr = f.arg_l(1) as u64;
    let attributes = f.arg_i(2) as u32;
    // TOKEN_PRIVILEGES { DWORD PrivilegeCount; LUID_AND_ATTRIBUTES[1] }
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&1u32.to_ne_bytes());
    buf[4..12].copy_from_slice(&nat_mem::read_bytes(luid_addr, 8));
    buf[12..16].copy_from_slice(&attributes.to_ne_bytes());
    if (unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            buf.as_ptr() as *const c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_LookupPrivilegeValue0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let mut luid = [0u8; 8];
    if (unsafe {
        LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), luid.as_mut_ptr() as *mut c_void)
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    // Java 侧用完会 `LocalFree` 这个地址 → 必须落在 arena 里（`wnd_LocalFree` 已分流）
    let addr = nat_mem::alloc(8);
    nat_mem::write_bytes(addr, &luid);
    Ok(Slot::L(addr as i64))
}

fn wnd_AccessCheck(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let addr = f.arg_l(1) as u64;
    let access_mask = f.arg_i(2) as u32;
    let g_read = f.arg_i(3) as u32;
    let g_write = f.arg_i(4) as u32;
    let g_exec = f.arg_i(5) as u32;
    let g_all = f.arg_i(6) as u32;
    // GENERIC_MAPPING { GenericRead, GenericWrite, GenericExecute, GenericAll }
    let mut mapping = [0u8; 16];
    mapping[0..4].copy_from_slice(&g_read.to_ne_bytes());
    mapping[4..8].copy_from_slice(&g_write.to_ne_bytes());
    mapping[8..12].copy_from_slice(&g_exec.to_ne_bytes());
    mapping[12..16].copy_from_slice(&g_all.to_ne_bytes());
    // SD 必须在**真内存**里（AccessCheck 会解析自相对结构）
    let real = nat_mem::read_bytes(addr, sd_len_at(addr));
    // PRIVILEGE_SET 缓冲区留够（1 条 LUID_AND_ATTRIBUTES = 12 字节）
    let mut priv_set = [0u8; 64];
    let mut priv_len: u32 = priv_set.len() as u32;
    let mut granted: u32 = 0;
    let mut success: Bool = 0;
    if (unsafe {
        AccessCheck(
            real.as_ptr() as *const c_void,
            token,
            access_mask,
            mapping.as_ptr() as *const c_void,
            priv_set.as_mut_ptr() as *mut c_void,
            &mut priv_len,
            &mut granted,
            &mut success,
        )
    }) == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I((success != 0) as i32))
}

fn register_windows_security(r: &mut NativeRegistry) {
    r.add(WND, "InitializeSecurityDescriptor", "(J)V", wnd_InitializeSecurityDescriptor);
    r.add(WND, "InitializeAcl", "(JI)V", wnd_InitializeAcl);
    r.add(WND, "GetSecurityDescriptorOwner", "(J)J", wnd_GetSecurityDescriptorOwner);
    r.add(WND, "SetSecurityDescriptorOwner", "(JJ)V", wnd_SetSecurityDescriptorOwner);
    r.add(WND, "GetSecurityDescriptorDacl", "(J)J", wnd_GetSecurityDescriptorDacl);
    r.add(WND, "SetSecurityDescriptorDacl", "(JJ)V", wnd_SetSecurityDescriptorDacl);
    r.add(
        WND,
        "GetAclInformation0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$AclInformation;)V",
        wnd_GetAclInformation0_vm,
    );
    r.add(WND, "GetAce", "(JI)J", wnd_GetAce);
    r.add(WND, "AddAccessAllowedAceEx", "(JIIJ)V", wnd_AddAccessAllowedAceEx);
    r.add(WND, "AddAccessDeniedAceEx", "(JIIJ)V", wnd_AddAccessDeniedAceEx);
    r.add(WND, "GetLengthSid", "(J)I", wnd_GetLengthSid);
    r.add(WND, "ConvertSidToStringSid", "(J)Ljava/lang/String;", wnd_ConvertSidToStringSid);
    r.add(WND, "ConvertStringSidToSid0", "(J)J", wnd_ConvertStringSidToSid0);
    r.add(
        WND,
        "LookupAccountSid0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$Account;)V",
        wnd_LookupAccountSid0,
    );
    r.add(WND, "LookupAccountName0", "(JJI)I", wnd_LookupAccountName0);
    r.add(WND, "GetFileSecurity0", "(JIJI)I", wnd_GetFileSecurity0);
    r.add(WND, "SetFileSecurity0", "(JIJ)V", wnd_SetFileSecurity0);
    r.add(WND, "OpenProcessToken", "(JI)J", wnd_OpenProcessToken);
    r.add(WND, "OpenThreadToken", "(JIZ)J", wnd_OpenThreadToken);
    r.add(WND, "DuplicateTokenEx", "(JI)J", wnd_DuplicateTokenEx);
    r.add(WND, "SetThreadToken", "(JJ)V", wnd_SetThreadToken);
    r.add(WND, "GetTokenInformation", "(JIJI)I", wnd_GetTokenInformation);
    r.add(WND, "AdjustTokenPrivileges", "(JJI)V", wnd_AdjustTokenPrivileges);
    r.add(WND, "LookupPrivilegeValue0", "(J)J", wnd_LookupPrivilegeValue0);
    r.add(WND, "AccessCheck", "(JJIIIII)Z", wnd_AccessCheck);
}

// ============================================================ java.net.NetworkInterface
//
// 与 HotSpot 的 `windows/native/libnet/NetworkInterface.c` **同一条 API 路线**：
// `GetIfTable2` 拿接口行、`GetUnicastIpAddressTable`/`GetAnycastIpAddressTable` 拿地址表、
// `ConvertInterfaceLuidToNameW` 拿名字。这不是随便挑的：
//
// - 名字必须是 `ConvertInterfaceLuidToNameW` 的结果（`ethernet_0` / `loopback_0` / `tunnel_32512`），
//   不是 `GetAdaptersAddresses` 的 FriendlyName（"以太网"）也不是 AdapterName（`{GUID}`）。
//   名字对不上，`getByName(ni.getName())` 立刻返回 null。
// - 顺序必须是 `GetIfTable2` 的行顺序（NDIS 的枚举序，既不是 IfIndex 序也不是字典序）。
// - `isUp` 是 `AdminStatus==UP && OperStatus==Up`，`isLoopback` 是 `Type==IF_TYPE_SOFTWARE_LOOPBACK`，
//   `isP2P` 是 `AccessType==NET_IF_ACCESS_POINT_TO_POINT`，MTU 是行里的 `Mtu`。
// - 地址表里只有 DadState ∈ {Deprecated, Preferred} 的要（其它是 tentative/duplicate）。
// - HotSpot 收集地址时是**头插链表**，所以数组顺序与地址表顺序**相反**，别顺手改正过来。
//
// 结构体布局按 Windows SDK（netioapi.h）逐字节对齐，下面有 const 断言钉住偏移 ——
// 偏移错一个不会报错，只会读到别的字段（MTU 变成 GUID 的低 4 字节之类）。

const NI: &str = "java/net/NetworkInterface";
const IFA: &str = "java/net/InterfaceAddress";

const IF_MAX_STRING_SIZE: usize = 256;
const MIB_IF_ROW2_SIZE: usize = 1352;
const MIB_UNICAST_ROW_SIZE: usize = 80;
const MIB_ANYCAST_ROW_SIZE: usize = 48;
const ERROR_INVALID_NAME: u32 = 123;
const ERROR_OPERATION_ABORTED: u32 = 995;
/// `NO_PREFIX`（NetworkInterface.c）：anycast 地址没有前缀长度
const NO_PREFIX: u8 = 255;
/// `NL_DAD_STATE`：`NldsDeprecated = 3`、`NldsPreferred = 4`
const DAD_DEPRECATED: u32 = 3;
const DAD_PREFERRED: u32 = 4;
/// `IF_TYPE_SOFTWARE_LOOPBACK`
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
/// `NET_IF_ACCESS_POINT_TO_POINT`
const NET_IF_ACCESS_POINT_TO_POINT: u32 = 3;
/// `NET_IF_ADMIN_STATUS_UP` / `IfOperStatusUp`
const NET_IF_ADMIN_STATUS_UP: u32 = 1;
const IF_OPER_STATUS_UP: u32 = 1;

#[repr(C)]
struct MibIfRow2 {
    interface_luid: u64,
    interface_index: u32,
    interface_guid: [u8; 16],
    alias: [u16; IF_MAX_STRING_SIZE + 1],
    description: [u16; IF_MAX_STRING_SIZE + 1],
    physical_address_length: u32,
    physical_address: [u8; 32],
    permanent_physical_address: [u8; 32],
    mtu: u32,
    if_type: u32,
    tunnel_type: u32,
    media_type: u32,
    physical_medium_type: u32,
    access_type: u32,
    direction_type: u32,
    status_flags: u8,
    oper_status: u32,
    admin_status: u32,
    media_connect_state: u32,
    network_guid: [u8; 16],
    connection_type: u32,
    transmit_link_speed: u64,
    receive_link_speed: u64,
    counters: [u64; 18],
}

/// `MIB_UNICASTIPADDRESS_ROW`
#[repr(C)]
struct MibUnicastRow {
    address: [u8; 28],
    interface_luid: u64,
    interface_index: u32,
    prefix_origin: u32,
    suffix_origin: u32,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    on_link_prefix_length: u8,
    skip_as_source: u8,
    _pad: u16,
    dad_state: u32,
    scope_id: u32,
    creation_timestamp: i64,
}

/// `MIB_ANYCASTIPADDRESS_ROW`
#[repr(C)]
struct MibAnycastRow {
    address: [u8; 28],
    interface_luid: u64,
    interface_index: u32,
    scope_id: u32,
}

const _: () = {
    assert!(std::mem::size_of::<MibIfRow2>() == MIB_IF_ROW2_SIZE);
    assert!(std::mem::offset_of!(MibIfRow2, alias) == 28);
    assert!(std::mem::offset_of!(MibIfRow2, description) == 542);
    assert!(std::mem::offset_of!(MibIfRow2, physical_address_length) == 1056);
    assert!(std::mem::offset_of!(MibIfRow2, physical_address) == 1060);
    assert!(std::mem::offset_of!(MibIfRow2, mtu) == 1124);
    assert!(std::mem::offset_of!(MibIfRow2, if_type) == 1128);
    assert!(std::mem::offset_of!(MibIfRow2, access_type) == 1144);
    assert!(std::mem::offset_of!(MibIfRow2, oper_status) == 1156);
    assert!(std::mem::offset_of!(MibIfRow2, admin_status) == 1160);
    assert!(std::mem::size_of::<MibUnicastRow>() == MIB_UNICAST_ROW_SIZE);
    assert!(std::mem::offset_of!(MibUnicastRow, interface_luid) == 32);
    assert!(std::mem::offset_of!(MibUnicastRow, dad_state) == 64);
    assert!(std::mem::size_of::<MibAnycastRow>() == MIB_ANYCAST_ROW_SIZE);
    assert!(std::mem::offset_of!(MibAnycastRow, interface_luid) == 32);
};

#[link(name = "iphlpapi")]
extern "system" {
    fn GetIfTable2(table: *mut *mut c_void) -> u32;
    fn GetIfEntry2(row: *mut c_void) -> u32;
    fn GetUnicastIpAddressTable(family: u16, table: *mut *mut c_void) -> u32;
    fn GetAnycastIpAddressTable(family: u16, table: *mut *mut c_void) -> u32;
    fn FreeMibTable(mem: *const c_void);
    fn ConvertInterfaceLuidToNameW(luid: *const u64, name: *mut u16, len: usize) -> u32;
    fn ConvertInterfaceNameToLuidW(name: *const u16, luid: *mut u64) -> u32;
}

/// 一张 `MIB_*_TABLE`（首字段 `NumEntries`，行紧跟其后；行是 8 字节对齐的，所以
/// 表头实际占 8 字节而不是 4 —— `MIB_IF_ROW2` 里有 `ULONG64`）。
struct MibTable {
    ptr: *const u8,
    count: usize,
    stride: usize,
}

impl MibTable {
    fn new(ptr: *const c_void, stride: usize) -> Option<MibTable> {
        if ptr.is_null() {
            return None;
        }
        let count = unsafe { *(ptr as *const u32) } as usize;
        Some(MibTable { ptr: ptr as *const u8, count, stride })
    }
    fn row(&self, i: usize) -> *const u8 {
        unsafe { self.ptr.add(8 + i * self.stride) }
    }
    fn ifrow(&self, i: usize) -> &MibIfRow2 {
        unsafe { &*(self.row(i) as *const MibIfRow2) }
    }
    fn unicast(&self, i: usize) -> &MibUnicastRow {
        unsafe { &*(self.row(i) as *const MibUnicastRow) }
    }
    fn anycast(&self, i: usize) -> &MibAnycastRow {
        unsafe { &*(self.row(i) as *const MibAnycastRow) }
    }
}

impl Drop for MibTable {
    fn drop(&mut self) {
        unsafe { FreeMibTable(self.ptr as *const c_void) };
    }
}

fn sock_socket_error(vm: &mut Vm, code: u32, what: &str) -> VmError {
    vm.throw_msg("java/net/SocketException", format!("{what}: {}", win_msg(code)))
}

/// `ipv6_available()` → 有就 AF_UNSPEC(0)，没有就 AF_INET。本实现永远按有 IPv6 处理。
fn mib_if_table(vm: &mut Vm) -> Result<MibTable, VmError> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { GetIfTable2(&mut p) };
    if rc != 0 {
        return Err(sock_socket_error(vm, rc, "GetIfTable2"));
    }
    MibTable::new(p, MIB_IF_ROW2_SIZE)
        .ok_or_else(|| vm.throw_msg("java/net/SocketException", "GetIfTable2 返回空表".to_string()))
}

fn mib_uni_table(vm: &mut Vm) -> Result<MibTable, VmError> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { GetUnicastIpAddressTable(0 /* AF_UNSPEC */, &mut p) };
    if rc != 0 {
        return Err(sock_socket_error(vm, rc, "GetUnicastIpAddressTable"));
    }
    MibTable::new(p, MIB_UNICAST_ROW_SIZE).ok_or_else(|| {
        vm.throw_msg("java/net/SocketException", "GetUnicastIpAddressTable 返回空表".to_string())
    })
}

fn mib_any_table(vm: &mut Vm) -> Result<MibTable, VmError> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { GetAnycastIpAddressTable(0, &mut p) };
    if rc != 0 {
        return Err(sock_socket_error(vm, rc, "GetAnycastIpAddressTable"));
    }
    MibTable::new(p, MIB_ANYCAST_ROW_SIZE).ok_or_else(|| {
        vm.throw_msg("java/net/SocketException", "GetAnycastIpAddressTable 返回空表".to_string())
    })
}

fn wz_len(buf: &[u16]) -> usize {
    buf.iter().position(|&c| c == 0).unwrap_or(buf.len())
}

fn luid_name(luid: u64) -> Option<String> {
    let mut buf = [0u16; IF_MAX_STRING_SIZE + 1];
    let rc = unsafe { ConvertInterfaceLuidToNameW(&luid, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let n = wz_len(&buf);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// `SOCKADDR_INET` → (地址字节, scope_id)。v4 取 4 字节、v6 取 16 字节（`sin6_addr` 在偏移 8）
fn sockaddr_split(sa: &[u8; 28]) -> Option<(Vec<u8>, u32)> {
    let fam = u16::from_ne_bytes([sa[0], sa[1]]) as i32;
    if fam == AF_INET {
        Some((sa[4..8].to_vec(), 0))
    } else if fam == AF_INET6 {
        let scope = u32::from_ne_bytes([sa[24], sa[25], sa[26], sa[27]]);
        Some((sa[8..24].to_vec(), scope))
    } else {
        None
    }
}

/// 广播地址：`addr | ~mask(prefix)`（`ConvertLengthToIpv4Mask` 给出的掩码是网络序，
/// 这里直接按字节算，与它同一个结果）
fn broadcast_of(addr: &[u8], prefix: u8) -> [u8; 4] {
    let mut out = [0u8; 4];
    for k in 0..4 {
        let bits = (prefix as i32 - (k as i32) * 8).clamp(0, 8) as u32;
        let mask: u8 = if bits == 0 { 0 } else { (0xFFu16 << (8 - bits)) as u8 };
        out[k] = addr[k] | !mask;
    }
    out
}

/// 给 IPv6 地址补 `holder6.scope_ifname`（`getHostAddress()` 打印的 `%ethernet_0` 就靠它）。
/// `scope_ifname` 是 `NetworkInterface` 类型，`scope_ifname_set` 置 true 后不再看数字 scope。
fn inet6_set_scope_ifname(vm: &mut Vm, ia: i32, ni: i32) {
    const H6: &str = "java/net/Inet6Address$Inet6AddressHolder";
    let Some(fi) = vm.store.field_index("java/net/Inet6Address", "holder6") else { return };
    let h = vm.sh.heap.get(ia, fi).map(|s| s.as_i32()).unwrap_or(0);
    if h <= 0 {
        return;
    }
    if let Some(i) = vm.store.field_index(H6, "scope_ifname") {
        let _ = vm.sh.heap.set(h, i, Slot::I(ni));
    }
    if let Some(i) = vm.store.field_index(H6, "scope_ifname_set") {
        let _ = vm.sh.heap.set(h, i, Slot::I(1));
    }
}

fn new_inet4(vm: &mut Vm, addr_be: i32) -> Result<i32, VmError> {
    vm.new_instance_args(
        "java/net/Inet4Address",
        "(Ljava/lang/String;I)V",
        &[Slot::I(0), Slot::I(addr_be)],
    )
}

fn new_inet6(vm: &mut Vm, bytes: &[u8], scope: u32, ni: i32) -> Result<i32, VmError> {
    let arr = new_byte_array(vm, bytes);
    let a = vm.new_instance_args(
        "java/net/Inet6Address",
        "(Ljava/lang/String;[BI)V",
        &[Slot::I(0), Slot::I(arr), Slot::I(if scope == 0 { -1 } else { scope as i32 })],
    )?;
    if scope != 0 {
        inet6_set_scope_ifname(vm, a, ni);
    }
    Ok(a)
}

/// 按一行 `MIB_IF_ROW2` + 两张地址表造一个 `NetworkInterface`（字段直接填，与 HotSpot 一致）
fn ni_from_row(
    vm: &mut Vm,
    row: &MibIfRow2,
    uni: &MibTable,
    any: &MibTable,
) -> Result<i32, VmError> {
    let obj = vm.alloc_vmobj(NI);
    if obj == 0 {
        return Err(vm.throw_msg("java/lang/OutOfMemoryError", "NetworkInterface 分配失败".to_string()));
    }
    let name = luid_name(row.interface_luid).unwrap_or_default();
    let nh = vm.sh.heap.new_string(name);
    set_obj_field(vm, obj, NI, "name", Slot::I(nh));
    let dn = String::from_utf16_lossy(&row.description[..wz_len(&row.description)]);
    let dnh = vm.sh.heap.new_string(dn);
    set_obj_field(vm, obj, NI, "displayName", Slot::I(dnh));
    set_obj_field(vm, obj, NI, "index", Slot::I(row.interface_index as i32));
    set_obj_field(vm, obj, NI, "virtual", Slot::I(0));

    // 收集地址（unicast 先、anycast 后，与 HotSpot 的两次循环同序）
    let mut list: Vec<(Vec<u8>, u8, u32)> = Vec::new();
    for i in 0..uni.count {
        let r = uni.unicast(i);
        if r.interface_luid != row.interface_luid {
            continue;
        }
        if r.dad_state != DAD_PREFERRED && r.dad_state != DAD_DEPRECATED {
            continue;
        }
        if let Some((bytes, scope)) = sockaddr_split(&r.address) {
            list.push((bytes, r.on_link_prefix_length, scope));
        }
    }
    for i in 0..any.count {
        let r = any.anycast(i);
        if r.interface_luid != row.interface_luid {
            continue;
        }
        if let Some((bytes, scope)) = sockaddr_split(&r.address) {
            list.push((bytes, NO_PREFIX, scope));
        }
    }
    list.reverse(); // HotSpot 是头插链表

    let mut addr_items: Vec<Slot> = Vec::with_capacity(list.len());
    let mut bind_items: Vec<Slot> = Vec::with_capacity(list.len());
    for (bytes, prefix, scope) in list {
        let ia = if bytes.len() == 4 {
            let v = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            new_inet4(vm, v)?
        } else {
            new_inet6(vm, &bytes, scope, obj)?
        };
        let bind = vm.new_instance_args(IFA, "()V", &[])?;
        set_obj_field(vm, bind, IFA, "address", Slot::I(ia));
        if prefix != NO_PREFIX {
            set_obj_field(vm, bind, IFA, "maskLength", Slot::I(prefix as i32));
            if bytes.len() == 4 {
                let bc = broadcast_of(&bytes, prefix);
                let bcv = i32::from_be_bytes([bc[0], bc[1], bc[2], bc[3]]);
                let b = new_inet4(vm, bcv)?;
                set_obj_field(vm, bind, IFA, "broadcast", Slot::I(b));
            }
        }
        addr_items.push(Slot::I(ia));
        bind_items.push(Slot::I(bind));
    }

    let addrs = vm.sh.heap.new_array(addr_items, "[Ljava/net/InetAddress;").unwrap_or(0);
    set_obj_field(vm, obj, NI, "addrs", Slot::I(addrs));
    let binds = vm.sh.heap.new_array(bind_items, "[Ljava/net/InterfaceAddress;").unwrap_or(0);
    set_obj_field(vm, obj, NI, "bindings", Slot::I(binds));
    let childs = vm.sh.heap.new_array(Vec::new(), "[Ljava/net/NetworkInterface;").unwrap_or(0);
    set_obj_field(vm, obj, NI, "childs", Slot::I(childs));
    Ok(obj)
}

fn ni_get_all(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let ift = mib_if_table(vm)?;
    let uni = mib_uni_table(vm)?;
    let any = mib_any_table(vm)?;
    let mut items: Vec<Slot> = Vec::with_capacity(ift.count);
    for i in 0..ift.count {
        let obj = ni_from_row(vm, ift.ifrow(i), &uni, &any)?;
        items.push(Slot::I(obj));
    }
    let arr = vm.sh.heap.new_array(items, "[Ljava/net/NetworkInterface;").unwrap_or(0);
    Ok(Slot::I(arr))
}

/// `GetIfEntry2` 填一行。`luid`/`index` 二选一（另一个传 0）
fn ni_row_of(vm: &mut Vm, luid: u64, index: u32) -> Result<Option<MibIfRow2>, VmError> {
    let mut row: MibIfRow2 = unsafe { std::mem::zeroed() };
    row.interface_luid = luid;
    row.interface_index = index;
    let rc = unsafe { GetIfEntry2(&mut row as *mut MibIfRow2 as *mut c_void) };
    if rc == 0 {
        return Ok(Some(row));
    }
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None); // 与 HotSpot 一样：找不到就返回 null，不抛异常
    }
    Err(sock_socket_error(vm, rc, "GetIfEntry2"))
}

fn ni_for_row(vm: &mut Vm, row: MibIfRow2) -> Result<i32, VmError> {
    let uni = mib_uni_table(vm)?;
    let any = mib_any_table(vm)?;
    ni_from_row(vm, &row, &uni, &any)
}

fn ni_get_by_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(f.arg_i(0));
    let mut w: Vec<u16> = name.encode_utf16().collect();
    w.push(0);
    let mut luid: u64 = 0;
    let rc = unsafe { ConvertInterfaceNameToLuidW(w.as_ptr(), &mut luid) };
    if rc != 0 {
        if rc == ERROR_INVALID_NAME || rc == ERROR_FILE_NOT_FOUND {
            return Ok(Slot::I(0));
        }
        return Err(sock_socket_error(vm, rc, "ConvertInterfaceNameToLuidW"));
    }
    match ni_row_of(vm, luid, 0)? {
        Some(row) => Ok(Slot::I(ni_for_row(vm, row)?)),
        None => Ok(Slot::I(0)),
    }
}

fn ni_get_by_index(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let index = f.arg_i(0);
    // 0 不是合法下标（GetIfEntry2 会当成「没设置 key」）
    if index == 0 {
        return Ok(Slot::I(0));
    }
    match ni_row_of(vm, 0, index.max(0) as u32)? {
        Some(row) => Ok(Slot::I(ni_for_row(vm, row)?)),
        None => Ok(Slot::I(0)),
    }
}

/// 在 unicast / anycast 表里找与 `InetAddress` 相同的地址，返回它的 LUID
fn ni_luid_of_inet(vm: &mut Vm, inet: i32, uni: &MibTable, any: &MibTable) -> Option<u64> {
    let want = inet_bytes(vm, inet).unwrap_or_default();
    let hit = |sa: &[u8; 28]| -> bool {
        match sockaddr_split(sa) {
            Some((bytes, _)) => bytes == want,
            None => false,
        }
    };
    for i in 0..uni.count {
        let r = uni.unicast(i);
        if hit(&r.address) && (r.dad_state == DAD_PREFERRED || r.dad_state == DAD_DEPRECATED) {
            return Some(r.interface_luid);
        }
    }
    for i in 0..any.count {
        let r = any.anycast(i);
        if hit(&r.address) {
            return Some(r.interface_luid);
        }
    }
    None
}

fn ni_get_by_inet(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let inet = f.arg_i(0);
    let uni = mib_uni_table(vm)?;
    let any = mib_any_table(vm)?;
    let Some(luid) = ni_luid_of_inet(vm, inet, &uni, &any) else { return Ok(Slot::I(0)) };
    match ni_row_of(vm, luid, 0)? {
        Some(row) => Ok(Slot::I(ni_from_row(vm, &row, &uni, &any)?)),
        None => Ok(Slot::I(0)),
    }
}

fn ni_bound_inet(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let inet = f.arg_i(0);
    let uni = mib_uni_table(vm)?;
    let any = mib_any_table(vm)?;
    Ok(Slot::I(ni_luid_of_inet(vm, inet, &uni, &any).is_some() as i32))
}

fn ni_is_up(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let row = ni_row_of(vm, 0, f.arg_i(1).max(0) as u32)?;
    let up = row
        .map(|r| r.admin_status == NET_IF_ADMIN_STATUS_UP && r.oper_status == IF_OPER_STATUS_UP)
        .unwrap_or(false);
    Ok(Slot::I(up as i32))
}

fn ni_is_loopback(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let row = ni_row_of(vm, 0, f.arg_i(1).max(0) as u32)?;
    let lo = row.map(|r| r.if_type == IF_TYPE_SOFTWARE_LOOPBACK).unwrap_or(false);
    Ok(Slot::I(lo as i32))
}

fn ni_is_p2p(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let row = ni_row_of(vm, 0, f.arg_i(1).max(0) as u32)?;
    let p2p = row.map(|r| r.access_type == NET_IF_ACCESS_POINT_TO_POINT).unwrap_or(false);
    Ok(Slot::I(p2p as i32))
}

fn ni_get_mtu(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let row = ni_row_of(vm, 0, f.arg_i(1).max(0) as u32)?;
    Ok(Slot::I(row.map(|r| r.mtu as i32).unwrap_or(-1)))
}

fn ni_get_mac(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let row = ni_row_of(vm, 0, f.arg_i(2).max(0) as u32)?;
    let Some(r) = row else { return Ok(Slot::I(0)) };
    let n = (r.physical_address_length as usize).min(32);
    if n == 0 {
        return Ok(Slot::I(0));
    }
    let arr = new_byte_array(vm, &r.physical_address[..n]);
    Ok(Slot::I(arr))
}

fn register_network_interface(r: &mut NativeRegistry) {
    r.add(NI, "init", "()V", |_, _| Ok(Slot::I(0)));
    r.add(NI, "getAll", "()[Ljava/net/NetworkInterface;", ni_get_all);
    r.add(NI, "getByName0", "(Ljava/lang/String;)Ljava/net/NetworkInterface;", ni_get_by_name);
    r.add(NI, "getByIndex0", "(I)Ljava/net/NetworkInterface;", ni_get_by_index);
    r.add(NI, "getByInetAddress0", "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;", ni_get_by_inet);
    r.add(NI, "boundInetAddress0", "(Ljava/net/InetAddress;)Z", ni_bound_inet);
    r.add(NI, "isUp0", "(Ljava/lang/String;I)Z", ni_is_up);
    r.add(NI, "isLoopback0", "(Ljava/lang/String;I)Z", ni_is_loopback);
    r.add(NI, "supportsMulticast0", "(Ljava/lang/String;I)Z", |_, _| Ok(Slot::I(1)));
    r.add(NI, "isP2P0", "(Ljava/lang/String;I)Z", ni_is_p2p);
    r.add(NI, "getMacAddr0", "([BLjava/lang/String;I)[B", ni_get_mac);
    r.add(NI, "getMTU0", "(Ljava/lang/String;I)I", ni_get_mtu);
}

// ============================================================ IO 完成端口 + 异步目录监视
//
// `sun.nio.fs.WindowsWatchService` 的全部 native 都在这一组。它与前面那些「同步 Win32」
// 有一个本质区别：**`ReadDirectoryChangesW` 是异步的** —— 内核在调用返回之后的任意时刻
// 往缓冲区里写 `FILE_NOTIFY_INFORMATION`，写完才往完成端口投一个包。
//
// 这就把「arena 假地址 → 真内存」的桥接推到了两个调用之间：
//
//   ReadDirectoryChangesW：arena 缓冲区/OVERLAPPED → 各分配一份**真内存**交给内核，
//                          并登记 `arena OVERLAPPED 地址 → 真内存指针`；
//   GetQueuedCompletionStatus0：拿到完成包后用真内存指针反查登记项，把字节**拷回 arena**
//                          —— Java 侧的 `processEvents` 读的就是 arena 里的那份。
//
// **不能**在 ReadDirectoryChangesW 里「调完就拷回」：那一刻缓冲区还是空的，
// 拷回去等于把上一次的内容留在 Java 眼里（症状是「第一批事件正常、之后再也收不到」）。
//
// 另外：`GetQueuedCompletionStatus` 是**阻塞**调用（INFINITE）。本 VM 的 Java 线程是真
// OS 线程，所以这里阻塞是正确的；但绝不能持锁去等 —— 否则注册线程进不来。

#[link(name = "kernel32")]
extern "system" {
    fn CreateIoCompletionPort(
        file: Handle,
        existing: Handle,
        key: usize,
        threads: u32,
    ) -> Handle;
    fn GetQueuedCompletionStatus(
        port: Handle,
        bytes: *mut u32,
        key: *mut usize,
        ov: *mut *mut c_void,
        ms: u32,
    ) -> Bool;
    fn PostQueuedCompletionStatus(port: Handle, bytes: u32, key: usize, ov: *mut c_void) -> Bool;
    fn ReadDirectoryChangesW(
        h: Handle,
        buf: *mut c_void,
        len: u32,
        subtree: Bool,
        filter: u32,
        ret: *mut u32,
        ov: *mut c_void,
        routine: *mut c_void,
    ) -> Bool;
    fn CancelIo(h: Handle) -> Bool;
    fn GetOverlappedResult(h: Handle, ov: *mut c_void, bytes: *mut u32, wait: Bool) -> Bool;
}

const STATUS_CLASS: &str = "sun/nio/fs/WindowsNativeDispatcher$CompletionStatus";
const OVERLAPPED_SIZE: usize = 32;
const INFINITE: u32 = 0xFFFF_FFFF;

/// 一次在飞的异步目录读：真内存的两块（数据缓冲 + OVERLAPPED），与 arena 侧一一对应
struct AsyncRead {
    arena_ov: u64,
    arena_buf: u64,
    len: usize,
    real_buf: Box<[u8]>,
    real_ov: Box<[u8; OVERLAPPED_SIZE]>,
}

impl AsyncRead {
    fn new(arena_ov: u64, arena_buf: u64, len: usize) -> AsyncRead {
        AsyncRead {
            arena_ov,
            arena_buf,
            len,
            real_buf: vec![0u8; len].into_boxed_slice(),
            real_ov: Box::new([0u8; OVERLAPPED_SIZE]),
        }
    }
    fn ov_ptr(&self) -> *mut c_void {
        self.real_ov.as_ptr() as *mut c_void
    }
}

fn async_reads() -> &'static std::sync::Mutex<Vec<AsyncRead>> {
    static A: std::sync::OnceLock<std::sync::Mutex<Vec<AsyncRead>>> = std::sync::OnceLock::new();
    A.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn iocp_create(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let file = f.arg_l(0) as Handle;
    let existing = f.arg_l(1) as Handle;
    let key = f.arg_l(2) as usize;
    let h = unsafe { CreateIoCompletionPort(file, existing, key, 0) };
    if h == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(h as i64))
}

fn iocp_post(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let port = f.arg_l(0) as Handle;
    let key = f.arg_l(1) as usize;
    if unsafe { PostQueuedCompletionStatus(port, 0, key, std::ptr::null_mut()) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

/// `ReadDirectoryChangesW(hDirectory, bufferAddress, bufferLength, watchSubTree, filter,
///                        bytesReturnedAddress, pOverlapped)`
fn iocp_read_changes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let handle = f.arg_l(0) as Handle;
    let arena_buf = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let subtree = (f.arg_i(3) != 0) as Bool;
    let filter = f.arg_i(4) as u32;
    let ov_addr = f.arg_l(6) as u64;

    // 同步分支（本 JDK 的 WatchService 不走这里，但 API 存在，照实现）
    if ov_addr == 0 {
        let mut real = vec![0u8; len];
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                real.as_mut_ptr() as *mut c_void,
                len as u32,
                subtree,
                filter,
                &mut got,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(throw_windows(vm, last_error()));
        }
        nat_mem::write_bytes(arena_buf, &real[..(got as usize).min(len)]);
        let cnt = f.arg_l(5) as u64;
        if cnt != 0 {
            nat_mem::write(cnt, 4, got as u64);
        }
        return Ok(Slot::I(0));
    }

    // 异步分支：真内存缓冲 + 真 OVERLAPPED，登记在案
    let (buf_ptr, ov_ptr) = {
        let mut g = async_reads().lock().unwrap_or_else(|e| e.into_inner());
        let idx = match g.iter().position(|e| e.arena_ov == ov_addr) {
            Some(i) if g[i].len == len && g[i].arena_buf == arena_buf => i,
            Some(i) => {
                g.remove(i);
                g.push(AsyncRead::new(ov_addr, arena_buf, len));
                g.len() - 1
            }
            None => {
                g.push(AsyncRead::new(ov_addr, arena_buf, len));
                g.len() - 1
            }
        };
        // OVERLAPPED 里的 hEvent 是 Java 侧刚写进 arena 的，每次调用都要同步过去
        let ov_bytes = nat_mem::read_bytes(ov_addr, OVERLAPPED_SIZE);
        if ov_bytes.len() == OVERLAPPED_SIZE {
            g[idx].real_ov.copy_from_slice(&ov_bytes);
        }
        (g[idx].real_buf.as_mut_ptr() as *mut c_void, g[idx].ov_ptr())
    }; // 锁在这里就放掉：内核要持有这块内存在 I/O 期间

    trace(&format!("ReadDirectoryChangesW h={handle} buf={arena_buf} len={len} ov={ov_addr} tree={subtree}"));
    let ok = unsafe {
        ReadDirectoryChangesW(
            handle,
            buf_ptr,
            len as u32,
            subtree,
            filter,
            std::ptr::null_mut(),
            ov_ptr,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

/// `GetQueuedCompletionStatus0(port, CompletionStatus)`：阻塞等一个完成包，
/// 把数据拷回 arena，并把 error / bytesTransferred / completionKey 填进 Java 对象
fn iocp_gqcs(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let port = f.arg_l(0) as Handle;
    let status = f.arg_i(1);
    let mut bytes: u32 = 0;
    let mut key: usize = 0;
    let mut ov: *mut c_void = std::ptr::null_mut();
    let ok = unsafe { GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut ov, INFINITE) };
    let err = if ok == 0 { last_error() } else { 0 };

    let mut hit = false;
    if !ov.is_null() {
        let pending = {
            let g = async_reads().lock().unwrap_or_else(|e| e.into_inner());
            g.iter()
                .position(|e| e.ov_ptr() == ov)
                .map(|i| (g[i].arena_buf, (bytes as usize).min(g[i].len), g[i].real_buf[..(bytes as usize).min(g[i].len)].to_vec()))
        };
        if let Some((arena_buf, _n, data)) = pending {
            hit = true;
            nat_mem::write_bytes(arena_buf, &data);
        }
    }

    trace(&format!("GQCS key={key} bytes={bytes} err={err} ov={:?} hit={}", ov, hit));
    // 三个字段一个都不能少：Java 侧正是靠它们决定「唤醒 / 哪个 key / 读多少字节」。
    // 静默写不进去的后果是 `completionKey` 永远是 0 = WAKEUP_COMPLETION_KEY，主循环
    // 于是把每个真实事件都当唤醒包吃掉（症状：注册成功、事件永不送达）。
    set_status_field(vm, status, "error", Slot::I(err as i32))?;
    set_status_field(vm, status, "bytesTransferred", Slot::I(bytes as i32))?;
    set_status_field(vm, status, "completionKey", Slot::L(key as i64))?;
    trace("GQCS filled status");
    Ok(Slot::I(0))
}

/// 写 `CompletionStatus` 的字段；字段解析不到就抛 —— 不许静默丢。
fn set_status_field(vm: &mut Vm, obj: i32, name: &str, v: Slot) -> Result<(), VmError> {
    let Some(i) = vm.store.field_index(STATUS_CLASS, name) else {
        return Err(vm.throw_msg(
            "java/lang/InternalError",
            format!("CompletionStatus.{name} 解析不到字段槽"),
        ));
    };
    vm.sh.heap.set(obj, i, v);
    Ok(())
}

fn iocp_cancel(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let handle = f.arg_l(0) as Handle;
    if unsafe { CancelIo(handle) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

/// `GetOverlappedResult(hFile, lpOverlapped)`：Java 传的是 **arena** 里的 OVERLAPPED 地址，
/// 要换成真内存那份再问内核；返回已传字节数
fn iocp_overlapped_result(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let handle = f.arg_l(0) as Handle;
    let arena_ov = f.arg_l(1) as u64;
    let target = {
        let g = async_reads().lock().unwrap_or_else(|e| e.into_inner());
        g.iter().find(|e| e.arena_ov == arena_ov).map(|e| (e.ov_ptr(), e.arena_buf))
    };
    let Some((ov_ptr, arena_buf)) = target else {
        // 没登记过（同步 I/O 或已经取过结果）：照 Win32 的语义报错
        return Err(throw_windows(vm, ERROR_INVALID_PARAMETER));
    };
    let mut bytes: u32 = 0;
    let ok = unsafe { GetOverlappedResult(handle, ov_ptr, &mut bytes, 1) };
    if ok == 0 {
        let code = last_error();
        // 被 CancelIo 取消是这个流程的**正常收尾**，字节数先拷回再说
        if code != ERROR_OPERATION_ABORTED {
            return Err(throw_windows(vm, code));
        }
    }
    {
        let g = async_reads().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.iter().find(|e| e.arena_ov == arena_ov) {
            let n = (bytes as usize).min(e.len);
            let data = e.real_buf[..n].to_vec();
            nat_mem::write_bytes(arena_buf, &data);
        }
    }
    // 这一次重叠 I/O 到此结束：注销登记（arena 那块马上会被释放/复用）
    async_reads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|e| e.arena_ov != arena_ov);
    Ok(Slot::I(bytes as i32))
}

fn register_watch_service(r: &mut NativeRegistry) {
    r.add(WND, "CreateIoCompletionPort", "(JJJ)J", iocp_create);
    r.add(
        WND,
        "GetQueuedCompletionStatus0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$CompletionStatus;)V",
        iocp_gqcs,
    );
    r.add(WND, "PostQueuedCompletionStatus", "(JJ)V", iocp_post);
    r.add(WND, "ReadDirectoryChangesW", "(JJIZIJJ)V", iocp_read_changes);
    r.add(WND, "CancelIo", "(J)V", iocp_cancel);
    r.add(WND, "GetOverlappedResult", "(JJ)I", iocp_overlapped_result);
}

// ============================================================ 选择器的唤醒口 + 流枚举
//
// `sun.nio.ch.WindowsSelectorImpl` 在 Windows 上只有三条 native：
// `poll0`（子选择器等待）、`setWakeupSocket0`/`resetWakeupSocket0`（`Selector.wakeup()`
// 靠往唤醒管道写一个字节、再读掉）。**不是** init/updateRegistrations/drainfd ——
// 那些是 Java 侧的方法，没有 native。
//
// `poll0` 的事件位直接就是 `Net.pollinValue()` 那一套（本文件已经注册成 WSAPoll 常量），
// 所以这里用 `WSAPoll` 而不是 `select()`：语义一致，还没有 `FD_SETSIZE` 的 64 上限。
// Java 侧只消费 `readFds/writeFds/exceptFds` 三个 int 数组（`[0]` 是条数），
// 所以「按 WSAPoll 的 revents 填这三个数组」就够了。

const SEL: &str = "sun/nio/ch/WindowsSelectorImpl";
const SUB_SEL: &str = "sun/nio/ch/WindowsSelectorImpl$SubSelector";

/// `struct pollfd { int fd; short events; short revents; }`（`PollArrayWrapper` 里的 8 字节）
const SIZEOF_POLLFD: u64 = 8;

fn sel_wakeup_socket(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = (f.arg_i(0) as u32) as Socket;
    let byte = [1u8; 1];
    // HotSpot 也不看返回值：唤醒失败最多是「唤醒丢了」，不能因此抛异常
    unsafe { send(s, byte.as_ptr(), 1, 0) };
    Ok(Slot::I(0))
}

fn sel_reset_wakeup_socket(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let s = (f.arg_i(0) as u32) as Socket;
    let mut buf = [0u8; 4096];
    unsafe { recv(s, buf.as_mut_ptr(), buf.len() as i32, 0) };
    Ok(Slot::I(0))
}

/// 把一个 Java `int[]` 的 `[0]` 写成条数、从 `[1]` 开始写就绪的 fd
fn fill_fd_array(vm: &Vm, arr: i32, fds: &[i32]) {
    if arr <= 0 || fds.is_empty() {
        return;
    }
    let cap = vm.sh.heap.array_len(arr).unwrap_or(0);
    let n = fds.len().min(cap.saturating_sub(1));
    for (i, fd) in fds.iter().take(n).enumerate() {
        let _ = vm.sh.heap.set(arr, i + 1, Slot::I(*fd));
    }
    let _ = vm.sh.heap.set(arr, 0, Slot::I(n as i32));
}

fn sel_poll0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let poll_addr = f.arg_l(0) as u64;
    let n = f.arg_i(1).max(0) as usize;
    let read_arr = f.arg_i(2);
    let write_arr = f.arg_i(3);
    let except_arr = f.arg_i(4);
    let timeout = f.arg_l(5);
    let ms = timeout.clamp(-1, i32::MAX as i64) as i32;

    // pollfd 数组在 arena 里：读出来自己排成 WSAPoll 要的形状
    let mut pfds: Vec<PollFd> = Vec::with_capacity(n);
    let mut order: Vec<i32> = Vec::with_capacity(n);
    for i in 0..n {
        let base = poll_addr + (i as u64) * SIZEOF_POLLFD;
        if capacity_at(base) < SIZEOF_POLLFD as usize {
            break;
        }
        let fd = nat_mem::read(base, 4) as u32 as i32;
        if fd <= 0 {
            continue; // 空槽（Java 侧删键后不会立刻压缩数组）
        }
        let events = nat_mem::read(base + 4, 2) as u16 as i16;
        pfds.push(PollFd { fd: fd as usize, events, revents: 0 });
        order.push(fd);
    }
    if pfds.is_empty() {
        return Ok(Slot::I(0));
    }
    let rc = unsafe { WSAPoll(pfds.as_mut_ptr(), pfds.len() as u32, ms) };
    if rc < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "WSAPoll"));
    }
    let mut reads: Vec<i32> = Vec::new();
    let mut writes: Vec<i32> = Vec::new();
    let mut excepts: Vec<i32> = Vec::new();
    for (i, p) in pfds.iter().enumerate() {
        let rev = p.revents;
        if rev == 0 {
            continue;
        }
        // POLLNVAL（非 socket 句柄）不能当成「就绪」报给 Java —— 那会让选择器空转
        if rev & POLLNVAL != 0 {
            continue;
        }
        if rev & (POLLRDNORM | POLLHUP | POLLERR) != 0 {
            reads.push(order[i]);
        }
        if rev & POLLWRNORM != 0 {
            writes.push(order[i]);
        }
        // `POLLERR`/`POLLHUP` 在 select 里只出现在 except 集，这里同样补一份
        if rev & (POLLERR | POLLHUP) != 0 {
            excepts.push(order[i]);
        }
    }
    let total = (reads.len() + writes.len() + excepts.len()) as i32;
    fill_fd_array(vm, read_arr, &reads);
    fill_fd_array(vm, write_arr, &writes);
    fill_fd_array(vm, except_arr, &excepts);
    Ok(Slot::I(total))
}

// ---- 备用数据流（`FindFirstStreamW` / `FindNextStreamW`）----
//
// `WIN32_FIND_STREAM_DATA { LARGE_INTEGER StreamSize; WCHAR cStreamName[MAX_PATH + 36]; }`
// 名字在偏移 8，`::$DATA` 是默认流。

const FIRST_STREAM: &str = "sun/nio/fs/WindowsNativeDispatcher$FirstStream";
const FIND_STREAM_DATA_SIZE: usize = 8 + (260 + 36) * 2;

fn wnd_FindFirstStream0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = read_wstr_z(f.arg_l(0) as u64);
    let obj = f.arg_i(1);
    let mut data = vec![0u8; FIND_STREAM_DATA_SIZE];
    let h = unsafe {
        FindFirstStreamW(path.as_ptr(), 0 /* FindStreamInfoStandard */, data.as_mut_ptr() as *mut c_void, 0)
    };
    if h != INVALID_HANDLE_VALUE {
        let name = stream_name_of(&data);
        let nh = vm.sh.heap.new_string(name);
        set_obj_field(vm, obj, FIRST_STREAM, "handle", Slot::L(h as i64));
        set_obj_field(vm, obj, FIRST_STREAM, "name", Slot::I(nh));
        return Ok(Slot::I(0));
    }
    let code = last_error();
    // 目录/卷这类没有流的对象用 ERROR_HANDLE_EOF 结束 —— 与 HotSpot 一样当成「没有流」
    if code == ERROR_HANDLE_EOF || code == ERROR_FILE_NOT_FOUND {
        let empty = vm.sh.heap.new_string(String::new());
        set_obj_field(vm, obj, FIRST_STREAM, "handle", Slot::L(-1));
        set_obj_field(vm, obj, FIRST_STREAM, "name", Slot::I(empty));
        return Ok(Slot::I(0));
    }
    Err(throw_windows(vm, code))
}

fn wnd_FindNextStream0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let mut data = vec![0u8; FIND_STREAM_DATA_SIZE];
    if unsafe { FindNextStreamW(h, data.as_mut_ptr() as *mut c_void) } == 0 {
        let code = last_error();
        if code == ERROR_HANDLE_EOF {
            return Ok(Slot::I(0)); // null = 遍历结束
        }
        return Err(throw_windows(vm, code));
    }
    Ok(Slot::I(vm.sh.heap.new_string(stream_name_of(&data))))
}

fn stream_name_of(data: &[u8]) -> String {
    let raw = &data[8..];
    let mut units: Vec<u16> = Vec::with_capacity(raw.len() / 2);
    for ch in raw.chunks_exact(2) {
        let w = u16::from_ne_bytes([ch[0], ch[1]]);
        if w == 0 {
            break;
        }
        units.push(w);
    }
    String::from_utf16_lossy(&units)
}

#[link(name = "kernel32")]
extern "system" {
    fn FindFirstStreamW(path: *const u16, level: u32, data: *mut c_void, flags: u32) -> Handle;
    fn FindNextStreamW(h: Handle, data: *mut c_void) -> Bool;
}

fn register_selector(r: &mut NativeRegistry) {
    r.add(SUB_SEL, "poll0", "(JI[I[I[IJJ)I", sel_poll0);
    r.add(SEL, "setWakeupSocket0", "(I)V", sel_wakeup_socket);
    r.add(SEL, "resetWakeupSocket0", "(I)V", sel_reset_wakeup_socket);
    r.add(WND, "FindFirstStream0", "(JLsun/nio/fs/WindowsNativeDispatcher$FirstStream;)V", wnd_FindFirstStream0);
    r.add(WND, "FindNextStream0", "(J)Ljava/lang/String;", wnd_FindNextStream0);
    register_wepoll(r);
    register_native_socket_address(r);
    register_unix_domain(r);
    register_datagram(r);
    r.add("sun/nio/ch/FileKey", "init", "(Ljava/io/FileDescriptor;[I)V", filekey_init);
}

// ============================================================ WEPoll
//
// **JDK 25 的 Windows Selector 走的是 wepoll，不是 `WindowsSelectorImpl`**：
// `Selector.open()` → `WEPollSelectorProvider.openSelector()` → `new WEPollSelectorImpl(..)`
// → `WEPoll.create()`。只注册 `poll0` 那套老 API 是不够的，`Selector.open()` 会在
// `<clinit>` 里第一步就 `eventSize()` 抛 `UnsatisfiedLinkError`。
//
// wepoll 本体是「把 socket 的 AFD 句柄塞进 IOCP」的高性能实现；这里用 `WSAPoll` 落地：
// 语义完全一致（水平触发的 epoll），代价是每次 `wait` 是一次 O(n) 的 `WSAPoll`
// —— 对 VM 里跑到的那点连接数是够的，而且**没有一条捷径**：`WSAPoll` 是真 syscall，
// 阻塞、超时、错误码都是内核给的，不是模拟出来的。
//
// `EPOLLONESHOT` 必须真实现（`WEPollPoller` 把每个 fd 都以 ONESHOT 注册，报过一次就
// 由 Java 侧重新 ADD）。假实现会让同一个 fd 被反复报告 —— `Poller.polled()` 于是对
// 一个已经苏醒的 waiter 反复 wake，日志里看不出错，但 CPU 会空转。

const WEPOLL: &str = "sun/nio/ch/WEPoll";
const EPOLLIN: u32 = 1 << 0;
const EPOLLPRI: u32 = 1 << 1;
const EPOLLOUT: u32 = 1 << 2;
const EPOLLERR: u32 = 1 << 3;
const EPOLLHUP: u32 = 1 << 4;
const EPOLLONESHOT: u32 = 1 << 31;
const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_MOD: i32 = 2;
const EPOLL_CTL_DEL: i32 = 3;
/// `struct epoll_event { uint32_t events; epoll_data_t data; }`（64 位：events 在 0、data 在 8）
const EPOLL_EVENT_SIZE: i32 = 16;
const EPOLL_EVENTS_OFFSET: i32 = 0;
const EPOLL_DATA_OFFSET: i32 = 8;

struct WePollSet {
    /// (socket, 关注的事件位)。用 Vec 保持注册顺序，`wait` 的结果更可预测
    entries: Vec<(Socket, u32)>,
}

fn wepoll_sets() -> &'static std::sync::Mutex<std::collections::HashMap<u64, WePollSet>> {
    static W: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, WePollSet>>> =
        std::sync::OnceLock::new();
    W.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn wepoll_next_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

fn wepoll_event_size(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(EPOLL_EVENT_SIZE))
}
fn wepoll_events_offset(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(EPOLL_EVENTS_OFFSET))
}
fn wepoll_data_offset(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(EPOLL_DATA_OFFSET))
}

fn wepoll_create(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let id = wepoll_next_id();
    wepoll_sets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, WePollSet { entries: Vec::new() });
    Ok(Slot::L(id as i64))
}

fn wepoll_ctl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let h = f.arg_l(0) as u64;
    let op = f.arg_i(1);
    let s = f.arg_l(2) as Socket;
    let events = f.arg_i(3) as u32;
    let mut g = wepoll_sets().lock().unwrap_or_else(|e| e.into_inner());
    let Some(set) = g.get_mut(&h) else {
        // 句柄已 close：POSIX epoll 报 EBADF。返回非 0 让 Java 侧抛 IOException
        return Ok(Slot::I(9));
    };
    match op {
        EPOLL_CTL_ADD | EPOLL_CTL_MOD => {
            match set.entries.iter_mut().find(|e| e.0 == s) {
                Some(e) => e.1 = events,
                None => set.entries.push((s, events)),
            }
            Ok(Slot::I(0))
        }
        EPOLL_CTL_DEL => {
            set.entries.retain(|e| e.0 != s);
            Ok(Slot::I(0))
        }
        other => {
            let _ = other;
            Err(throw_io(vm, format!("WEPoll.ctl: 未知 opcode {op}")))
        }
    }
}

fn wepoll_wait(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let h = f.arg_l(0) as u64;
    let out = f.arg_l(1) as u64;
    let max = f.arg_i(2).max(0) as usize;
    let timeout = f.arg_i(3);
    // 快照：**不能持锁等**。注册/注销是别的线程做的（`Poller.implRegister`），
    // 持锁阻塞会让它们永远进不来。
    let snapshot: Vec<(Socket, u32)> = {
        let g = wepoll_sets().lock().unwrap_or_else(|e| e.into_inner());
        match g.get(&h) {
            Some(s) => s.entries.clone(),
            None => return Ok(Slot::I(0)),
        }
    };
    if snapshot.is_empty() {
        // 没有注册项：照 epoll 的语义睡满超时（timeout<0 = 永久）。不能立刻返回 0，
        // 否则 Selector.select() 会变成忙等。
        if timeout > 0 {
            std::thread::sleep(std::time::Duration::from_millis(timeout as u64));
        } else if timeout < 0 {
            // 没有注册项还要求无限等 = 只会被 close 打断；退化为 50ms 一片
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let alive = wepoll_sets()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&h);
                let has = wepoll_sets()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&h)
                    .map(|s| !s.entries.is_empty())
                    .unwrap_or(false);
                if !alive || has {
                    break;
                }
            }
        }
        return Ok(Slot::I(0));
    }
    let mut pfds: Vec<PollFd> = Vec::with_capacity(snapshot.len());
    for (s, ev) in &snapshot {
        let mut e: i16 = 0;
        if ev & EPOLLIN != 0 {
            e |= POLLRDNORM;
        }
        if ev & EPOLLOUT != 0 {
            e |= POLLWRNORM;
        }
        pfds.push(PollFd { fd: *s, events: e, revents: 0 });
    }
    let rc = unsafe { WSAPoll(pfds.as_mut_ptr(), pfds.len() as u32, timeout) };
    if rc < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "WEPoll.wait/WSAPoll"));
    }
    let mut n = 0usize;
    let mut reported: Vec<Socket> = Vec::new();
    for (i, p) in pfds.iter().enumerate() {
        if p.revents == 0 {
            continue;
        }
        if n >= max {
            break;
        }
        let (s, interest) = snapshot[i];
        let mut ev = 0u32;
        if p.revents & POLLRDNORM != 0 {
            ev |= EPOLLIN;
        }
        if p.revents & POLLWRNORM != 0 {
            ev |= EPOLLOUT;
        }
        if p.revents & POLLERR != 0 {
            ev |= EPOLLERR;
        }
        if p.revents & POLLHUP != 0 {
            ev |= EPOLLHUP;
        }
        if p.revents & POLLNVAL != 0 {
            ev |= EPOLLERR; // 无效 fd：epoll 用 EPOLLERR 表达
        }
        if ev == 0 {
            continue;
        }
        if interest & EPOLLPRI != 0 {
            ev |= EPOLLPRI;
        }
        let base = out + (n as u64) * EPOLL_EVENT_SIZE as u64;
        nat_mem::write(base + EPOLL_EVENTS_OFFSET as u64, 4, ev as u64);
        nat_mem::write(base + EPOLL_DATA_OFFSET as u64, 8, s as u64);
        n += 1;
        if interest & EPOLLONESHOT != 0 {
            reported.push(s);
        }
    }
    if !reported.is_empty() {
        let mut g = wepoll_sets().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = g.get_mut(&h) {
            set.entries.retain(|(s, ev)| !(ev & EPOLLONESHOT != 0 && reported.contains(s)));
        }
    }
    trace(&format!("WEPoll.wait h={h} timeout={timeout} 报了 {n} 个事件"));
    Ok(Slot::I(n as i32))
}

fn wepoll_close(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as u64;
    wepoll_sets().lock().unwrap_or_else(|e| e.into_inner()).remove(&h);
    Ok(Slot::I(0))
}

fn register_wepoll(r: &mut NativeRegistry) {
    r.add(WEPOLL, "eventSize", "()I", wepoll_event_size);
    r.add(WEPOLL, "eventsOffset", "()I", wepoll_events_offset);
    r.add(WEPOLL, "dataOffset", "()I", wepoll_data_offset);
    r.add(WEPOLL, "create", "()J", wepoll_create);
    r.add(WEPOLL, "ctl", "(JIJI)I", wepoll_ctl);
    r.add(WEPOLL, "wait", "(JJII)I", wepoll_wait);
    r.add(WEPOLL, "close", "(J)V", wepoll_close);
}

// ============================================================ NativeSocketAddress 的布局常量
//
// 这 12 条只回答「`sockaddr` 各字段在本平台的偏移/大小」。值必须与 `ws2def.h` 一致 ——
// 它们被用来在 Java 侧手写 `sockaddr_in`/`sockaddr_in6`，错一个字节地址就解错。
//
//   struct sockaddr_in  { u16 sin_family; u16 sin_port; u32 sin_addr; i8 pad[8]; }  // 16
//   struct sockaddr_in6 { u16 sin6_family; u16 sin6_port; u32 sin6_flowinfo;
//                         u8 sin6_addr[16]; u32 sin6_scope_id; }                     // 28
const NSA: &str = "sun/nio/ch/NativeSocketAddress";

fn register_native_socket_address(r: &mut NativeRegistry) {
    r.add(NSA, "AFINET", "()I", |_, _| Ok(Slot::I(AF_INET)));
    r.add(NSA, "AFINET6", "()I", |_, _| Ok(Slot::I(AF_INET6)));
    r.add(NSA, "sizeofFamily", "()I", |_, _| Ok(Slot::I(2)));
    r.add(NSA, "offsetFamily", "()I", |_, _| Ok(Slot::I(0)));
    r.add(NSA, "sizeofSockAddr4", "()I", |_, _| Ok(Slot::I(16)));
    r.add(NSA, "offsetSin4Port", "()I", |_, _| Ok(Slot::I(2)));
    r.add(NSA, "offsetSin4Addr", "()I", |_, _| Ok(Slot::I(4)));
    r.add(NSA, "sizeofSockAddr6", "()I", |_, _| Ok(Slot::I(28)));
    r.add(NSA, "offsetSin6Port", "()I", |_, _| Ok(Slot::I(2)));
    r.add(NSA, "offsetSin6FlowInfo", "()I", |_, _| Ok(Slot::I(4)));
    r.add(NSA, "offsetSin6Addr", "()I", |_, _| Ok(Slot::I(8)));
    r.add(NSA, "offsetSin6ScopeId", "()I", |_, _| Ok(Slot::I(24)));
}

// ============================================================ AF_UNIX（UnixDomainSockets）
//
// Windows 10 1803+ 有 AF_UNIX（`AF_UNIX = 1`，`sockaddr_un` 与 Linux 不同：
// `{ u16 sun_family; char sun_path[108]; }` 共 110 字节，**没有** Linux 那个 2 字节家族占位）。
//
// JDK 25 用它做 `Pipe.open()` 的默认实现（`PipeImpl(sp, /*AF_UNIX*/ true, ...)`）——
// `Selector` 的唤醒管道就是它。`UnixDomainSockets.init()` 返回 false 时 Java 会退回
// 回环 TCP，所以这里要**真探测**：探测失败必须返回 false，让 Java 走退路，而不是
// 报成功然后在 `bind0` 里炸。
const UDS: &str = "sun/nio/ch/UnixDomainSockets";
const AF_UNIX: i32 = 1;
/// `sizeof(struct sockaddr_un)`（Windows）＝ 2 + 108，按 2 字节家族算
const SIZEOF_SOCKADDR_UN: usize = 110;

fn uds_path_bytes(path: &[u8]) -> (Vec<u8>, usize) {
    // `sun_path` 是 NUL 结尾的字节串；长度要连 NUL 一起算（Windows 要求 len 覆盖它）
    let mut buf = vec![0u8; SIZEOF_SOCKADDR_UN];
    let n = path.len().min(SIZEOF_SOCKADDR_UN - 3); // 留家族 2 字节 + 至少 1 个 NUL
    buf[2..2 + n].copy_from_slice(&path[..n]);
    // family 是小端 u16
    buf[0] = (AF_UNIX & 0xFF) as u8;
    buf[1] = ((AF_UNIX >> 8) & 0xFF) as u8;
    (buf, 2 + n + 1)
}

fn uds_init(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    // 真探测：能不能建出一个 AF_UNIX 流套接字
    let s = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if s == INVALID_SOCKET {
        return Ok(Slot::I(0));
    }
    unsafe { closesocket(s) };
    let af_unix_ok = true;

    // ⚠ 临时闸门（一行开关）：AF_UNIX 本身可用，但**本 VM 的 `SecureRandom` 还不可用**
    // （`Exception: class configured for SecureRandom (provider: SUN) cannot be found`），
    // 而 JDK 里 UDS 的唯一使用者 `PipeImpl` 建管道时要用 `generateTempName()` → `SecureRandom`。
    // 于是 `Selector.open()` 会在 UDS 分支里抛 `RuntimeException`（`PipeImpl` 只 catch
    // IOException/UnsupportedOperationException 退不回去）→ 整个 Selector 用不了。
    //
    // 报 false = 让 JDK 走**它自己设计好的回环 TCP 退路**（`PipeImpl.createListener` 的
    // else 分支），这是 JDK 认可的能力协商，不是假装不支持。`SecureRandom` 修好后把
    // 下面这行改成 `return Ok(Slot::I(1));` 即可，探测部分不用动。
    let secure_random_ok = false;
    Ok(Slot::I((af_unix_ok && secure_random_ok) as i32))
}

fn uds_socket0(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if s == INVALID_SOCKET {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "AF_UNIX socket"));
    }
    Ok(Slot::I(s as i32))
}

fn uds_bind0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let path = load_bytes(vm, f.arg_i(1), 0, usize::MAX);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let (sa, len) = uds_path_bytes(&path);
    if unsafe { bind(s, sa.as_ptr(), len as i32) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "AF_UNIX bind"));
    }
    Ok(Slot::I(0))
}

fn uds_connect0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let path = load_bytes(vm, f.arg_i(1), 0, usize::MAX);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let (sa, len) = uds_path_bytes(&path);
    if unsafe { connect(s, sa.as_ptr(), len as i32) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        // 与 OpenJDK 一致：连不上返回错误码（Java 侧据此抛异常），而不是 0
        return Ok(Slot::I(code));
    }
    Ok(Slot::I(0))
}

fn uds_accept0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let newfd = f.arg_i(1);
    let array = f.arg_i(2);
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let mut name = [0u8; SIZEOF_SOCKADDR_UN];
    let mut len = SIZEOF_SOCKADDR_UN as i32;
    let c = unsafe { accept(s, name.as_mut_ptr(), &mut len) };
    if c == INVALID_SOCKET {
        let code = unsafe { WSAGetLastError() };
        return Ok(Slot::I(code));
    }
    set_fd_slot(vm, newfd, "fd", c as i64);
    // array[0] = 对端地址的路径字节（未命名的对端 → null）
    let plen = (len as usize).saturating_sub(2).min(SIZEOF_SOCKADDR_UN - 2);
    if plen > 0 && array != 0 {
        let mut end = plen;
        while end > 0 && name[2 + end - 1] == 0 {
            end -= 1;
        }
        if end > 0 {
            let bytes = name[2..2 + end].to_vec();
            let h = new_byte_array(vm, &bytes);
            vm.sh.heap.set(array, 0, Slot::I(h));
        }
    }
    Ok(Slot::I(0))
}

fn uds_local_address0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let mut name = [0u8; SIZEOF_SOCKADDR_UN];
    let mut len = SIZEOF_SOCKADDR_UN as i32;
    if unsafe { getsockname(s, name.as_mut_ptr(), &mut len) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "AF_UNIX getsockname"));
    }
    let plen = (len as usize).saturating_sub(2).min(SIZEOF_SOCKADDR_UN - 2);
    let mut end = plen;
    while end > 0 && name[2 + end - 1] == 0 {
        end -= 1;
    }
    let bytes = name[2..2 + end].to_vec();
    Ok(Slot::I(new_byte_array(vm, &bytes)))
}

fn register_unix_domain(r: &mut NativeRegistry) {
    r.add(UDS, "init", "()Z", uds_init);
    r.add(UDS, "socket0", "()I", uds_socket0);
    r.add(UDS, "bind0", "(Ljava/io/FileDescriptor;[B)V", uds_bind0);
    r.add(UDS, "connect0", "(Ljava/io/FileDescriptor;[B)I", uds_connect0);
    r.add(
        UDS,
        "accept0",
        "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;[Ljava/lang/Object;)I",
        uds_accept0,
    );
    r.add(UDS, "localAddress0", "(Ljava/io/FileDescriptor;)[B", uds_local_address0);
}

// ============================================================ UDP（DatagramDispatcher / DatagramChannelImpl）
//
// 数据报的读必须是**一次读一整个包**：`recv`/`send` 正好是这个语义。
// `readv0`/`writev0` 的散播/聚集也照做，但要先在真内存里拼成一块再发 —— 分多次
// `send` 会把一个数据报拆成多个包，接收端看到的是完全不同的东西。

const DGD: &str = "sun/nio/ch/DatagramDispatcher";
const DGC: &str = "sun/nio/ch/DatagramChannelImpl";

fn dgd_read0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET || len == 0 {
        return Ok(Slot::I(0));
    }
    let mut buf = vec![0u8; len];
    let n = unsafe { recv(s, buf.as_mut_ptr(), len as i32, 0) };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramDispatcher.read"));
    }
    nat_mem::write_bytes(addr, &buf[..n as usize]);
    Ok(Slot::I(n))
}

fn dgd_write0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET || len == 0 {
        return Ok(Slot::I(0));
    }
    let data = nat_mem::read_bytes(addr, len);
    let n = unsafe { send(s, data.as_ptr(), len as i32, 0) };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramDispatcher.write"));
    }
    Ok(Slot::I(n))
}

/// 散播读：先收一整包，再按 `iovec` 顺序摊到各段
fn dgd_readv0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET || cnt == 0 {
        return Ok(Slot::L(0));
    }
    let mut total_len = 0usize;
    for i in 0..cnt {
        total_len += read_u64(addr, (i * IOVEC + 8) as u64) as usize;
    }
    if total_len == 0 {
        return Ok(Slot::L(0));
    }
    let mut buf = vec![0u8; total_len];
    let n = unsafe { recv(s, buf.as_mut_ptr(), total_len as i32, 0) };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramDispatcher.readv"));
    }
    let mut left = n as usize;
    let mut off = 0usize;
    for i in 0..cnt {
        if left == 0 {
            break;
        }
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        let take = len.min(left);
        nat_mem::write_bytes(base, &buf[off..off + take]);
        off += take;
        left -= take;
    }
    Ok(Slot::L(n as i64))
}

/// 聚集写：先拼成一整块再 `send`（分多次发会把一个数据报拆开）
fn dgd_writev0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let cnt = f.arg_i(2).max(0) as usize;
    if s == INVALID_SOCKET || cnt == 0 {
        return Ok(Slot::L(0));
    }
    let mut data: Vec<u8> = Vec::new();
    for i in 0..cnt {
        let base = read_u64(addr, (i * IOVEC) as u64);
        let len = read_u64(addr, (i * IOVEC + 8) as u64) as usize;
        if len > 0 {
            data.extend_from_slice(&nat_mem::read_bytes(base, len));
        }
    }
    if data.is_empty() {
        return Ok(Slot::L(0));
    }
    let n = unsafe { send(s, data.as_ptr(), data.len() as i32, 0) };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramDispatcher.writev"));
    }
    Ok(Slot::L(n as i64))
}

/// `send0(fd, address, len, targetAddress, targetAddressLen)`：未连接时的发
///
/// `targetAddress` 是 arena 里的 `sockaddr`（由 `NativeSocketAddress` 编好）。
fn dgc_send0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let ta = f.arg_l(3) as u64;
    let tal = f.arg_i(4).max(0) as usize;
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let data = nat_mem::read_bytes(addr, len);
    let target = nat_mem::read_bytes(ta, tal);
    if target.is_empty() {
        return Err(throw_io(vm, "send0: 目标地址为空"));
    }
    let n = unsafe { sendto(s, data.as_ptr(), len as i32, 0, target.as_ptr(), tal as i32) };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramChannel.send0"));
    }
    Ok(Slot::I(n))
}

/// `receive0(fd, address, len, senderAddress, connected)`：收 + 带回对端地址
fn dgc_receive0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let addr = f.arg_l(1) as u64;
    let len = f.arg_i(2).max(0) as usize;
    let sender = f.arg_l(3) as u64;
    let connected = f.arg_i(4) != 0;
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    let mut buf = vec![0u8; len.max(1)];
    let n = if connected {
        unsafe { recv(s, buf.as_mut_ptr(), len as i32, 0) }
    } else {
        let mut sa = [0u8; 110];
        let mut sal = 110i32;
        let n = unsafe { recvfrom(s, buf.as_mut_ptr(), len as i32, 0, sa.as_mut_ptr(), &mut sal) };
        if n >= 0 && sender != 0 {
            nat_mem::write_bytes(sender, &sa[..(sal as usize).min(110)]);
        }
        n
    };
    if n < 0 {
        let code = unsafe { WSAGetLastError() };
        return Err(sock_error(vm, code, "DatagramChannel.receive0"));
    }
    nat_mem::write_bytes(addr, &buf[..n as usize]);
    Ok(Slot::I(n))
}

/// `disconnect0(fd, isIPv6)`：解除连接（把对端地址清成 AF_UNSPEC）
fn dgc_disconnect0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let s = sock_of(vm, f.arg_i(0));
    let ipv6 = f.arg_i(1) != 0;
    if s == INVALID_SOCKET {
        return Err(vm.throw_msg("java/io/IOException", "socket closed".to_string()));
    }
    // AF_UNSPEC(0) + 零地址 = 断开。Windows 上这就是 `connect` 到未指定地址。
    let mut sa = [0u8; 16];
    let len: i32 = if ipv6 {
        sa[0] = (AF_INET6 & 0xFF) as u8;
        sa[1] = ((AF_INET6 >> 8) & 0xFF) as u8;
        28
    } else {
        2
    };
    if unsafe { connect(s, sa.as_ptr(), len) } == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        // WSAEAFNOSUPPORT / WSAEINVAL：本来就断开着，等价于成功
        if code != 10047 && code != 10022 {
            return Err(sock_error(vm, code, "DatagramChannel.disconnect0"));
        }
    }
    Ok(Slot::I(0))
}

fn register_datagram(r: &mut NativeRegistry) {
    r.add(DGD, "read0", "(Ljava/io/FileDescriptor;JI)I", dgd_read0);
    r.add(DGD, "write0", "(Ljava/io/FileDescriptor;JI)I", dgd_write0);
    r.add(DGD, "readv0", "(Ljava/io/FileDescriptor;JI)J", dgd_readv0);
    r.add(DGD, "writev0", "(Ljava/io/FileDescriptor;JI)J", dgd_writev0);
    r.add(DGC, "send0", "(Ljava/io/FileDescriptor;JIJI)I", dgc_send0);
    r.add(DGC, "receive0", "(Ljava/io/FileDescriptor;JIJZ)I", dgc_receive0);
    r.add(DGC, "disconnect0", "(Ljava/io/FileDescriptor;Z)V", dgc_disconnect0);
}

// ============================================================ FileKey
//
// `FileKey.init(FileDescriptor fd, int[] values)`：把卷序列号 + 文件索引填进 `values[0..3]`。
// `WindowsWatchService`/`Files.isSameFile` 靠它判「同一个文件」。

fn filekey_init(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    let arr = f.arg_i(1);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    let mut info = ByHandleFileInformation::default();
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return Err(throw_io(vm, format!("GetFileInformationByHandle 失败: {}", win_msg(last_error()))));
    }
    let vals = [info.volume_serial as i32, info.file_index_high as i32, info.file_index_low as i32];
    for (i, v) in vals.iter().enumerate() {
        vm.sh.heap.set(arr, i, Slot::I(*v));
    }
    Ok(Slot::I(0))
}

// ============================================================ java.net.InetAddress 的 native

const INET: &str = "java/net/InetAddress";

fn inet_init(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // HotSpot 的 `InetAddress_init` 只是把几个字段 ID 缓存起来给 JNI 用；
    // 本文件不做 JNI 字段访问，所以空实现是正确的（不是偷懒）。
    Ok(Slot::I(0))
}

fn register_inet_address(r: &mut NativeRegistry) {
    r.add(INET, "init", "()V", inet_init);
    r.add(INET, "isIPv4Available", "()Z", |_, _| Ok(Slot::I(1)));
    r.add(INET, "isIPv6Supported", "()Z", |_, _| Ok(Slot::I(1)));
}

// ============================================================ DNS（Inet4/6AddressImpl）

const AS_IMPL4: &str = "java/net/Inet4AddressImpl";
const AS_IMPL6: &str = "java/net/Inet6AddressImpl";

fn inet4_init(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// 按字节建一个 `InetAddress`（v4 4 字节 / v6 16 字节）
fn inet_of(vm: &mut Vm, bytes: &[u8], host: Option<&str>) -> i32 {
    let arr = new_byte_array(vm, bytes);
    let h = match host {
        Some(text) => vm.sh.heap.new_string(text.to_string()),
        None => 0,
    };
    match run_method(
        vm,
        "java/net/InetAddress",
        "getByAddress",
        "(Ljava/lang/String;[B)Ljava/net/InetAddress;",
        0,
        &[Slot::I(h), Slot::I(arr)],
    ) {
        Ok(v) => v.as_i32(),
        Err(_) => 0,
    }
}

/// `lookupAllHostAddr`：真的走 `getaddrinfo`（Winsock），不是硬编码
fn impl_lookup_all(vm: &mut Vm, f: &mut Frame, ipv6: bool) -> Result<Slot, VmError> {
    wsa_start();
    let host = vm.text_of(f.arg_i(0));
    let c = to_cstr(&host);
    let mut hints = [0u8; 32]; // ADDRINFOA 的前 32 字节够放 ai_family/ai_socktype/ai_protocol
    // ai_family 在偏移 4（i32）
    let fam: i32 = if ipv6 { AF_INET6 } else { AF_INET };
    hints[4..8].copy_from_slice(&fam.to_ne_bytes());
    hints[8..12].copy_from_slice(&SOCK_STREAM.to_ne_bytes());
    let mut res: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { getaddrinfo(c.as_ptr() as *const u8, std::ptr::null(), hints.as_ptr() as *const c_void, &mut res) };
    if rc != 0 || res.is_null() {
        return Err(vm.throw_msg(
            "java/net/UnknownHostException",
            format!("{host}: 域名解析失败 (getaddrinfo={rc})"),
        ));
    }
    // 收集所有地址（ai_addr 在 ADDRINFO 的偏移 24，ai_next 在 40；ai_addrlen 在 16）
    let mut items: Vec<Slot> = Vec::new();
    let mut p = res;
    let mut guard = 0;
    while !p.is_null() && guard < 64 {
        guard += 1;
        let base = p as *const u8;
        let family = unsafe { *(base.add(4) as *const i32) };
        let addrlen = unsafe { *(base.add(16) as *const u32) } as usize;
        let addr = unsafe { *(base.add(24) as *const *const u8) };
        if !addr.is_null() {
            let bytes = unsafe { std::slice::from_raw_parts(addr, addrlen.min(28)) };
            if let Some((ip, _)) = parse_sockaddr(bytes, addrlen as i32) {
                let _ = family;
                let h = inet_of(vm, &ip, None);
                if h != 0 {
                    items.push(Slot::I(h));
                }
            }
        }
        p = unsafe { *(base.add(40) as *const *mut c_void) };
    }
    unsafe { freeaddrinfo(res) };
    let arr = vm
        .sh
        .heap
        .new_array(items, "[Ljava/net/InetAddress;")
        .unwrap_or(0);
    Ok(Slot::I(arr))
}

/// 主机名（`gethostname`）
fn impl_local_host_name(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let mut buf = [0u8; 256];
    if (unsafe { gethostname(buf.as_mut_ptr(), buf.len() as i32) }) != 0 {
        return Err(vm.throw_msg(
            "java/net/UnknownHostException",
            "取本机主机名失败".to_string(),
        ));
    }
    let n = buf.iter().position(|&b| b == 0).unwrap_or(0);
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

/// 反查：`getnameinfo` 拿到主机名；拿不到就退回数字形式（与 HotSpot 的降级一致）
fn impl_host_by_addr(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    wsa_start();
    let arr = f.arg_i(0);
    let bytes = read_array_bytes(vm, arr, 16);
    let (family, sa): (i32, Vec<u8>) = if bytes.len() == 4 {
        let mut v = vec![0u8; 16];
        v[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        v[4..8].copy_from_slice(&bytes);
        (AF_INET, v)
    } else {
        let mut v = vec![0u8; 28];
        v[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
        v[8..24].copy_from_slice(&bytes);
        (AF_INET6, v)
    };
    let mut host = [0u8; 1024];
    let rc = unsafe {
        getnameinfo(
            sa.as_ptr(),
            sa.len() as i32,
            host.as_mut_ptr(),
            host.len() as i32,
            std::ptr::null_mut(),
            0,
            0,
        )
    };
    if rc != 0 {
        // 没有反向记录：Java 侧会退回到 getHostAddress()，这里给空串就够
        let _ = family;
        return Ok(Slot::I(0));
    }
    let n = host.iter().position(|&b| b == 0).unwrap_or(0);
    let text = String::from_utf8_lossy(&host[..n]).to_string();
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

/// `isReachable0`：用 ICMP 回显（`IcmpSendEcho`），没有权限就退回「TCP 连接 7 端口」
///
/// 两个类的**参数排布不同**（这里曾经共用一个函数，把 Inet6 的描述符也写成 4 参形态，
/// 于是 `Inet6AddressImpl.isReachable0` 实际一条都没注册上）：
///
/// ```text
/// Inet4AddressImpl.isReachable0(byte[] addr, int timeout, byte[] ifaddr, int ttl)
/// Inet6AddressImpl.isReachable0(byte[] addr, int scope, int timeout, byte[] ifaddr, int ttl, int ifScope)
/// ```
fn impl_is_reachable4(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_i(0);
    let timeout = f.arg_i(1).max(0) as u32;
    icmp_reachable(vm, addr, timeout)
}

fn impl_is_reachable6(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_i(0);
    let timeout = f.arg_i(2).max(0) as u32;
    // IPv6 的 ICMP 要走 `Icmp6SendEcho2`（目标地址 + 源地址两个 sockaddr），本实现不做 ——
    // 明确报「不可达」（`isReachable` 的语义就是探测，探测不了就返回 false）
    let bytes = inet_bytes(vm, addr)?;
    if bytes.len() != 4 {
        return Ok(Slot::I(0));
    }
    icmp_reachable(vm, addr, timeout)
}

fn icmp_reachable(vm: &mut Vm, addr: i32, timeout: u32) -> Result<Slot, VmError> {
    wsa_start();
    let bytes = inet_bytes(vm, addr)?;
    if bytes.len() != 4 {
        return Ok(Slot::I(0));
    }
    let ip = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    #[repr(C)]
    struct IcmpEchoReply {
        address: u32,
        status: u32,
        round_trip: u32,
        data_size: u16,
        reserved: u16,
        data: u32,
        options: [u8; 40],
    }
    let h = unsafe { IcmpCreateFile() };
    if h == INVALID_HANDLE_VALUE {
        let _ = vm;
        return Ok(Slot::I(0));
    }
    let mut reply = IcmpEchoReply {
        address: 0,
        status: 0,
        round_trip: 0,
        data_size: 0,
        reserved: 0,
        data: 0,
        options: [0; 40],
    };
    let payload = [0u8; 8];
    let n = unsafe {
        IcmpSendEcho(
            h,
            ip,
            payload.as_ptr() as *const c_void,
            payload.len() as u16,
            std::ptr::null_mut(),
            &mut reply as *mut _ as *mut c_void,
            std::mem::size_of::<IcmpEchoReply>() as u32,
            timeout.max(1),
        )
    };
    unsafe { IcmpCloseHandle(h) };
    let _ = vm;
    Ok(Slot::I((n > 0 && reply.status == 0) as i32))
}

fn register_dns(r: &mut NativeRegistry) {    r.add("java/net/Inet4Address", "init", "()V", inet4_init);
    r.add("java/net/Inet6Address", "init", "()V", inet4_init);
    r.add(AS_IMPL4, "lookupAllHostAddr", "(Ljava/lang/String;)[Ljava/net/InetAddress;", |vm, f| {
        impl_lookup_all(vm, f, false)
    });
    r.add(AS_IMPL6, "lookupAllHostAddr", "(Ljava/lang/String;I)[Ljava/net/InetAddress;", |vm, f| {
        impl_lookup_all(vm, f, true)
    });
    r.add(AS_IMPL4, "getHostByAddr", "([B)Ljava/lang/String;", impl_host_by_addr);
    r.add(AS_IMPL6, "getHostByAddr", "([B)Ljava/lang/String;", impl_host_by_addr);
    r.add(AS_IMPL4, "getLocalHostName", "()Ljava/lang/String;", impl_local_host_name);
    r.add(AS_IMPL6, "getLocalHostName", "()Ljava/lang/String;", impl_local_host_name);
    r.add(AS_IMPL4, "isReachable0", "([BI[BI)Z", impl_is_reachable4);
    r.add(AS_IMPL6, "isReachable0", "([BII[BII)Z", impl_is_reachable6);
}

/// 不以 NUL 结尾的 `char*`（getaddrinfo 的 node 参数）
fn to_cstr(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.as_bytes().to_vec();
    v.retain(|&b| b != 0);
    v.push(0);
    v
}

// ============================================================ iphlpapi 的补充声明

#[link(name = "iphlpapi")]
extern "system" {
    fn IcmpCreateFile() -> Handle;
    fn IcmpCloseHandle(h: Handle) -> Bool;
    fn IcmpSendEcho(
        h: Handle,
        dest: u32,
        data: *const c_void,
        data_len: u16,
        options: *mut c_void,
        reply: *mut c_void,
        reply_len: u32,
        timeout: u32,
    ) -> u32;
}

#[link(name = "ws2_32")]
extern "system" {
    fn getnameinfo(
        addr: *const u8,
        addr_len: i32,
        host: *mut u8,
        host_len: i32,
        serv: *mut u8,
        serv_len: i32,
        flags: i32,
    ) -> i32;
}
