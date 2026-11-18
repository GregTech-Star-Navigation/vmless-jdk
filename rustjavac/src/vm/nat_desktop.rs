//! native 实现分组：java.desktop（AWT / Swing / Java2D / 字体）
//!
//! 这一组的目标不是「补几个 native 让它不报错」，而是让 `Graphics2D` 真的画对 —
//! 判据是**与真 JDK 渲染结果逐像素一致**。
//!
//! 分三层：
//!
//! | 层 | 内容 | 状态 |
//! | --- | --- | --- |
//! | 库/环境 | `awt` 认成内置库、headless 判定、`initIDs` 全量空实现 | 跑通 |
//! | 面 | `BufImgSurfaceData.initRaster` → 本文件的 [`Surface`] 表 | 跑通 |
//! | 光栅 | `sun.java2d.loops.*` 的 native 落到本文件的光栅化器 | 部分（矩形/线/span/blit） |
//!
//! **不做**：`shape` 的 AA 光栅（走 Ductus/Marlin 的纯 Java 路径）、字体栅格化
//! （见文件末尾「字体」一节：未实现时**明确抛异常**，不静默给黑图）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::super::frame::Frame;
use super::super::slot::Slot;
use super::super::{Vm, VmError};
use super::lk;
use super::NativeRegistry;

pub(super) fn register(r: &mut NativeRegistry) {
    register_library_policy(r);
    register_headless(r);
    register_init_ids(r);
    register_win_env(r);
    register_module_stub(r);
    register_classloader_bridge(r);
    register_surfaces(r);
    register_loops(r);
}

