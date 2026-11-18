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
    register_ioutil(r);
    register_net(r);
    register_socket_dispatcher(r);
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
    // FileChannel.map/unmap：本 VM 的地址是 arena 句柄，没有真内存映射 —— 明确报错
    r.add(FDP, "map0", "(Ljava/io/FileDescriptor;IJJZ)J", fdp_map_unsupported);
    r.add(FDP, "unmap0", "(JJ)I", |_, _| Ok(Slot::I(0)));
    r.add(FDP, "maxDirectTransferSize0", "()I", |_, _| Ok(Slot::I(16 * 1024 * 1024)));
    r.add(
        FDP,
        "transferTo0",
        "(Ljava/io/FileDescriptor;JJLjava/io/FileDescriptor;Z)J",
        fdp_transfer_to_unsupported,
    );
    r.add(FDP, "setDirect0", "(Ljava/io/FileDescriptor;Ljava/nio/CharBuffer;)I", |_, _| Ok(Slot::I(0)));
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

fn fdp_map_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(throw_io(
        vm,
        "FileChannel.map 未实现：本 VM 的 native 地址是 arena 句柄，没有真内存映射",
    ))
}

fn fdp_transfer_to_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(throw_io(vm, "FileChannel.transferTo 未实现"))
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

fn net_block_or_unblock4(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(vm.throw_msg(
        "java/io/IOException",
        "IPv4 组播源过滤（blockOrUnblock4）未实现".to_string(),
    ))
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

fn net_block_or_unblock6(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(vm.throw_msg(
        "java/io/IOException",
        "IPv6 组播源过滤（blockOrUnblock6）未实现".to_string(),
    ))
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
// `Files.isWritable` / `Files.getFileAttributeView(AclFileAttributeView)` /
// `Files.setOwner` 全都落在这组 native 上。结构体一律由 Java 侧用
// `WindowsSecurityDescriptor`（`Unsafe.allocateMemory`）分配，本文件只读写它的地址 ——
// 所以这些地址同样在 `nat_mem` 的 arena 里。
//
// 布局（与 Win32 一致，别改）：
//   SECURITY_DESCRIPTOR              20 字节（Revision/Sbz1/Control 各 1 字节 + Owner/Group/Sacl/Dacl 指针）
//   GENERIC_MAPPING                  16 字节（4 个 DWORD）
//   PRIVILEGE_SET                    8 + 12*N
//   TOKEN_PRIVILEGES                 4 + 12*N（LUID 8 + Attributes 4）

const SE_FILE_OBJECT: i32 = 1;

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

const WND_ACL_INFO: &str = "sun/nio/fs/WindowsNativeDispatcher$AclInformation";
const WND_ACCOUNT: &str = "sun/nio/fs/WindowsNativeDispatcher$Account";

fn wnd_InitializeSecurityDescriptor(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as *mut c_void;
    // SECURITY_DESCRIPTOR_REVISION = 1
    if unsafe { InitializeSecurityDescriptor(addr, 1) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_InitializeAcl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *mut c_void;
    let size = f.arg_i(1) as u32;
    // ACL_REVISION = 2
    if unsafe { InitializeAcl(acl, size, 2) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_SetSecurityDescriptorOwner(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sd = f.arg_l(0) as *mut c_void;
    let owner = f.arg_l(1) as *const c_void;
    if unsafe { SetSecurityDescriptorOwner(sd, owner, 0) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_SetSecurityDescriptorDacl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sd = f.arg_l(0) as *mut c_void;
    let dacl = f.arg_l(1) as *const c_void;
    if unsafe { SetSecurityDescriptorDacl(sd, 1, dacl, 0) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

/// `GetSecurityDescriptorOwner(sd)` → SID 地址（没有就 0）
fn wnd_GetSecurityDescriptorOwner(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sd = f.arg_l(0) as *const c_void;
    let mut owner: *mut c_void = std::ptr::null_mut();
    let mut defaulted: Bool = 0;
    if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(owner as u64 as i64))
}

fn wnd_GetSecurityDescriptorDacl(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sd = f.arg_l(0) as *const c_void;
    let mut present: Bool = 0;
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut defaulted: Bool = 0;
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let _ = vm;
    Ok(Slot::L(if present != 0 { dacl as u64 as i64 } else { 0 }))
}

fn wnd_GetAclInformation0(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *const c_void;
    let obj = f.arg_i(1);
    // ACL_INFORMATION_CLASS = AclSizeInformation(2) → ACL_SIZE_INFORMATION：
    // DWORD AceCount; DWORD AclBytesInUse; DWORD AclBytesFree;
    let mut info = [0u32; 3];
    let ok = unsafe {
        GetAclInformation(acl, info.as_mut_ptr() as *mut c_void, 12, 2)
    };
    if ok == 0 {
        return Ok(Slot::I(0));
    }
    // 无 vm 可用（上面没取）—— 这里改成按名字写字段
    Ok(Slot::I(info[0] as i32))
}

fn wnd_GetAclInformation0_vm(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *const c_void;
    let obj = f.arg_i(1);
    let mut info = [0u32; 3];
    if unsafe { GetAclInformation(acl, info.as_mut_ptr() as *mut c_void, 12, 2) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    set_obj_field(vm, obj, WND_ACL_INFO, "aceCount", Slot::I(info[0] as i32));
    Ok(Slot::I(0))
}

fn wnd_GetAce(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *const c_void;
    let index = f.arg_i(1) as u32;
    let mut ace: *mut c_void = std::ptr::null_mut();
    if unsafe { GetAce(acl, index, &mut ace) } == 0 {
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(ace as u64 as i64))
}

fn wnd_AddAccessAllowedAceEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *mut c_void;
    let flags = f.arg_i(1) as u32;
    let mask = f.arg_i(2) as u32;
    let sid = f.arg_l(3) as *const c_void;
    // ACL_REVISION_DS = 4（带对象 ACE 时要 4，普通 allow ACE 用 2 也行；HotSpot 用 4）
    if unsafe { AddAccessAllowedAceEx(acl, 4, flags, mask, sid) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_AddAccessDeniedAceEx(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let acl = f.arg_l(0) as *mut c_void;
    let flags = f.arg_i(1) as u32;
    let mask = f.arg_i(2) as u32;
    let sid = f.arg_l(3) as *const c_void;
    if unsafe { AddAccessDeniedAceEx(acl, 4, flags, mask, sid) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetLengthSid(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sid = f.arg_l(0) as *const c_void;
    let n = unsafe { GetLengthSid(sid) };
    Ok(Slot::I(n as i32))
}

fn wnd_ConvertSidToStringSid(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sid = f.arg_l(0) as *const c_void;
    let mut out: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    let n = (0..).take_while(|&i| unsafe { *out.add(i) } != 0).count();
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, n) });
    unsafe { LocalFree(out as *mut c_void) };
    Ok(Slot::I(vm.sh.heap.new_string(text)))
}

/// `ConvertStringSidToSid0(addr)`：返回的 SID 由 `LocalFree` 释放。
/// 注意 `LocalFree` 在 Java 侧会调本文件的 `LocalFree` —— 那边的指针是 Win32 分配的，
/// 所以这里也走 Win32 的 `LocalAlloc`（`ConvertStringSidToSidW` 内部用的就是它）。
fn wnd_ConvertStringSidToSid0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let mut sid: *mut c_void = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::L(sid as u64 as i64))
}

fn wnd_LookupAccountSid0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sid = f.arg_l(0) as *const c_void;
    let obj = f.arg_i(1);
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut nlen, mut dlen) = (name.len() as u32, domain.len() as u32);
    let mut use_kind: u32 = 0;
    if unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut nlen,
            domain.as_mut_ptr(),
            &mut dlen,
            &mut use_kind,
        )
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    let name_s = String::from_utf16_lossy(&name[..nlen as usize]);
    let dom_s = String::from_utf16_lossy(&domain[..dlen as usize]);
    let nh = vm.sh.heap.new_string(name_s);
    let dh = vm.sh.heap.new_string(dom_s);
    set_obj_field(vm, obj, WND_ACCOUNT, "name", Slot::I(nh));
    set_obj_field(vm, obj, WND_ACCOUNT, "domain", Slot::I(dh));
    set_obj_field(vm, obj, WND_ACCOUNT, "use", Slot::I(use_kind as i32));
    Ok(Slot::I(0))
}

/// `LookupAccountName0(nameAddr, pSid, cbSid)` → 需要的 SID 字节数
fn wnd_LookupAccountName0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = read_wstr_z(f.arg_l(0) as u64);
    let sid = f.arg_l(1) as *mut c_void;
    let mut cb: u32 = f.arg_i(2) as u32;
    let mut domain = [0u16; 256];
    let mut dlen: u32 = domain.len() as u32;
    let mut use_kind: u32 = 0;
    if unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid,
            &mut cb,
            domain.as_mut_ptr(),
            &mut dlen,
            &mut use_kind,
        )
    } == 0
    {
        let code = last_error();
        if code == ERROR_INSUFFICIENT_BUFFER {
            // 这不是失败：Java 侧据这个返回值再分配一次
            return Ok(Slot::I(cb as i32));
        }
        return Err(throw_windows(vm, code));
    }
    Ok(Slot::I(cb as i32))
}

