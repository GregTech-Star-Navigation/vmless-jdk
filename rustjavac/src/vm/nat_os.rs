//! native 实现分组：nat_os —— **操作系统层**（文件系统 / 进程）。
//!
//! 由 `native_reg.rs` 的 `register_all` 末尾调用 `register(r)`。
//! 这样几条并行工作线各改各的文件，不用碰 `native_reg.rs`。
//!
//! 覆盖面（OpenJDK 里带 `native` 关键字的那一层）：
//!
//! | 类 | 干什么 |
//! | --- | --- |
//! | `java.io.FileDescriptor` / `FileCleanable` | 句柄的关闭 / 同步 / 标准句柄 |
//! | `java.io.FileInputStream` / `FileOutputStream` / `RandomAccessFile` | 真文件读写 |
//! | `java.io.WinNTFileSystem` | `File` 的属性 / 目录 / 改名 / 删除 / 规范化路径 |
//! | `java.lang.ProcessImpl` / `ProcessEnvironment` / `ProcessHandleImpl` | 起进程 + 拿退出码 |
//!
//! 约定（与 HotSpot 一致，别自己发明）：
//!
//! * **句柄存在 `FileDescriptor.handle` 字段里**（Windows 上没有 `fd` 概念，
//!   `FileDescriptor.fd` 恒为 -1；`valid()` 的判据是 `handle != -1 || fd != -1`）。
//!   所以 `close0` 必须真 `CloseHandle` 并把 `handle` 写回 -1，否则句柄泄漏。
//! * 错误码走 `GetLastError` → 对应的 Java 异常（`FileNotFoundException` /
//!   `IOException` / `AccessDeniedException` 由上层按 errno 语义包装，我们只给
//!   `IOException` 语义最接近的那个，消息里带 Win32 的本地化文本）。
//! * 路径一律按 **UTF-16** 交给 Win32（`to_wide`），Java 的 `String` 本来就是 UTF-16。

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use super::super::frame::Frame;
use super::super::slot::Slot;
use super::super::{Vm, VmError};
use super::NativeRegistry;

// ============================================================ Win32 裸 FFI

type Handle = isize;
type Bool = i32;

const INVALID_HANDLE_VALUE: Handle = -1;

// access rights
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
const FILE_WRITE_DATA: u32 = 0x0000_0002;

// share modes
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;

// creation dispositions
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;

// attributes / flags
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

// seek methods
const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;

const FILE_TYPE_DISK: u32 = 1;
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_FILE_EXISTS: u32 = 80;

const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;

const HANDLE_FLAG_INHERIT: u32 = 0x1;

const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const INFINITE: u32 = 0xFFFF_FFFF;
const STILL_ACTIVE: u32 = 259;

const STARTF_USESTDHANDLES: u32 = 0x100;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

const VOLUME_NAME_DOS: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn to_u64(self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
    fn from_u64(v: u64) -> Self {
        FileTime { low: v as u32, high: (v >> 32) as u32 }
    }
    /// FILETIME（1601-01-01 起 100ns）→ Unix 毫秒
    fn to_millis(self) -> i64 {
        (self.to_u64() / 10_000) as i64 - 11_644_473_600_000
    }
}

#[repr(C)]
#[derive(Default)]
struct Win32FileAttributeData {
    attrs: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    size_high: u32,
    size_low: u32,
}

#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    attrs: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

#[repr(C)]
struct Win32FindDataW {
    attrs: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    size_high: u32,
    size_low: u32,
    reserved0: u32,
    reserved1: u32,
    name: [u16; 260],
    alt_name: [u16; 14],
}

impl Default for Win32FindDataW {
    fn default() -> Self {
        Win32FindDataW {
            attrs: 0,
            creation: FileTime::default(),
            access: FileTime::default(),
            write: FileTime::default(),
            size_high: 0,
            size_low: 0,
            reserved0: 0,
            reserved1: 0,
            name: [0; 260],
            alt_name: [0; 14],
        }
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: Bool,
}

#[repr(C)]
#[derive(Default)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count: u32,
    y_count: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved2: u16,
    reserved3: *mut u8,
    std_input: Handle,
    std_output: Handle,
    std_error: Handle,
}

#[repr(C)]
#[derive(Default)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    pid: u32,
    tid: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *const c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn ReadFile(h: Handle, buf: *mut u8, n: u32, read: *mut u32, ov: *mut c_void) -> Bool;
    fn WriteFile(h: Handle, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> Bool;
    fn CloseHandle(h: Handle) -> Bool;
    fn GetLastError() -> u32;
    fn SetFilePointerEx(h: Handle, dist: i64, new_ptr: *mut i64, method: u32) -> Bool;
    fn GetFileSizeEx(h: Handle, size: *mut i64) -> Bool;
    fn SetEndOfFile(h: Handle) -> Bool;
    fn FlushFileBuffers(h: Handle) -> Bool;
    fn GetFileType(h: Handle) -> u32;
    fn GetFileInformationByHandle(h: Handle, info: *mut ByHandleFileInformation) -> Bool;
    fn GetFileAttributesExW(name: *const u16, level: u32, info: *mut Win32FileAttributeData) -> Bool;
    fn GetFileAttributesW(name: *const u16) -> u32;
    fn SetFileAttributesW(name: *const u16, attrs: u32) -> Bool;
    fn SetFileTime(h: Handle, creation: *const FileTime, access: *const FileTime, write: *const FileTime)
        -> Bool;
    fn DeleteFileW(name: *const u16) -> Bool;
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> Bool;
    fn CreateDirectoryW(name: *const u16, sa: *const c_void) -> Bool;
    fn RemoveDirectoryW(name: *const u16) -> Bool;
    fn GetFullPathNameW(name: *const u16, len: u32, buf: *mut u16, file_part: *mut *mut u16) -> u32;
    fn GetFinalPathNameByHandleW(h: Handle, buf: *mut u16, len: u32, flags: u32) -> u32;
    fn FindFirstFileW(name: *const u16, data: *mut Win32FindDataW) -> Handle;
    fn FindNextFileW(h: Handle, data: *mut Win32FindDataW) -> Bool;
    fn FindClose(h: Handle) -> Bool;
    fn GetLogicalDrives() -> u32;
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_available: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> Bool;
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
    fn GetStdHandle(which: u32) -> Handle;
    fn SetHandleInformation(h: Handle, mask: u32, flags: u32) -> Bool;
    fn DuplicateHandle(
        process: Handle,
        src: Handle,
        target: Handle,
        out: *mut Handle,
        access: u32,
        inherit: Bool,
        options: u32,
    ) -> Bool;
    fn GetCurrentProcess() -> Handle;
    fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        msg_id: u32,
        lang: u32,
        buf: *mut u16,
        size: u32,
        args: *const u8,
    ) -> u32;
    fn CreatePipe(read: *mut Handle, write: *mut Handle, sa: *const SecurityAttributes, size: u32)
        -> Bool;
    fn CreateProcessW(
        app: *const u16,
        cmdline: *mut u16,
        proc_sa: *const c_void,
        thread_sa: *const c_void,
        inherit: Bool,
        flags: u32,
        env: *const c_void,
        dir: *const u16,
        si: *const StartupInfoW,
        pi: *mut ProcessInformation,
    ) -> Bool;
    fn WaitForSingleObject(h: Handle, millis: u32) -> u32;
    fn GetExitCodeProcess(h: Handle, code: *mut u32) -> Bool;
    fn TerminateProcess(h: Handle, code: u32) -> Bool;
    fn GetProcessId(h: Handle) -> u32;
    fn OpenProcess(access: u32, inherit: Bool, pid: u32) -> Handle;
    fn GetCurrentProcessId() -> u32;
    fn GetEnvironmentStringsW() -> *mut u16;
    fn FreeEnvironmentStringsW(block: *mut u16) -> Bool;
    fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, len: u32) -> u32;
}