fn void() -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn no_op(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

fn zi(v: i32) -> Result<Slot, VmError> {
    Ok(Slot::I(v))
}

/// 「这条 native 还没实现」—— 明确报错，绝不静默返回 0（否则上层拿到黑图）。
fn unsupported(what: &str) -> VmError {
    VmError {
        msg: format!("vmless: {what} 尚未实现"),
        exc: None,
    }
}

// ============================================================ 一、库加载策略

/// `jdk.internal.loader.NativeLibraries.findBuiltinLib(String)String`
///
/// 语义：返回非 null = 「这是静态链进 VM 的库，不要去文件系统找 .dll」。
/// 本文件接管整张表**是为了把 AWT 家族也认成内置** —— 我们不走 dlopen，
/// `awt`/`fontmanager`/`javajpeg`/`lcms` 的 native 全部由本文件提供。
/// 不认的话，`sun.awt.NativeLibLoader` 第一次 `System.loadLibrary("awt")` 就
/// `UnsatisfiedLinkError: no awt in system library path`。
///
/// 表里前 7 个与 `nat_util::find_builtin_lib` 对齐（那边认 java.base 的静态库）。
fn find_builtin_lib(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(f.arg_i(0));
    let base = match name.rfind(['/', '\\']) {
        Some(i) => &name[i + 1..],
        None => &name[..],
    };
    let stem = base
        .trim_end_matches(".dll")
        .trim_end_matches(".DLL")
        .trim_end_matches(".so")
        .trim_end_matches(".dylib");
    let builtin = matches!(
        stem,
        "zip" | "java" | "nio" | "jimage" | "net" | "extnet" | "management"
            | "awt" | "awt_headless" | "fontmanager" | "javajpeg" | "lcms"
            | "splashscreen" | "freetype"
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

// ============================================================ 二、headless 判定

fn register_headless(r: &mut NativeRegistry) {
    // `PlatformGraphicsInfo.hasDisplays0()`：Windows 上决定「默认是不是 headless」。
    // 我们没有窗口系统 → false（没有显示器），于是不传 `-Djava.awt.headless=true`
    // 也会自动进入 headless，与「无桌面会话的 JDK」一致。
    r.add("sun/awt/PlatformGraphicsInfo", "hasDisplays0", "()Z", |_, _| Ok(Slot::I(0)));
}

// ============================================================ 三、initIDs

/// 这些 `initIDs()` 在 HotSpot 里只做「把 Java 侧字段/方法 ID 存到 C 静态变量」，
/// 我们是直接按名字读写字段的 Rust 实现，所以全部是空操作 —— 但**必须注册**：
/// 不注册就是 `UnsatisfiedLinkError`，类初始化直接失败。
macro_rules! init_ids {
    ($r:expr, $($c:literal),* $(,)?) => {
        $( $r.add($c, "initIDs", "()V", no_op); )*
    };
}

fn register_init_ids(r: &mut NativeRegistry) {
    init_ids!(
        r,
        "java/awt/image/ColorModel",
        "java/awt/image/IndexColorModel",
        "java/awt/image/Kernel",
        "java/awt/image/Raster",
        "java/awt/image/SampleModel",
        "java/awt/image/SinglePixelPackedSampleModel",
        "java/awt/image/BufferedImage",
        "java/awt/Font",
        "java/awt/FontMetrics",
        "java/awt/Dimension",
        "java/awt/Insets",
        "java/awt/Component",
        "java/awt/Container",
        "java/awt/Cursor",
        "java/awt/AWTEvent",
        "java/awt/Event",
        "java/awt/Toolkit",
        "java/awt/event/InputEvent",
        "java/awt/event/KeyEvent",
        "java/awt/event/MouseEvent",
        "sun/awt/FontDescriptor",
        "sun/awt/PlatformFont",
        "sun/awt/Win32GraphicsConfig",
        "sun/awt/Win32GraphicsDevice",
        "sun/awt/image/BufImgSurfaceData",
        "sun/awt/image/ByteComponentRaster",
        "sun/awt/image/BytePackedRaster",
        "sun/awt/image/IntegerComponentRaster",
        "sun/awt/image/ShortComponentRaster",
        "sun/awt/image/ImageRepresentation",
        "sun/awt/windows/WDefaultFontCharset",
        "sun/awt/windows/WFontMetrics",
        "sun/awt/windows/WFontPeer",
        "sun/awt/windows/WToolkit",
        "sun/awt/windows/WObjectPeer",
        "sun/font/SunFontManager",
        "sun/java2d/SurfaceData",
        "sun/java2d/Disposer",
        "sun/java2d/pipe/Region",
        "sun/java2d/pipe/ShapeSpanIterator",
        "sun/java2d/pipe/SpanClipRenderer",
        "sun/java2d/windows/GDIWindowSurfaceData",
    );
    // 带 Class 参数的 initIDs：同样只缓存 ID
    r.add(
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V",
        no_op,
    );
    r.add(
        "sun/awt/image/BufImgSurfaceData",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        no_op,
    );
    r.add(
        "sun/java2d/pipe/SpanClipRenderer",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        no_op,
    );
}

// ============================================================ 四、Windows 图形环境（headless）

fn register_win_env(r: &mut NativeRegistry) {
    // `WindowsFlags.initNativeFlags()`：D3D/OpenGL/directdraw 等本机特性探测。
    // 全是「开启加速」的开关，我们一个都没有 → false（全部走软件路径）。
    r.add("sun/java2d/windows/WindowsFlags", "initNativeFlags", "()Z", |_, _| Ok(Slot::I(0)));
    // `Win32GraphicsEnvironment.<clinit>` 里的 `initDisplay()`：在 HotSpot 里初始化
    // GDI/显示器枚举。headless 下没有任何要初始化的东西。
    r.add("sun/awt/Win32GraphicsEnvironment", "initDisplay", "()V", no_op);
    // 没有显示器 → 0 块屏幕。`getScreenDevices()` 返回空数组。
    r.add("sun/awt/Win32GraphicsEnvironment", "getNumScreens", "()I", |_, _| zi(0));
    r.add("sun/awt/Win32GraphicsEnvironment", "getDefaultScreen", "()I", |_, _| zi(0));
    r.add("sun/awt/Win32GraphicsEnvironment", "getXResolution", "()I", |_, _| zi(96));
    r.add("sun/awt/Win32GraphicsEnvironment", "getYResolution", "()I", |_, _| zi(96));
    r.add("sun/awt/Win32GraphicsEnvironment", "isVistaOS", "()Z", |_, _| zi(1));
    r.add("sun/awt/Win32GraphicsDevice", "initIDs", "()V", no_op);
    // `StackWalker` 的自检：Java 侧常量就是真相 → true
    r.add("java/lang/StackStreamFactory", "checkStackWalkModes", "()Z", |_, _| zi(1));
}

// ============================================================ 五、模块系统替身

/// `java.lang.Class.getModule()Ljava/lang/Module;`（**VM 接管**，不是 native）
///
/// 真 HotSpot 在造 Class 对象时就把 `Class.module` 填上（"set by VM"）。本 VM 没有
/// JPMS，那个字段恒 null —— 于是 `ResourceBundle.getBundle()` 走到
/// `CacheKey.<init>` 的 `Objects.requireNonNull(module)` 抛 NPE。
///
/// `java.awt.Toolkit.<clinit>` 里就有一次 `ResourceBundle.getBundle("sun.awt.resources.awt")`
/// （只 catch `MissingResourceException`），AWT 于是连类都初始化不了。
fn cls_get_module(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(unnamed_module(vm)))
}

fn unnamed_module(vm: &mut Vm) -> i32 {
    let key = ("java/lang/Module".to_string(), "$vmless_unnamed".to_string());
    let cached = {
        let g = lk(&vm.sh.statics);
        match g.get(&key).copied() {
            Some(Slot::I(h)) if h > 0 => Some(h),
            _ => None,
        }
    };
    if let Some(h) = cached {
        return h;
    }
    // `Module.<clinit>` 自己会造 `ALL_UNNAMED_MODULE = new Module(null)`。
    // 用它（而不是自造对象）是因为 `Module.ensureNativeAccess` 里用的是
    // `moduleForNativeAccess() → ALL_UNNAMED_MODULE`：不设**它**的 native-access 位，
    // 任何 native 调用都会走「warning + System.err.printf」那条路，
    // 而 System.err 在本 VM 里是 null → NPE。
    if vm.ensure_init("java/lang/Module").is_err() {
        return 0;
    }
    let all = {
        let g = lk(&vm.sh.statics);
        match g.get(&("java/lang/Module".to_string(), "ALL_UNNAMED_MODULE".to_string())).copied() {
            Some(Slot::I(h)) if h > 0 => Some(h),
            _ => None,
        }
    };
    let h = match all {
        Some(h) => h,
        None => vm.alloc_vmobj("java/lang/Module"),
    };
    if h > 0 {
        // `--enable-native-access=ALL-UNNAMED` 的等价物：本 VM 的 native 全部内置，
        // 没有「受限 native 访问」这回事。
        vm.put_named(h, "java/lang/Module", "enableNativeAccess", Slot::I(1));
        lk(&vm.sh.statics).insert(key, Slot::I(h));
    }
    h
}

fn register_module_stub(r: &mut NativeRegistry) {
    // `Class` 在 `is_override_owner` 的白名单里（exec.rs:1557），所以可以直接接管
    r.add_override("java/lang/Class", "getModule", "()Ljava/lang/Module;", cls_get_module);
}

// ============================================================ 六、类加载桥

/// `java.lang.ClassLoader.findLoadedClass0(String)Class`
///
/// java.base 侧本应有一条（HotSpot 里是 VM 内建），但 AWT 初始化链
/// （`Toolkit.<clinit>` → `ResourceBundle` → `Class.forName(..., loader)`）会撞上它。
/// 本 VM 只有一个类表、一个「加载器」，所以「已经加载过」= 类表里有。
fn cl_find_loaded_class(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(f.arg_i(1)).replace('.', "/");
    if vm.store.has_class(&name) {
        Ok(Slot::I(vm.class_obj(&name)))
    } else {
        Ok(Slot::I(0))
    }
}

fn register_classloader_bridge(r: &mut NativeRegistry) {
    r.add(
        "java/lang/ClassLoader",
        "findLoadedClass0",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_loaded_class,
    );
    r.add(
        "java/lang/ClassLoader",
        "findBootstrapClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_loaded_class,
    );
}

// ============================================================ 七、面（Surface）

/// `java.awt.image.BufferedImage` 的 `imageType` 常量（与 Java 侧一致）
const TYPE_INT_RGB: i32 = 1;
const TYPE_INT_ARGB: i32 = 2;
const TYPE_INT_ARGB_PRE: i32 = 3;
const TYPE_INT_BGR: i32 = 4;
const TYPE_3BYTE_BGR: i32 = 5;
const TYPE_4BYTE_ABGR: i32 = 6;
const TYPE_4BYTE_ABGR_PRE: i32 = 7;
const TYPE_USHORT_565_RGB: i32 = 8;
const TYPE_USHORT_555_RGB: i32 = 9;

/// 一块 `BufImgSurfaceData` 的像素视图。
///
/// `initRaster` 给的 `offset/pixStr/scanStr` 都是**字节**单位（见 `BufImgSurfaceData.createData*`），
/// 这里换成「元素下标」来读写 Java 数组 —— 这样 `getRGB()` 读到的就是同一份数据。
#[derive(Clone, Copy)]
struct Surface {
    array: i32,
    /// 第 (0,0) 像素在数组里的元素下标
    base: i32,
    width: i32,
    height: i32,
    /// 每个像素跨几个元素
    pix_step: i32,
    /// 每行跨几个元素
    scan_step: i32,
    image_type: i32,
    /// 每个元素是：0 = int[]，1 = short[]，2 = byte[]
    elem: u8,
}

static SURFACES: OnceLock<Mutex<HashMap<i32, Surface>>> = OnceLock::new();

fn surfaces() -> &'static Mutex<HashMap<i32, Surface>> {
    SURFACES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn surface_of(handle: i32) -> Option<Surface> {
    surfaces().lock().unwrap_or_else(|e| e.into_inner()).get(&handle).copied()
}

fn get_i32_field(vm: &Vm, obj: i32, class: &str, name: &str) -> i32 {
    vm.store
        .field_index(class, name)
        .and_then(|i| vm.sh.heap.get(obj, i))
        .map(|s| s.as_i32())
        .unwrap_or(0)
}

fn get_ref_field(vm: &Vm, obj: i32, class: &str, name: &str) -> i32 {
    vm.store
        .field_index(class, name)
        .and_then(|i| vm.sh.heap.get(obj, i))
        .map(|s| s.as_i32())
        .unwrap_or(0)
}

fn surface_put(vm: &mut Vm, s: &Surface, index: i32, value: i32) {
    let h = s.array;
    match s.elem {
        0 => vm.sh.heap.set(h, index as usize, Slot::I(value)),
        1 => vm.sh.heap.set(h, index as usize, Slot::I(value & 0xFFFF)),
        _ => vm.sh.heap.set(h, index as usize, Slot::I(value & 0xFF)),
    }
}

fn surface_get(vm: &Vm, s: &Surface, index: i32) -> i32 {
    vm.sh.heap.get(s.array, index as usize).map(|v| v.as_i32()).unwrap_or(0)
}

/// `BufImgSurfaceData.initRaster(Object theArray, int offset, int bitoffset, int width,
/// int height, int pixStr, int scanStr, IndexColorModel icm)`
///
/// 记下「哪个 SurfaceData 对象 ↔ 哪块 Java 数组 + 视图参数」。像素**始终留在 Java 数组里**，
/// 所以 `BufferedImage.getRGB()` 读到的就是我们画进去的字节 —— 没有第二份真相。
fn surf_init_raster(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let array = f.arg_i(0);
    let offset = f.arg_i(1);
    let _bitoffset = f.arg_i(2);
    let width = f.arg_i(3);
    let height = f.arg_i(4);
    let pix_str = f.arg_i(5);
    let scan_str = f.arg_i(6);
    let elem = match vm.class_of(array).as_deref() {
        Some("[I") => 0u8,
        Some("[S") => 1,
        Some("[B") => 2,
        _ => 0,
    };
    let elem_size = match elem {
        0 => 4,
        1 => 2,
        _ => 1,
    };
    // `BufImgSurfaceData.bufImg` → `BufferedImage.imageType`
    let img = get_ref_field(vm, this, "sun/awt/image/BufImgSurfaceData", "bufImg");
    let image_type = if img > 0 { get_i32_field(vm, img, "java/awt/image/BufferedImage", "imageType") } else { 0 };
    let s = Surface {
        array,
        base: offset / elem_size,
        width,
        height,
        pix_step: (pix_str / elem_size).max(1),
        scan_step: (scan_str / elem_size).max(1),
        image_type,
        elem,
    };
    surfaces().lock().unwrap_or_else(|e| e.into_inner()).insert(this, s);
    void()
}

fn register_surfaces(r: &mut NativeRegistry) {
    r.add(
        "sun/awt/image/BufImgSurfaceData",
        "initRaster",
        "(Ljava/lang/Object;IIIIIILjava/awt/image/IndexColorModel;)V",
        surf_init_raster,
    );
}

// ---------------------------------------------------------------- 像素道具

/// `SunGraphics2D` 里光栅化需要的那几个字段
struct G2d {
    /// ARGB，已把 extra alpha 乘进去（`SunGraphics2D.eargb`）
    eargb: i32,
    /// 目标格式下的像素值（`SunGraphics2D.pixel`）
    pixel: i32,
    composite_state: i32,
    paint_state: i32,
    /// `SunGraphics2D.devClip`（设备坐标裁剪）
    dev_clip: i32,
}

fn read_g2d(vm: &Vm, sg: i32) -> G2d {
    G2d {
        eargb: get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "eargb"),
        pixel: get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "pixel"),
        composite_state: get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "compositeState"),
        paint_state: get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "paintState"),
        dev_clip: get_ref_field(vm, sg, "sun/java2d/SunGraphics2D", "devClip"),
    }
}