fn wnd_GetFileSecurity0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let info = f.arg_i(1) as u32;
    let sd = f.arg_l(2) as *mut c_void;
    let size = f.arg_i(3) as u32;
    let mut needed: u32 = size;
    if unsafe { GetFileSecurityW(w.as_ptr(), info, sd, size, &mut needed) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(needed as i32))
}

fn wnd_SetFileSecurity0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    let info = f.arg_i(1) as u32;
    let sd = f.arg_l(2) as *const c_void;
    if unsafe { SetFileSecurityW(w.as_ptr(), info, sd) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
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
        // 没有线程令牌是**正常情况**（返回 0，Java 侧会退回到进程令牌），不抛
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
    let thread: *mut Handle = if thread_arg == 0 {
        std::ptr::null_mut()
    } else {
        &thread_arg as *const i64 as *mut Handle
    };
    if unsafe { SetThreadToken(thread, token) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_GetTokenInformation(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let class = f.arg_i(1) as u32;
    let buf = f.arg_l(2) as *mut c_void;
    let len = f.arg_i(3) as u32;
    let mut needed: u32 = len;
    if unsafe { GetTokenInformation(token, class, buf, len, &mut needed) } == 0 {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(needed as i32))
}

fn wnd_AdjustTokenPrivileges(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let luid_addr = f.arg_l(1) as *const c_void;
    let attributes = f.arg_i(2) as u32;
    // TOKEN_PRIVILEGES { DWORD PrivilegeCount; LUID_AND_ATTRIBUTES[1] }
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&1u32.to_ne_bytes());
    let luid = nat_mem::read_bytes(luid_addr as u64, 8);
    buf[4..12].copy_from_slice(&luid);
    buf[12..16].copy_from_slice(&attributes.to_ne_bytes());
    if unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            buf.as_ptr() as *const c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(throw_windows(vm, last_error()));
    }
    Ok(Slot::I(0))
}