// ============================================================ 小工具

/// Java `String` → NUL 结尾的 UTF-16（Win32 的宽字符 API 要这个）
fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
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
    let raw = String::from_utf16_lossy(&buf[..n as usize]);
    raw.trim_end_matches(['\r', '\n', ' ', '.']).to_string()
}

/// `path (Win32 消息)` —— 与 HotSpot 的 `throwFileNotFoundException` 同形
fn io_text(path: &str, code: u32) -> String {
    format!("{path} ({})", win_msg(code))
}

fn io_error(vm: &mut Vm, what: &str, code: u32) -> VmError {
    vm.throw_msg("java/io/IOException", format!("{what} ({})", win_msg(code)))
}

fn fnf_error(vm: &mut Vm, path: &str, code: u32) -> VmError {
    vm.throw_msg("java/io/FileNotFoundException", io_text(path, code))
}

// ---- 对象字段 ----

fn field_index(vm: &Vm, class: &str, name: &str) -> Option<usize> {
    vm.store.field_index(class, name)
}

fn get_slot(vm: &Vm, obj: i32, class: &str, name: &str) -> Slot {
    field_index(vm, class, name)
        .and_then(|i| vm.sh.heap.get(obj, i))
        .unwrap_or(Slot::I(0))
}

fn set_slot(vm: &Vm, obj: i32, class: &str, name: &str, v: Slot) {
    if let Some(i) = field_index(vm, class, name) {
        vm.sh.heap.set(obj, i, v);
    }
}

/// `FileDescriptor.handle`（Windows 上唯一有效的句柄）
fn fd_handle(vm: &Vm, fd_obj: i32) -> Handle {
    get_slot(vm, fd_obj, "java/io/FileDescriptor", "handle").as_i64() as Handle
}

fn set_fd_handle(vm: &Vm, fd_obj: i32, h: Handle) {
    set_slot(vm, fd_obj, "java/io/FileDescriptor", "handle", Slot::L(h as i64));
}

/// `File.path` → Rust 字符串
fn file_path(vm: &Vm, file_obj: i32) -> String {
    let s = get_slot(vm, file_obj, "java/io/File", "path").as_i32();
    vm.text_of(s)
}

/// `this.fd`（FileInputStream / FileOutputStream / RandomAccessFile 的字段名都一样）
fn this_fd(vm: &Vm, this: i32, class: &str) -> i32 {
    get_slot(vm, this, class, "fd").as_i32()
}

// ============================================================ 注册

pub(super) fn register(r: &mut NativeRegistry) {
    register_fd(r);
    register_streams(r);
    register_fs(r);
    register_process(r);

    // ---- 启动期的一次性铺垫 ----
    //
    // `FileOutputStream.<clinit>` 里第一句就是
    // `SharedSecrets.getJavaIOFileDescriptorAccess()`；这个 access 是
    // `FileDescriptor.<clinit>` 装上的。装之前 SharedSecrets 会走
    // `MethodHandles.lookup().ensureInitialized(FileDescriptor.class)`，
    // 而 VM 的 indy 层目前没实现 `Lookup.ensureInitialized`
    //（`src/vm/indy.rs:1643` 的兜底分支）—— 于是**任何文件写入**在类初始化阶段
    // 就 BootstrapMethodError，连我这些 native 都轮不到跑。
    //
    // 这里借 `System.<clinit>` 的 `registerNatives()`（`run_main` 保证跑在
    // `initPhase1` 与用户 `main` 之前）把 `FileDescriptor` 提前初始化，
    // 让 SharedSecrets 那条「缺失就初始化」的分支根本不会走到。
    // 正解仍是给 indy 补上 `ensureInitialized`，那是另一个文件的事。
    r.add("java/lang/System", "registerNatives", "()V", |vm, _| {
        if vm.store.class_file("java/io/FileDescriptor").is_some() {
            let _ = vm.ensure_init("java/io/FileDescriptor");
        }
        Ok(Slot::I(0))
    });
}

// ---------------------------------------------------------------- 句柄