/// `sun.java2d.SunGraphics2D` 的 compositeState 常量（与 Java 侧一致）
const COMP_ISCOPY: i32 = 0;
const COMP_XOR: i32 = 2;
const COMP_CUSTOM: i32 = 3;

/// 把 `Region` 摊平成「每条扫描线一组 [x0,x1)」。
///
/// `Region.bands` 的布局（`Region.getInstance(...)`）：每段是
/// `[yStart, yEnd, numSpans, x0start, x0end, ...]`，段与段首尾相接。
fn region_spans(vm: &Vm, region: i32) -> Option<Vec<(i32, i32, i32)>> {
    if region <= 0 {
        return None;
    }
    let lo_y = get_i32_field(vm, region, "sun/java2d/pipe/Region", "loy");
    let hi_y = get_i32_field(vm, region, "sun/java2d/pipe/Region", "hiy");
    let bands_h = get_ref_field(vm, region, "sun/java2d/pipe/Region", "bands");
    if bands_h <= 0 {
        return None;
    }
    let n = vm.sh.heap.len(bands_h);
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut guard = 0;
    while i + 3 <= n && guard < 10_000 {
        guard += 1;
        let y0 = vm.sh.heap.get(bands_h, i).map(|s| s.as_i32()).unwrap_or(0);
        let y1 = vm.sh.heap.get(bands_h, i + 1).map(|s| s.as_i32()).unwrap_or(0);
        let spans = vm.sh.heap.get(bands_h, i + 2).map(|s| s.as_i32()).unwrap_or(0).max(0) as usize;
        i += 3;
        for _ in 0..spans {
            if i + 2 > n {
                break;
            }
            let x0 = vm.sh.heap.get(bands_h, i).map(|s| s.as_i32()).unwrap_or(0);
            let x1 = vm.sh.heap.get(bands_h, i + 1).map(|s| s.as_i32()).unwrap_or(0);
            i += 2;
            if x0 < x1 {
                out.push((y0, x0, x1));
            }
        }
        if y1 <= y0 {
            break;
        }
    }
    if out.is_empty() && hi_y > lo_y {
        // bands 解不出来就退化成「整块」（调用方另有限制）；比静默不画好
        return Some(vec![(lo_y, i32::MIN, i32::MAX)]);
    }
    Some(out)
}