fn wnd_LookupPrivilegeValue0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let w = read_wstr_z(f.arg_l(0) as u64);
    // 分配的 LUID 由 Java 侧 `LocalFree` 释放 → 必须落在 nat_mem 里（见 wnd_LocalFree 的分流）
    let addr = nat_mem::alloc(8);
    let mut luid = [0u8; 8];
    if unsafe {
        LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), luid.as_mut_ptr() as *mut c_void)
    } == 0
    {
        nat_mem::free(addr);
        return Err(throw_windows(vm, last_error()));
    }
    nat_mem::write_bytes(addr, &luid);
    Ok(Slot::L(addr as i64))
}

fn wnd_AccessCheck(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let token = f.arg_l(0) as Handle;
    let sd = f.arg_l(1) as *const c_void;
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
    // PRIVILEGE_SET 缓冲区留够（1 条 LUID_AND_ATTRIBUTES = 12 字节）
    let mut priv_set = [0u8; 64];
    let mut priv_len: u32 = priv_set.len() as u32;
    let mut granted: u32 = 0;
    let mut success: Bool = 0;
    if unsafe {
        AccessCheck(
            sd,
            token,
            access_mask,
            mapping.as_ptr() as *const c_void,
            priv_set.as_mut_ptr() as *mut c_void,
            &mut priv_len,
            &mut granted,
            &mut success,
        )
    } == 0
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
