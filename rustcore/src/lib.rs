//! 工具链的 Rust 内存层：arena 分配，唯一释放点，无 GC。
use std::alloc::{alloc, dealloc, Layout};
use std::ptr;

pub struct Arena {
    blocks: Vec<(*mut u8, usize)>, // (ptr, layout size)
    bytes: u64,
}

#[no_mangle]
pub extern "C" fn arena_new() -> *mut Arena {
    Box::into_raw(Box::new(Arena { blocks: Vec::new(), bytes: 0 }))
}

#[no_mangle]
pub unsafe extern "C" fn arena_alloc(a: *mut Arena, n: usize) -> *mut u8 {
    if a.is_null() { return ptr::null_mut(); }
    let layout = match Layout::from_size_align(n, 8) { Ok(l) => l, Err(_) => return ptr::null_mut() };
    let p = alloc(layout);
    if p.is_null() { return p; }
    let arena = &mut *a;
    arena.blocks.push((p, n));
    arena.bytes += n as u64;
    p
}

#[no_mangle]
pub unsafe extern "C" fn arena_free(a: *mut Arena) {
    if a.is_null() { return; }
    let arena = Box::from_raw(a);
    for (p, n) in arena.blocks.iter() {
        if let Ok(l) = Layout::from_size_align(*n, 8) { dealloc(*p, l); }
    }
}

#[no_mangle]
pub unsafe extern "C" fn arena_stats(a: *const Arena, out_bytes: *mut u64, out_blocks: *mut u64) {
    if a.is_null() { return; }
    let arena = &*a;
    if !out_bytes.is_null() { *out_bytes = arena.bytes; }
    if !out_blocks.is_null() { *out_blocks = arena.blocks.len() as u64; }
}
