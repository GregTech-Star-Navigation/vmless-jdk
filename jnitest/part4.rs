
// ============================================================ 字符串

jni!(NewStringUTF, (env: *mut JNIEnv, bytes: *const c_char) => jstring, std::ptr::null_mut(), {
    let _ = env;
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    let raw = CStr::from_ptr(bytes).to_bytes();
    let text = java_from_utf8(raw);
    match with_vm(|vm| vm.str_new(text)) {
        Some(h) => h2p(h),
        None => std::ptr::null_mut(),
    }
});

jni!(NewString, (env: *mut JNIEnv, chars: *const jchar, len: jsize) => jstring, std::ptr::null_mut(), {
    let _ = env;
    if chars.is_null() || len < 0 {
        return std::ptr::null_mut();
    }
    let units: Vec<u16> = (0..len as usize).map(|i| *chars.add(i)).collect();
    let text = String::from_utf16_lossy(&units);
    match with_vm(|vm| vm.str_new(text)) {
        Some(h) => h2p(h),
        None => std::ptr::null_mut(),
    }
});

jni!(GetStringLength, (env: *mut JNIEnv, s: jstring) => jsize, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| match text_of(vm, h) {
        t => utf16_len(&t) as jsize,
    })
    .unwrap_or(0)
});

jni!(GetStringUTFLength, (env: *mut JNIEnv, s: jstring) => jsize, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| utf8_from_java(&text_of(vm, h)).len() as jsize).unwrap_or(0)
});

jni!(GetStringUTFLengthAsLong, (env: *mut JNIEnv, s: jstring) => jsize_t, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| utf8_from_java(&text_of(vm, h)).len() as jsize_t).unwrap_or(0)
});

jni!(GetStringUTFChars, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const c_char, std::ptr::null(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1; // 总是拷贝（没有「钉住 Java 内部缓冲」这回事）
    }
    let h = p2h(s);
    if h == 0 {
        return std::ptr::null();
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return std::ptr::null() };
    let bytes = utf8_from_java(&text);
    // modified UTF-8 里 NUL 编成 C0 80，所以 CString::new 不会失败
    let c = CString::new(bytes).unwrap_or_else(|_| CString::new("").unwrap());
    let ptr = c.as_ptr();
    if let Some(ctxp) = ctx() {
        (*ctxp).strbufs.push((ptr as usize, c));
    }
    ptr
});

jni!(ReleaseStringUTFChars, (env: *mut JNIEnv, s: jstring, chars: *const c_char) => (), (), {
    let _ = (env, s);
    if let Some(c) = ctx() {
        let key = chars as usize;
        if let Some(i) = (*c).strbufs.iter().position(|(p, _)| *p == key) {
            (*c).strbufs.remove(i);
        }
    }
});

jni!(GetStringChars, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const jchar, std::ptr::null(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1;
    }
    let h = p2h(s);
    if h == 0 {
        return std::ptr::null();
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return std::ptr::null() };
    let units: Vec<u16> = text.encode_utf16().collect();
    let ptr = units.as_ptr();
    if let Some(ctxp) = ctx() {
        (*ctxp).strbufs16.push((ptr as usize, units));
    }
    ptr
});

jni!(ReleaseStringChars, (env: *mut JNIEnv, s: jstring, chars: *const jchar) => (), (), {
    let _ = (env, s);
    if let Some(c) = ctx() {
        let key = chars as usize;
        if let Some(i) = (*c).strbufs16.iter().position(|(p, _)| *p == key) {
            (*c).strbufs16.remove(i);
        }
    }
});

jni!(GetStringCritical, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const jchar, std::ptr::null(), {
    GetStringChars(env, s, is_copy)
});

jni!(ReleaseStringCritical, (env: *mut JNIEnv, s: jstring, chars: *const jchar) => (), (), {
    ReleaseStringChars(env, s, chars)
});

jni!(GetStringRegion, (env: *mut JNIEnv, s: jstring, start: jsize, len: jsize, buf: *mut jchar) => (), (), {
    let _ = env;
    let h = p2h(s);
    if h == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return };
    let units: Vec<u16> = text.encode_utf16().collect();
    for i in 0..len as usize {
        let idx = start as usize + i;
        *buf.add(i) = units.get(idx).copied().unwrap_or(0);
    }
});