/// SrcOver 合成：src（非预乘 ARGB）覆盖到 dst
/// 非预乘 ARGB 的 SrcOver 必须**先预乘、合成、再反预乘**（Java2D 的 IntArgb 循环就是这么做的）：
///   outA  = sa + div8(da * (255-sa))
///   outPc = div8(sc * sa) + div8(div8(dc * da) * (255-sa))
///   outc  = div8(outPc * 255 / outA)
/// 少了反预乘这一步，半透明色叠在**不透明**背景上会偏亮（实测 ref r=0x8f，直算得 0xff）。
fn blend_src_over(dst: i32, src: i32, image_type: i32) -> i32 {
    let ch = |v: i32, sh: u32| -> i32 { ((v as u32) >> sh) as i32 & 0xFF };
    let sa = ch(src, 24);
    if sa == 0 {
        return dst;
    }
    if sa == 255 {
        return match image_type {
            TYPE_INT_ARGB | TYPE_INT_ARGB_PRE => src,
            TYPE_INT_BGR => src,
            _ => src | 0xFF00_0000u32 as i32,
        };
    }
    let inv = 255 - sa;
    let da = ch(dst, 24);
    let out_a = sa + div255(da * inv);
    match image_type {
        TYPE_INT_ARGB | TYPE_INT_ARGB_PRE => {
            let unpremul = image_type == TYPE_INT_ARGB;
            let c = |dv: i32, sv: i32| -> i32 {
                let sp = div255(sv * sa);
                let dp = div255(dv * da);
                let outp = sp + div255(dp * inv);
                if unpremul && out_a > 0 {
                    ((outp * 255 + out_a / 2) / out_a).min(255)
                } else {
                    outp
                }
            };
            (out_a << 24) | (c(ch(dst, 16), ch(src, 16)) << 16) | (c(ch(dst, 8), ch(src, 8)) << 8) | c(ch(dst, 0), ch(src, 0))
        }
        _ => {
            let c = |dv: i32, sv: i32| -> i32 { sv + div255(dv * inv) };
            let r = c(ch(dst, 16), ch(src, 16));
            let g = c(ch(dst, 8), ch(src, 8));
            let bl = c(ch(dst, 0), ch(src, 0));
            0xFF00_0000u32 as i32 | (r << 16) | (g << 8) | bl
        }
    }
}