fn register_fd(r: &mut NativeRegistry) {
    let fd = "java/io/FileDescriptor";
    r.add(fd, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    // 标准句柄：`FileDescriptor.<clinit>` 里 `new FileDescriptor(0/1/2)` 就靠它，
    // 拿不到的话 `System.out` 整条链写不出去。
    r.add(fd, "getHandle", "(I)J", |_, f| {
        let which = match f.arg_i(0) {
            0 => STD_INPUT_HANDLE,
            1 => STD_OUTPUT_HANDLE,
            2 => STD_ERROR_HANDLE,
            _ => return Ok(Slot::L(-1)),
        };
        Ok(Slot::L(unsafe { GetStdHandle(which) } as i64))
    });
    r.add(fd, "getAppend", "(I)Z", |_, _| Ok(Slot::I(0)));
    r.add(fd, "sync0", "()V", fd_sync);
    r.add(fd, "close0", "()V", fd_close);
    // `FileCleanable.performCleanup` 的落点：直接关裸句柄（对象已不可达）
    r.add("java/io/FileCleanable", "cleanupClose0", "(IJ)V", |vm, f| {
        let h = f.arg_l(1) as Handle;
        if h != INVALID_HANDLE_VALUE && h != 0 {
            unsafe { CloseHandle(h) };
        }
        let _ = vm;
        Ok(Slot::I(0))
    });
}

fn fd_sync(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.this());
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Ok(Slot::I(0));
    }
    if unsafe { FlushFileBuffers(h) } == 0 {
        let code = last_error();
        return Err(vm.throw_msg(
            "java/io/SyncFailedException",
            format!("句柄同步失败 ({})", win_msg(code)),
        ));
    }
    Ok(Slot::I(0))
}

fn fd_close(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this);
    if h != INVALID_HANDLE_VALUE && h != 0 {
        set_fd_handle(vm, this, -1);
        if unsafe { CloseHandle(h) } == 0 {
            let code = last_error();
            return Err(io_error(vm, "关闭文件失败", code));
        }
    }
    Ok(Slot::I(0))
}

// ---------------------------------------------------------------- 流

fn register_streams(r: &mut NativeRegistry) {
    let fis = "java/io/FileInputStream";
    r.add(fis, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(fis, "open0", "(Ljava/lang/String;)V", fis_open);
    r.add(fis, "read0", "()I", fis_read0);
    r.add(fis, "readBytes", "([BII)I", fis_read_bytes);
    r.add(fis, "available0", "()I", fis_available);
    r.add(fis, "length0", "()J", fis_length);
    r.add(fis, "position0", "()J", fis_position);
    r.add(fis, "skip0", "(J)J", fis_skip);
    r.add(fis, "isRegularFile0", "(Ljava/io/FileDescriptor;)Z", fis_is_regular);

    let fos = "java/io/FileOutputStream";
    r.add(fos, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(fos, "open0", "(Ljava/lang/String;Z)V", fos_open);
    r.add(fos, "write", "(IZ)V", fos_write);
    r.add(fos, "writeBytes", "([BIIZ)V", fos_write_bytes);

    let raf = "java/io/RandomAccessFile";
    r.add(raf, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(raf, "open0", "(Ljava/lang/String;I)V", raf_open);
    r.add(raf, "read0", "()I", raf_read0);
    r.add(raf, "readBytes0", "([BII)I", raf_read_bytes);
    r.add(raf, "write0", "(I)V", raf_write0);
    r.add(raf, "writeBytes0", "([BII)V", raf_write_bytes);
    r.add(raf, "getFilePointer", "()J", raf_position);
    r.add(raf, "seek0", "(J)V", raf_seek);
    r.add(raf, "length0", "()J", raf_length);
    r.add(raf, "setLength0", "(J)V", raf_set_length);
}

/// 共享模式：与 HotSpot 一样全放（读/写/删），这样 `Files.move` 之类不会被我们自己卡住
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn open_file(path: &str, access: u32, disposition: u32, flags: u32) -> Result<Handle, u32> {
    let w = to_wide(path);
    let h = unsafe {
        CreateFileW(w.as_ptr(), access, SHARE_ALL, std::ptr::null(), disposition, flags, 0)
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(h)
    }
}

fn fis_open(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    let fd_obj = this_fd(vm, f.this(), "java/io/FileInputStream");
    match open_file(&path, GENERIC_READ, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL) {
        Ok(h) => {
            // 打开后取一次文件类型：`FileInputStream.isRegularFile0` 之外，
            // HotSpot 也把这里当作「能打开」的判据
            set_fd_handle(vm, fd_obj, h);
            Ok(Slot::I(0))
        }
        Err(code) => Err(fnf_error(vm, &path, code)),
    }
}

fn fos_open(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    let append = f.arg_i(1) != 0;
    let fd_obj = this_fd(vm, f.this(), "java/io/FileOutputStream");
    let access = if append { FILE_APPEND_DATA } else { GENERIC_WRITE };
    let disposition = if append { OPEN_ALWAYS } else { CREATE_ALWAYS };
    match open_file(&path, access, disposition, FILE_ATTRIBUTE_NORMAL) {
        Ok(h) => {
            set_fd_handle(vm, fd_obj, h);
            Ok(Slot::I(0))
        }
        Err(code) => Err(fnf_error(vm, &path, code)),
    }
}

fn raf_open(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // 常量来自 java.io.RandomAccessFile：O_RDONLY=1 / O_RDWR=2 / O_SYNC=4 / O_DSYNC=8 / O_TEMPORARY=16
    let path = vm.text_of(f.arg_i(0));
    let mode = f.arg_i(1);
    let fd_obj = this_fd(vm, f.this(), "java/io/RandomAccessFile");
    let read_only = mode & 2 == 0;
    let mut access = if read_only { GENERIC_READ } else { GENERIC_READ | GENERIC_WRITE };
    if read_only {
        access = GENERIC_READ;
    }
    let mut flags = FILE_ATTRIBUTE_NORMAL;
    if mode & 12 != 0 {
        flags |= FILE_FLAG_WRITE_THROUGH;
    }
    if mode & 16 != 0 {
        flags |= FILE_FLAG_DELETE_ON_CLOSE;
    }
    let disposition = if read_only { OPEN_EXISTING } else { OPEN_ALWAYS };
    let _ = &mut access;
    match open_file(&path, access, disposition, flags) {
        Ok(h) => {
            set_fd_handle(vm, fd_obj, h);
            Ok(Slot::I(0))
        }
        Err(code) => Err(fnf_error(vm, &path, code)),
    }
}

// ---- 读 ----

fn read_bytes_into(h: Handle, buf: &mut [u8]) -> Result<usize, u32> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut n: u32 = 0;
    if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 {
        return Err(last_error());
    }
    Ok(n as usize)
}

/// 把读到的字节写进 `[B`（数组里按 **符号扩展的 i32** 存，和 `bastore` 一致）
fn store_bytes(vm: &Vm, arr: i32, off: usize, data: &[u8]) {
    for (i, b) in data.iter().enumerate() {
        vm.sh.heap.set(arr, off + i, Slot::I(*b as i8 as i32));
    }
}

/// 从 `[B` 取出连续的字节
fn load_bytes(vm: &Vm, arr: i32, off: usize, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        let b = vm.sh.heap.get(arr, off + i).map(|s| s.as_i32()).unwrap_or(0);
        v.push(b as u8);
    }
    v
}

fn check_range(vm: &mut Vm, arr: i32, off: i32, len: i32) -> Result<bool, VmError> {
    let n = vm.sh.heap.array_len(arr).unwrap_or(0) as i32;
    if off < 0 || len < 0 || (off as i64) + (len as i64) > n as i64 {
        return Err(vm.throw_msg(
            "java/lang/IndexOutOfBoundsException",
            format!("off={off} len={len} length={n}"),
        ));
    }
    Ok(len == 0)
}

fn read_stream(vm: &mut Vm, f: &mut Frame, this_class: &str, with_offset: bool) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, this_class));
    let (arr, off, len) = if with_offset {
        (f.arg_i(0), f.arg_i(1), f.arg_i(2))
    } else {
        (f.arg_i(0), f.arg_i(1), f.arg_i(2))
    };
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    if check_range(vm, arr, off, len)? {
        return Ok(Slot::I(0));
    }
    let mut buf = vec![0u8; len as usize];
    match read_bytes_into(h, &mut buf) {
        Ok(0) => Ok(Slot::I(-1)),
        Ok(n) => {
            store_bytes(vm, arr, off as usize, &buf[..n]);
            Ok(Slot::I(n as i32))
        }
        Err(code) => Err(io_error(vm, "读取失败", code)),
    }
}