jni!(GetStringUTFRegion, (env: *mut JNIEnv, s: jstring, start: jsize, len: jsize, buf: *mut c_char) => (), (), {
    let _ = env;
    let h = p2h(s);
    if h == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return };
    let units: Vec<u16> = text.encode_utf16().collect();
    let sub: String = units
        .iter()
        .skip(start as usize)
        .take(len as usize)
        .map(|u| char::from_u32(*u as u32).unwrap_or('\u{fffd}'))
        .collect();
    let bytes = utf8_from_java(&sub);
    for (i, b) in bytes.iter().enumerate() {
        *buf.add(i) = *b as c_char;
    }
});

// ============================================================ 数组

jni!(GetArrayLength, (env: *mut JNIEnv, arr: jarray) => jsize, 0, {
    let _ = env;
    let h = p2h(arr);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| vm.sh.heap.array_len(h)).unwrap_or(None).unwrap_or(0) as jsize
});

/// 数组句柄 → 元素类型（不匹配返回 None）
fn array_kind(vm: &Vm, h: i32, want: ElemKind) -> Option<Vec<Slot>> {
    let desc = vm.class_of(h)?;
    let expect = match want {
        ElemKind::Object => desc.starts_with("[L") || desc.starts_with("[[") || desc == "[Ljava/lang/Object;",
        _ => desc == format!("[{}", want.desc()),
    };
    if !expect {
        return None;
    }
    vm.sh.heap.snapshot(h)
}

macro_rules! array_ops {
    ($(($newa:ident, $geta:ident, $rela:ident, $getr:ident, $setr:ident, $kind:expr, $ty:ty);)*) => {
        $(
            jni!($newa, (env: *mut JNIEnv, len: jsize) => jarray, std::ptr::null_mut(), {
                let _ = env;
                if len < 0 {
                    return std::ptr::null_mut();
                }
                let desc = concat!("[", $kind.desc());
                match with_vm(|vm| vm.sh.heap.alloc_array(len as usize, desc, Slot::I(0))) {
                    Some(Some(h)) => h2p(h),
                    _ => std::ptr::null_mut(),
                }
            });

            jni!($geta, (env: *mut JNIEnv, arr: jarray, is_copy: *mut jboolean) => *mut $ty,
                 std::ptr::null_mut(), {
                let _ = env;
                if !is_copy.is_null() {
                    *is_copy = 1;
                }
                let h = p2h(arr);
                if h == 0 {
                    return std::ptr::null_mut();
                }
                let Some(src) = with_vm(|vm| array_kind(vm, h, $kind)).flatten() else {
                    return std::ptr::null_mut();
                };
                let mut buf = ArrBuf::alloc($kind, src.len(), &src);
                buf.obj = h;
                let ptr = buf.ptr() as *mut $ty;
                if let Some(c) = ctx() {
                    (*c).arrbufs.push(buf);
                }
                ptr
            });

            jni!($rela, (env: *mut JNIEnv, arr: jarray, elems: *mut $ty, mode: jint) => (), (), {
                let _ = env;
                let h = p2h(arr);
                let key = elems as usize;
                let mut buf = None;
                if let Some(c) = ctx() {
                    if let Some(i) = (*c).arrbufs.iter().position(|b| b.obj == h && b.ptr() as usize == key) {
                        buf = Some((*c).arrbufs.remove(i));
                    }
                }
                let Some(mut buf) = buf else { return };
                if mode != JNI_ABORT {
                    let vals = buf.as_slice();
                    with_vm(|vm| {
                        for (i, v) in vals.iter().enumerate() {
                            vm.sh.heap.set(h, i, *v);
                        }
                    });
                }
                buf.released = true;
            });

            jni!($getr, (env: *mut JNIEnv, arr: jarray, start: jsize, len: jsize, buf: *mut $ty) => (), (), {
                let _ = env;
                let h = p2h(arr);
                if h == 0 || buf.is_null() || start < 0 || len < 0 {
                    return;
                }
                let Some(vals) = with_vm(|vm| array_kind(vm, h, $kind)).flatten() else { return };
                for i in 0..len as usize {
                    let v = vals.get(start as usize + i).copied().unwrap_or(Slot::I(0));
                    *buf.add(i) = match $kind {
                        ElemKind::Long => v.as_i64() as $ty,
                        ElemKind::Float => v.as_f32() as $ty,
                        ElemKind::Double => v.as_f64() as $ty,
                        _ => v.as_i32() as $ty,
                    };
                }
            });

            jni!($setr, (env: *mut JNIEnv, arr: jarray, start: jsize, len: jsize, buf: *const $ty) => (), (), {
                let _ = env;
                let h = p2h(arr);
                if h == 0 || buf.is_null() || start < 0 || len < 0 {
                    return;
                }
                if with_vm(|vm| array_kind(vm, h, $kind)).flatten().is_none() {
                    return;
                }
                with_vm(|vm| {
                    for i in 0..len as usize {
                        let raw = *buf.add(i);
                        let v = match $kind {
                            ElemKind::Long => Slot::L(raw as i64),
                            ElemKind::Float => Slot::Fl(raw as f32),
                            ElemKind::Double => Slot::F(raw as f64),
                            _ => Slot::I(raw as i32),
                        };
                        vm.sh.heap.set(h, start as usize + i, v);
                    }
                });
            });
        )*
    };
}