/// round(a*b/255)
fn div255(a: i32) -> i32 {
    let t = a + 0x80;
    ((t >> 8) + t) >> 8
}

/// 一块矩形填充（已在设备坐标；`clip` 已裁剪好）
fn fill_span(vm: &mut Vm, s: &Surface, g: &G2d, y: i32, x0: i32, x1: i32) {
    if y < 0 || y >= s.height {
        return;
    }
    let x0 = x0.max(0);
    let x1 = x1.min(s.width);
    if x0 >= x1 {
        return;
    }
    let base = s.base + y * s.scan_step + x0 * s.pix_step;
    let alpha = ((g.eargb as u32) >> 24) as i32 & 0xFF;
    for i in 0..(x1 - x0) {
        let idx = base + i * s.pix_step;
        if alpha == 255 {
            surface_put(vm, s, idx, g.pixel);
        } else if alpha > 0 {
            let dst = surface_get(vm, s, idx);
            surface_put(vm, s, idx, blend_src_over(dst, g.eargb, s.image_type));
        }
    }
}

// ============================================================ 八、loops

/// `sun.java2d.loops.FillRect.FillRect(SunGraphics2D, SurfaceData, x, y, w, h)`
///
/// 这是软件管线的**主力**：`LoopPipe.fillRect` 直接调它。坐标已经是设备坐标
/// （`LoopPipe` 加了 `transX/transY`），裁剪走 `devClip`。
fn loop_fill_rect(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let x = f.arg_i(2);
    let y = f.arg_i(3);
    let w = f.arg_i(4);
    let h = f.arg_i(5);
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("FillRect：目标 SurfaceData 不是本 VM 记录的 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    if g.composite_state == COMP_CUSTOM || g.composite_state == COMP_XOR {
        return Err(unsupported("FillRect：自定义/XOR 合成"));
    }
    let x1 = x.saturating_add(w);
    let y1 = y.saturating_add(h);
    let clip = region_spans(vm, g.dev_clip);
    match clip {
        Some(spans) if !spans.is_empty() => {
            for (cy0, cx0, cx1) in spans {
                let cy = cy0.max(y);
                let cy1 = (cy0 + 1).min(y1);
                let cx0 = cx0.max(x);
                let cx1 = cx1.min(x1);
                let mut yy = cy;
                while yy < cy1 {
                    fill_span(vm, &s, &g, yy, cx0, cx1);
                    yy += 1;
                }
            }
        }
        _ => {
            let mut yy = y.max(0);
            while yy < y1.min(s.height) {
                fill_span(vm, &s, &g, yy, x, x1);
                yy += 1;
            }
        }
    }
    void()
}

