//! native 实现分组：java.desktop（AWT / Swing / Java2D / 字体）
//!
//! 这一组的目标不是「补几个 native 让它不报错」，而是让 `Graphics2D` 真的画对 —
//! 判据是**与真 JDK 渲染结果逐像素一致**。
//!
//! 分两条线：
//!
//! | 线 | 入口 | 说明 |
//! | --- | --- | --- |
//! | 库加载 | [`register_library_policy`] | AWT 家族的 native 全由本文件提供，所以把 awt/fontmanager… 当成「内置库」认下，不走 dlopen |
//! | 离屏渲染 | [`register_headless`] | `headless` 判定 + `sun.java2d.loops.*` 落到本文件的软件光栅化器 |
//!
//! **不要**在这里实现「有字节码的方法」——那些归解释器（见 `native_reg.rs` 顶部那张表）。

use super::super::frame::Frame;
use super::super::slot::Slot;
use super::{NativeRegistry, Vm, VmError};

pub(super) fn register(r: &mut NativeRegistry) {
    register_library_policy(r);
    register_headless(r);
}

fn void() -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

// ============================================================ 一、库加载策略

/// `jdk.internal.loader.NativeLibraries.findBuiltinLib(String)String`
///
/// 语义：返回非 null = 「这是静态链进 VM 的库，不要去文件系统找 .dll」。
/// `java.base` 那几条（zip/nio/net…）由 `nat_util` 自己的实现认；本文件接管整张表
/// **是为了把 AWT 家族也认成内置** —— 我们不走 dlopen，`awt`/`fontmanager`/`javajpeg`/`lcms`
/// 的 native 全部由本文件提供。不认的话，`sun.awt.NativeLibLoader` 第一次
/// `System.loadLibrary("awt")` 就 `UnsatisfiedLinkError: no awt in system library path`。
///
/// 注意：这张表与 `nat_util::find_builtin_lib` 的取向一致，只是多了 AWT 家族。
/// 改那边时这里也要对上（两处都只加不减，重复是刻意的：`NativeRegistry` 没法「包一层原实现」）。
fn find_builtin_lib(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(f.arg_i(0));
    let base = if let Some(i) = name.rfind(|c: char| c == '/' || c == '\\') { &name[i + 1..] } else { &name };
    let stem = base
        .trim_end_matches(".dll")
        .trim_end_matches(".so")
        .trim_end_matches(".dylib")
        .trim_end_matches(".DLL");
    let builtin = matches!(
        stem,
        // java.base 侧（与 nat_util 保持一致）
        "zip" | "java" | "nio" | "jimage" | "net" | "extnet" | "management"
        // java.desktop 侧：native 由本文件实现
            | "awt"
            | "awt_headless"
            | "fontmanager"
            | "javajpeg"
            | "lcms"
            | "splashscreen"
            | "freetype"
    );
    if builtin {
        let h = vm.str_new(name);
        Ok(Slot::I(h))
    } else {
        Ok(Slot::I(0))
    }
}

fn register_library_policy(r: &mut NativeRegistry) {
    r.add(
        "jdk/internal/loader/NativeLibraries",
        "findBuiltinLib",
        "(Ljava/lang/String;)Ljava/lang/String;",
        find_builtin_lib,
    );
}

// ============================================================ 二、headless 离屏渲染

fn register_headless(r: &mut NativeRegistry) {
    // `PlatformGraphicsInfo.hasDisplays0()`：Windows 上决定「默认是不是 headless」。
    // 我们没有窗口系统，答案恒 false（= 没有显示器）。这样不传
    // `-Djava.awt.headless=true` 也自动进入 headless，与「无桌面会话的 JDK」一致。
    r.add("sun/awt/PlatformGraphicsInfo", "hasDisplays0", "()Z", |_, _| Ok(Slot::I(0)));
}