array_ops! {
    (NewBooleanArray, GetBooleanArrayElements, ReleaseBooleanArrayElements, GetBooleanArrayRegion, SetBooleanArrayRegion, ElemKind::Bool, jboolean);
    (NewByteArray, GetByteArrayElements, ReleaseByteArrayElements, GetByteArrayRegion, SetByteArrayRegion, ElemKind::Byte, jbyte);
    (NewCharArray, GetCharArrayElements, ReleaseCharArrayElements, GetCharArrayRegion, SetCharArrayRegion, ElemKind::Char, jchar);
    (NewShortArray, GetShortArrayElements, ReleaseShortArrayElements, GetShortArrayRegion, SetShortArrayRegion, ElemKind::Short, jshort);
    (NewIntArray, GetIntArrayElements, ReleaseIntArrayElements, GetIntArrayRegion, SetIntArrayRegion, ElemKind::Int, jint);
    (NewLongArray, GetLongArrayElements, ReleaseLongArrayElements, GetLongArrayRegion, SetLongArrayRegion, ElemKind::Long, jlong);
    (NewFloatArray, GetFloatArrayElements, ReleaseFloatArrayElements, GetFloatArrayRegion, SetFloatArrayRegion, ElemKind::Float, jfloat);
    (NewDoubleArray, GetDoubleArrayElements, ReleaseDoubleArrayElements, GetDoubleArrayRegion, SetDoubleArrayRegion, ElemKind::Double, jdouble);
}

jni!(NewObjectArray, (env: *mut JNIEnv, len: jsize, elem: jclass, init: jobject) => jarray, std::ptr::null_mut(), {
    let _ = env;
    if len < 0 {
        return std::ptr::null_mut();
    }
    let Some(elem_name) = with_vm(|vm| class_of_class_obj(vm, elem)).flatten() else {
        return std::ptr::null_mut();
    };
    let desc = if elem_name.starts_with('[') {
        format!("[{elem_name}")
    } else {
        format!("[L{elem_name};")
    };
    let init_h = p2h(init);
    match with_vm(|vm| vm.sh.heap.alloc_array(len as usize, &desc, Slot::I(init_h))) {
        Some(Some(h)) => h2p(h),
        _ => std::ptr::null_mut(),
    }
});

jni!(GetObjectArrayElement, (env: *mut JNIEnv, arr: jarray, i: jsize) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(arr);
    if h == 0 || i < 0 {
        return std::ptr::null_mut();
    }
    match with_vm(|vm| vm.sh.heap.get(h, i as usize)) {
        Some(Some(Slot::I(v))) if v != 0 => h2p(v),
        _ => std::ptr::null_mut(),
    }
});

