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
    register_window(r);
    register_shapes(r);
    register_text(r);
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

/// 给任意 native 套一层「进/出」追踪（`VMLESS_AWT_TRACE=1` 才打）
macro_rules! traced {
    ($name:literal, $f:expr) => {{
        fn w(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
            atrace(concat!("> ", $name));
            let r = $f(vm, f);
            atrace(concat!("< ", $name));
            r
        }
        w
    }};
}

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
    r.add("sun/awt/PlatformGraphicsInfo", "hasDisplays0", "()Z", |_, _| Ok(Slot::I(1)));
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
    r.add("sun/awt/Win32GraphicsEnvironment", "getNumScreens", "()I", trace_num_screens);
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
    atrace("Class.getModule");
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
    atrace(&format!("findLoadedClass0 {name}"));
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
    flush_surface(vm, dst);
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
    flush_surface(vm, dst);
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
    let (x0, y0, x1, y1) = (f.arg_i(2), f.arg_i(3), f.arg_i(4), f.arg_i(5));
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("DrawLine：目标不是 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    bresenham_line(vm, &s, &g, x0, y0, x1, y1);
    flush_surface(vm, dst);
    void()
}

/// 读 `Path2D.Float` 的内部坐标，摊成 `(type, coords)` 段（坐标已加 transX/transY）。
///
/// 不走 `PathIterator` —— 那是 Java 方法，读一次要再进解释器；直接读它的四个字段：
/// `floatCoords`（`Path2D.Float`）、`pointTypes`/`numTypes`/`windingRule`（`Path2D`）。
/// 段类型常量与 `PathIterator` 一致：0=move 1=line 2=quad 3=cubic 4=close。
fn path_segs(vm: &Vm, path: i32, tx: i32, ty: i32) -> Option<(Vec<(u8, [f32; 6])>, i32)> {
    if path <= 0 {
        return None;
    }
    let coords = get_ref_field(vm, path, "java/awt/geom/Path2D$Float", "floatCoords");
    let types = get_ref_field(vm, path, "java/awt/geom/Path2D", "pointTypes");
    if coords <= 0 || types <= 0 {
        return None;
    }
    let ntypes = get_i32_field(vm, path, "java/awt/geom/Path2D", "numTypes");
    let rule = get_i32_field(vm, path, "java/awt/geom/Path2D", "windingRule");
    let (fx, fy) = (tx as f32, ty as f32);
    let mut segs: Vec<(u8, [f32; 6])> = Vec::new();
    let mut ci: usize = 0;
    for ti in 0..ntypes.max(0) as usize {
        let t = vm.sh.heap.get(types, ti).map(|v| v.as_i32()).unwrap_or(0) as u8;
        let mut c = [0.0f32; 6];
        let n = match t {
            0 | 1 => 2,
            2 => 4,
            3 => 6,
            _ => 0,
        };
        for k in 0..n {
            let v = vm.sh.heap.get(coords, ci + k).map(fl).unwrap_or(0.0);
            c[k] = if k % 2 == 0 { v + fx } else { v + fy };
        }
        ci += n;
        segs.push((t, c));
    }
    Some((segs, rule))
}

/// 一条 Bresenham 线（thin stroke 的像素级做法，与 `IntArgbDrawLine` 同一取向）
fn bresenham_line(vm: &mut Vm, s: &Surface, g: &G2d, mut x0: i32, mut y0: i32, x1: i32, y1: i32) {
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;
    loop {
        for (a, b) in clip_span(vm, g.dev_clip, y0, x0, x0 + 1) {
            fill_span(vm, s, g, y0, a, b);
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
}

/// `sun.java2d.loops.FillPath.FillPath(SunGraphics2D, SurfaceData, int transX, int transY, Path2D.Float)`
///
/// 这条 loop 是 **JDK 25 里 `Graphics2D.fill(Shape)` / `fillOval` 的唯一出口**
/// （`LoopPipe.fill` 在 `STROKE_THIN` 下直接调它，不再走 `ShapeSpanIterator`）。
/// 不实现它就落到 `GeneralRenderer.SetFillPathANY`（纯 Java 的 span 管线），
/// 那条路在本 VM 上一像素都不落 —— 实测 `fillOval` 整块消失。
///
/// 实现直接复用 `Spi` 的扫描线（与 `fillPolygon` 同一套规则），所以两种画法边界一致。
fn loop_fill_path(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let (tx, ty) = (f.arg_i(2), f.arg_i(3));
    let path = f.arg_i(4);
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("FillPath：目标 SurfaceData 不是本 VM 记录的 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    if g.composite_state == COMP_CUSTOM || g.composite_state == COMP_XOR {
        return Err(unsupported("FillPath：自定义/XOR 合成"));
    }
    let Some((segs, rule)) = path_segs(vm, path, tx, ty) else {
        return Err(unsupported("FillPath：Path2D.Float 的 floatCoords/pointTypes 读不到"));
    };
    if segs.is_empty() {
        return void();
    }
    let mut spi = Spi::new();
    spi.segs = segs;
    spi.rule = rule;
    spi.build();
    let (_, by0, _, by1) = spi.path_box();
    for y in by0..by1 {
        for (x0, x1) in spi.spans_at(y) {
            for (a, b) in clip_span(vm, g.dev_clip, y, x0, x1) {
                fill_span(vm, &s, &g, y, a, b);
            }
        }
    }
    flush_surface(vm, dst);
    void()
}

/// `sun.java2d.loops.DrawPath.DrawPath(SunGraphics2D, SurfaceData, int, int, Path2D.Float)`
///
/// `STROKE_THIN` 下的 `draw(Shape)` / `drawOval` / `drawArc` 都走这里。
/// 折线化后逐段 Bresenham —— 和 C 侧 `DEFINE_DRAW_PATH` 的做法一致（它也是每段
/// 调一次 Bresenham，而不是先扩成填充区）。
fn loop_draw_path(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let (tx, ty) = (f.arg_i(2), f.arg_i(3));
    let path = f.arg_i(4);
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("DrawPath：目标 SurfaceData 不是本 VM 记录的 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    if g.composite_state == COMP_CUSTOM || g.composite_state == COMP_XOR {
        return Err(unsupported("DrawPath：自定义/XOR 合成"));
    }
    let Some((segs, _)) = path_segs(vm, path, tx, ty) else {
        return Err(unsupported("DrawPath：Path2D.Float 的 floatCoords/pointTypes 读不到"));
    };
    for (x0, y0, x1, y1) in flatten_segs(&segs) {
        // 端点半像素取整：与 Java2D 的 thin-stroke 取整一致（坐标中心落在 x+0.5）
        let rx0 = (x0 + 0.5).floor() as i32;
        let ry0 = (y0 + 0.5).floor() as i32;
        let rx1 = (x1 + 0.5).floor() as i32;
        let ry1 = (y1 + 0.5).floor() as i32;
        if rx0 == rx1 && ry0 == ry1 {
            continue;
        }
        bresenham_line(vm, &s, &g, rx0, ry0, rx1, ry1);
    }
    flush_surface(vm, dst);
    void()
}

// `FillSpans` 的真实现在文件末尾「shape 的非 AA 填充」一节（要 ShapeSpanIterator 配合）。

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
    flush_surface(vm, dst);
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
    // 参数约定：`args` **只放实参**，接收者由 `recv` 单独给 —— `Vm::execute` 那边
    // `receiver=true` 的含义是「args[0] 就是 this」（`place_args` 的 `off = 1`），
    // 所以这里要把 `recv` 补到最前面。**漏了这一步的后果是静默的**：args[0]（第一个实参）
    // 被当成 `this`，其余实参整体前移一格、最后一个槽留 0 ——
    // 实测 `new DisplayMode(320,200,32,0)` 回显 `0x0[Multi depth][Unknown refresh rate]`。
    let mut full: Vec<Slot> = Vec::with_capacity(args.len() + 1);
    if receiver {
        full.push(Slot::I(recv));
    }
    full.extend_from_slice(args);
    vm.depth += 1;
    let r = vm.execute(ci, mi, desc, &full, receiver);
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
    let _ = call_java(vm, class, "<init>", desc, &[Slot::L(id), Slot::I(src), Slot::I(comp), Slot::I(dst)], obj);
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
        "sun/java2d/loops/FillPath",
        "sun/java2d/loops/DrawPath",
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

    // 4) 窗口面（`GDIWindowSurfaceData.IntRgbGdi`）：窗口上的 `Graphics2D` 用的是这个
    //    SurfaceType，不注册的话 `Win32GraphicsConfig.getSolidLoops()` 直接抛
    //    "No loops found"（离屏 BufferedImage 走 IntRgb，和它是两个类型）。
    let mut gdi = static_ref(vm, "sun/java2d/windows/GDIWindowSurfaceData", "IntRgbGdi");
    if gdi == 0 {
        let _ = vm.ensure_init("sun/java2d/windows/GDIWindowSurfaceData");
        gdi = static_ref(vm, "sun/java2d/windows/GDIWindowSurfaceData", "IntRgbGdi");
    }
    if gdi > 0 {
        for ct in COMPOSITE_TYPES {
            let c = static_ref(vm, "sun/java2d/loops/CompositeType", ct);
            for class in [
                "sun/java2d/loops/FillRect",
                "sun/java2d/loops/DrawRect",
                "sun/java2d/loops/DrawLine",
                "sun/java2d/loops/Blit",
                "sun/java2d/loops/FillPath",
                "sun/java2d/loops/DrawPath",
            ] {
                id += 1;
                let o = make_prim(vm, class, id, gdi, c, gdi);
                if o > 0 {
                    items.push(Slot::I(o));
                }
            }
        }
    }

    if items.is_empty() {
        return Err(unsupported("registerNativeLoops：一个 primitive 都没造出来（SurfaceType/CompositeType 未初始化？）"));
    }
    let n = items.len();
    let arr = vm.sh.heap.new_array(items, "[Lsun/java2d/loops/GraphicsPrimitive;").unwrap_or(0);
    if arr == 0 {
        return Err(unsupported("registerNativeLoops：GraphicsPrimitive[] 分配失败"));
    }
    // 注册失败**不能再吞掉**：吞掉的后果是 locate() 全部落到 GeneralRenderer，
    // 于是 fill(Shape) 整块不出像素（JDK 25 的 ProcessPath 那条 Java 扫描线在本 VM 上
    // 一像素都不落），而现象只是「什么都没画」，极难定位。
    call_java(
        vm,
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "register",
        "([Lsun/java2d/loops/GraphicsPrimitive;)V",
        &[Slot::I(arr)],
        0,
    )
    .map_err(|e| VmError {
        msg: format!("registerNativeLoops：注册 {n} 个 primitive 失败：{}", e.msg),
        exc: None,
    })?;

    presort_primitives(vm)?;
    void()
}

/// **替 `GraphicsPrimitiveMgr` 把 primitive 表排好序**，并把 `needssort` 清零。
///
/// 为什么需要：`locate()` 第一次调用时会跑 `Arrays.sort(primitives, primSorter)`，
/// 而本 VM 在这条调用上抛
/// `ClassCastException: GraphicsPrimitiveProxy cannot be cast to java.lang.Comparable`
/// —— 也就是 `Arrays.sort(T[], Comparator)` 被当成了 `Arrays.sort(Object[])`
/// （自然序）走。异常从 `locate()` 抛到 `RenderLoops` 构造器，再被 `SunGraphics2D`
/// 的 pipe 校验吞掉 → 全部落到 `GeneralRenderer` → `fill(Shape)`/`draw(Shape)`
/// 一像素不出（`fillRect` 恰好还能работать，所以很难发现）。
///
/// 这里是**我们这边能做的**：等 register() 装完，按 `getUniqueID()` 排好序写回去，
/// 再把 `needssort` 置 false，让 `locate()` 只用 `Arrays.binarySearch`（这条是好的）。
/// 真正的修法在 `java.util.Arrays` / 方法解析层，见回报。
fn presort_primitives(vm: &mut Vm) -> Result<(), VmError> {
    const MGR: &str = "sun/java2d/loops/GraphicsPrimitiveMgr";
    let arr = static_ref(vm, MGR, "primitives");
    if arr == 0 {
        return Ok(());
    }
    let n = vm.sh.heap.len(arr);
    let mut items: Vec<(i32, i32)> = Vec::with_capacity(n);
    for i in 0..n {
        let h = vm.sh.heap.get(arr, i).map(|v| v.as_i32()).unwrap_or(0);
        if h == 0 {
            continue;
        }
        // **不能调 `getUniqueID()`**：此刻还在 `GraphicsPrimitiveMgr.<clinit>` 里，
        // 再进解释器会递归触发类初始化，直接撞「调用深度超限」。读字段等价且安全。
        let id = get_i32_field(vm, h, "sun/java2d/loops/GraphicsPrimitive", "uniqueID");
        items.push((id, h));
    }
    if items.is_empty() {
        return Ok(());
    }
    items.sort_by_key(|(id, _)| *id);
    let slots: Vec<Slot> = items.iter().map(|(_, h)| Slot::I(*h)).collect();
    let sorted = vm.sh.heap.new_array(slots, "[Lsun/java2d/loops/GraphicsPrimitive;").unwrap_or(0);
    if sorted == 0 {
        return Ok(());
    }
    {
        let mut st = lk(&vm.sh.statics);
        st.insert((MGR.to_string(), "primitives".to_string()), Slot::I(sorted));
        st.insert((MGR.to_string(), "needssort".to_string()), Slot::I(0));
    }
    Ok(())
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
        traced!("loop_fill_rect", loop_fill_rect),
    );
    r.add(
        "sun/java2d/loops/DrawRect",
        "DrawRect",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        traced!("loop_draw_rect", loop_draw_rect),
    );
    r.add(
        "sun/java2d/loops/DrawLine",
        "DrawLine",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        traced!("loop_draw_line", loop_draw_line),
    );
    r.add(
        "sun/java2d/loops/FillPath",
        "FillPath",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IILjava/awt/geom/Path2D$Float;)V",
        traced!("loop_fill_path", loop_fill_path),
    );
    r.add(
        "sun/java2d/loops/DrawPath",
        "DrawPath",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IILjava/awt/geom/Path2D$Float;)V",
        traced!("loop_draw_path", loop_draw_path),
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
        traced!("loop_blit", loop_blit),
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

// ============================================================ 九、真窗口（Win32）

/// Win32 裸 FFI。**只声明本文件用到的那几条**，全部 `extern "system"`，不加任何 crate。
mod w32 {
    use std::os::raw::c_void;

    pub type HWND = isize;

    #[repr(C)]
    pub struct WndClassExW {
        pub cb_size: u32,
        pub style: u32,
        pub wnd_proc: Option<unsafe extern "system" fn(HWND, u32, usize, isize) -> isize>,
        pub cls_extra: i32,
        pub wnd_extra: i32,
        pub instance: isize,
        pub icon: isize,
        pub cursor: isize,
        pub background: isize,
        pub menu_name: *const u16,
        pub class_name: *const u16,
        pub icon_small: isize,
    }