/// `sun.java2d.loops.DrawRect.DrawRect(SunGraphics2D, SurfaceData, x, y, w, h)`
fn loop_draw_rect(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let (x, y, w, h) = (f.arg_i(2), f.arg_i(3), f.arg_i(4), f.arg_i(5));
    draw_rect_impl(vm, sg, dst, x, y, w, h)
}

fn draw_rect_impl(vm: &mut Vm, sg: i32, dst: i32, x: i32, y: i32, w: i32, h: i32) -> Result<Slot, VmError> {
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("DrawRect：目标不是 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    if w <= 0 || h <= 0 {
        return void();
    }
    let x2 = x + w - 1;
    let y2 = y + h - 1;
    // 四条边（与 Java2D 的 DrawRect 一致：把矩形边当作 4 条 1px 线，**不重复画角**）
    let edges = [
        (y, x, x + w),          // top
        (y2, x, x + w),         // bottom
        (y + 1, x, x + 1),      // left（不含角）
        (y + 1, x2, x2 + 1),    // right
    ];
    for (yy, x0, x1) in edges {
        if yy < 0 || yy >= s.height {
            continue;
        }
        let clipped = clip_span(vm, g.dev_clip, yy, x0, x1);
        for (a, b) in clipped {
            fill_span(vm, &s, &g, yy, a, b);
        }
    }
    void()
}

/// 用 `devClip` 裁一条水平 span
fn clip_span(vm: &Vm, region: i32, y: i32, x0: i32, x1: i32) -> Vec<(i32, i32)> {
    match region_spans(vm, region) {
        Some(spans) => spans
            .into_iter()
            .filter(|(sy, _, _)| *sy == y)
            .map(|(_, a, b)| (a.max(x0), b.min(x1)))
            .filter(|(a, b)| a < b)
            .collect(),
        None => vec![(x0, x1)],
    }
}

/// `sun.java2d.loops.DrawLine.DrawLine(SunGraphics2D, SurfaceData, x1, y1, x2, y2)`
///
/// Bresenham（与 Java2D 的 `IntArgbDrawLine` 同一取向：取整方向按 dx/dy 符号）。
fn loop_draw_line(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let (mut x0, mut y0, x1, y1) = (f.arg_i(2), f.arg_i(3), f.arg_i(4), f.arg_i(5));
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("DrawLine：目标不是 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;
    loop {
        for (a, b) in clip_span(vm, g.dev_clip, y0, x0, x0 + 1) {
            fill_span(vm, &s, &g, y0, a, b);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x0 += sx;
        }
        if e2 < dx {
            err += dx;
            y0 += sy;
        }
    }
    void()
}

/// `sun.java2d.loops.FillSpans.FillSpans(SunGraphics2D, SurfaceData, int, long, SpanIterator)`
///
/// shape 的**非 AA** 填充最终都走这里（`SpanShapeRenderer`）。`SpanIterator` 是
/// `ShapeSpanIterator`（Ductus native）—— 那 19 个 native 我们还没做，所以这里明确报错，
/// 让 `fillOval`/`fillPolygon` 之类**大声失败**而不是画错。
fn loop_fill_spans(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported("FillSpans（shape 填充，需要 ShapeSpanIterator/Ductus）"))
}