jni!(SetObjectArrayElement, (env: *mut JNIEnv, arr: jarray, i: jsize, val: jobject) => (), (), {
    let _ = env;
    let h = p2h(arr);
    if h == 0 || i < 0 {
        return;
    }
    with_vm(|vm| vm.sh.heap.set_ref(h, i as usize, Slot::I(p2h(val))));
});

jni!(GetPrimitiveArrayCritical, (env: *mut JNIEnv, arr: jarray, is_copy: *mut jboolean) => *mut c_void, std::ptr::null_mut(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1;
    }
    let h = p2h(arr);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let Some(desc) = with_vm(|vm| vm.class_of(h)).flatten() else { return std::ptr::null_mut() };
    let kind = match desc.as_str() {
        "[Z" => ElemKind::Bool,
        "[B" => ElemKind::Byte,
        "[C" => ElemKind::Char,
        "[S" => ElemKind::Short,
        "[I" => ElemKind::Int,
        "[J" => ElemKind::Long,
        "[F" => ElemKind::Float,
        "[D" => ElemKind::Double,
        _ => return std::ptr::null_mut(),
    };
    let Some(src) = with_vm(|vm| vm.sh.heap.snapshot(h)).flatten() else { return std::ptr::null_mut() };
    let mut buf = ArrBuf::alloc(kind, src.len(), &src);
    buf.obj = h;
    let ptr = buf.ptr();
    if let Some(c) = ctx() {
        (*c).arrbufs.push(buf);
    }
    ptr
});

jni!(ReleasePrimitiveArrayCritical, (env: *mut JNIEnv, arr: jarray, elems: *mut c_void, mode: jint) => (), (), {
    let _ = env;
    let h = p2h(arr);
    let key = elems as usize;
    let mut buf = None;
    if let Some(c) = ctx() {
        if let Some(i) = (*c).arrbufs.iter().position(|b| b.obj == h && b.ptr() as usize == key) {
            buf = Some((*c).arrbufs.remove(i));
        }
    }
    let Some(buf) = buf else { return };
    if mode != JNI_ABORT {
        let vals = buf.as_slice();
        with_vm(|vm| {
            for (i, v) in vals.iter().enumerate() {
                vm.sh.heap.set(h, i, *v);
            }
        });
    }
});

// ============================================================ 监视器 / 其它

jni!(MonitorEnter, (env: *mut JNIEnv, obj: jobject) => jint, JNI_ERR, {
    let _ = env;
    let h = p2h(obj);
    match with_vm(|vm| vm.monitor_enter(h)) {
        Some(Ok(())) => JNI_OK,
        Some(Err(e)) => {
            if let Some(x) = e.exc {
                set_pending(x);
            }
            JNI_ERR
        }
        None => JNI_ERR,
    }
});

jni!(MonitorExit, (env: *mut JNIEnv, obj: jobject) => jint, JNI_ERR, {
    let _ = env;
    let h = p2h(obj);
    match with_vm(|vm| vm.monitor_exit(h)) {
        Some(Ok(())) => JNI_OK,
        Some(Err(e)) => {
            if let Some(x) = e.exc {
                set_pending(x);
            }
            JNI_ERR
        }
        None => JNI_ERR,
    }
});

/// `JNINativeMethod`
#[repr(C)]
pub(crate) struct JNINativeMethod {
    pub(crate) name: *const c_char,
    pub(crate) signature: *const c_char,
    pub(crate) fnPtr: *mut c_void,
}

jni!(RegisterNatives, (env: *mut JNIEnv, clazz: jclass, methods: *const JNINativeMethod, n: jint) => jint, JNI_ERR, {
    let _ = env;
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return JNI_ERR;
    };
    if methods.is_null() || n < 0 {
        return JNI_ERR;
    }
    let mut list = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let m = &*methods.add(i);
        if m.name.is_null() || m.signature.is_null() {
            return JNI_ERR;
        }
        let name = CStr::from_ptr(m.name).to_string_lossy().into_owned();
        let sig = CStr::from_ptr(m.signature).to_string_lossy().into_owned();
        list.push((name, sig, m.fnPtr as usize));
    }
    let count = super::jni_lib::register_natives(&cls, &list);
    if count != list.len() {
        return JNI_ERR;
    }
    JNI_OK
});