fn fis_read0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/FileInputStream"));
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    let mut b = [0u8; 1];
    match read_bytes_into(h, &mut b) {
        Ok(0) => Ok(Slot::I(-1)),
        Ok(_) => Ok(Slot::I(b[0] as i32)),
        Err(code) => Err(io_error(vm, "读取失败", code)),
    }
}

fn fis_read_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    read_stream(vm, f, "java/io/FileInputStream", true)
}

fn raf_read0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/RandomAccessFile"));
    let mut b = [0u8; 1];
    match read_bytes_into(h, &mut b) {
        Ok(0) => Ok(Slot::I(-1)),
        Ok(_) => Ok(Slot::I(b[0] as i32)),
        Err(code) => Err(io_error(vm, "读取失败", code)),
    }
}

fn raf_read_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    read_stream(vm, f, "java/io/RandomAccessFile", true)
}

// ---- 写 ----

fn write_all(h: Handle, data: &[u8]) -> Result<(), u32> {
    let mut written_total = 0usize;
    while written_total < data.len() {
        let mut n: u32 = 0;
        let chunk = &data[written_total..];
        if unsafe {
            WriteFile(h, chunk.as_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut())
        } == 0
        {
            return Err(last_error());
        }
        if n == 0 {
            return Err(0);
        }
        written_total += n as usize;
    }
    Ok(())
}

fn fos_write(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/FileOutputStream"));
    let append = f.arg_i(1) != 0;
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    if append {
        unsafe { SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END) };
    }
    let b = [f.arg_i(0) as u8];
    match write_all(h, &b) {
        Ok(()) => Ok(Slot::I(0)),
        Err(code) => Err(io_error(vm, "写入失败", code)),
    }
}

fn fos_write_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/FileOutputStream"));
    let (arr, off, len, append) = (f.arg_i(0), f.arg_i(1), f.arg_i(2), f.arg_i(3) != 0);
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    if check_range(vm, arr, off, len)? {
        return Ok(Slot::I(0));
    }
    let data = load_bytes(vm, arr, off as usize, len as usize);
    if append {
        unsafe { SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END) };
    }
    match write_all(h, &data) {
        Ok(()) => Ok(Slot::I(0)),
        Err(code) => Err(io_error(vm, "写入失败", code)),
    }
}

fn raf_write0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/RandomAccessFile"));
    let b = [f.arg_i(0) as u8];
    match write_all(h, &b) {
        Ok(()) => Ok(Slot::I(0)),
        Err(code) => Err(io_error(vm, "写入失败", code)),
    }
}

fn raf_write_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let h = fd_handle(vm, this_fd(vm, this, "java/io/RandomAccessFile"));
    let (arr, off, len) = (f.arg_i(0), f.arg_i(1), f.arg_i(2));
    if check_range(vm, arr, off, len)? {
        return Ok(Slot::I(0));
    }
    let data = load_bytes(vm, arr, off as usize, len as usize);
    match write_all(h, &data) {
        Ok(()) => Ok(Slot::I(0)),
        Err(code) => Err(io_error(vm, "写入失败", code)),
    }
}

// ---- 位置 / 长度 ----

fn stream_handle(vm: &mut Vm, f: &Frame, class: &str) -> Result<Handle, VmError> {
    let h = fd_handle(vm, this_fd(vm, f.this(), class));
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Err(vm.throw_msg("java/io/IOException", "Stream Closed".to_string()));
    }
    Ok(h)
}

fn fis_available(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/FileInputStream")?;
    if unsafe { GetFileType(h) } != FILE_TYPE_DISK {
        // 管道 / 控制台：说不准，按 0 报（与 HotSpot 的 `IO_Available` 一致）
        return Ok(Slot::I(0));
    }
    let mut size: i64 = 0;
    let mut cur: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } != 0
        && unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } != 0
    {
        let avail = (size - cur).max(0);
        return Ok(Slot::I(avail.min(i32::MAX as i64) as i32));
    }
    Ok(Slot::I(0))
}

fn fis_length(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/FileInputStream")?;
    let mut size: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "取文件长度失败", code));
    }
    Ok(Slot::L(size))
}

fn fis_position(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/FileInputStream")?;
    let mut cur: i64 = 0;
    if unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "取文件位置失败", code));
    }
    Ok(Slot::L(cur))
}

fn fis_skip(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/FileInputStream")?;
    let n = f.arg_l(0);
    let mut before: i64 = 0;
    if unsafe { SetFilePointerEx(h, 0, &mut before, FILE_CURRENT) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "取文件位置失败", code));
    }
    let mut after: i64 = 0;
    if unsafe { SetFilePointerEx(h, n, &mut after, FILE_CURRENT) } == 0 {
        // skip 越过文件尾：HotSpot 允许（返回 0 表示没跳过）
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(after - before))
}