    #[repr(C)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    pub struct Msg {
        pub hwnd: HWND,
        pub message: u32,
        pub w_param: usize,
        pub l_param: isize,
        pub time: u32,
        pub pt: Point,
    }

    #[repr(C)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    pub struct PaintStruct {
        pub hdc: isize,
        pub erase: i32,
        pub rc: Rect,
        pub restore: i32,
        pub inc_update: i32,
        pub reserved: [u8; 32],
    }

    #[repr(C)]
    pub struct BitmapInfoHeader {
        pub size: u32,
        pub width: i32,
        pub height: i32,
        pub planes: u16,
        pub bit_count: u16,
        pub compression: u32,
        pub size_image: u32,
        pub xppm: i32,
        pub yppm: i32,
        pub clr_used: u32,
        pub clr_important: u32,
    }

    #[repr(C)]
    pub struct BitmapInfo {
        pub header: BitmapInfoHeader,
        /// 调色板（32bpp 用不到，但结构要够大）
        pub colors: [u32; 256],
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn GetModuleHandleW(name: *const u16) -> isize;
        pub fn RegisterClassExW(wc: *const WndClassExW) -> u16;
        #[allow(clippy::too_many_arguments)]
        pub fn CreateWindowExW(
            exstyle: u32,
            class: *const u16,
            name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: HWND,
            menu: isize,
            inst: isize,
            param: *mut c_void,
        ) -> HWND;
        pub fn DefWindowProcW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
        pub fn DestroyWindow(hwnd: HWND) -> i32;
        pub fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
        pub fn UpdateWindow(hwnd: HWND) -> i32;
        pub fn TranslateMessage(msg: *const Msg) -> i32;
        pub fn DispatchMessageW(msg: *const Msg) -> isize;
        pub fn PeekMessageW(msg: *mut Msg, hwnd: HWND, min: u32, max: u32, remove: u32) -> i32;
        pub fn PostQuitMessage(code: i32);
        pub fn SetWindowTextW(hwnd: HWND, s: *const u16) -> i32;
        pub fn GetWindowRect(hwnd: HWND, rc: *mut Rect) -> i32;
        pub fn GetClientRect(hwnd: HWND, rc: *mut Rect) -> i32;
        pub fn MoveWindow(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, repaint: i32) -> i32;
        pub fn GetSystemMetrics(idx: i32) -> i32;
        pub fn AdjustWindowRectEx(rc: *mut Rect, style: u32, menu: i32, exstyle: u32) -> i32;
        pub fn LoadCursorW(inst: isize, name: *const u16) -> isize;
        pub fn InvalidateRect(hwnd: HWND, rc: *const Rect, erase: i32) -> i32;
        pub fn SetForegroundWindow(hwnd: HWND) -> i32;
        pub fn BeginPaint(hwnd: HWND, ps: *mut PaintStruct) -> isize;
        pub fn EndPaint(hwnd: HWND, ps: *const PaintStruct) -> i32;
        pub fn GetDC(hwnd: HWND) -> isize;
        pub fn ReleaseDC(hwnd: HWND, dc: isize) -> i32;
        pub fn SetFocus(hwnd: HWND) -> HWND;
        pub fn GetForegroundWindow() -> HWND;
    }

    /// `GetTextMetricsW` 返回的 `TEXTMETRICW`（只声明我们读的那几个字段，
    /// 但**必须按完整布局给够大小** —— GDI 会往里写满 60 字节）
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TextMetricW {
        pub height: i32,
        pub ascent: i32,
        pub descent: i32,
        pub internal_leading: i32,
        pub external_leading: i32,
        pub ave_char_width: i32,
        pub max_char_width: i32,
        pub weight: i32,
        pub overhang: i32,
        pub digitized_aspect_x: i32,
        pub digitized_aspect_y: i32,
        pub italic: u8,
        pub underlined: u8,
        pub struck_out: u8,
        pub pitch_and_family: u8,
        pub char_set: u8,
        pub precision: u8,
        pub clip_precision: u8,
        pub quality: u8,
        pub pitch_and_family2: u8,
        pub face_name: [u16; 32],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Size {
        pub cx: i32,
        pub cy: i32,
    }

    #[link(name = "gdi32")]
    extern "system" {
        pub fn CreateCompatibleDC(hdc: isize) -> isize;
        pub fn CreateDIBSection(
            hdc: isize,
            pbmi: *const BitmapInfo,
            usage: u32,
            bits: *mut *mut core::ffi::c_void,
            section: isize,
            offset: u32,
        ) -> isize;
        pub fn SelectObject(hdc: isize, obj: isize) -> isize;
        pub fn DeleteObject(obj: isize) -> i32;
        pub fn DeleteDC(hdc: isize) -> i32;
        pub fn SetBkMode(hdc: isize, mode: i32) -> i32;
        pub fn SetTextColor(hdc: isize, color: u32) -> u32;
        pub fn TextOutW(hdc: isize, x: i32, y: i32, s: *const u16, c: i32) -> i32;
        pub fn GetTextExtentPoint32W(hdc: isize, s: *const u16, c: i32, sz: *mut Size) -> i32;
        pub fn GetTextMetricsW(hdc: isize, tm: *mut TextMetricW) -> i32;
        #[allow(clippy::too_many_arguments)]
        pub fn CreateFontW(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strikeout: u32,
            charset: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_and_family: u32,
            face: *const u16,
        ) -> isize;
    }

    #[link(name = "gdi32")]
    extern "system" {
        #[allow(clippy::too_many_arguments)]
        pub fn StretchDIBits(
            hdc: isize,
            x_dst: i32,
            y_dst: i32,
            w_dst: i32,
            h_dst: i32,
            x_src: i32,
            y_src: i32,
            w_src: i32,
            h_src: i32,
            bits: *const c_void,
            bmi: *const BitmapInfo,
            usage: u32,
            rop: u32,
        ) -> i32;
    }
}

use std::sync::mpsc::{channel, Sender};

const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_QUIT: u32 = 0x0012;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

/// Win32 的 `wParam` 鼠标按键位 → `InputEvent` 修饰符位（SHIFT=1 CTRL=2 ALT=8 之外，
/// 鼠标键位 BUTTON1=0x400 BUTTON2=0x800 BUTTON3=0x1000 与大写锁定 0x200 都对齐）
fn mouse_modifiers(wp: usize) -> i32 {
    let mut m = 0;
    if wp & 0x0004 != 0 {
        m |= 1; // SHIFT_DOWN_MASK
    }
    if wp & 0x0008 != 0 {
        m |= 2; // CTRL_DOWN_MASK
    }
    if wp & 0x0020 != 0 {
        m |= 8; // ALT_DOWN_MASK
    }
    if wp & 0x0001 != 0 {
        m |= 0x400; // BUTTON1_DOWN_MASK
    }
    if wp & 0x0002 != 0 {
        m |= 0x800; // BUTTON2_DOWN_MASK (右键)
    }
    if wp & 0x0010 != 0 {
        m |= 0x1000; // BUTTON3_DOWN_MASK (中键)
    }
    m
}

/// Win32 虚拟键码 → `java.awt.event.KeyEvent.VK_*`。
///
/// 两者**并不等价**：字母/数字/功能键/F 键基本一致，但 `VK_BACK(8)` 对
/// `VK_BACK_SPACE(8)` 一致、`VK_RETURN(13)` 对 `VK_ENTER(10)` 不一致，方向键
/// `VK_LEFT(37)` 对 `VK_LEFT(37)` 又一致。这里只映射常用键，其余退化成
/// `VK_UNDEFINED(0)` —— **宁可没有 keyCode 也不能给错的**（错的会让按键处理逻辑错乱）。
fn vk_to_java(vk: i32) -> i32 {
    match vk {
        0x08 => 8,    // BACK_SPACE
        0x09 => 9,    // TAB
        0x0D => 10,   // ENTER
        0x1B => 27,   // ESCAPE
        0x20 => 32,   // SPACE
        0x21 => 33,   // PAGE_UP
        0x22 => 34,   // PAGE_DOWN
        0x23 => 35,   // END
        0x24 => 36,   // HOME
        0x25 => 37,   // LEFT
        0x26 => 38,   // UP
        0x27 => 39,   // RIGHT
        0x28 => 40,   // DOWN
        0x2E => 127,  // DELETE
        0x10 => 16,   // SHIFT
        0x11 => 17,   // CONTROL
        0x12 => 18,   // ALT
        0x14 => 20,   // CAPS_LOCK
        0x70..=0x7B => 0x70 + (vk - 0x70), // F1..F12
        // 字母/数字/主键盘符号：Java 的 VK 就是大写 ASCII
        0x41..=0x5A => vk,
        0x30..=0x39 => vk,
        _ => 0, // VK_UNDEFINED
    }
}

const SW_SHOW: i32 = 5;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_POPUP: u32 = 0x8000_0000;
const SRCCOPY: u32 = 0x00CC_0020;

/// 一个真窗口 + 它的像素缓冲。
///
/// 像素有两份：**Java `int[]`（真相，光栅化器读写它）** 和 `px`（Rust 侧镜像，
/// 只用于 `StretchDIBits` 上屏）。每次光栅操作结束后 `flush_surface` 把前者同步到后者。
struct WinInfo {
    hwnd: isize,
    w: i32,
    h: i32,
    /// Java `int[]`（TYPE_INT_RGB 语义：0x00RRGGBB）
    jbuf: i32,
    px: Vec<u32>,
    /// 这个窗口当前的 `GDIWindowSurfaceData` 句柄（0 = 还没建面）
    gdi: i32,
    visible: bool,
}

#[derive(Default)]
struct WinState {
    /// peer 句柄 → 窗口
    by_peer: HashMap<i32, WinInfo>,
    /// HWND → peer
    by_hwnd: HashMap<isize, i32>,
    /// SurfaceData 句柄 → peer（光栅操作后按它找窗口上屏）
    by_surf: HashMap<i32, i32>,
}

static WSTATE: OnceLock<Mutex<WinState>> = OnceLock::new();

fn wstate() -> &'static Mutex<WinState> {
    WSTATE.get_or_init(|| Mutex::new(WinState::default()))
}

enum UiJob {
    Create {
        frame: bool,
        title: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        peer: i32,
        tx: Sender<isize>,
    },
    Show(isize, bool),
    Move(isize, i32, i32, i32, i32),
    Title(isize, String),
    Destroy(isize),
    Focus(isize),
}

static UI_TX: OnceLock<Mutex<Sender<UiJob>>> = OnceLock::new();

fn ui_thread_start_once() {
    UI_TX.get_or_init(|| {
        let (tx, rx) = channel::<UiJob>();
        std::thread::Builder::new()
            .name("vmless-win32-ui".into())
            .spawn(move || ui_thread_main(rx))
            .expect("起 Win32 UI 线程失败");
        Mutex::new(tx)
    });
}