jni!(UnregisterNatives, (env: *mut JNIEnv, clazz: jclass) => jint, JNI_ERR, {
    let _ = env;
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return JNI_ERR;
    };
    super::jni_lib::unregister_natives(&cls);
    JNI_OK
});

/// 全局唯一的 `JavaVM`（`JNI_OnLoad` 拿到的那个）
static JAVA_VM: Mutex<Option<&'static JavaVM>> = Mutex::new(None);

pub(crate) fn java_vm_ptr() -> *mut JavaVM {
    let mut g = JAVA_VM.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let b: &'static JavaVM = Box::leak(Box::new(JavaVM { functions: invoke_interface() }));
        *g = Some(b);
    }
    g.map(|v| v as *const JavaVM as *mut JavaVM).unwrap_or(std::ptr::null_mut())
}

jni!(GetJavaVM, (env: *mut JNIEnv, out: *mut *mut JavaVM) => jint, JNI_ERR, {
    let _ = env;
    if out.is_null() {
        return JNI_ERR;
    }
    *out = java_vm_ptr();
    JNI_OK
});

// ---- 直接缓冲区（native 侧 `NewDirectByteBuffer`；Java 侧 `allocateDirect` 还没接） ----

static DIRECT_BUFS: Mutex<Vec<(i32, usize, i64)>> = Mutex::new(Vec::new());

jni!(NewDirectByteBuffer, (env: *mut JNIEnv, addr: *mut c_void, cap: jlong) => jobject, std::ptr::null_mut(), {
    let _ = env;
    if addr.is_null() {
        return std::ptr::null_mut();
    }
    let h = with_vm(|vm| {
        let name = "java/nio/DirectByteBuffer";
        let n = vm.store.field_count(name).max(1);
        vm.sh.heap.new_object(vec![Slot::I(0); n], name)
    })
    .unwrap_or(None)
    .unwrap_or(0);
    if h == 0 {
        return std::ptr::null_mut();
    }
    if let Ok(mut g) = DIRECT_BUFS.lock() {
        g.push((h, addr as usize, cap));
    }
    h2p(h)
});

jni!(GetDirectBufferAddress, (env: *mut JNIEnv, buf: jobject) => *mut c_void, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(buf);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let g = DIRECT_BUFS.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().find(|(x, _, _)| *x == h) {
        Some((_, a, _)) => *a as *mut c_void,
        None => std::ptr::null_mut(),
    }
});

jni!(GetDirectBufferCapacity, (env: *mut JNIEnv, buf: jobject) => jlong, -1, {
    let _ = env;
    let h = p2h(buf);
    if h == 0 {
        return -1;
    }
    let g = DIRECT_BUFS.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().find(|(x, _, _)| *x == h) {
        Some((_, _, c)) => *c,
        None => -1,
    }
});

// ---- JDK 21+ / 22+ 追加的三个槽位 ----

jni!(GetModule, (env: *mut JNIEnv, clazz: jclass) => jobject, std::ptr::null_mut(), {
    // 这个 VM 没有模块对象；返回 null 是合法的（`Class.getModule()` 不走它）
    let _ = (env, clazz);
    std::ptr::null_mut()
});

jni!(IsVirtualThread, (env: *mut JNIEnv, obj: jobject) => jboolean, 0, {
    let _ = (env, obj);
    0
});

// ---- 未实现的槽位：**不返回 0 之外的东西，也不改异常** ----
//
// 一个 stub 还会被当成"调用成功"（返回 0/null）。对 native 库来说这比崩溃好定位：
// 只有在真的用到那些功能时才出错，而且错误点就在库那一侧。
unsafe extern "C" fn unimplemented_slot() -> u64 {
    0
}

// ---- 非 x86_64-Windows 的 `...` 垫片占位 ----
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
macro_rules! va_shims {
    ($(($name:ident, $op:expr);)*) => {
        $(unsafe extern "C" fn $name() -> u64 { va_list_unsupported() })*
    };
}