fn fis_is_regular(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = fd_handle(vm, f.arg_i(0));
    if h == INVALID_HANDLE_VALUE || h == 0 {
        return Ok(Slot::I(0));
    }
    if unsafe { GetFileType(h) } != FILE_TYPE_DISK {
        return Ok(Slot::I(0));
    }
    let mut info = ByHandleFileInformation::default();
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return Ok(Slot::I(0));
    }
    Ok(Slot::I(i32::from(info.attrs & FILE_ATTRIBUTE_DIRECTORY == 0)))
}

fn raf_position(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/RandomAccessFile")?;
    let mut cur: i64 = 0;
    if unsafe { SetFilePointerEx(h, 0, &mut cur, FILE_CURRENT) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "取文件位置失败", code));
    }
    Ok(Slot::L(cur))
}

fn raf_seek(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/RandomAccessFile")?;
    let pos = f.arg_l(0);
    if pos < 0 || unsafe { SetFilePointerEx(h, pos, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "定位失败", code));
    }
    Ok(Slot::I(0))
}

fn raf_length(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/RandomAccessFile")?;
    let mut size: i64 = 0;
    if unsafe { GetFileSizeEx(h, &mut size) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "取文件长度失败", code));
    }
    Ok(Slot::L(size))
}

fn raf_set_length(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = stream_handle(vm, f, "java/io/RandomAccessFile")?;
    let len = f.arg_l(0);
    if unsafe { SetFilePointerEx(h, len, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "定位失败", code));
    }
    if unsafe { SetEndOfFile(h) } == 0 {
        let code = last_error();
        return Err(io_error(vm, "截断失败", code));
    }
    Ok(Slot::I(0))
}

// ---------------------------------------------------------------- java.io.File 的文件系统

fn register_fs(r: &mut NativeRegistry) {
    let fs = "java/io/WinNTFileSystem";
    r.add(fs, "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add(fs, "getBooleanAttributes0", "(Ljava/io/File;)I", fs_attributes);
    r.add(fs, "checkAccess0", "(Ljava/io/File;I)Z", fs_check_access);
    r.add(fs, "getLastModifiedTime0", "(Ljava/io/File;)J", fs_mtime);
    r.add(fs, "getLength0", "(Ljava/io/File;)J", fs_length);
    r.add(fs, "setPermission0", "(Ljava/io/File;IZZ)Z", fs_set_permission);
    r.add(fs, "createFileExclusively0", "(Ljava/lang/String;)Z", fs_create_new);
    r.add(fs, "list0", "(Ljava/io/File;)[Ljava/lang/String;", fs_list);
    r.add(fs, "createDirectory0", "(Ljava/io/File;)Z", fs_mkdir);
    r.add(fs, "setLastModifiedTime0", "(Ljava/io/File;J)Z", fs_set_mtime);
    r.add(fs, "setReadOnly0", "(Ljava/io/File;)Z", fs_set_readonly);
    r.add(fs, "delete0", "(Ljava/io/File;Z)Z", fs_delete);
    r.add(fs, "rename0", "(Ljava/io/File;Ljava/io/File;)Z", fs_rename);
    r.add(fs, "listRoots0", "()I", |_, _| Ok(Slot::I(unsafe { GetLogicalDrives() } as i32)));
    r.add(fs, "getSpace0", "(Ljava/io/File;I)J", fs_space);
    r.add(fs, "getNameMax0", "(Ljava/lang/String;)I", fs_name_max);
    r.add(fs, "getDriveDirectory", "(I)Ljava/lang/String;", fs_drive_dir);
    r.add(fs, "canonicalize0", "(Ljava/lang/String;)Ljava/lang/String;", fs_canonicalize);
    r.add(fs, "getFinalPath0", "(Ljava/lang/String;)Ljava/lang/String;", fs_final_path);
}

/// Win32 文件属性；文件不存在返回 None
fn file_attrs(path: &str) -> Option<Win32FileAttributeData> {
    let w = to_wide(path);
    let mut data = Win32FileAttributeData::default();
    // GetFileExInfoStandard = 0
    let ok = unsafe { GetFileAttributesExW(w.as_ptr(), 0, &mut data) };
    if ok == 0 {
        None
    } else {
        Some(data)
    }
}

fn attrs_size(d: &Win32FileAttributeData) -> i64 {
    (((d.size_high as u64) << 32) | d.size_low as u64) as i64
}

fn fs_attributes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // BA_EXISTS=1 / BA_REGULAR=2 / BA_DIRECTORY=4 / BA_HIDDEN=8
    let path = file_path(vm, f.arg_i(0));
    let Some(d) = file_attrs(&path) else { return Ok(Slot::I(0)) };
    let mut rv = 1;
    if d.attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        rv |= 4;
    } else {
        rv |= 2;
    }
    if d.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
        rv |= 8;
    }
    Ok(Slot::I(rv))
}

fn fs_check_access(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // ACCESS_READ=4 / ACCESS_WRITE=2 / ACCESS_EXECUTE=1
    let path = file_path(vm, f.arg_i(0));
    let access = f.arg_i(1);
    let Some(d) = file_attrs(&path) else { return Ok(Slot::I(0)) };
    if access & 2 != 0 && d.attrs & FILE_ATTRIBUTE_READONLY != 0 {
        return Ok(Slot::I(0));
    }
    Ok(Slot::I(1))
}

fn fs_mtime(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    match file_attrs(&path) {
        Some(d) => Ok(Slot::L(d.write.to_millis())),
        None => Ok(Slot::L(0)),
    }
}

fn fs_length(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    match file_attrs(&path) {
        Some(d) if d.attrs & FILE_ATTRIBUTE_DIRECTORY == 0 => Ok(Slot::L(attrs_size(&d))),
        _ => Ok(Slot::L(0)),
    }
}