fn ui_send(job: UiJob) {
    ui_thread_start_once();
    if let Some(tx) = UI_TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(job);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把 `px` 画到窗口客户区（`StretchDIBits`，32bpp 自顶向下 DIB）
fn blit_px(hwnd: isize, px: &[u32], w: i32, h: i32) {
    if w <= 0 || h <= 0 || px.len() < (w as usize) * (h as usize) {
        return;
    }
    unsafe {
        let dc = w32::GetDC(hwnd);
        if dc == 0 {
            return;
        }
        let bmi = w32::BitmapInfo {
            header: w32::BitmapInfoHeader {
                size: std::mem::size_of::<w32::BitmapInfoHeader>() as u32,
                width: w,
                // 负高度 = 自顶向下（`px[0]` 是左上角）
                height: -h,
                planes: 1,
                bit_count: 32,
                compression: 0, // BI_RGB
                size_image: 0,
                xppm: 0,
                yppm: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0; 256],
        };
        w32::StretchDIBits(
            dc,
            0,
            0,
            w,
            h,
            0,
            0,
            w,
            h,
            px.as_ptr() as *const _,
            &bmi,
            0, // DIB_RGB_COLORS
            SRCCOPY,
        );
        w32::ReleaseDC(hwnd, dc);
    }
}

extern "system" fn wnd_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_PAINT => unsafe {
            let mut ps: w32::PaintStruct = std::mem::zeroed();
            let _ = w32::BeginPaint(hwnd, &mut ps);
            // 只读一次状态：`try_lock` 防止和创建/上屏路径互锁（拿不到就交给系统重画）
            let mut ask_java = None;
            if let Ok(st) = wstate().try_lock() {
                if let Some(peer) = st.by_hwnd.get(&hwnd) {
                    if let Some(w) = st.by_peer.get(peer) {
                        if w.visible {
                            blit_px(hwnd, &w.px, w.w, w.h);
                            ask_java = Some((w.w, w.h));
                        }
                    }
                }
            }
            let _ = w32::EndPaint(hwnd, &ps);
            // 系统只是提醒「这块要重画」。像素得由 Java 侧 `Component.paint()` 产出来，
            // 所以把重画请求转给事件泵：`handlePaint` → `PaintEvent` → `paint()`。
            // 不转这一步窗口就永远是我们自己那张空白 `px`（实测：客户区全白）。
            if let Some((w, h)) = ask_java {
                push_event(InputEvt::Paint { hwnd, w, h });
            }
            0
        },
        // 背景我们自己画（`WM_PAINT` 里整块重绘），别让系统刷白
        WM_ERASEBKGND => 1,
        // ---- 鼠标：只入队，由 `eventLoop` 那条 Java 线程投给组件（见「十三、输入事件」）
        WM_MOUSEMOVE => {
            push_event(InputEvt::Mouse {
                hwnd,
                id: 503, // MOUSE_MOVED
                x: (lp as i32) & 0xFFFF,
                y: ((lp as i32) >> 16) & 0xFFFF,
                button: 0,
                clicks: 0,
                modifiers: mouse_modifiers(wp),
            });
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let button = match msg {
                WM_LBUTTONDOWN => 1, // BUTTON1
                WM_MBUTTONDOWN => 2, // BUTTON2
                _ => 3,              // BUTTON3
            };
            let (x, y) = ((lp as i32) & 0xFFFF, ((lp as i32) >> 16) & 0xFFFF);
            let m = mouse_modifiers(wp);
            push_event(InputEvt::Mouse { hwnd, id: 501, x, y, button, clicks: 1, modifiers: m });
            push_event(InputEvt::Mouse { hwnd, id: 500, x, y, button, clicks: 1, modifiers: m });
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let button = match msg {
                WM_LBUTTONUP => 1,
                WM_MBUTTONUP => 2,
                _ => 3,
            };
            push_event(InputEvt::Mouse {
                hwnd,
                id: 502, // MOUSE_RELEASED
                x: (lp as i32) & 0xFFFF,
                y: ((lp as i32) >> 16) & 0xFFFF,
                button,
                clicks: 1,
                modifiers: mouse_modifiers(wp),
            });
            0
        }
        // ---- 键盘
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wp as i32;
            let m = mouse_modifiers(0);
            push_event(InputEvt::Key { hwnd, id: 401, code: vk_to_java(vk), ch: 0, modifiers: m });
            0
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let vk = wp as i32;
            push_event(InputEvt::Key { hwnd, id: 402, code: vk_to_java(vk), ch: 0, modifiers: 0 });
            0
        }
        WM_CHAR => {
            let ch = wp as u16;
            if ch >= 32 || ch == 8 || ch == 9 || ch == 10 || ch == 13 {
                push_event(InputEvt::Key { hwnd, id: 400, code: 0, ch, modifiers: 0 });
            }
            0
        }
        WM_CLOSE => unsafe {
            let _ = w32::DestroyWindow(hwnd);
            0
        },
        WM_DESTROY => {
            if let Ok(mut st) = wstate().try_lock() {
                if let Some(peer) = st.by_hwnd.remove(&hwnd) {
                    if let Some(w) = st.by_peer.get_mut(&peer) {
                        w.hwnd = 0;
                        w.visible = false;
                    }
                }
            }
            0
        }
        _ => unsafe { w32::DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn ui_thread_main(rx: std::sync::mpsc::Receiver<UiJob>) {
    unsafe {
        let inst = w32::GetModuleHandleW(std::ptr::null());
        let cls = wide("VmlessAwtWindow");
        let cur = w32::LoadCursorW(0, 32512isize as *const u16); // IDC_ARROW
        let wc = w32::WndClassExW {
            cb_size: std::mem::size_of::<w32::WndClassExW>() as u32,
            style: 0x0002 /* CS_HREDRAW */ | 0x0001 /* CS_VREDRAW */,
            wnd_proc: Some(wnd_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance: inst,
            icon: 0,
            cursor: cur,
            background: 5isize + 1, // COLOR_WINDOW + 1
            menu_name: std::ptr::null(),
            class_name: cls.as_ptr(),
            icon_small: 0,
        };
        w32::RegisterClassExW(&wc);
    }
    let mut running = true;
    while running {
        // 1) 先把排队的活干完
        loop {
            match rx.try_recv() {
                Ok(job) => match job {
                    UiJob::Create { frame, title, x, y, w, h, peer, tx } => {
                        let hwnd = create_hwnd(frame, &title, x, y, w, h, peer);
                        let _ = tx.send(hwnd);
                    }
                    UiJob::Show(hwnd, show) => unsafe {
                        if hwnd != 0 {
                            w32::ShowWindow(hwnd, if show { SW_SHOW } else { 0 });
                            if show {
                                let _ = w32::SetForegroundWindow(hwnd);
                                let _ = w32::UpdateWindow(hwnd);
                            }
                        }
                    },
                    UiJob::Move(hwnd, x, y, w, h) => unsafe {
                        if hwnd != 0 {
                            w32::MoveWindow(hwnd, x, y, w, h, 1);
                        }
                    },
                    UiJob::Title(hwnd, t) => unsafe {
                        if hwnd != 0 {
                            let s = wide(&t);
                            w32::SetWindowTextW(hwnd, s.as_ptr());
                        }
                    },
                    UiJob::Destroy(hwnd) => unsafe {
                        if hwnd != 0 {
                            w32::DestroyWindow(hwnd);
                        }
                    },
                    UiJob::Focus(hwnd) => unsafe {
                        if hwnd != 0 {
                            let _ = w32::SetFocus(hwnd);
                        }
                    },
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }
        // 2) 抽一次消息（没有就睡 4ms：窗口交互够用，也不烧 CPU）
        unsafe {
            let mut msg: w32::Msg = std::mem::zeroed();
            while w32::PeekMessageW(&mut msg, 0, 0, 0, 1 /* PM_REMOVE */) != 0 {
                if msg.message == WM_QUIT {
                    running = false;
                    break;
                }
                let _ = w32::TranslateMessage(&msg);
                w32::DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
}

fn create_hwnd(frame: bool, title: &str, x: i32, y: i32, w: i32, h: i32, peer: i32) -> isize {
    let cls = wide("VmlessAwtWindow");
    let name = wide(title);
    let (style, exstyle) = if frame {
        (WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0x0000_0100 /* WS_EX_WINDOWEDGE */)
    } else {
        (WS_POPUP | WS_VISIBLE, 0)
    };
    // Java 侧的 w/h 是**客户区**尺寸；CreateWindowEx 收的是整窗尺寸
    let mut rc = w32::Rect { left: 0, top: 0, right: w.max(1), bottom: h.max(1) };
    unsafe {
        w32::AdjustWindowRectEx(&mut rc, style, 0, exstyle);
        let hwnd = w32::CreateWindowExW(
            exstyle,
            cls.as_ptr(),
            name.as_ptr(),
            style,
            x,
            y,
            rc.right - rc.left,
            rc.bottom - rc.top,
            0,
            0,
            0,
            peer as usize as *mut _,
        );
        if hwnd != 0 {
            let _ = w32::ShowWindow(hwnd, SW_SHOW);
        }
        hwnd
    }
}

// ---------------------------------------------------------------- 字段/对象道具

fn set_i64_field(vm: &Vm, obj: i32, class: &str, name: &str, v: i64) {
    if let Some(i) = vm.store.field_index(class, name) {
        vm.sh.heap.set(obj, i, Slot::L(v));
    }
}

/// 写引用字段。**必须 `Slot::I`**：`scan_refs`/`scan_refs_young` 只认 `Slot::I`，
/// 存成 `Slot::L` 会被当 primitive long 跳过 —— 引用不是 GC 根，随后变悬空句柄。
fn set_ref_field(vm: &Vm, obj: i32, class: &str, name: &str, v: i32) {
    if let Some(i) = vm.store.field_index(class, name) {
        vm.sh.heap.set_ref(obj, i, Slot::I(v));
    }
}

fn component_size(vm: &Vm, target: i32) -> (i32, i32) {
    if target <= 0 {
        return (1, 1);
    }
    let w = get_i32_field(vm, target, "java/awt/Component", "width").max(1);
    let h = get_i32_field(vm, target, "java/awt/Component", "height").max(1);
    (w, h)
}

fn new_int_array(vm: &mut Vm, n: usize, fill: i32) -> i32 {
    let v = vec![Slot::I(fill); n];
    vm.sh.heap.new_array(v, "[I").unwrap_or(0)
}

/// 为 peer 建（或重建）一块 `w*h` 的像素缓冲
fn win_resize_buf(vm: &mut Vm, peer: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let jbuf = new_int_array(vm, (w as usize) * (h as usize), 0x00FF_FFFF);
    let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(info) = st.by_peer.get_mut(&peer) {
        info.w = w;
        info.h = h;
        info.jbuf = jbuf;
        info.px = vec![0x00FF_FFFFu32; (w as usize) * (h as usize)];
    }
}

/// 光栅操作之后调：Java `int[]` → Rust 镜像 → 屏幕
fn flush_surface(vm: &mut Vm, surf: i32) {
    let peer = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        match st.by_surf.get(&surf) {
            Some(p) => *p,
            None => return,
        }
    };
    let Some(s) = surface_of(surf) else { return };
    let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
    let Some(info) = st.by_peer.get_mut(&peer) else { return };
    if !info.visible || info.hwnd == 0 {
        return;
    }
    let w = info.w;
    let h = info.h;
    if s.width != w || s.height != h {
        return;
    }
    for y in 0..h {
        let row = s.base + y * s.scan_step;
        let dst = (y as usize) * (w as usize);
        for x in 0..w {
            let v = surface_get(vm, &s, row + x * s.pix_step);
            info.px[dst + x as usize] = (v as u32) & 0x00FF_FFFF;
        }
    }
    let (hwnd, px, pw, ph) = (info.hwnd, info.px.clone(), info.w, info.h);
    drop(st);
    blit_px(hwnd, &px, pw, ph);
}

// ============================================================ 十、AWT 窗口树

/// 调试用：`VMLESS_AWT_TRACE=1` 时把 AWT native 的调用顺序打到 stderr。
/// 窗口起不来时这是唯一能看清走到哪一步的东西（Java 侧 println 在挂死时看不见）。
fn atrace(msg: &str) {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VMLESS_AWT_TRACE").is_some()) {
        return;
    }
    eprintln!("[awt] {msg}");
}

fn new_obj(vm: &mut Vm, class: &str, desc: &str, args: &[Slot]) -> i32 {
    atrace("new_obj");
    // **只分配，不在这里跑构造器**：`Vm::new_instance(class, desc)` 只给得出 `this`，
    // 带参数的构造器会被零填充跑一遍（`DirectColorModel(0,0,0,0)` 直接抛
    // IllegalArgumentException → 返回 0 → Java 侧拿到 null
    // → 建窗时 `InvalidPipeException: Unsupported bit depth: 0`）。
    // 构造器这条路统一走 `call_java`：它会把 `this` 补到 args 最前面。
    let handle = alloc_instance(vm, class);
    if handle == 0 {
        atrace(&format!("new_obj {class}: 分配失败（类没加载到）"));
        return 0;
    }
    if let Err(e) = call_java(vm, class, "<init>", desc, args, handle) {
        // 吞掉构造器异常会让对象「字段全 0」地流出去（表现为 pixelSize=0 之类），
        // 排查时先看这一行。
        atrace(&format!("new_obj {class}.<init>{desc} 失败: {}", e.msg));
    }
    handle
}

fn make_cm(vm: &mut Vm) -> i32 {
    atrace("make_cm");
    // 5 参 `DirectColorModel(bits, rmask, gmask, bmask, amask)`（JDK 25 没有 3 参版本，
    // `javap java.awt.image.DirectColorModel` 只有 (IIII)、(IIIII) 和 8 参那份）。
    // bits 必须是 **32**：之前错写成 `(IIII)V` 并把 0x00FF0000 当 bits，
    // pixelSize 变成 16711680 → `GDIWindowSurfaceData.getSurfaceType` 直接
    // `InvalidPipeException: Unsupported bit depth: 16711680`。
    new_obj(
        vm,
        "java/awt/image/DirectColorModel",
        "(IIIII)V",
        &[Slot::I(32), Slot::I(0x00FF_0000), Slot::I(0xFF00), Slot::I(0xFF), Slot::I(0)],
    )
}

/// `WToolkit.makeColorModel()Ljava/awt/image/ColorModel;`
fn toolkit_make_color_model(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(make_cm(vm)))
}

// ---------------------------------------------------------------- WToolkit

fn toolkit_get_windows_version(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("getWindowsVersion");
    Ok(Slot::I(vm.sh.heap.new_string("Windows 10 (vmless)")))
}

/// `WToolkit.init()` —— 返回 false = **不要**起 AWT 自己的消息泵线程。
///
/// 我们的消息泵在 Rust 侧的 UI 线程里（见 [`ui_thread_main`]）：Win32 的消息只能由
/// **建窗口的那个线程**取出来，而 Java 侧的 `AWT-Windows` 线程并不建窗口。
fn toolkit_init(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("WToolkit.init enter");
    // 返回 true = 「原生侧已经把消息泵起好了」：Java 侧于是
    // ① 装关机钩子（它调 `shutdown()` 让 eventLoop 退出）② 调 `eventLoop()`。
    //
    // 上一轮这里返回 false（「不起泵」），理由是「泵在 Rust UI 线程里，不需要
    // Java 线程泵」。但那样 `AWT-Windows` 线程跑完 `run()` 就结束了 —— 而**事件
    // 必须由一条 Java 线程投给组件**（Rust 侧没有解释器上下文）。所以现在
    // `eventLoop()` 就是那个投递点：它不碰 Win32，只把 Rust 队列里的事件
    // `dispatchEvent` 出去。
    //
    // **必须配平 `AWTAutoShutdown` 的记账**：`WToolkit` 构造器一进门就
    // `notifyToolkitThreadBusy()`（= `toolkitThreadBusy = true` + 拉起一条**非 daemon**
    // 的 `AWT-Shutdown` 线程）。真 JVM 里这个标志由 **awt.dll 的消息泵**在空闲时清
    // （`AwtToolkit::SetBusy(false)` → `notifyToolkitThreadFree()`）。我们这条 Java 线程
    // 跑完 `run()` 就没了，没人清 → `isReadyToShutdown()` 永远 false → `AWT-Shutdown`
    // 永不退出 → `main` 在 `wait_non_daemon()` 里死等（现象：进程不退出、CPU 0、
    // main 帧已弹空）。所以在返回「不起泵」的同时，把 busy 记成 false ——
    // 这正是「原生侧此刻空闲」的真实语义。
    let _ = call_java(vm, "sun/awt/AWTAutoShutdown", "notifyToolkitThreadFree", "()V", &[], 0);
    atrace("WToolkit.init -> false");
    Ok(Slot::I(0))
}

/// `WToolkit.startToolkitThread(Runnable, ThreadGroup)` —— 返回 false：
/// **让 Java 自己建 `AWT-Windows` 线程**，由它跑 `WToolkit.run()`。
///
/// 这里有两条曾经走过的弯路，都删掉了（A/B 用真 JDK 验证过）：
///
/// 1. 「先把 `WToolkit.inited` 硬置 1」—— 构造器就不等了。但那是拿掉了一个真实的
///    跨线程握手：`run()` 里 `init()` 之后要 `inited = true; notifyAll()` 唤醒构造函数。
///    绕过它等于让「另一个线程 notifyAll → 本线程 wait 醒来」这条路永远不被测到。
/// 2. 「返回 true 跳过 AWT-Windows 线程」—— 那条线程根本不建，于是 `init()` 只有
///    Rust 侧调用过，`inited` 永远为 false / 或者得靠上面那条 hack 顶着；副作用是
///    AWT 初始化之后进程退出也会挂（`startPump=false` 时没人 `shutdown()`）。
///
/// 现在的语义与 HotSpot 一致：Java 建线程 → `run()` → `init()` 返回 false
/// （我们不在这条线程上跑 Win32 消息泵，泵在 Rust 的 UI 线程里）→ `inited = true;
/// notifyAll()` → 线程正常结束。
fn toolkit_start_thread(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("startToolkitThread -> false");
    Ok(Slot::I(0))
}

fn toolkit_screen_insets(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("getScreenInsets");
    Ok(Slot::I(new_obj(vm, "java/awt/Insets", "(IIII)V", &[Slot::I(0), Slot::I(0), Slot::I(0), Slot::I(0)])))
}

/// `WToolkit.loadSystemColors(int[])`：填一组 Windows 默认系统色。
///
/// 索引就是 `java.awt.SystemColor` 的常量（DESKTOP=0 … CONTROL_HIGHLIGHT_TEXT=20）。
fn toolkit_load_system_colors(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    atrace("loadSystemColors");
    let arr = f.arg_i(0);
    if arr <= 0 {
        return void();
    }
    let n = vm.sh.heap.len(arr);
    let table: [i32; 21] = [
        0x008080, // DESKTOP
        0x0A246A, // ACTIVE_CAPTION
        0xFFFFFF, // ACTIVE_CAPTION_TEXT
        0x0A246A, // ACTIVE_CAPTION_BORDER
        0xC0C0C0, // INACTIVE_CAPTION
        0x000000, // INACTIVE_CAPTION_TEXT
        0xC0C0C0, // INACTIVE_CAPTION_BORDER
        0xFFFFFF, // WINDOW
        0x000000, // WINDOW_BORDER
        0x000000, // WINDOW_TEXT
        0xF0F0F0, // MENU
        0x000000, // MENU_TEXT
        0xFFFFFF, // TEXT
        0x000000, // TEXT_TEXT
        0x0A246A, // TEXT_HIGHLIGHT
        0xFFFFFF, // TEXT_HIGHLIGHT_TEXT
        0x808080, // TEXT_INACTIVE_TEXT
        0xF0F0F0, // CONTROL
        0x000000, // CONTROL_TEXT
        0x0A246A, // CONTROL_HIGHLIGHT
        0xFFFFFF, // CONTROL_HIGHLIGHT_TEXT
    ];
    for i in 0..n.min(table.len()) {
        vm.sh.heap.set(arr, i, Slot::I(table[i]));
    }
    // 剩下的（阴影/边框）统一给灰
    for i in table.len()..n {
        vm.sh.heap.set(arr, i, Slot::I(0x808080));
    }
    void()
}

// ---------------------------------------------------------------- 图形环境 / 设备

fn dev_init_scale(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("getNativeScaleX/Y");
    // 1.0f：不做 DPI 缩放（缩放会把所有坐标乘一遍，且我们只有一套缓冲）
    Ok(Slot::Fl(1.0))
}

fn dev_make_color_model(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("makeColorModel(device)");
    Ok(Slot::I(make_cm(vm)))
}

fn dev_display_mode(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // 真实屏幕尺寸（`SM_CXSCREEN`/`SM_CYSCREEN`）。以前这里写死 320×200：
    // AWT 会拿它做窗口位置钳制/居中，写死会让窗口跑到屏幕外。
    let (w, h) = screen_size();
    Ok(Slot::I(new_obj(
        vm,
        "java/awt/DisplayMode",
        "(IIII)V",
        &[Slot::I(w), Slot::I(h), Slot::I(32), Slot::I(0)],
    )))
}

/// 主屏像素尺寸（`GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`）
fn screen_size() -> (i32, i32) {
    unsafe { (w32::GetSystemMetrics(0), w32::GetSystemMetrics(1)) }
}

fn gconfig_get_bounds(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("Win32GraphicsConfig.getBounds");
    // 真实屏幕边界（以前写死 320×200：AWT 的窗口夹取/居中都用它，
    // 写死会让 setSize(320,200) 的窗口看起来"刚好贴满屏幕"、坐标全错）
    let (w, h) = screen_size();
    Ok(Slot::I(new_obj(
        vm,
        "java/awt/Rectangle",
        "(IIII)V",
        &[Slot::I(0), Slot::I(0), Slot::I(w), Slot::I(h)],
    )))
}

// ---------------------------------------------------------------- peer 建窗/显示

fn frame_title(vm: &Vm, target: i32) -> String {
    let h = get_ref_field(vm, target, "java/awt/Frame", "title");
    if h <= 0 {
        "vmless".to_string()
    } else {
        vm.text_of(h)
    }
}

fn peer_create(vm: &mut Vm, f: &mut Frame, frame: bool) -> Result<Slot, VmError> {
    let peer = f.this();
    atrace(if frame { "createAwtFrame" } else { "createAwtWindow" });
    let target = get_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "target");
    let (w, h) = component_size(vm, target);
    let title = if frame { frame_title(vm, target) } else { "vmless".to_string() };
    let (tx, rx) = channel::<isize>();
    ui_send(UiJob::Create { frame, title, x: 80, y: 80, w, h, peer, tx });
    let hwnd = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(v) => v,
        Err(_) => 0,
    };
    if hwnd == 0 {
        return Err(unsupported("createAwtWindow：Win32 建窗口失败"));
    }
    set_i64_field(vm, peer, "sun/awt/windows/WComponentPeer", "hwnd", hwnd as i64);
    set_i64_field(vm, peer, "sun/awt/windows/WObjectPeer", "pData", 1);
    // `WComponentPeer.<init>` **不填** `winGraphicsConfig` —— 真实 HotSpot 是
    // `Component.addNotify()` → `Component.updateGraphicsData(gc)` → `peer.updateGraphicsData(gc)`
    // 那条路填的。我们绕开 addNotify 直接 `new WFramePeer` + `createAwtFrame`，于是
    // `WWindowPeer.show()` → `updateGC()` 第 33 行 `winGraphicsConfig.getDevice()` 撞 null
    // （NPE 消息里的 "receiver is null" 指的是这一步，不是 `graphicsConfig`）。
    // 没有 `setGraphicsConfiguration` 方法可调：那是 `AWTAccessor$ComponentAccessor` 上的，
    // 改的是 Component 一侧的字段，peer 这一侧还得靠这里补。
    let gc = call_java(
        vm,
        "sun/awt/windows/WComponentPeer",
        "getGraphicsConfiguration",
        "()Ljava/awt/GraphicsConfiguration;",
        &[],
        peer,
    )?;
    if gc.as_i32() > 0 {
        set_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "winGraphicsConfig", gc.as_i32());
        atrace("peer.winGraphicsConfig <- target.getGraphicsConfiguration()");
    }
    let jbuf = new_int_array(vm, (w as usize) * (h as usize), 0x00FF_FFFF);
    {
        let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        st.by_hwnd.insert(hwnd, peer);
        st.by_peer.insert(
            peer,
            WinInfo { hwnd, w, h, jbuf, px: vec![0x00FF_FFFFu32; (w as usize) * (h as usize)], gdi: 0, visible: false },
        );
    }
    void()
}

fn peer_hwnd(peer: i32) -> isize {
    let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
    st.by_peer.get(&peer).map(|w| w.hwnd).unwrap_or(0)
}

/// `WComponentPeer.pShow()V`
fn peer_show(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // 事件泵只起一次：`init()` 返回 false 时 Java 侧的 `eventLoop()` 不会跑，
    // 所以**在这里兜底**（第一扇窗显示出来的那一刻起，鼠标/键盘就该有反应）。
    event_pump_start_once(vm);
    let peer = f.this();
    atrace("pShow");
    // 第一次显示前把 GDI 面建出来（`replaceSurfaceData` 里有 `pData == 0` 的早退保护，
    // 所以 `createAwtWindow` 里必须先写 pData = 1，见 `peer_create`）
    let _ = call_java(vm, "sun/awt/windows/WComponentPeer", "replaceSurfaceData", "()V", &[], peer);
    let hwnd = peer_hwnd(peer);
    {
        let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = st.by_peer.get_mut(&peer) {
            w.visible = true;
        }
    }
    if hwnd != 0 {
        ui_send(UiJob::Show(hwnd, true));
    }
    let gdi = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        st.by_peer.get(&peer).map(|w| w.gdi).unwrap_or(0)
    };
    if gdi != 0 {
        flush_surface(vm, gdi);
    }
    // 第一次上屏后主动要一帧 Java 的 `paint()`：`ShowWindow` 带来的 `WM_PAINT` 不保证
    // 落在这之后（实测它可能在 `pShow` 之前就被系统合并掉了），缺这一下客户区会一直是我们
    // 自己那张空白 `px`。
    if hwnd != 0 {
        let (w, h) = {
            let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
            st.by_peer.get(&peer).map(|i| (i.w, i.h)).unwrap_or((0, 0))
        };
        push_event(InputEvt::Paint { hwnd, w, h });
    }
    void()
}