/// `sun.java2d.loops.Blit.Blit(...)`：同一套像素格式之间的矩形复制（`copyArea`/`drawImage`）
fn loop_blit(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let src = f.arg_i(0);
    let dst = f.arg_i(1);
    let sx = f.arg_i(4);
    let sy = f.arg_i(5);
    let dx = f.arg_i(6);
    let dy = f.arg_i(7);
    let w = f.arg_i(8);
    let h = f.arg_i(9);
    let (Some(ss), Some(ds)) = (surface_of(src), surface_of(dst)) else {
        return Err(unsupported("Blit：源或目标不是 BufImgSurfaceData"));
    };
    if ss.image_type != ds.image_type {
        return Err(unsupported("Blit：源/目标格式不同（需要转换循环）"));
    }
    // 复制到临时缓冲，避免重叠时踩到自己的尾巴
    let mut tmp = Vec::with_capacity((w.max(0) * h.max(0)) as usize);
    for yy in 0..h {
        for xx in 0..w {
            let i = ss.base + (sy + yy) * ss.scan_step + (sx + xx) * ss.pix_step;
            tmp.push(surface_get(vm, &ss, i));
        }
    }
    // SrcOver：源像素是不透明就直写，半透明就按 alpha 混合（Java2D 的 Blit 在
    // SrcOver 下就是这个语义；拷贝到临时缓冲是为了重叠区域不踩自己）
    for yy in 0..h {
        for xx in 0..w {
            let i = ds.base + (dy + yy) * ds.scan_step + (dx + xx) * ds.pix_step;
            let v = tmp[(yy * w + xx) as usize];
            let a = ((v as u32) >> 24) as i32 & 0xFF;
            if a == 255 {
                surface_put(vm, &ds, i, v);
            } else if a > 0 {
                let d = surface_get(vm, &ds, i);
                surface_put(vm, &ds, i, blend_src_over(d, v, ds.image_type));
            }
        }
    }
    void()
}

fn loop_blit_bg(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported("BlitBg（带背景色的 blit）"))
}

fn loop_mask_fill(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported("MaskFill（AA 填充，需要 AATileGenerator）"))
}

fn loop_mask_blit(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported("MaskBlit（AA 合成）"))
}

fn loop_glyph_list(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported("DrawGlyphList（字体栅格化；见文件末尾「字体」）"))
}

// ---------------------------------------------------------------- 注册 native loops

/// 目标面类型（按  的静态字段名）
const SURFACE_TYPES: [&str; 8] = [
    "IntRgb", "IntArgb", "IntArgbPre", "IntRgbx", "ThreeByteBgr", "FourByteAbgr", "FourByteAbgrPre",
    "Ushort565Rgb",
];
const COMPOSITE_TYPES: [&str; 5] = ["SrcNoEa", "SrcOver", "Src", "SrcIn", "SrcOut"];

fn static_ref(vm: &Vm, class: &str, name: &str) -> i32 {
    lk(&vm.sh.statics)
        .get(&(class.to_string(), name.to_string()))
        .map(|s| s.as_i32())
        .unwrap_or(0)
}

fn call_java(
    vm: &mut Vm,
    class: &str,
    name: &str,
    desc: &str,
    args: &[Slot],
    recv: i32,
) -> Result<Slot, VmError> {
    let receiver = recv > 0;
    let target = if receiver {
        vm.store.resolve_method(class, name, desc)
    } else {
        vm.store.method_in_class(class, name, desc)
    };
    let Some((ci, mi)) = target else {
        return Err(VmError { msg: format!("找不到 {class}.{name}{desc}"), exc: None });
    };
    vm.depth += 1;
    let r = vm.execute(ci, mi, desc, args, receiver);
    vm.depth -= 1;
    r
}

fn alloc_instance(vm: &mut Vm, class: &str) -> i32 {
    if vm.ensure_class_loaded(class) {
        vm.alloc_vmobj(class)
    } else {
        0
    }
}

/// 造一个 primitive 实例：公开构造器 `(J, SurfaceType, CompositeType, SurfaceType)`
fn make_prim(vm: &mut Vm, class: &str, id: i64, src: i32, comp: i32, dst: i32) -> i32 {
    let obj = alloc_instance(vm, class);
    if obj == 0 || src == 0 || comp == 0 || dst == 0 {
        return 0;
    }
    let desc = "(JLsun/java2d/loops/SurfaceType;Lsun/java2d/loops/CompositeType;Lsun/java2d/loops/SurfaceType;)V";
    let _ = call_java(vm, class, "<init>", desc, &[Slot::I(obj), Slot::L(id), Slot::I(src), Slot::I(comp), Slot::I(dst)], obj);
    obj
}