fn fs_set_permission(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let access = f.arg_i(1);
    let enable = f.arg_i(2) != 0;
    let w = to_wide(&path);
    let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
    if attrs == u32::MAX {
        return Ok(Slot::I(0));
    }
    if access & 2 != 0 {
        let new = if enable {
            attrs & !FILE_ATTRIBUTE_READONLY
        } else {
            attrs | FILE_ATTRIBUTE_READONLY
        };
        if new != attrs && unsafe { SetFileAttributesW(w.as_ptr(), new) } == 0 {
            return Ok(Slot::I(0));
        }
    }
    Ok(Slot::I(1))
}

fn fs_create_new(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    match open_file(&path, GENERIC_WRITE, CREATE_NEW, FILE_ATTRIBUTE_NORMAL) {
        Ok(h) => {
            unsafe { CloseHandle(h) };
            Ok(Slot::I(1))
        }
        Err(code) => {
            if code == ERROR_FILE_EXISTS || code == ERROR_ALREADY_EXISTS || code == ERROR_ACCESS_DENIED {
                Ok(Slot::I(0))
            } else {
                Err(io_error(vm, &format!("无法创建文件 {path}"), code))
            }
        }
    }
}

fn fs_list(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let pattern = if path.ends_with('\\') { format!("{path}*") } else { format!("{path}\\*") };
    let w = to_wide(&pattern);
    let mut data = Win32FindDataW::default();
    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return Ok(Slot::I(0)); // null：与 HotSpot 一致
    }
    let mut names: Vec<String> = Vec::new();
    loop {
        let name = wide_to_string(&data.name);
        if name != "." && name != ".." && !name.is_empty() {
            names.push(name);
        }
        if unsafe { FindNextFileW(h, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(h) };
    let mut handles: Vec<i32> = names.iter().map(|n| vm.str_new(n.clone())).collect();
    let arr = vm
        .sh
        .heap
        .alloc_array(handles.len(), "[Ljava/lang/String;", Slot::I(0))
        .ok_or_else(|| VmError { msg: "OutOfMemoryError: 目录项太多".into(), exc: None })?;
    for (i, h) in handles.drain(..).enumerate() {
        vm.sh.heap.set_ref(arr, i, Slot::I(h));
    }
    Ok(Slot::I(arr))
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn fs_mkdir(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let w = to_wide(&path);
    let ok = unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) };
    Ok(Slot::I(i32::from(ok != 0)))
}

fn fs_set_mtime(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let millis = f.arg_l(1);
    let ft = if millis < 0 {
        FileTime::from_u64(0)
    } else {
        FileTime::from_u64((millis as u64 + 11_644_473_600_000) * 10_000)
    };
    match open_file(
        &path,
        FILE_WRITE_ATTRIBUTES,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
    ) {
        Ok(h) => {
            let ok = unsafe { SetFileTime(h, std::ptr::null(), std::ptr::null(), &ft) };
            unsafe { CloseHandle(h) };
            Ok(Slot::I(i32::from(ok != 0)))
        }
        Err(_) => Ok(Slot::I(0)),
    }
}

fn fs_set_readonly(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let w = to_wide(&path);
    let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
    if attrs == u32::MAX {
        return Ok(Slot::I(0));
    }
    let ok = unsafe { SetFileAttributesW(w.as_ptr(), attrs | FILE_ATTRIBUTE_READONLY) };
    Ok(Slot::I(i32::from(ok != 0)))
}

fn fs_delete(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let allow_ro = f.arg_i(1) != 0;
    let w = to_wide(&path);
    if unsafe { DeleteFileW(w.as_ptr()) } != 0 {
        return Ok(Slot::I(1));
    }
    let code = last_error();
    if code == ERROR_ACCESS_DENIED && allow_ro {
        // 只读文件：清掉 READONLY 再删
        let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
        if attrs != u32::MAX {
            unsafe { SetFileAttributesW(w.as_ptr(), attrs & !FILE_ATTRIBUTE_READONLY) };
            if unsafe { DeleteFileW(w.as_ptr()) } != 0 {
                return Ok(Slot::I(1));
            }
        }
    }
    if code == ERROR_ACCESS_DENIED {
        // 目录：`File.delete` 也负责删空目录
        if unsafe { RemoveDirectoryW(w.as_ptr()) } != 0 {
            return Ok(Slot::I(1));
        }
    }
    Ok(Slot::I(0))
}

fn fs_rename(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let from = file_path(vm, f.arg_i(0));
    let to = file_path(vm, f.arg_i(1));
    let (wf, wt) = (to_wide(&from), to_wide(&to));
    let ok = unsafe { MoveFileExW(wf.as_ptr(), wt.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    Ok(Slot::I(i32::from(ok != 0)))
}

fn fs_space(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = file_path(vm, f.arg_i(0));
    let kind = f.arg_i(1);
    let w = to_wide(&path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Ok(Slot::L(0));
    }
    Ok(Slot::L(match kind {
        0 => total as i64,
        1 => free as i64,
        _ => avail as i64,
    }))
}

fn fs_name_max(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    let root = drive_root(&path);
    let w = to_wide(&root);
    let mut max_component: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut max_component,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Ok(Slot::I(0));
    }
    Ok(Slot::I(max_component as i32))
}

/// `D:\a\b` → `D:\`
fn drive_root(path: &str) -> String {
    let b: Vec<char> = path.chars().take(3).collect();
    if b.len() >= 2 && b[1] == ':' {
        let d = b[0].to_ascii_uppercase();
        format!("{d}:\\")
    } else {
        "C:\\".to_string()
    }
}

fn fs_drive_dir(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // drive 是 1..=26（A=1）。返回该盘上的当前目录；拿不到就 null。
    let idx = f.arg_i(0);
    if !(1..=26).contains(&idx) {
        return Ok(Slot::I(0));
    }
    let letter = (b'A' + (idx - 1) as u8) as char;
    let spec = format!("{letter}:");
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFullPathNameW(to_wide(&spec).as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) };
    if n == 0 || n as usize >= buf.len() {
        return Ok(Slot::I(0));
    }
    let s = wide_to_string(&buf[..n as usize + 1]);
    let h = vm.str_new(s);
    Ok(Slot::I(h))
}

fn fs_canonicalize(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    let w = to_wide(&path);
    let mut buf = vec![0u16; 4096];
    let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) };
    if n == 0 {
        let code = last_error();
        return Err(io_error(vm, &format!("无法规范化路径 {path}"), code));
    }
    if n as usize >= buf.len() {
        return Err(io_error(vm, &format!("路径过长 {path}"), 0));
    }
    let s = wide_to_string(&buf[..n as usize + 1]);
    let h = vm.str_new(s);
    Ok(Slot::I(h))
}