fn peer_hide(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    let hwnd = peer_hwnd(peer);
    {
        let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = st.by_peer.get_mut(&peer) {
            w.visible = false;
        }
    }
    if hwnd != 0 {
        ui_send(UiJob::Show(hwnd, false));
    }
    void()
}

/// `WComponentPeer.reshape(int,int,int,int)V` / `reshapeNoCheck`
fn peer_reshape(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    atrace("reshape");
    let (x, y, w, h) = (f.arg_i(0), f.arg_i(1), f.arg_i(2), f.arg_i(3));
    let hwnd = peer_hwnd(peer);
    if w > 0 && h > 0 {
        let old = {
            let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
            st.by_peer.get(&peer).map(|i| (i.w, i.h))
        };
        if old != Some((w, h)) {
            win_resize_buf(vm, peer, w, h);
            // 面要跟着换缓冲：重跑一次 initOps 等价动作
            let gdi = {
                let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
                st.by_peer.get(&peer).map(|i| i.gdi).unwrap_or(0)
            };
            if gdi != 0 {
                bind_surface(vm, gdi, peer);
            }
        }
        if hwnd != 0 {
            ui_send(UiJob::Move(hwnd, x, y, w, h));
        }
    }
    void()
}

fn peer_title(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    let s = f.arg_i(0);
    let t = if s > 0 { vm.text_of(s) } else { String::new() };
    let hwnd = peer_hwnd(peer);
    if hwnd != 0 {
        ui_send(UiJob::Title(hwnd, t));
    }
    void()
}

fn peer_dispose(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    atrace("_dispose");
    let (hwnd, gdi) = {
        let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        let hwnd = match st.by_peer.remove(&peer) {
            Some(w) => {
                st.by_hwnd.remove(&w.hwnd);
                if w.gdi != 0 {
                    st.by_surf.remove(&w.gdi);
                }
                w.hwnd
            }
            None => 0,
        };
        (hwnd, 0)
    };
    let _ = gdi;
    if hwnd != 0 {
        ui_send(UiJob::Destroy(hwnd));
    }
    set_i64_field(vm, peer, "sun/awt/windows/WComponentPeer", "hwnd", 0);
    set_i64_field(vm, peer, "sun/awt/windows/WObjectPeer", "pData", 0);
    void()
}

fn peer_location_on_screen(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    let target = get_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "target");
    let x = get_i32_field(vm, target, "java/awt/Component", "x");
    let y = get_i32_field(vm, target, "java/awt/Component", "y");
    Ok(Slot::I(new_obj(vm, "java/awt/Point", "(II)V", &[Slot::I(x), Slot::I(y)])))
}

fn peer_native_window_size(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    let target = get_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "target");
    let (w, h) = component_size(vm, target);
    Ok(Slot::I(new_obj(vm, "java/awt/Dimension", "(II)V", &[Slot::I(w), Slot::I(h)])))
}

fn peer_update_window(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let peer = f.this();
    let gdi = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        st.by_peer.get(&peer).map(|w| w.gdi).unwrap_or(0)
    };
    if gdi != 0 {
        flush_surface(vm, gdi);
    }
    void()
}

// ---------------------------------------------------------------- GDI 面

/// 把一块 `GDIWindowSurfaceData` 绑到 peer 的像素缓冲上（建面和换缓冲都走这里）
fn bind_surface(vm: &mut Vm, sd: i32, peer: i32) {
    let (w, h, jbuf) = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        match st.by_peer.get(&peer) {
            Some(i) => (i.w, i.h, i.jbuf),
            None => return,
        }
    };
    if jbuf == 0 || w <= 0 || h <= 0 {
        return;
    }
    let s = Surface {
        array: jbuf,
        base: 0,
        width: w,
        height: h,
        pix_step: 1,
        scan_step: w,
        image_type: TYPE_INT_RGB,
        elem: 0,
    };
    surfaces().lock().unwrap_or_else(|e| e.into_inner()).insert(sd, s);
    let mut st = wstate().lock().unwrap_or_else(|e| e.into_inner());
    st.by_surf.insert(sd, peer);
    if let Some(i) = st.by_peer.get_mut(&peer) {
        i.gdi = sd;
    }
    let _ = vm;
}

/// `GDIWindowSurfaceData.initOps(WComponentPeer peer, int depth, int rMask, int gMask, int bMask, int screen)V`
///
/// 真实现里这里建的是 GDI 的 DC/位图；我们换成「Java `int[]` 缓冲」——
/// 这样 `FillRect`/`Blit`/`FillSpans` 那套软件光栅化器**原样复用**，窗口和离屏图走同一条路。
fn gdi_init_ops(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    atrace("GDIWindowSurfaceData.initOps");
    let sd = f.this();
    let peer = f.arg_i(0);
    let depth = f.arg_i(1);
    if depth != 32 && depth != 24 {
        return Err(unsupported("GDIWindowSurfaceData.initOps：只支持 32/24bpp"));
    }
    bind_surface(vm, sd, peer);
    void()
}

fn trace_set_dynamic_layout(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("setDynamicLayoutNative");
    void()
}

fn trace_extra_buttons(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("setExtraMouseButtonsEnabledNative");
    void()
}

fn trace_num_screens(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("getNumScreens");
    zi(1)
}

fn no_op_taking(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("no_op_taking");
    void()
}

// ---------------------------------------------------------------- 注册表