/// `sun.java2d.loops.GraphicsPrimitiveMgr.registerNativeLoops()`
///
/// HotSpot 里这条 native 用 C 侧循环表造出上千个 GraphicsPrimitive 实例，再交给
/// GraphicsPrimitiveMgr.register(...)。我们不实现那套函数指针表，只造**我们真能服务的**
/// 组合：实例是 FillRect/DrawRect/DrawLine/Blit/MaskBlit 本身（不是 General 子类），
/// 所以 Java 侧 locate() 命中后调到的就是本文件的 native。
///
/// 不注册 MaskFill：它没有公开的 (J,...) 构造器（HotSpot 走另一条注册路），
/// 让它落到 MaskFill.General（纯 Java），后者会调我们的 FillRect + MaskBlit。
fn prim_register_native_loops(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    for c in [
        "sun/java2d/loops/SurfaceType",
        "sun/java2d/loops/CompositeType",
        "sun/java2d/loops/GraphicsPrimitive",
    ] {
        let _ = vm.ensure_init(c);
    }
    let mut items: Vec<Slot> = Vec::new();
    let mut id: i64 = 1;
    let argb = static_ref(vm, "sun/java2d/loops/SurfaceType", "IntArgb");
    // 1) 颜色 → 面：FillRect / DrawRect / DrawLine
    for class in [
        "sun/java2d/loops/FillRect",
        "sun/java2d/loops/DrawRect",
        "sun/java2d/loops/DrawLine",
    ] {
        for st in SURFACE_TYPES {
            let s = static_ref(vm, "sun/java2d/loops/SurfaceType", st);
            for ct in COMPOSITE_TYPES {
                let c = static_ref(vm, "sun/java2d/loops/CompositeType", ct);
                id += 1;
                let o = make_prim(vm, class, id, s, c, s);
                if o > 0 {
                    items.push(Slot::I(o));
                }
            }
        }
    }
    // 2) Blit：同格式复制（IntArgb → IntArgb 等）
    for st in SURFACE_TYPES {
        let s = static_ref(vm, "sun/java2d/loops/SurfaceType", st);
        for ct in COMPOSITE_TYPES {
            let c = static_ref(vm, "sun/java2d/loops/CompositeType", ct);
            id += 1;
            let o = make_prim(vm, "sun/java2d/loops/Blit", id, s, c, s);
            if o > 0 {
                items.push(Slot::I(o));
            }
        }
    }
    // 3) MaskBlit：掩膜（IntArgb）→ 面
    for st in SURFACE_TYPES {
        let s = static_ref(vm, "sun/java2d/loops/SurfaceType", st);
        for ct in COMPOSITE_TYPES {
            let c = static_ref(vm, "sun/java2d/loops/CompositeType", ct);
            id += 1;
            let o = make_prim(vm, "sun/java2d/loops/MaskBlit", id, argb, c, s);
            if o > 0 {
                items.push(Slot::I(o));
            }
        }
    }
    if items.is_empty() {
        return Err(unsupported("registerNativeLoops：一个 primitive 都没造出来（SurfaceType/CompositeType 未初始化？）"));
    }
    let arr = vm.sh.heap.new_array(items, "[Lsun/java2d/loops/GraphicsPrimitive;").unwrap_or(0);
    let _ = call_java(
        vm,
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "register",
        "([Lsun/java2d/loops/GraphicsPrimitive;)V",
        &[Slot::I(arr)],
        0,
    );
    void()
}

fn register_loops(r: &mut NativeRegistry) {
    // Unsafe.addressSize()：nat_intrinsics 里返回 0，而 JDK 25 的 jdk.internal.foreign.Utils.<clinit>
    // 用它算 ValueLayout.ADDRESS 的对齐，0 会抛 IllegalArgumentException: Invalid alignment: 0，
    // 于是任何用到 java.lang.foreign 的类（含 JDK 25 的 HarfBuzz 文本布局）都初始化不了。
    r.add("jdk/internal/misc/Unsafe", "addressSize", "()I", |_, _| zi(8));

    r.add("sun/java2d/loops/GraphicsPrimitiveMgr", "registerNativeLoops", "()V", prim_register_native_loops);
    r.add(
        "sun/java2d/loops/FillRect",
        "FillRect",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        loop_fill_rect,
    );
    r.add(
        "sun/java2d/loops/DrawRect",
        "DrawRect",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        loop_draw_rect,
    );
    r.add(
        "sun/java2d/loops/DrawLine",
        "DrawLine",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        loop_draw_line,
    );
    r.add(
        "sun/java2d/loops/FillSpans",
        "FillSpans",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IJLsun/java2d/pipe/SpanIterator;)V",
        loop_fill_spans,
    );
    r.add(
        "sun/java2d/loops/Blit",
        "Blit",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIII)V",
        loop_blit,
    );
    r.add(
        "sun/java2d/loops/BlitBg",
        "BlitBg",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIIII)V",
        loop_blit_bg,
    );
    r.add(
        "sun/java2d/loops/MaskFill",
        "MaskFill",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Ljava/awt/Composite;IIII[BII)V",
        loop_mask_fill,
    );
    r.add(
        "sun/java2d/loops/MaskBlit",
        "MaskBlit",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIII[BII)V",
        loop_mask_blit,
    );
    for (cls, name) in [
        ("sun/java2d/loops/DrawGlyphList", "DrawGlyphList"),
        ("sun/java2d/loops/DrawGlyphListAA", "DrawGlyphListAA"),
        ("sun/java2d/loops/DrawGlyphListLCD", "DrawGlyphListLCD"),
        ("sun/java2d/loops/DrawGlyphListColor", "DrawGlyphListColor"),
    ] {
        // 描述符逐个写（`GlyphList` 在 sun.font 包里）
        let desc = match name {
            "DrawGlyphList" | "DrawGlyphListAA" | "DrawGlyphListLCD" => {
                "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Lsun/font/GlyphList;II)V"
            }
            _ => "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Lsun/font/GlyphList;III)V",
        };
        r.add(cls, name, desc, loop_glyph_list);
    }
}