fn fs_final_path(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let path = vm.text_of(f.arg_i(0));
    let h = match open_file(
        &path,
        0,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
    ) {
        Ok(h) => h,
        Err(code) => return Err(io_error(vm, &format!("无法解析真实路径 {path}"), code)),
    };
    let mut buf = vec![0u16; 4096];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS) };
    unsafe { CloseHandle(h) };
    if n == 0 {
        let code = last_error();
        return Err(io_error(vm, &format!("无法解析真实路径 {path}"), code));
    }
    if n as usize >= buf.len() {
        return Err(io_error(vm, &format!("路径过长 {path}"), 0));
    }
    let mut s = wide_to_string(&buf[..n as usize]);
    // `\\?\D:\x` → `D:\x`；`\\?\UNC\srv\share` → `\\srv\share`
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    let h = vm.str_new(s);
    Ok(Slot::I(h))
}

// ---------------------------------------------------------------- 进程

fn register_process(r: &mut NativeRegistry) {
    let p = "java/lang/ProcessImpl";
    r.add(p, "getStillActive", "()I", |_, _| Ok(Slot::I(STILL_ACTIVE as i32)));
    r.add(p, "create", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[JZ)J", proc_create);
    r.add(p, "getExitCodeProcess", "(J)I", proc_exit_code);
    r.add(p, "getProcessId0", "(J)I", |_, f| {
        Ok(Slot::I(unsafe { GetProcessId(f.arg_l(0) as Handle) } as i32))
    });
    r.add(p, "isProcessAlive", "(J)Z", proc_is_alive);
    r.add(p, "waitForInterruptibly", "(J)V", |vm, f| {
        let h = f.arg_l(0) as Handle;
        if h != 0 {
            unsafe { WaitForSingleObject(h, INFINITE) };
        }
        let _ = vm;
        Ok(Slot::I(0))
    });
    r.add(p, "waitForTimeoutInterruptibly", "(JJ)V", |_, f| {
        let h = f.arg_l(0) as Handle;
        let millis = f.arg_l(1);
        if h != 0 {
            let ms = if millis < 0 { INFINITE } else { millis.min(u32::MAX as i64) as u32 };
            unsafe { WaitForSingleObject(h, ms) };
        }
        Ok(Slot::I(0))
    });
    r.add(p, "terminateProcess", "(J)V", |_, f| {
        let h = f.arg_l(0) as Handle;
        if h != 0 {
            unsafe { TerminateProcess(h, 1) };
        }
        Ok(Slot::I(0))
    });
    r.add(p, "closeHandle", "(J)Z", |_, f| {
        let h = f.arg_l(0) as Handle;
        if h == 0 || h == INVALID_HANDLE_VALUE {
            return Ok(Slot::I(0));
        }
        Ok(Slot::I(i32::from(unsafe { CloseHandle(h) } != 0)))
    });
    r.add(p, "openForAtomicAppend", "(Ljava/lang/String;)J", |vm, f| {
        let path = vm.text_of(f.arg_i(0));
        match open_file(&path, GENERIC_WRITE | FILE_WRITE_DATA, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL) {
            Ok(h) => Ok(Slot::L(h as i64)),
            Err(code) => Err(io_error(vm, &format!("无法打开 {path}"), code)),
        }
    });

    // 父进程的环境块：`ProcessEnvironment.<clinit>` 就靠它建 map
    r.add("java/lang/ProcessEnvironment", "environmentBlock", "()Ljava/lang/String;", |vm, _| {
        let block = unsafe { GetEnvironmentStringsW() };
        if block.is_null() {
            return Ok(Slot::I(0));
        }
        let mut chars: Vec<u16> = Vec::new();
        let mut p = block;
        loop {
            let c = unsafe { *p };
            if c == 0 {
                break;
            }
            // 一段 `k=v\0`
            let mut q = p;
            while unsafe { *q } != 0 {
                chars.push(unsafe { *q });
                q = unsafe { q.add(1) };
            }
            chars.push(0);
            p = unsafe { q.add(1) };
            if unsafe { *p } == 0 {
                break;
            }
            // 下一段（`GetEnvironmentStringsW` 返回的是连续的双 NUL 结尾块）
        }
        unsafe { FreeEnvironmentStringsW(block) };
        let text = String::from_utf16_lossy(&chars);
        // Java 侧靠 `\u0000` 分段：最后再补一个空段（与 HotSpot 的块尾一致）
        let h = vm.str_new(format!("{text}\u{0}"));
        Ok(Slot::I(h))
    });

    // System.currentTimeMillis 之外唯一需要的「当前 pid」
    let ph = "java/lang/ProcessHandleImpl";
    r.add(ph, "initNative", "()V", |_, _| Ok(Slot::I(0)));
    r.add(ph, "getCurrentPid0", "()J", |_, _| Ok(Slot::L(unsafe { GetCurrentProcessId() } as i64)));
    r.add(ph, "isAlive0", "(J)J", |_, f| {
        let pid = f.arg_l(0) as u32;
        let h = unsafe { OpenProcess(0x1000, 0, pid) }; // PROCESS_QUERY_LIMITED_INFORMATION
        if h == 0 {
            return Ok(Slot::L(-1));
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(h, &mut code) };
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return Ok(Slot::L(-1));
        }
        Ok(Slot::L(if code == STILL_ACTIVE { pid as i64 } else { -(code as i64) }))
    });
    r.add(ph, "parent0", "(JJ)J", |_, _| Ok(Slot::L(0)));
    r.add(ph, "destroy0", "(JJZ)Z", |_, f| {
        let pid = f.arg_l(0) as u32;
        let h = unsafe { OpenProcess(0x0001, 0, pid) }; // PROCESS_TERMINATE
        if h == 0 {
            return Ok(Slot::I(0));
        }
        let ok = unsafe { TerminateProcess(h, 1) };
        unsafe { CloseHandle(h) };
        Ok(Slot::I(i32::from(ok != 0)))
    });
    r.add(ph, "waitForProcessExit0", "(JZ)I", |_, f| {
        let pid = f.arg_l(0) as u32;
        let h = unsafe { OpenProcess(0x0010_0000, 0, pid) }; // SYNCHRONIZE
        if h == 0 {
            return Ok(Slot::I(-1));
        }
        unsafe { WaitForSingleObject(h, INFINITE) };
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(h, &mut code) };
        unsafe { CloseHandle(h) };
        Ok(Slot::I(if ok == 0 { -1 } else { code as i32 }))
    });
    r.add(ph, "getProcessPids0", "(J[J[J[J)I", |_, _| Ok(Slot::I(0)));
    r.add("java/lang/ProcessHandleImpl$Info", "initIDs", "()V", |_, _| Ok(Slot::I(0)));
    r.add("java/lang/ProcessHandleImpl$Info", "info0", "(J)V", |_, _| Ok(Slot::I(0)));
}