pub(super) fn register_window(r: &mut NativeRegistry) {
    // ---- 全量 initIDs（java.desktop 的 75 个原生 initIDs：真 HotSpot 只缓存字段 ID，
    // 我们按名字读写字段，所以全部空实现 —— 但**必须注册**，不然类初始化就 UnsatisfiedLinkError）
    r.add("java/awt/AWTEvent", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Checkbox", "initIDs", "()V", no_op_taking);
    r.add("java/awt/CheckboxMenuItem", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Choice", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Component", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Container", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Cursor", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Dialog", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Dimension", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Event", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Font", "initIDs", "()V", no_op_taking);
    r.add("java/awt/FontMetrics", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Frame", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Insets", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Label", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Menu", "initIDs", "()V", no_op_taking);
    r.add("java/awt/MenuBar", "initIDs", "()V", no_op_taking);
    r.add("java/awt/MenuItem", "initIDs", "()V", no_op_taking);
    r.add("java/awt/ScrollPane", "initIDs", "()V", no_op_taking);
    r.add("java/awt/ScrollPaneAdjustable", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Scrollbar", "initIDs", "()V", no_op_taking);
    r.add("java/awt/TextArea", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Toolkit", "initIDs", "()V", no_op_taking);
    r.add("java/awt/TrayIcon", "initIDs", "()V", no_op_taking);
    r.add("java/awt/Window", "initIDs", "()V", no_op_taking);
    r.add("java/awt/event/InputEvent", "initIDs", "()V", no_op_taking);
    r.add("java/awt/event/KeyEvent", "initIDs", "()V", no_op_taking);
    r.add("java/awt/event/MouseEvent", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/BufferedImage", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/ColorModel", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/IndexColorModel", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/Kernel", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/Raster", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/SampleModel", "initIDs", "()V", no_op_taking);
    r.add("java/awt/image/SinglePixelPackedSampleModel", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/FontDescriptor", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/PlatformFont", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/Win32GraphicsConfig", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/Win32GraphicsDevice", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/BufImgSurfaceData", "initIDs", "(Ljava/lang/Class;Ljava/lang/Class;)V", no_op_taking);
    r.add("sun/awt/image/ByteComponentRaster", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/BytePackedRaster", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/GifImageDecoder", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/ImageRepresentation", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/IntegerComponentRaster", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/image/JPEGImageDecoder", "initIDs", "(Ljava/lang/Class;)V", no_op_taking);
    r.add("sun/awt/image/ShortComponentRaster", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/shell/Win32ShellFolder2", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WButtonPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WDefaultFontCharset", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WDesktopProperties", "initIDs", "()V", no_op_taking);
    // `WDesktopProperties.init()`：真机上它读注册表/系统主题色。我们不读，
    // 但**必须注册**——`Win32GraphicsEnvironment.<clinit>` 之后必经此处，
    // 缺了就是 UnsatisfiedLinkError，连 `new Frame()` 都过不去。
    r.add("sun/awt/windows/WDesktopProperties", "init", "()V", no_op_taking);
    // 同一条链上的另两个：`getWindowsParameters()` 填主题/字体参数，`playWindowsSound`
    // 播放系统音 —— 我们都不做，但**必须注册**（少一个就是建窗期 UnsatisfiedLinkError）。
    r.add("sun/awt/windows/WDesktopProperties", "getWindowsParameters", "()V", no_op_taking);
    r.add(
        "sun/awt/windows/WDesktopProperties",
        "playWindowsSound",
        "(Ljava/lang/String;)V",
        no_op_taking,
    );
    // `D3DGraphicsDevice.<clinit>`：真 JDK 默认 `WindowsFlags.d3dEnabled=true`，
    // 所以 `makeScreenDevice` **一定会**走这里。返回 false = 「本机没有 D3D」，
    // 于是 `createDevice` 返回 null 回落 `Win32GraphicsDevice`（这就是真 JDK 在
    // 无 D3D 机器上的行为）。**不要**去实现 D3D。
    r.add("sun/java2d/d3d/D3DGraphicsDevice", "initD3D", "()Z", |_, _| zi(0));
    // `WCanvasPeer.setNativeBackgroundErase(ZZ)V`：告诉原生侧「别擦背景」。
    // 我们的绘制是自己 blit 到窗口像素缓冲，没有 WM_ERASEBKGND 这条路径，no-op 即可；
    // 不注册的话 `WCanvasPeer.initialize()` 里直接 UnsatisfiedLinkError，建 peer 就断。
    r.add("sun/awt/windows/WCanvasPeer", "setNativeBackgroundErase", "(ZZ)V", no_op_taking);
    // `SunToolkit.closeSplashScreen()`：第一个窗口 show 的时候调，用来关掉启动画面。
    // 我们没有启动画面 → no-op（**static**，所以用 no_op 不是 no_op_taking）。
    r.add("sun/awt/SunToolkit", "closeSplashScreen", "()V", no_op);
    r.add("sun/awt/windows/WEmbeddedFrame", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WFileDialogPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WFontMetrics", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WFontPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WFramePeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WMenuItemPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WObjectPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WPageDialog", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WPanelPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WPrintDialog", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WPrintDialogPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WPrinterJob", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WScrollPanePeer", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WToolkit", "initIDs", "()V", no_op_taking);
    r.add("sun/awt/windows/WWindowPeer", "initIDs", "()V", no_op_taking);
    r.add("sun/font/FreetypeFontScaler", "initIDs", "(Ljava/lang/Class;)V", no_op_taking);
    r.add("sun/font/SunFontManager", "initIDs", "()V", no_op_taking);
    r.add("sun/java2d/Disposer", "initIDs", "()V", no_op_taking);
    r.add("sun/java2d/SurfaceData", "initIDs", "()V", no_op_taking);
    r.add("sun/java2d/loops/GraphicsPrimitiveMgr", "initIDs", "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V", no_op_taking);
    r.add("sun/java2d/pipe/Region", "initIDs", "()V", no_op_taking);
    r.add("sun/java2d/pipe/ShapeSpanIterator", "initIDs", "()V", no_op_taking);
    r.add("sun/java2d/pipe/SpanClipRenderer", "initIDs", "(Ljava/lang/Class;Ljava/lang/Class;)V", no_op_taking);
    r.add("sun/java2d/windows/GDIWindowSurfaceData", "initIDs", "(Ljava/lang/Class;)V", no_op_taking);

    // ---- WToolkit
    r.add("sun/awt/windows/WToolkit", "getWindowsVersion", "()Ljava/lang/String;", traced!("getWindowsVersion", toolkit_get_windows_version));
    r.add("sun/awt/windows/WToolkit", "init", "()Z", traced!("init", toolkit_init));
    r.add(
        "sun/awt/windows/WToolkit",
        "startToolkitThread",
        "(Ljava/lang/Runnable;Ljava/lang/ThreadGroup;)Z",
        toolkit_start_thread,
    );
    r.add("sun/awt/windows/WToolkit", "eventLoop", "()V", toolkit_event_loop);
    r.add("sun/awt/windows/WToolkit", "shutdown", "()V", toolkit_shutdown);
    r.add("sun/awt/windows/WToolkit", "postDispose", "()V", traced!("postDispose", no_op));
    r.add("sun/awt/windows/WToolkit", "makeColorModel", "()Ljava/awt/image/ColorModel;", traced!("makeColorModel", toolkit_make_color_model));
    r.add("sun/awt/windows/WToolkit", "getScreenInsets", "(I)Ljava/awt/Insets;", traced!("getScreenInsets", toolkit_screen_insets));
    r.add("sun/awt/windows/WToolkit", "nativeSync", "()V", traced!("nativeSync", no_op));
    r.add("sun/awt/windows/WToolkit", "beep", "()V", traced!("beep", no_op));
    r.add("sun/awt/windows/WToolkit", "getLockingKeyStateNative", "(I)Z", traced!("getLockingKeyStateNative", |_, _| zi(0)));
    r.add("sun/awt/windows/WToolkit", "setLockingKeyStateNative", "(IZ)V", traced!("setLockingKeyStateNative", no_op));
    r.add("sun/awt/windows/WToolkit", "loadSystemColors", "([I)V", traced!("loadSystemColors", toolkit_load_system_colors));
    r.add("sun/awt/windows/WToolkit", "getMaximumCursorColors", "()I", traced!("getMaximumCursorColors", |_, _| zi(256)));
    r.add("sun/awt/windows/WToolkit", "showTouchKeyboard", "(Z)V", traced!("showTouchKeyboard", no_op));
    r.add("sun/awt/windows/WToolkit", "hideTouchKeyboard", "()V", traced!("hideTouchKeyboard", no_op));
    r.add("sun/awt/windows/WToolkit", "syncNativeQueue", "(J)Z", traced!("syncNativeQueue", |_, _| zi(1)));
    r.add("sun/awt/windows/WToolkit", "setExtraMouseButtonsEnabledNative", "(Z)V", traced!("setExtraMouseButtonsEnabledNative", no_op));
    r.add("sun/awt/windows/WToolkit", "getNumberOfButtonsImpl", "()I", traced!("getNumberOfButtonsImpl", |_, _| zi(3)));
    r.add("sun/awt/windows/WToolkit", "startSecondaryEventLoop", "()V", traced!("startSecondaryEventLoop", no_op));
    r.add("sun/awt/windows/WToolkit", "quitSecondaryEventLoop", "()V", traced!("quitSecondaryEventLoop", no_op));
    r.add("sun/awt/windows/WToolkit", "setDynamicLayoutNative", "(Z)V", traced!("setDynamicLayoutNative", no_op));
    r.add("sun/awt/windows/WToolkit", "isDynamicLayoutSupportedNative", "()Z", traced!("isDynamicLayoutSupportedNative", |_, _| zi(1)));

    // ---- WGlobalCursorManager
    //
    // `Component.show()` 收尾会 `Toolkit.updateCursorImmediately()` →
    // `GlobalCursorManager._updateCursor()` → 这四个 native。
    // 鼠标还没接进来之前统一答「光标下没有窗口」：`_updateCursor` 拿到 null 就直接
    // `updateCursorOutOfJava()` 返回，后两个（`getCursorPos`/`getLocationOnScreen`）走不到。
    r.add(
        "sun/awt/windows/WGlobalCursorManager",
        "findHeavyweightUnderCursor",
        "(Z)Ljava/awt/Component;",
        traced!("findHeavyweightUnderCursor", |_, _| zi(0)),
    );
    r.add("sun/awt/windows/WGlobalCursorManager", "setCursor", "(Ljava/awt/Component;Ljava/awt/Cursor;Z)V", traced!("setCursor", no_op_taking));
    r.add("sun/awt/windows/WGlobalCursorManager", "getCursorPos", "(Ljava/awt/Point;)V", traced!("getCursorPos", no_op_taking));
    r.add("sun/awt/windows/WGlobalCursorManager", "getLocationOnScreen", "(Ljava/awt/Component;)Ljava/awt/Point;", traced!("getLocationOnScreen", |_, _| zi(0)));
    r.add("sun/awt/windows/WGlobalCursorManager", "nativeUpdateCursor", "(Ljava/awt/Component;)V", traced!("nativeUpdateCursor", no_op_taking));

    // ---- WInputMethod
    //
    // `WInputMethodDescriptor.getNativeAvailableLocales` 返回**空数组**而不是 null：
    // `getAvailableLocales()` 拿到就遍历，null 会在 `AWT-EventQueue` 上炸掉那条线程。
    //
    fn ime_available_locales(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
        Ok(Slot::I(vm.sh.heap.new_array(Vec::<Slot>::new(), "[Ljava/util/Locale;").unwrap_or(0)))
    }
    //
    // 不走 IME（输入法）整条链：窗口 dispose/失焦时会摸到它，缺一个 native 就把
    // `AWT-EventQueue` 线程打死、整个进程收不了尾。统一答「没有 native IME」。
    r.add("sun/awt/windows/WInputMethod", "createNativeContext", "()I", traced!("createNativeContext", |_, _| zi(1)));
    r.add("sun/awt/windows/WInputMethod", "destroyNativeContext", "(I)V", traced!("destroyNativeContext", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "enableNativeIME", "(Lsun/awt/windows/WComponentPeer;IZ)V", traced!("enableNativeIME", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "disableNativeIME", "(Lsun/awt/windows/WComponentPeer;)V", traced!("disableNativeIME", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "handleNativeIMEEvent", "(Lsun/awt/windows/WComponentPeer;Ljava/awt/AWTEvent;)V", traced!("handleNativeIMEEvent", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "endCompositionNative", "(IZ)V", traced!("endCompositionNative", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "setConversionStatus", "(II)V", traced!("setConversionStatus", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "getConversionStatus", "(I)I", traced!("getConversionStatus", |_, _| zi(0)));
    r.add("sun/awt/windows/WInputMethod", "setOpenStatus", "(IZ)V", traced!("setOpenStatus", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "getOpenStatus", "(I)Z", traced!("getOpenStatus", |_, _| zi(0)));
    r.add("sun/awt/windows/WInputMethod", "setStatusWindowVisible", "(Lsun/awt/windows/WComponentPeer;Z)V", traced!("setStatusWindowVisible", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "getNativeIMMDescription", "()Ljava/lang/String;", traced!("getNativeIMMDescription", |_, _| zi(0)));
    r.add("sun/awt/windows/WInputMethod", "getNativeLocale", "()Ljava/util/Locale;", traced!("getNativeLocale", |_, _| zi(0)));
    r.add("sun/awt/windows/WInputMethod", "setNativeLocale", "(Ljava/lang/String;Z)Z", traced!("setNativeLocale", |_, _| zi(0)));
    r.add("sun/awt/windows/WInputMethod", "openCandidateWindow", "(Lsun/awt/windows/WComponentPeer;II)V", traced!("openCandidateWindow", no_op_taking));
    r.add("sun/awt/windows/WInputMethod", "isCompositionStringAvailable", "(I)Z", traced!("isCompositionStringAvailable", |_, _| zi(0)));
    // 没有 native IME → 可用 locale 列表就是空数组（返回 null 会让 `getAvailableLocales` 里
    // 直接遍历数组时报 NPE，而它是在 `AWT-EventQueue` 上跑的，一炸线程就没了）。
    r.add(
        "sun/awt/windows/WInputMethodDescriptor",
        "getNativeAvailableLocales",
        "()[Ljava/util/Locale;",
        ime_available_locales,
    );

    // ---- WComponentPeer
    r.add("sun/awt/windows/WComponentPeer", "isObscured", "()Z", traced!("isObscured", |_, _| zi(0)));
    r.add("sun/awt/windows/WComponentPeer", "pShow", "()V", traced!("pShow", peer_show));
    r.add("sun/awt/windows/WComponentPeer", "hide", "()V", traced!("hide", peer_hide));
    r.add("sun/awt/windows/WComponentPeer", "enable", "()V", traced!("enable", no_op));
    r.add("sun/awt/windows/WComponentPeer", "disable", "()V", traced!("disable", no_op));
    r.add("sun/awt/windows/WComponentPeer", "getLocationOnScreen", "()Ljava/awt/Point;", traced!("getLocationOnScreen", peer_location_on_screen));
    r.add("sun/awt/windows/WComponentPeer", "reshapeNoCheck", "(IIII)V", traced!("reshapeNoCheck", peer_reshape));
    r.add("sun/awt/windows/WComponentPeer", "updateWindow", "()V", traced!("updateWindow", peer_update_window));
    r.add("sun/awt/windows/WComponentPeer", "createPrintedPixels", "(IIIII)[I", traced!("createPrintedPixels", |_, _| Ok(Slot::I(0))));
    r.add("sun/awt/windows/WComponentPeer", "reshape", "(IIII)V", traced!("reshape", peer_reshape));
    r.add("sun/awt/windows/WComponentPeer", "nativeHandleEvent", "(Ljava/awt/AWTEvent;)V", traced!("nativeHandleEvent", no_op_taking));
    r.add("sun/awt/windows/WComponentPeer", "setFocus", "(Z)V", traced!("setFocus", no_op_taking));
    r.add("sun/awt/windows/WComponentPeer", "_dispose", "()V", traced!("_dispose", peer_dispose));
    r.add("sun/awt/windows/WComponentPeer", "_setForeground", "(I)V", traced!("_setForeground", no_op_taking));
    r.add("sun/awt/windows/WComponentPeer", "_setBackground", "(I)V", traced!("_setBackground", no_op_taking));
    r.add("sun/awt/windows/WComponentPeer", "_setFont", "(Ljava/awt/Font;)V", traced!("_setFont", no_op_taking));
    r.add("sun/awt/windows/WComponentPeer", "start", "()V", traced!("start", no_op));
    r.add("sun/awt/windows/WComponentPeer", "beginValidate", "()V", traced!("beginValidate", no_op));
    r.add("sun/awt/windows/WComponentPeer", "endValidate", "()V", traced!("endValidate", no_op));
    r.add("sun/awt/windows/WComponentPeer", "addNativeDropTarget", "()J", traced!("addNativeDropTarget", |_, _| Ok(Slot::L(0))));
    r.add("sun/awt/windows/WComponentPeer", "removeNativeDropTarget", "()V", traced!("removeNativeDropTarget", no_op));
    r.add("sun/awt/windows/WComponentPeer", "nativeHandlesWheelScrolling", "()Z", traced!("nativeHandlesWheelScrolling", |_, _| zi(0)));
    r.add("sun/awt/windows/WComponentPeer", "pSetParent", "(Ljava/awt/peer/ComponentPeer;)V", traced!("pSetParent", no_op_taking));
    r.add(
        "sun/awt/windows/WComponentPeer",
        "setRectangularShape",
        "(IIIILsun/java2d/pipe/Region;)V",
        no_op_taking,
    );
    r.add("sun/awt/windows/WComponentPeer", "setZOrder", "(J)V", traced!("setZOrder", |_, _| Ok(Slot::L(0))));

    // ---- WWindowPeer
    r.add("sun/awt/windows/WWindowPeer", "_toFront", "()V", traced!("_toFront", no_op));
    r.add("sun/awt/windows/WWindowPeer", "toBack", "()V", traced!("toBack", no_op));
    r.add("sun/awt/windows/WWindowPeer", "setAlwaysOnTopNative", "(Z)V", traced!("setAlwaysOnTopNative", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "setFocusableWindow", "(Z)V", traced!("setFocusableWindow", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "_setTitle", "(Ljava/lang/String;)V", traced!("_setTitle", peer_title));
    r.add("sun/awt/windows/WWindowPeer", "_setResizable", "(Z)V", traced!("_setResizable", no_op_taking));
    r.add(
        "sun/awt/windows/WWindowPeer",
        "createAwtWindow",
        "(Lsun/awt/windows/WComponentPeer;)V",
        |vm, f| peer_create(vm, f, false),
    );
    r.add("sun/awt/windows/WWindowPeer", "updateInsets", "(Ljava/awt/Insets;)V", traced!("updateInsets", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "getSysMinWidth", "()I", traced!("getSysMinWidth", |_, _| zi(120)));
    r.add("sun/awt/windows/WWindowPeer", "getSysMinHeight", "()I", traced!("getSysMinHeight", |_, _| zi(40)));
    r.add("sun/awt/windows/WWindowPeer", "getSysIconWidth", "()I", traced!("getSysIconWidth", |_, _| zi(32)));
    r.add("sun/awt/windows/WWindowPeer", "getSysIconHeight", "()I", traced!("getSysIconHeight", |_, _| zi(32)));
    r.add("sun/awt/windows/WWindowPeer", "getSysSmIconWidth", "()I", traced!("getSysSmIconWidth", |_, _| zi(16)));
    r.add("sun/awt/windows/WWindowPeer", "getSysSmIconHeight", "()I", traced!("getSysSmIconHeight", |_, _| zi(16)));
    r.add("sun/awt/windows/WWindowPeer", "setIconImagesData", "([III[III)V", traced!("setIconImagesData", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "reshapeFrame", "(IIII)V", traced!("reshapeFrame", peer_reshape));
    r.add("sun/awt/windows/WWindowPeer", "getNativeWindowSize", "()Ljava/awt/Dimension;", traced!("getNativeWindowSize", peer_native_window_size));
    r.add("sun/awt/windows/WWindowPeer", "requestWindowFocus", "(Z)Z", traced!("requestWindowFocus", |_, _| zi(0)));
    r.add("sun/awt/windows/WWindowPeer", "setMinSize", "(II)V", traced!("setMinSize", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "modalDisable", "(Ljava/awt/Dialog;J)V", traced!("modalDisable", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "modalEnable", "(Ljava/awt/Dialog;)V", traced!("modalEnable", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "getScreenImOn", "()I", traced!("getScreenImOn", |_, _| zi(0)));
    r.add("sun/awt/windows/WWindowPeer", "setFullScreenExclusiveModeState", "(Z)V", traced!("setFullScreenExclusiveModeState", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "nativeGrab", "()V", traced!("nativeGrab", no_op));
    r.add("sun/awt/windows/WWindowPeer", "nativeUngrab", "()V", traced!("nativeUngrab", no_op));
    r.add("sun/awt/windows/WWindowPeer", "setOpacity", "(I)V", traced!("setOpacity", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "setOpaqueImpl", "(Z)V", traced!("setOpaqueImpl", no_op_taking));
    r.add("sun/awt/windows/WWindowPeer", "updateWindowImpl", "([III)V", traced!("updateWindowImpl", no_op_taking));

    // ---- WFramePeer
    r.add("sun/awt/windows/WFramePeer", "setState", "(I)V", traced!("setState", no_op_taking));
    r.add("sun/awt/windows/WFramePeer", "getState", "()I", traced!("getState", |_, _| zi(0)));
    r.add("sun/awt/windows/WFramePeer", "setMaximizedBounds", "(IIII)V", traced!("setMaximizedBounds", no_op_taking));
    r.add("sun/awt/windows/WFramePeer", "clearMaximizedBounds", "()V", traced!("clearMaximizedBounds", no_op));
    r.add("sun/awt/windows/WFramePeer", "setMenuBar0", "(Lsun/awt/windows/WMenuBarPeer;)V", traced!("setMenuBar0", no_op_taking));
    r.add(
        "sun/awt/windows/WFramePeer",
        "createAwtFrame",
        "(Lsun/awt/windows/WComponentPeer;)V",
        |vm, f| peer_create(vm, f, true),
    );
    r.add("sun/awt/windows/WFramePeer", "getSysMenuHeight", "()I", traced!("getSysMenuHeight", |_, _| zi(19)));
    r.add("sun/awt/windows/WFramePeer", "pSetIMMOption", "(Ljava/lang/String;)V", traced!("pSetIMMOption", no_op_taking));
    r.add("sun/awt/windows/WFramePeer", "synthesizeWmActivate", "(Z)V", traced!("synthesizeWmActivate", no_op_taking));

    // ---- GDI 窗口面
    r.add("sun/java2d/windows/GDIWindowSurfaceData", "initIDs", "(Ljava/lang/Class;)V", traced!("initIDs", no_op_taking));
    r.add("sun/java2d/windows/GDIWindowSurfaceData", "initOps", "(Lsun/awt/windows/WComponentPeer;IIIII)V", traced!("initOps", gdi_init_ops));
    r.add("sun/java2d/windows/GDIWindowSurfaceData", "invalidateSD", "()V", traced!("invalidateSD", no_op));

    // ---- 图形环境 / 设备 / 配置
    r.add("sun/awt/Win32GraphicsDevice", "initDevice", "(I)V", traced!("initDevice", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "initNativeScale", "(I)V", traced!("initNativeScale", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "setNativeScale", "(IFF)V", traced!("setNativeScale", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "getNativeScaleX", "(I)F", traced!("getNativeScaleX", dev_init_scale));
    r.add("sun/awt/Win32GraphicsDevice", "getNativeScaleY", "(I)F", traced!("getNativeScaleY", dev_init_scale));
    r.add("sun/awt/Win32GraphicsDevice", "getMaxConfigsImpl", "(I)I", traced!("getMaxConfigsImpl", |_, _| zi(1)));
    r.add("sun/awt/Win32GraphicsDevice", "isPixFmtSupported", "(II)Z", traced!("isPixFmtSupported", |_, _| zi(1)));
    r.add("sun/awt/Win32GraphicsDevice", "getDefaultPixIDImpl", "(I)I", traced!("getDefaultPixIDImpl", |_, _| zi(1)));
    r.add("sun/awt/Win32GraphicsDevice", "enterFullScreenExclusive", "(ILjava/awt/peer/WindowPeer;)V", traced!("enterFullScreenExclusive", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "exitFullScreenExclusive", "(ILjava/awt/peer/WindowPeer;)V", traced!("exitFullScreenExclusive", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "getCurrentDisplayMode", "(I)Ljava/awt/DisplayMode;", traced!("getCurrentDisplayMode", dev_display_mode));
    r.add("sun/awt/Win32GraphicsDevice", "configDisplayMode", "(ILjava/awt/peer/WindowPeer;IIII)V", traced!("configDisplayMode", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "enumDisplayModes", "(ILjava/util/ArrayList;)V", traced!("enumDisplayModes", no_op_taking));
    r.add("sun/awt/Win32GraphicsDevice", "makeColorModel", "(IZ)Ljava/awt/image/ColorModel;", traced!("makeColorModel", dev_make_color_model));
    r.add("sun/awt/Win32GraphicsConfig", "getBounds", "(I)Ljava/awt/Rectangle;", traced!("getBounds", gconfig_get_bounds));
}

// ============================================================ 十一、shape 的非 AA 填充
//
// `Graphics2D.fillOval/fillPolygon/fill(Shape)` 在**非 AA** 下走
// `sun.java2d.pipe.LoopPipe` / `SpanShapeRenderer` → `ShapeSpanIterator`
// （Ductus 的 19 个 native，C 里是 `ShapeSpanIterator.c`）→ `FillSpans` 光栅循环。
//
// 这里用一份自带的**扫描线**实现顶上：路径按 `ShapeSpanIterator` 的 Java 契约分段累积，
// `pathDone()` 时才建边表，`nextSpan()` 逐行给出 `[x0, y0, x1, y1]`。
//
// **与 HotSpot 不一致的地方（明确记下）**：曲线（quad/cubic）是定步长折线近似的，
// Ductus 用的是自适应的 flatten，所以椭圆/曲线的边缘像素可能与真 JDK 差一两列；
// 多边形（纯直线）走的是同一套整数扫描线规则（奇偶 = 采样点 `y+0.5`），误差仅在
// 恰好落在半像素上的退化情形。AA 不在这一层解决（`MaskFill` 明确抛异常）。

/// 把路径段摊平成折线（曲线按定步长细分：二次 8 段、三次 16 段）。
///
/// `FillPath` / `DrawPath` / `ShapeSpanIterator` 三条路共用它 —— 保证**同一个形状在
/// 「fillPolygon」「fill(Shape)」「draw(Shape)」下折线化结果一致**，不然同一张图上
/// 会出现两套边界。
fn flatten_segs(segs: &[(u8, [f32; 6])]) -> Vec<(f32, f32, f32, f32)> {
    let mut out: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut cx = 0.0f32;
    let mut cy = 0.0f32;
    let mut sx = 0.0f32;
    let mut sy = 0.0f32;
    for (t, c) in segs {
        match *t {
            0 => {
                cx = c[0];
                cy = c[1];
                sx = cx;
                sy = cy;
            }
            1 => {
                out.push((cx, cy, c[0], c[1]));
                cx = c[0];
                cy = c[1];
            }
            2 => {
                let (x1, y1, x2, y2) = (c[0], c[1], c[2], c[3]);
                let (ax, ay) = (cx, cy);
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    let mt = 1.0 - t;
                    let px = mt * mt * ax + 2.0 * mt * t * x1 + t * t * x2;
                    let py = mt * mt * ay + 2.0 * mt * t * y1 + t * t * y2;
                    out.push((cx, cy, px, py));
                    cx = px;
                    cy = py;
                }
                cx = x2;
                cy = y2;
            }
            3 => {
                let (x1, y1, x2, y2, x3, y3) = (c[0], c[1], c[2], c[3], c[4], c[5]);
                let (ax, ay) = (cx, cy);
                for i in 1..=16 {
                    let t = i as f32 / 16.0;
                    let mt = 1.0 - t;
                    let px = mt * mt * mt * ax + 3.0 * mt * mt * t * x1
                        + 3.0 * mt * t * t * x2 + t * t * t * x3;
                    let py = mt * mt * mt * ay + 3.0 * mt * mt * t * y1
                        + 3.0 * mt * t * t * y2 + t * t * t * y3;
                    out.push((cx, cy, px, py));
                    cx = px;
                    cy = py;
                }
                cx = x3;
                cy = y3;
            }
            _ => {
                // SEG_CLOSE
                out.push((cx, cy, sx, sy));
                cx = sx;
                cy = sy;
            }
        }
    }
    out
}

/// 一条边（扫描线交点用）
struct SpiEdge {
    y0: f32,
    y1: f32,
    x0: f32,
    dxdy: f32,
    dir: i32,
}

struct Spi {
    /// 路径段：`(type, coords)`，type 用 `PathIterator` 常量（0=move 1=line 2=quad 3=cubic 4=close）
    segs: Vec<(u8, [f32; 6])>,
    /// 0 = 奇偶（WIND_EVEN_ODD），1 = 非零环绕（WIND_NON_ZERO）
    rule: i32,
    /// 输出区（**右上开区间**，与 `Region.getHiX/getHiY` 一致）
    lox: i32,
    loy: i32,
    hix: i32,
    hiy: i32,
    /// 边表（`pathDone` 后首次 `nextSpan` 时建）
    edges: Vec<SpiEdge>,
    built: bool,
    cur_y: i32,
    /// 当前行剩下的 span（`(x0, x1)`）
    pending: Vec<(i32, i32)>,
}

impl Spi {
    fn new() -> Spi {
        Spi {
            segs: Vec::new(),
            rule: 1,
            lox: 0,
            loy: 0,
            hix: 0,
            hiy: 0,
            edges: Vec::new(),
            built: false,
            cur_y: 0,
            pending: Vec::new(),
        }
    }

    fn path_box(&self) -> (i32, i32, i32, i32) {
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::MIN;
        let mut y1 = f32::MIN;
        let mut cx = 0.0f32;
        let mut cy = 0.0f32;
        for (t, c) in &self.segs {
            let pts: &[f32] = match *t {
                0 | 1 => &c[0..2],
                2 => &c[0..4],
                3 => &c[0..6],
                _ => continue,
            };
            for p in pts.chunks(2) {
                if p.len() == 2 {
                    cx = p[0];
                    cy = p[1];
                    x0 = x0.min(cx);
                    y0 = y0.min(cy);
                    x1 = x1.max(cx);
                    y1 = y1.max(cy);
                }
            }
        }
        if x0 > x1 {
            return (0, 0, 0, 0);
        }
        (x0.floor() as i32, y0.floor() as i32, x1.ceil() as i32, y1.ceil() as i32)
    }

    fn push_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        // 水平线不产生交点
        if y0 == y1 {
            return;
        }
        let (dir, (ya, yb, xa, xb)) = if y0 < y1 {
            (1, (y0, y1, x0, x1))
        } else {
            (-1, (y1, y0, x1, x0))
        };
        self.edges.push(SpiEdge { y0: ya, y1: yb, x0: xa, dxdy: (xb - xa) / (yb - ya), dir });
    }

    /// 把路径段摊平成边表（曲线定步长折线化）
    fn build(&mut self) {
        if self.built {
            return;
        }
        self.built = true;
        for (x0, y0, x1, y1) in flatten_segs(&self.segs) {
            self.push_line(x0, y0, x1, y1);
        }
        self.edges
            .sort_by(|a, b| a.y0.partial_cmp(&b.y0).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// 算一行（`yc = y + 0.5`）的交点，按规则折成 span
    fn spans_at(&self, y: i32) -> Vec<(i32, i32)> {
        let yc = y as f32 + 0.5;
        let mut xs: Vec<(f32, i32)> = Vec::new();
        for e in &self.edges {
            if e.y0 <= yc && yc < e.y1 {
                xs.push((e.x0 + (yc - e.y0) * e.dxdy, e.dir));
            }
        }
        if xs.len() < 2 {
            return Vec::new();
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::new();
        if self.rule == 0 {
            // 奇偶：成对取
            let mut i = 0;
            while i + 1 < xs.len() {
                let (a, b) = (xs[i].0, xs[i + 1].0);
                push_span(&mut out, a, b);
                i += 2;
            }
        } else {
            // 非零环绕
            let mut wind = 0;
            let mut start = 0.0f32;
            let mut open = false;
            for (x, d) in &xs {
                let before = wind;
                wind += *d;
                if before == 0 && wind != 0 {
                    start = *x;
                    open = true;
                } else if before != 0 && wind == 0 && open {
                    push_span(&mut out, start, *x);
                    open = false;
                }
            }
        }
        out
    }
}

fn push_span(out: &mut Vec<(i32, i32)>, a: f32, b: f32) {
    // 半开区间取整：`ceil(a-0.5)` .. `ceil(b-0.5)`（像素中心在 x+0.5）
    let x0 = (a - 0.5).ceil() as i32;
    let x1 = (b - 0.5).ceil() as i32;
    if x1 > x0 {
        out.push((x0, x1));
    }
}

static SPIS: OnceLock<Mutex<HashMap<i32, Spi>>> = OnceLock::new();

fn spis() -> &'static Mutex<HashMap<i32, Spi>> {
    SPIS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spi_mut<R>(handle: i32, f: impl FnOnce(&mut Spi) -> R) -> Option<R> {
    let mut g = spis().lock().unwrap_or_else(|e| e.into_inner());
    let e = g.entry(handle).or_insert_with(Spi::new);
    Some(f(e))
}

fn spi_append_poly(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let xs = f.arg_i(0);
    let ys = f.arg_i(1);
    let n = f.arg_i(2);
    let xoff = f.arg_i(3);
    let yoff = f.arg_i(4);
    let mut pts = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n.max(0) as usize {
        let x = vm.sh.heap.get(xs, i).map(|s| s.as_i32()).unwrap_or(0) + xoff;
        let y = vm.sh.heap.get(ys, i).map(|s| s.as_i32()).unwrap_or(0) + yoff;
        pts.push((x as f32, y as f32));
    }
    spi_mut(this, |s| {
        if let Some(&(x, y)) = pts.first() {
            s.segs.push((0, [x, y, 0.0, 0.0, 0.0, 0.0]));
        }
        for &(x, y) in pts.iter().skip(1) {
            s.segs.push((1, [x, y, 0.0, 0.0, 0.0, 0.0]));
        }
        s.segs.push((4, [0.0; 6]));
    });
    let _ = vm;
    void()
}

fn spi_add_segment(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let t = f.arg_i(0) as u8;
    let coords = f.arg_i(1);
    let n = match t {
        0 | 1 => 2,
        2 => 4,
        3 => 6,
        _ => 0,
    };
    let mut c = [0.0f32; 6];
    for i in 0..n {
        c[i] = vm
            .sh
            .heap
            .get(coords, i)
            .map(|s| match s {
                Slot::Fl(v) => v,
                Slot::F(v) => v as f32,
                other => other.as_i32() as f32,
            })
            .unwrap_or(0.0);
    }
    spi_mut(this, |s| s.segs.push((t, c)));
    void()
}

fn spi_set_rule(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let r = f.arg_i(0);
    spi_mut(this, |s| s.rule = r);
    void()
}

fn spi_set_normalize(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // Ductus 的 adjust 是「坐标对齐到 0.25 子像素」；我们的扫描线本来就按整数行采样，
    // 没有 D3D 那种亚像素需求，所以只记不做事（行为差异只可能落在 .25/.75 的边界上）。
    void()
}

fn spi_set_output_area(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let (lox, loy, hix, hiy) = (f.arg_i(0), f.arg_i(1), f.arg_i(2), f.arg_i(3));
    spi_mut(this, |s| {
        s.lox = lox;
        s.loy = loy;
        s.hix = hix;
        s.hiy = hiy;
    });
    void()
}

fn spi_intersect_clip(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let (lox, loy, hix, hiy) = (f.arg_i(0), f.arg_i(1), f.arg_i(2), f.arg_i(3));
    spi_mut(this, |s| {
        s.lox = s.lox.max(lox);
        s.loy = s.loy.max(loy);
        s.hix = s.hix.min(hix);
        s.hiy = s.hiy.min(hiy);
    });
    void()
}

fn spi_get_path_box(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let arr = f.arg_i(0);
    if let Some((x0, y0, x1, y1)) = spi_mut(this, |s| s.path_box()) {
        for (i, v) in [x0, y0, x1, y1].iter().enumerate() {
            vm.sh.heap.set(arr, i, Slot::I(*v));
        }
    }
    void()
}

fn spi_next_span(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let box_arr = f.arg_i(0);
    let out = spi_mut(this, |s| {
        s.build();
        loop {
            if let Some((x0, x1)) = s.pending.pop() {
                let x0 = x0.max(s.lox);
                let x1 = x1.min(s.hix);
                if x0 < x1 {
                    return Some((x0, s.cur_y, x1));
                }
                continue;
            }
            if s.cur_y >= s.hiy {
                return None;
            }
            let y = s.cur_y;
            s.cur_y += 1;
            if y < s.loy {
                continue;
            }
            let mut sp = s.spans_at(y);
            sp.reverse();
            s.pending = sp;
        }
    });
    match out.flatten() {
        Some((x0, y, x1)) => {
            for (i, v) in [x0, y, x1, y + 1].iter().enumerate() {
                vm.sh.heap.set(box_arr, i, Slot::I(*v));
            }
            Ok(Slot::I(1))
        }
        None => Ok(Slot::I(0)),
    }
}

fn spi_skip_down_to(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let y = f.arg_i(0);
    spi_mut(this, |s| {
        s.build();
        if y > s.cur_y {
            s.cur_y = y;
            s.pending.clear();
        }
    });
    void()
}

fn spi_dispose(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    spis().lock().unwrap_or_else(|e| e.into_inner()).remove(&this);
    void()
}

fn spi_path_done(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    spi_mut(this, |s| s.build());
    void()
}

fn spi_zero_long(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // `getNativeIterator` / `getNativeConsumer` 给的是 C 侧的 SpanIterator 函数块指针。
    // 我们全部在 Rust 里迭代，Java 侧只通过 `nextSpan` 走，所以给 0；
    // 真有人拿它去喂 native（AA 的 MaskFill 那条路）会明确失败 —— 那也正是我们抛异常的地方。
    Ok(Slot::L(0))
}

fn spi_move_to(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let (x, y) = (f.arg_f(0), f.arg_f(1));
    spi_mut(this, |s| s.segs.push((0, [x, y, 0.0, 0.0, 0.0, 0.0])));
    void()
}

fn spi_line_to(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let (x, y) = (f.arg_f(0), f.arg_f(1));
    spi_mut(this, |s| s.segs.push((1, [x, y, 0.0, 0.0, 0.0, 0.0])));
    void()
}

fn spi_quad_to(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let c = [
        f.arg_f(0),
        f.arg_f(1),
        f.arg_f(2),
        f.arg_f(3),
        0.0,
        0.0,
    ];
    spi_mut(this, |s| s.segs.push((2, c)));
    void()
}

fn spi_curve_to(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let c = [
        f.arg_f(0),
        f.arg_f(1),
        f.arg_f(2),
        f.arg_f(3),
        f.arg_f(4),
        f.arg_f(5),
    ];
    spi_mut(this, |s| s.segs.push((3, c)));
    void()
}

fn spi_close_path(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    spi_mut(this, |s| s.segs.push((4, [0.0; 6])));
    void()
}

fn fl(s: Slot) -> f32 {
    match s {
        Slot::Fl(v) => v,
        Slot::F(v) => v as f32,
        other => other.as_i32() as f32,
    }
}

/// `sun.java2d.loops.FillSpans` 的私有 native：`(sg2d, sd, int, long, SpanIterator)`
///
/// 逐 span 调 `SpanIterator.nextSpan(int[])`，再用 `fill_span` 落到面上 ——
/// 与 `FillRect` 共用同一套像素写入（颜色/alpha/裁剪语义天然一致）。
fn loop_fill_spans(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.arg_i(0);
    let dst = f.arg_i(1);
    let si = f.arg_i(4);
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("FillSpans：目标不是本 VM 记录的面"));
    };
    let g = read_g2d(vm, sg);
    if g.composite_state == COMP_CUSTOM || g.composite_state == COMP_XOR {
        return Err(unsupported("FillSpans：自定义/XOR 合成"));
    }
    if si <= 0 {
        return Err(unsupported("FillSpans：SpanIterator 为 null"));
    }
    let spanbox = new_int_array(vm, 4, 0);
    if spanbox == 0 {
        return Err(unsupported("FillSpans：分配 spanbox 失败"));
    }
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 2_000_000 {
            break;
        }
        let more = call_java(vm, "sun/java2d/pipe/SpanIterator", "nextSpan", "([I)Z", &[Slot::I(spanbox)], si)?;
        if more.as_i32() == 0 {
            break;
        }
        let x0 = vm.sh.heap.get(spanbox, 0).map(|v| v.as_i32()).unwrap_or(0);
        let y = vm.sh.heap.get(spanbox, 1).map(|v| v.as_i32()).unwrap_or(0);
        let x1 = vm.sh.heap.get(spanbox, 2).map(|v| v.as_i32()).unwrap_or(0);
        let y1 = vm.sh.heap.get(spanbox, 3).map(|v| v.as_i32()).unwrap_or(y + 1).max(y + 1);
        for yy in y..y1 {
            for (a, b) in clip_span(vm, g.dev_clip, yy, x0, x1) {
                fill_span(vm, &s, &g, yy, a, b);
            }
        }
    }
    flush_surface(vm, dst);
    void()
}

/// 注册 `ShapeSpanIterator` 的 19 个 native + 真的 `FillSpans`
pub(super) fn register_shapes(r: &mut NativeRegistry) {
    let c = "sun/java2d/pipe/ShapeSpanIterator";
    r.add(c, "initIDs", "()V", traced!("ssi.initIDs", no_op));
    r.add(c, "appendPoly", "([I[IIII)V", traced!("ssi.appendPoly", spi_append_poly));
    r.add(c, "setNormalize", "(Z)V", traced!("ssi.setNormalize", spi_set_normalize));
    r.add(c, "setOutputAreaXYXY", "(IIII)V", traced!("ssi.setOutputArea", spi_set_output_area));
    r.add(c, "setRule", "(I)V", traced!("ssi.setRule", spi_set_rule));
    r.add(c, "addSegment", "(I[F)V", traced!("ssi.addSegment", spi_add_segment));
    r.add(c, "getPathBox", "([I)V", traced!("ssi.getPathBox", spi_get_path_box));
    r.add(c, "intersectClipBox", "(IIII)V", traced!("ssi.intersectClip", spi_intersect_clip));
    r.add(c, "nextSpan", "([I)Z", traced!("ssi.nextSpan", spi_next_span));
    r.add(c, "skipDownTo", "(I)V", traced!("ssi.skipDownTo", spi_skip_down_to));
    r.add(c, "getNativeIterator", "()J", traced!("ssi.getNativeIterator", spi_zero_long));
    r.add(c, "dispose", "()V", traced!("ssi.dispose", spi_dispose));
    r.add(c, "moveTo", "(FF)V", traced!("ssi.moveTo", spi_move_to));
    r.add(c, "lineTo", "(FF)V", traced!("ssi.lineTo", spi_line_to));
    r.add(c, "quadTo", "(FFFF)V", traced!("ssi.quadTo", spi_quad_to));
    r.add(c, "curveTo", "(FFFFFF)V", traced!("ssi.curveTo", spi_curve_to));
    r.add(c, "closePath", "()V", traced!("ssi.closePath", spi_close_path));
    r.add(c, "pathDone", "()V", traced!("ssi.pathDone", spi_path_done));
    r.add(c, "getNativeConsumer", "()J", traced!("ssi.getNativeConsumer", spi_zero_long));
}

// ============================================================ 十二、文字（GDI 栅格化）

/// 一条 GDI 字体句柄（缓存项）
struct GdiFont {
    hfont: isize,
    ascent: i32,
    descent: i32,
    height: i32,
    leading: i32,
}

/// 字体缓存上限。超过就**整批 DeleteObject 清掉**再来 —— 宁可重建，也不让 GDI 句柄
/// 无界增长（每进程默认上限 10000 个句柄，程序里字号/字体名一多很容易撞上）。
const MAX_FONTS: usize = 32;

static FONTS: OnceLock<Mutex<HashMap<(String, i32, i32), GdiFont>>> = OnceLock::new();

fn fonts() -> &'static Mutex<HashMap<(String, i32, i32), GdiFont>> {
    FONTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或建）一个 GDI 字体。`style` 用 `java.awt.Font` 的位：1=BOLD 2=ITALIC。
///
/// **字体名不做映射**：直接把 `java.awt.Font.name` 交给 Windows 字体系统
/// （`Dialog`/`Serif`/`SansSerif` 这类逻辑名 GDI 认得，`Consolas` 这类物理名也认得；
/// 找不到时 GDI 自己替换成缺省字体，不会失败）。
fn gdi_font(name: &str, style: i32, px: i32) -> Option<GdiFont> {
    let px = px.max(1);
    let key = (name.to_string(), style & 3, px);
    if let Some(f) = fonts().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(GdiFont {
            hfont: f.hfont,
            ascent: f.ascent,
            descent: f.descent,
            height: f.height,
            leading: f.leading,
        });
    }
    let face = wide(name);
    let weight = if style & 1 != 0 { 700 } else { 400 };
    let italic = if style & 2 != 0 { 1 } else { 0 };
    let hfont = unsafe {
        w32::CreateFontW(
            -px, // 负值 = 字符高度（正值是「单元格高度」，会带上行距）
            0,
            0,
            0,
            weight,
            italic,
            0,
            0,
            1, // DEFAULT_CHARSET
            0, // OUT_DEFAULT_PRECIS
            0, // CLIP_DEFAULT_PRECIS
            4, // ANTIALIASED_QUALITY（灰度抗锯齿；CLEARTYPE 会带子像素彩边）
            0, // DEFAULT_PITCH | FF_DONTCARE
            face.as_ptr(),
        )
    };
    if hfont == 0 {
        return None;
    }
    // 量度量：Ascent 是基线以上的高度，画字时要拿它把 baseline 换成左上角
    let dc = unsafe { w32::CreateCompatibleDC(0) };
    if dc == 0 {
        unsafe { w32::DeleteObject(hfont) };
        return None;
    }
    let old = unsafe { w32::SelectObject(dc, hfont) };
    let mut tm: w32::TextMetricW = unsafe { std::mem::zeroed() };
    let ok = unsafe { w32::GetTextMetricsW(dc, &mut tm) };
    unsafe {
        w32::SelectObject(dc, old);
        w32::DeleteDC(dc);
    }
    if ok == 0 {
        unsafe { w32::DeleteObject(hfont) };
        return None;
    }
    let f = GdiFont {
        hfont,
        ascent: tm.ascent,
        descent: tm.descent,
        height: tm.height,
        leading: tm.internal_leading.max(0) + tm.external_leading.max(0),
    };
    let mut g = fonts().lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= MAX_FONTS {
        for (_, v) in g.drain() {
            unsafe { w32::DeleteObject(v.hfont) };
        }
    }
    g.insert(key, f);
    let f = g.get(&(name.to_string(), style & 3, px)).unwrap();
    Some(GdiFont {
        hfont: f.hfont,
        ascent: f.ascent,
        descent: f.descent,
        height: f.height,
        leading: f.leading,
    })
}

/// 一段文本在指定字体下的**像素宽高**（GDI `GetTextExtentPoint32W`）
fn text_extent(hfont: isize, text: &[u16]) -> (i32, i32) {
    let dc = unsafe { w32::CreateCompatibleDC(0) };
    if dc == 0 {
        return (0, 0);
    }
    let old = unsafe { w32::SelectObject(dc, hfont) };
    let mut sz = w32::Size { cx: 0, cy: 0 };
    unsafe { w32::GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut sz) };
    unsafe {
        w32::SelectObject(dc, old);
        w32::DeleteDC(dc);
    }
    (sz.cx.max(0), sz.cy.max(0))
}

/// 把 `src` 按覆盖率 `cov`(0..255) 混到 `dst` 上（前景不透明）
fn blend_cov(dst: i32, src: i32, cov: i32) -> i32 {
    if cov <= 0 {
        return dst;
    }
    if cov >= 255 {
        return src;
    }
    let inv = 255 - cov;
    let ch = |v: i32, sh: i32| (v >> sh) & 0xFF;
    let mix = |sh: i32| (ch(src, sh) * cov + ch(dst, sh) * inv) / 255;
    (0xFF00_0000u32 as i32) | (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// GDI 栅格化一段文字，把灰度覆盖混到目标面上（基线在 `(x, y)`）。
///
/// 过程：建一块 `w×h` 的 32bpp 顶向下 DIB → `SetBkMode(TRANSPARENT)` +
/// `SetTextColor(白)` 把覆盖度画成灰度 → 逐像素按覆盖度混色。
/// 句柄全部配对释放（`CreateDIBSection`↔`DeleteObject`、`CreateCompatibleDC`↔`DeleteDC`）；
/// 字体句柄留在缓存里复用（见 [`MAX_FONTS`] 上限）。
fn rasterize_text(
    vm: &mut Vm,
    s: &Surface,
    g: &G2d,
    dst_surf: i32,
    f: &GdiFont,
    text: &[u16],
    x: i32,
    y: i32,
) {
    if text.is_empty() {
        return;
    }
    let (w, h) = text_extent(f.hfont, text);
    if w <= 0 || h <= 0 {
        return;
    }
    let top = y - f.ascent;
    let dc = unsafe { w32::CreateCompatibleDC(0) };
    if dc == 0 {
        return;
    }
    let mut bi = w32::BitmapInfo {
        header: w32::BitmapInfoHeader {
            size: std::mem::size_of::<w32::BitmapInfoHeader>() as u32,
            width: w,
            height: -h, // 负 = 顶向下，行序与我们的缓冲一致
            planes: 1,
            bit_count: 32,
            compression: 0, // BI_RGB
            size_image: 0,
            xppm: 0,
            yppm: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0; 256],
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbmp = unsafe { w32::CreateDIBSection(dc, &mut bi, 0, &mut bits, 0, 0) };
    if hbmp == 0 || bits.is_null() {
        unsafe { w32::DeleteDC(dc) };
        return;
    }
    let old = unsafe { w32::SelectObject(dc, hbmp) };
    let ofont = unsafe { w32::SelectObject(dc, f.hfont) };
    unsafe {
        w32::SetBkMode(dc, 1); // TRANSPARENT
        w32::SetTextColor(dc, 0x00FF_FFFF);
        w32::TextOutW(dc, 0, 0, text.as_ptr(), text.len() as i32);
        w32::SelectObject(dc, ofont);
    }
    // GDI 写完就是最终内容：先拷出来，再逐个混（不能一边持 DC 一边写 surface）
    let n = (w as usize) * (h as usize);
    let cov: Vec<u8> = unsafe { std::slice::from_raw_parts(bits as *const u8, n * 4) }.to_vec();
    unsafe {
        w32::SelectObject(dc, old);
        w32::DeleteObject(hbmp);
        w32::DeleteDC(dc);
    }
    let alpha = ((g.eargb as u32) >> 24) as i32 & 0xFF;
    let fg = g.eargb | (0xFF00_0000u32 as i32);
    for row in 0..h {
        let yy = top + row;
        for col in 0..w {
            let i = ((row as usize) * (w as usize) + col as usize) * 4;
            // 白字黑底：三个通道相等，任取一个
            let c = cov[i] as i32;
            if c == 0 {
                continue;
            }
            let c = if alpha == 255 { c } else { c * alpha / 255 };
            if c == 0 {
                continue;
            }
            let xx = x + col;
            for (a, b) in clip_span(vm, g.dev_clip, yy, xx, xx + 1) {
                if a >= b {
                    continue;
                }
                let idx = s.base + yy * s.scan_step + a * s.pix_step;
                let dst = surface_get(vm, s, idx);
                surface_put(vm, s, idx, blend_cov(dst, fg, c));
            }
        }
    }
    flush_surface(vm, dst_surf);
}

/// 从 `SunGraphics2D` 上取（surface, 字体名/字号/粗斜, transX/Y），画一段文字。
fn sg2d_text(vm: &mut Vm, sg: i32, text: &str, x: i32, y: i32) -> Result<Slot, VmError> {
    let dst = get_ref_field(vm, sg, "sun/java2d/SunGraphics2D", "surfaceData");
    let Some(s) = surface_of(dst) else {
        return Err(unsupported("drawString：目标 SurfaceData 不是本 VM 记录的 BufImgSurfaceData"));
    };
    let g = read_g2d(vm, sg);
    if g.composite_state == COMP_CUSTOM || g.composite_state == COMP_XOR {
        return Err(unsupported("drawString：自定义/XOR 合成"));
    }
    let font = get_ref_field(vm, sg, "sun/java2d/SunGraphics2D", "font");
    let (name, style, size) = if font > 0 {
        (
            {
                let n = vm.sh.heap.string_text(get_ref_field(vm, font, "java/awt/Font", "name"));
                if n.is_empty() { "Dialog".to_string() } else { n }
            },
            get_i32_field(vm, font, "java/awt/Font", "style"),
            get_i32_field(vm, font, "java/awt/Font", "size").max(1),
        )
    } else {
        ("Dialog".to_string(), 0, 12)
    };
    let Some(f) = gdi_font(&name, style, size) else {
        return Err(unsupported("drawString：CreateFontW 失败"));
    };
    let tx = get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "transX");
    let ty = get_i32_field(vm, sg, "sun/java2d/SunGraphics2D", "transY");
    // Java2D 的 drawString 只画一行：`\n`/`\r` 不显示也不占位
    let units: Vec<u16> = text
        .encode_utf16()
        .filter(|c| *c != b'\n' as u16 && *c != b'\r' as u16)
        .collect();
    rasterize_text(vm, &s, &g, dst, &f, &units, x + tx, y + ty);
    void()
}

/// `SunGraphics2D.drawString(String, int, int)` —— **VM 接管**（`add_override`）。
///
/// 为什么接管：JDK 25 的文字栈（`sun.font.HBShaper` / `SunLayoutEngine` /
/// `StrikeCache`）已经改用 `java.lang.foreign`，本 VM 没有 FFM 运行时，
/// `drawString` 连 `GlyphList` 都造不出来，`DrawGlyphList` 那条 native 永远到不了。
///
/// **渲染结果不与 HotSpot 逐像素一致**：字体、hinting、抗锯齿都不一样（这里是 Windows
/// 系统字体 + GDI 灰度覆盖）。判据是「字能显示、位置对（基线）、颜色对、advance 对」。
fn sg2d_draw_string(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.this();
    let h = f.arg_i(0);
    let (x, y) = (f.arg_i(1), f.arg_i(2));
    let text = vm.sh.heap.string_text(h);
    atrace("drawString");
    sg2d_text(vm, sg, &text, x, y)
}

/// `SunGraphics2D.drawChars(char[], int off, int len, int x, int y)`
fn sg2d_draw_chars(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let sg = f.this();
    let arr = f.arg_i(0);
    let off = f.arg_i(1).max(0) as usize;
    let len = f.arg_i(2).max(0) as usize;
    let (x, y) = (f.arg_i(3), f.arg_i(4));
    let n = vm.sh.heap.len(arr).min(off + len);
    let mut units: Vec<u16> = Vec::with_capacity(len);
    for i in off..n {
        units.push(vm.sh.heap.get(arr, i).map(|v| v.as_i32() as u16).unwrap_or(0));
    }
    let text = String::from_utf16_lossy(&units);
    atrace("drawChars");
    sg2d_text(vm, sg, &text, x, y)
}

/// 注册文字相关的 VM 接管。
///
/// **需要 `exec.rs` 配合**：`is_override_owner` 的白名单里必须加
/// `b"sun/java2d/SunGraphics2D"`，否则这里登记的 overrides 永远不会被查到
/// （查表前先过白名单）。改动只有一行，见回报。
fn register_text(r: &mut NativeRegistry) {
    r.add_override(
        "sun/java2d/SunGraphics2D",
        "drawString",
        "(Ljava/lang/String;II)V",
        sg2d_draw_string,
    );
    r.add_override(
        "sun/java2d/SunGraphics2D",
        "drawString",
        "(Ljava/lang/String;FF)V",
        sg2d_draw_string,
    );
    r.add_override(
        "sun/java2d/SunGraphics2D",
        "drawChars",
        "([CIIII)V",
        sg2d_draw_chars,
    );
}

// ============================================================ 十三、输入事件（鼠标 / 键盘）

/// UI 线程（Win32 泵）攒下的输入事件，等 Java 侧的 `eventLoop()` 取走投给组件。
///
/// 为什么要分两段：Win32 的消息**只能由建窗口的那个线程**抽出来，而窗口建在 Rust 的
/// UI 线程上（见 [`ui_thread_main`]）；那条线程没有解释器上下文，不能调 Java。
/// 所以 WndProc 只入队，`eventLoop`（`AWT-Windows` 线程，是 Java 线程）出队并
/// `Component.dispatchEvent(...)` —— 与 HotSpot 里 `WWindowPeer::HandleEvent` 的位置等价。
#[derive(Clone, Copy)]
enum InputEvt {
    Mouse {
        hwnd: isize,
        /// `MouseEvent.MOUSE_*` 常量
        id: i32,
        x: i32,
        y: i32,
        button: i32,
        clicks: i32,
        modifiers: i32,
    },
    Key {
        hwnd: isize,
        /// `KeyEvent.KEY_PRESSED` / `KEY_RELEASED` / `KEY_TYPED`
        id: i32,
        code: i32,
        ch: u16,
        modifiers: i32,
    },
    /// `WM_PAINT`：让 Java 侧 `Component.paint()` 重新产出像素。
    ///
    /// 不带上「脏矩形」——`handlePaint(0,0,w,h)` 就是整窗重画，够用且省得同步 `BeginPaint`
    /// 的矩形。窗口还不大的时候整画比裁剪便宜。
    Paint {
        hwnd: isize,
        w: i32,
        h: i32,
    },
}

static EVENTS: OnceLock<Mutex<Vec<InputEvt>>> = OnceLock::new();

fn events() -> &'static Mutex<Vec<InputEvt>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_event(e: InputEvt) {
    let mut q = events().lock().unwrap_or_else(|err| err.into_inner());
    // 上限只是兜底：正常情况下 `eventLoop` 4ms 就取一次。没有消费者时不让它无限涨。
    if q.len() < 4096 {
        q.push(e);
    }
}

static AWT_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `hwnd → WComponentPeer.target`（要投给的 Java `Component`）
fn peer_target(vm: &Vm, hwnd: isize) -> i32 {
    let peer = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        match st.by_hwnd.get(&hwnd) {
            Some(p) => *p,
            None => return 0,
        }
    };
    get_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "target")
}

/// 从 `comp` 往下找到 `(x, y)`（comp 坐标系）处**最深的那个可见组件**。
///
/// Swing 是轻量组件（没有自己的 HWND），所以命中测试只能在 Java 组件树上做：
/// 逐个 `Container.getComponent(i)` 反着找（后加的在上层），落在哪个里面就递归下去。
/// 返回 `(组件, 该组件内的 x, 该组件内的 y)`。
fn deepest_at(vm: &mut Vm, comp: i32, x: i32, y: i32) -> (i32, i32, i32) {
    if comp <= 0 || !vm.is_instance(comp, "java/awt/Container") {
        return (comp, x, y);
    }
    let n = call_java(vm, "java/awt/Container", "getComponentCount", "()I", &[], comp)
        .map(|v| v.as_i32())
        .unwrap_or(0);
    for i in (0..n).rev() {
        let c = call_java(vm, "java/awt/Container", "getComponent", "(I)Ljava/awt/Component;", &[Slot::I(i)], comp)
            .map(|v| v.as_i32())
            .unwrap_or(0);
        if c <= 0 {
            continue;
        }
        let vis = call_java(vm, "java/awt/Component", "isVisible", "()Z", &[], c).map(|v| v.as_i32()).unwrap_or(0);
        if vis == 0 {
            continue;
        }
        let cx = call_java(vm, "java/awt/Component", "getX", "()I", &[], c).map(|v| v.as_i32()).unwrap_or(0);
        let cy = call_java(vm, "java/awt/Component", "getY", "()I", &[], c).map(|v| v.as_i32()).unwrap_or(0);
        let (lx, ly) = (x - cx, y - cy);
        if call_java(vm, "java/awt/Component", "contains", "(II)Z", &[Slot::I(lx), Slot::I(ly)], c)
            .map(|v| v.as_i32())
            .unwrap_or(0)
            != 0
        {
            return deepest_at(vm, c, lx, ly);
        }
    }
    (comp, x, y)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// 投递一个鼠标事件。`src`/`x`/`y` 用**最深那个组件**的坐标 —— 与 AWT 的语义一致
/// （轻量组件的 `MouseEvent` 坐标是相对它自己的）。
fn dispatch_mouse(
    vm: &mut Vm,
    hwnd: isize,
    id: i32,
    x: i32,
    y: i32,
    button: i32,
    clicks: i32,
    modifiers: i32,
) {
    let target = peer_target(vm, hwnd);
    if target <= 0 {
        return;
    }
    let (comp, lx, ly) = deepest_at(vm, target, x, y);
    if comp <= 0 {
        return;
    }
    let when = now_ms();
    let desc = "(Ljava/awt/Component;IJIIIIZI)V";
    let args = [
        Slot::I(comp),
        Slot::I(id),
        Slot::L(when),
        Slot::I(modifiers),
        Slot::I(lx),
        Slot::I(ly),
        Slot::I(clicks),
        Slot::I(0),
        Slot::I(button),
    ];
    let ev = new_obj(vm, "java/awt/event/MouseEvent", desc, &args);
    if ev <= 0 {
        return;
    }
    let _ = call_java(vm, "java/awt/Component", "dispatchEvent", "(Ljava/awt/AWTEvent;)V", &[Slot::I(ev)], comp);
}

/// 投递一个键盘事件
fn dispatch_key(vm: &mut Vm, hwnd: isize, id: i32, code: i32, ch: u16, modifiers: i32) {
    let target = peer_target(vm, hwnd);
    if target <= 0 {
        return;
    }
    // 键盘事件的目标是**持有焦点的组件**（`Window.getFocusOwner`），拿不到就发给窗口
    let mut comp = call_java(vm, "java/awt/Window", "getMostRecentFocusOwner", "()Ljava/awt/Component;", &[], target)
        .map(|v| v.as_i32())
        .unwrap_or(0);
    if comp <= 0 {
        comp = target;
    }
    let when = now_ms();
    let desc = "(Ljava/awt/Component;IJI[C)V";
    let chars = vm.sh.heap.new_array(vec![Slot::I(ch as i32)], "[C").unwrap_or(0);
    if chars == 0 {
        return;
    }
    let args = [Slot::I(comp), Slot::I(id), Slot::L(when), Slot::I(modifiers), Slot::I(code), Slot::I(chars)];
    let ev = new_obj(vm, "java/awt/event/KeyEvent", desc, &args);
    if ev <= 0 {
        return;
    }
    let _ = call_java(vm, "java/awt/Component", "dispatchEvent", "(Ljava/awt/AWTEvent;)V", &[Slot::I(ev)], comp);
}

/// 把队列里的事件全部投给 Java（在 `eventLoop` 里调，线程安全：VM 是每线程一份）
fn drain_events(vm: &mut Vm) {
    let batch: Vec<InputEvt> = {
        let mut q = events().lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return;
        }
        std::mem::take(&mut *q)
    };
    for e in batch {
        match e {
            InputEvt::Mouse { hwnd, id, x, y, button, clicks, modifiers } => {
                dispatch_mouse(vm, hwnd, id, x, y, button, clicks, modifiers)
            }
            InputEvt::Key { hwnd, id, code, ch, modifiers } => dispatch_key(vm, hwnd, id, code, ch, modifiers),
            InputEvt::Paint { hwnd, w, h } => dispatch_paint(vm, hwnd, w, h),
        }
    }
}

/// 把一次 `WM_PAINT` 转成 Java 的「该重画了」。
///
/// 三步必须按这个顺序，缺一步就是黑窗口：
/// 1. `WComponentPeer.handlePaint(x,y,w,h)` —— 只是 `postPaintIfNecessary`，
///    把 `PaintEvent` 投进 EventQueue 就返回（**同步画完的是下面那步**）；
/// 2. `SunToolkit.flushPendingEvents()` —— 等 EventQueue 真的把 `paint()` 跑完。
///    不等就会拿旧的 `px` 去 BitBlt，屏幕上还是上一帧；
/// 3. `flush_surface` —— 把 Java 面里的像素搬到 `px` 并 `BitBlt` 上屏。
fn dispatch_paint(vm: &mut Vm, hwnd: isize, w: i32, h: i32) {
    let peer = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        match st.by_hwnd.get(&hwnd) {
            Some(p) => *p,
            None => return,
        }
    };
    if peer <= 0 {
        return;
    }
    atrace("dispatch_paint");
    // **不走 `WComponentPeer.handlePaint`**：它只是把 `PaintEvent` 投进 `EventQueue`，
    // 真正跑 `paint()` 的是 `AWT-EventQueue-0`，而那条线程要执行 `Runnable.run()` ——
    // VM 对**匿名类**的接口方法解析目前是坏的（`new Runnable(){…}` 在 EDT 上直接报
    // 「找不到方法 java/lang/Runnable.run()V」，见样张 E1；命名类 E2 与静态嵌套类 E3 都正常）。
    // 于是事件投出去没人消费，客户区就永远是我们自己那张空白 `px`（实测）。
    // 改法：在这条泵线程上**同步**画一次，无异步依赖。
    let target = get_ref_field(vm, peer, "sun/awt/windows/WComponentPeer", "target");
    atrace(&format!("dispatch_paint peer={peer} target={target}"));
    if target > 0 {
        let g = call_java(
            vm,
            "sun/awt/windows/WComponentPeer",
            "getGraphics",
            "()Ljava/awt/Graphics;",
            &[],
            peer,
        )
        .map(|v| v.as_i32())
        .unwrap_or(0);
        if g > 0 {
            // 必须调 **peer** 的 `paint(Graphics)`：它体内是
            // `target.paint(g)` 的 `invokevirtual`，虚分派由字节码自己做，
            // 所以 `PaintFrame` 的覆盖方法一定会跑到。（若直接调
            // `Component.paint`，解析到的是基类那个空实现 —— 实测画不出来。）
            let r = call_java(
                vm,
                "sun/awt/windows/WComponentPeer",
                "paint",
                "(Ljava/awt/Graphics;)V",
                &[Slot::I(g)],
                peer,
            );
            if let Err(e) = &r {
                atrace(&format!("peer.paint 失败: {}", e.msg));
            }
            // Graphics 必须还回去：`SunGraphics2D` 里挂着 SurfaceData 的引用
            let _ = call_java(vm, "java/awt/Graphics", "dispose", "()V", &[], g);
        } else {
            atrace("getGraphics 返回 null");
        }
    }
    let gdi = {
        let st = wstate().lock().unwrap_or_else(|e| e.into_inner());
        st.by_peer.get(&peer).map(|i| i.gdi).unwrap_or(0)
    };
    if gdi != 0 {
        flush_surface(vm, gdi);
    }
}

/// `WToolkit.eventLoop()` —— `AWT-Windows` 线程上的事件泵。
///
/// Java 侧 `WToolkit.run()` 在 `init()` 返回 true 时会调它，并且**永不返回**
/// （HotSpot 里它跑的是 Win32 消息循环）。我们这边 Win32 的泵已经在 Rust UI 线程上，
/// 所以这条循环只负责「把 Rust 侧攒的事件投给 Java」。
///
/// 线程安全：本 VM 的每个 Java 线程有自己的解释器上下文（`Vm::fork`），共享 `Shared`，
/// 所以这里 sleep/调 Java 都不会挡住别的线程。
fn toolkit_event_loop(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("eventLoop");
    event_pump_start_once(vm);
    void()
}

/// 起一条 **Rust 线程**做事件投递：它 `fork()` 出自己的解释器上下文（和
/// `Thread.start()` 的子线程同一条路），共享 `Shared`，然后 4ms 一轮地把
/// Win32 队列里的事件 `dispatchEvent` 给组件。
///
/// 为什么不让 `AWT-Windows` 那条 Java 线程当泵：实测让 `init()` 返回 true 好让它进
/// `eventLoop()` 之后，整个 VM 在 AWT 类初始化阶段就段错误（一条永不返回的 native
/// 线程在当前线程模型下不稳）。Rust 线程 fork 出来的上下文与 Thread.start 一样是
/// 「新解释器 + 共享堆」，稳得多，而且不依赖 Java 侧的调用时机。
fn event_pump_start_once(vm: &mut Vm) {
    static PUMP: OnceLock<()> = OnceLock::new();
    PUMP.get_or_init(|| {
        let mut child = vm.fork();
        std::thread::spawn(move || loop {
            drain_events(&mut child);
            std::thread::sleep(std::time::Duration::from_millis(4));
        });
    });
}

/// `WToolkit.shutdown()`：关机钩子调它，让 `eventLoop()` 退出来
fn toolkit_shutdown(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    atrace("toolkit shutdown");
    AWT_QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
    void()
}