fn proc_exit_code(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let mut code: u32 = 0;
    if h == 0 || h == INVALID_HANDLE_VALUE || unsafe { GetExitCodeProcess(h, &mut code) } == 0 {
        let err = last_error();
        return Err(io_error(vm, "取退出码失败", err));
    }
    Ok(Slot::I(code as i32))
}

fn proc_is_alive(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_l(0) as Handle;
    let mut code: u32 = 0;
    if h == 0 || h == INVALID_HANDLE_VALUE || unsafe { GetExitCodeProcess(h, &mut code) } == 0 {
        let err = last_error();
        return Err(io_error(vm, "取退出码失败", err));
    }
    Ok(Slot::I(i32::from(code == STILL_ACTIVE)))
}

/// `ProcessImpl.create`：建管道 + `CreateProcessW`，返回进程句柄。
///
/// `stdHandles` 的三项约定（Java 侧写的）：-1 = 要一条管道（native 负责建，
/// 并把**父进程那一端**写回数组），否则是直接继承的现成句柄。
fn proc_create(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let cmd = vm.text_of(f.arg_i(0));
    let env_obj = f.arg_i(1);
    let dir_obj = f.arg_i(2);
    let handles_arr = f.arg_i(3);
    let redirect_err = f.arg_i(4) != 0;

    let mut std_handles = [0i64; 3];
    for (i, slot) in std_handles.iter_mut().enumerate() {
        *slot = vm.sh.heap.get(handles_arr, i).map(|s| s.as_i64()).unwrap_or(-1);
    }

    let mut cmd_w = to_wide(&cmd);
    let dir_w = if dir_obj == 0 {
        None
    } else {
        Some(to_wide(&vm.text_of(dir_obj)))
    };
    let env_w = if env_obj == 0 {
        None
    } else {
        let t = vm.text_of(env_obj);
        let mut v: Vec<u16> = t.encode_utf16().collect();
        if v.last() != Some(&0) {
            v.push(0);
        }
        Some(v)
    };

    // 管道：`sa` 让子进程那一端可继承
    let sa = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: std::ptr::null_mut(),
        inherit: 1,
    };
    let mut si = StartupInfoW { cb: std::mem::size_of::<StartupInfoW>() as u32, ..Default::default() };
    si.flags = STARTF_USESTDHANDLES;

    // child_ends：给 STARTUPINFO 用；parent_ends：写回 stdHandles
    let mut child_ends = [0 as Handle; 3];
    let mut parent_ends = [0 as Handle; 3];
    let mut created_pipes = [false; 3];

    let fail = |vm: &mut Vm, code: u32, what: &str| -> VmError {
        // 把已经建好的管道收掉，别漏句柄
        io_error(vm, what, code)
    };

    for i in 0..3 {
        if std_handles[i] == -1 {
            let (mut rd, mut wr) = (0 as Handle, 0 as Handle);
            if unsafe { CreatePipe(&mut rd, &mut wr, &sa, 0) } == 0 {
                let code = last_error();
                for j in 0..3 {
                    if created_pipes[j] {
                        if parent_ends[j] != 0 {
                            unsafe { CloseHandle(parent_ends[j]) };
                        }
                        if child_ends[j] != 0 {
                            unsafe { CloseHandle(child_ends[j]) };
                        }
                    }
                }
                return Err(fail(vm, code, "创建管道失败"));
            }
            // 父端不可继承：否则下一次 CreateProcess 会把它们也带进子进程
            if i == 0 {
                child_ends[i] = rd;
                parent_ends[i] = wr;
            } else {
                child_ends[i] = wr;
                parent_ends[i] = rd;
            }
            unsafe { SetHandleInformation(parent_ends[i], HANDLE_FLAG_INHERIT, 0) };
            created_pipes[i] = true;
        } else {
            child_ends[i] = std_handles[i] as Handle;
        }
    }

    if redirect_err {
        child_ends[2] = child_ends[1];
    }
    si.std_input = child_ends[0];
    si.std_output = child_ends[1];
    si.std_error = child_ends[2];

    let mut pi = ProcessInformation::default();
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP,
            env_w.as_ref().map(|v| v.as_ptr() as *const c_void).unwrap_or(std::ptr::null()),
            dir_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
            &si,
            &mut pi,
        )
    };

    // 子进程那一端的副本在父进程里一律关掉（读端/写端都是）
    for i in 0..3 {
        if created_pipes[i] {
            // 冻结：保留父端，关掉子端
            let child = child_ends[i];
            if child != 0 && (i == 0 || !redirect_err || i != 2) {
                // stdout 与 stderr 可能共用同一个句柄（redirectErrorStream），只关一次
                if i == 2 && redirect_err {
                    continue;
                }
                unsafe { CloseHandle(child) };
            }
        }
    }

    if ok == 0 {
        let code = last_error();
        for i in 0..3 {
            if created_pipes[i] && parent_ends[i] != 0 {
                unsafe { CloseHandle(parent_ends[i]) };
            }
        }
        return Err(fail(vm, code, "创建进程失败"));
    }
    // 线程句柄用不到，立刻关
    if pi.thread != 0 {
        unsafe { CloseHandle(pi.thread) };
    }

    // 把父端写回数组：Java 会拿它们当 FileDescriptor 用
    for i in 0..3 {
        if created_pipes[i] {
            std_handles[i] = parent_ends[i] as i64;
        }
        vm.sh.heap.set(handles_arr, i, Slot::L(std_handles[i]));
    }

    Ok(Slot::L(pi.process as i64))
}
