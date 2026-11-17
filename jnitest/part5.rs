
// ============================================================ JavaVM（JNIInvokeInterface_）

thread_local! {
    /// `AttachCurrentThread` 给这个线程建的 VM（第一次 attach 时 fork）
    static ATTACHED: RefCell<Option<&'static mut Vm>> = const { RefCell::new(None) };
}

/// `jint DestroyJavaVM(JavaVM*)`：我们只清掉当前线程的 attach 状态
unsafe extern "C" fn vm_destroy(_vm: *mut JavaVM) -> jint {
    ATTACHED.with(|a| *a.borrow_mut() = None);
    JNI_OK
}

/// `jint AttachCurrentThread(JavaVM*, void** penv, void* args)`
///
/// native 库自己起的线程要调 Java 就必须先 attach。这里给它 fork 一个 `Vm`
/// （共享同一个 `Shared`：堆/类表/监视器都是同一份），再准备好 `JNIEnv*`。
unsafe extern "C" fn vm_attach(_vm: *mut JavaVM, penv: *mut *mut c_void, _args: *mut c_void) -> jint {
    if penv.is_null() {
        return JNI_ERR;
    }
    let forked: &'static mut Vm = ATTACHED.with(|a| {
        let mut g = a.borrow_mut();
        if g.is_none() {
            // 从"当前线程的 VM"fork 一个：没有活动 VM 时说明是纯 native 线程，
            // 那就没有可 fork 的源 —— 报错比造一个空 VM 诚实
            let Some(src) = with_vm(|vm| vm as *mut Vm) else {
                return JNI_ERR;
            };
            let v: &'static mut Vm = Box::leak(Box::new((*src).fork()));
            *g = Some(v);
        }
        JNI_OK
    });
    if forked == JNI_ERR {
        return JNI_ERR;
    }
    // 让这个线程上的 JNI 函数能拿到 VM：push 一个"没有 Java 帧"的上下文。
    // 它不会被 pop（线程一直 attach 着），所以用 forget 语义：push 之后不返回。
    ctx_push(forked as *mut Vm);
    std::mem::forget(()); // 占位：attach 的上下文随线程结束
    *penv = env_ptr() as *mut c_void;
    JNI_OK
}

unsafe extern "C" fn vm_detach(_vm: *mut JavaVM) -> jint {
    let _ = ctx_pop();
    ATTACHED.with(|a| *a.borrow_mut() = None);
    JNI_OK
}

unsafe extern "C" fn vm_get_env(_vm: *mut JavaVM, penv: *mut *mut c_void, _version: jint) -> jint {
    if penv.is_null() {
        return JNI_ERR;
    }
    // 只有本线程真的在 native 调用里（或有 attach）才给 env
    if unsafe { ctx() }.is_none() {
        return -2; // JNI_EDETACHED
    }
    *penv = env_ptr() as *mut c_void;
    JNI_OK
}

unsafe extern "C" fn vm_attach_daemon(vm: *mut JavaVM, penv: *mut *mut c_void, args: *mut c_void) -> jint {
    vm_attach(vm, penv, args)
}

static INVOKE_TABLE: OnceLock<[usize; 8]> = OnceLock::new();

pub(crate) fn invoke_interface() -> *const c_void {
    let t = INVOKE_TABLE.get_or_init(|| {
        [
            0,
            0,
            0,
            vm_destroy as *const () as usize,
            vm_attach as *const () as usize,
            vm_detach as *const () as usize,
            vm_get_env as *const () as usize,
            vm_attach_daemon as *const () as usize,
        ]
    });
    t.as_ptr() as *const c_void
}

// ============================================================ 函数表

/// 名字 → 实现。**表按 `jni.h` 的声明顺序**（`SLOT_NAMES`），而名字到实现的映射是
/// 按名字查的 —— 所以"槽位错位"这类错误在这里不可能发生（顺序只有一处：`SLOT_NAMES`）。
fn slot_fn(name: &str) -> usize {
    macro_rules! m {
        ($($n:literal => $f:ident),* $(,)?) => {
            match name {
                $($n => $f as *const () as usize,)*
                _ => unimplemented_slot as *const () as usize,
            }
        };
    }
    m! {
        "GetVersion" => GetVersion,
        "FindClass" => FindClass,
        "GetObjectClass" => GetObjectClass,
        "GetSuperclass" => GetSuperclass,
        "IsAssignableFrom" => IsAssignableFrom,
        "IsInstanceOf" => IsInstanceOf,
        "AllocObject" => AllocObject,
        "Throw" => Throw,
        "ThrowNew" => ThrowNew,
        "ExceptionOccurred" => ExceptionOccurred,
        "ExceptionDescribe" => ExceptionDescribe,
        "ExceptionClear" => ExceptionClear,
        "ExceptionCheck" => ExceptionCheck,
        "FatalError" => FatalError,
        "PushLocalFrame" => PushLocalFrame,
        "PopLocalFrame" => PopLocalFrame,
        "NewGlobalRef" => NewGlobalRef,
        "DeleteGlobalRef" => DeleteGlobalRef,
        "DeleteLocalRef" => DeleteLocalRef,
        "NewLocalRef" => NewLocalRef,
        "IsSameObject" => IsSameObject,
        "EnsureLocalCapacity" => EnsureLocalCapacity,
        "NewWeakGlobalRef" => NewWeakGlobalRef,
        "DeleteWeakGlobalRef" => DeleteWeakGlobalRef,
        "GetObjectRefType" => GetObjectRefType,
        "GetMethodID" => GetMethodID,
        "GetStaticMethodID" => GetStaticMethodID,
        "GetFieldID" => GetFieldID,
        "GetStaticFieldID" => GetStaticFieldID,

        // ---- 字段读写（实例 + 静态） ----
        "GetObjectField" => GetObjectField,
        "SetObjectField" => SetObjectField,
        "GetBooleanField" => GetBooleanField,
        "SetBooleanField" => SetBooleanField,
        "GetByteField" => GetByteField,
        "SetByteField" => SetByteField,
        "GetCharField" => GetCharField,
        "SetCharField" => SetCharField,
        "GetShortField" => GetShortField,
        "SetShortField" => SetShortField,
        "GetIntField" => GetIntField,
        "SetIntField" => SetIntField,
        "GetLongField" => GetLongField,
        "SetLongField" => SetLongField,
        "GetFloatField" => GetFloatField,
        "SetFloatField" => SetFloatField,
        "GetDoubleField" => GetDoubleField,
        "SetDoubleField" => SetDoubleField,
        "GetStaticObjectField" => GetStaticObjectField,
        "SetStaticObjectField" => SetStaticObjectField,
        "GetStaticBooleanField" => GetStaticBooleanField,
        "SetStaticBooleanField" => SetStaticBooleanField,
        "GetStaticByteField" => GetStaticByteField,
        "SetStaticByteField" => SetStaticByteField,
        "GetStaticCharField" => GetStaticCharField,
        "SetStaticCharField" => SetStaticCharField,
        "GetStaticShortField" => GetStaticShortField,
        "SetStaticShortField" => SetStaticShortField,
        "GetStaticIntField" => GetStaticIntField,
        "SetStaticIntField" => SetStaticIntField,
        "GetStaticLongField" => GetStaticLongField,
        "SetStaticLongField" => SetStaticLongField,
        "GetStaticFloatField" => GetStaticFloatField,
        "SetStaticFloatField" => SetStaticFloatField,
        "GetStaticDoubleField" => GetStaticDoubleField,
        "SetStaticDoubleField" => SetStaticDoubleField,

        // ---- 调用（`...` 走裸汇编垫片，`A` 直接实现，`V` 明确不支持） ----
        "NewObject" => va_new_object,
        "NewObjectV" => va_list_unsupported,
        "NewObjectA" => NewObjectA,
        "CallObjectMethod" => va_call_object,
        "CallObjectMethodV" => va_list_unsupported,
        "CallObjectMethodA" => CallObjectMethodA,
        "CallBooleanMethod" => va_call_boolean,
        "CallBooleanMethodV" => va_list_unsupported,
        "CallBooleanMethodA" => CallBooleanMethodA,
        "CallByteMethod" => va_call_byte,
        "CallByteMethodV" => va_list_unsupported,
        "CallByteMethodA" => CallByteMethodA,
        "CallCharMethod" => va_call_char,
        "CallCharMethodV" => va_list_unsupported,
        "CallCharMethodA" => CallCharMethodA,
        "CallShortMethod" => va_call_short,
        "CallShortMethodV" => va_list_unsupported,
        "CallShortMethodA" => CallShortMethodA,
        "CallIntMethod" => va_call_int,
        "CallIntMethodV" => va_list_unsupported,
        "CallIntMethodA" => CallIntMethodA,
        "CallLongMethod" => va_call_long,
        "CallLongMethodV" => va_list_unsupported,
        "CallLongMethodA" => CallLongMethodA,
        "CallFloatMethod" => va_call_float,
        "CallFloatMethodV" => va_list_unsupported,
        "CallFloatMethodA" => CallFloatMethodA,
        "CallDoubleMethod" => va_call_double,
        "CallDoubleMethodV" => va_list_unsupported,
        "CallDoubleMethodA" => CallDoubleMethodA,
        "CallVoidMethod" => va_call_void,
        "CallVoidMethodV" => va_list_unsupported,
        "CallVoidMethodA" => CallVoidMethodA,
        "CallNonvirtualObjectMethod" => va_call_nonvirtual_object,
        "CallNonvirtualObjectMethodV" => va_list_unsupported,
        "CallNonvirtualObjectMethodA" => CallNonvirtualObjectMethodA,
        "CallNonvirtualBooleanMethod" => va_call_nonvirtual_boolean,
        "CallNonvirtualBooleanMethodV" => va_list_unsupported,
        "CallNonvirtualBooleanMethodA" => CallNonvirtualBooleanMethodA,
        "CallNonvirtualByteMethod" => va_call_nonvirtual_byte,
        "CallNonvirtualByteMethodV" => va_list_unsupported,
        "CallNonvirtualByteMethodA" => CallNonvirtualByteMethodA,
        "CallNonvirtualCharMethod" => va_call_nonvirtual_char,
        "CallNonvirtualCharMethodV" => va_list_unsupported,
        "CallNonvirtualCharMethodA" => CallNonvirtualCharMethodA,
        "CallNonvirtualShortMethod" => va_call_nonvirtual_short,
        "CallNonvirtualShortMethodV" => va_list_unsupported,
        "CallNonvirtualShortMethodA" => CallNonvirtualShortMethodA,
        "CallNonvirtualIntMethod" => va_call_nonvirtual_int,
        "CallNonvirtualIntMethodV" => va_list_unsupported,
        "CallNonvirtualIntMethodA" => CallNonvirtualIntMethodA,
        "CallNonvirtualLongMethod" => va_call_nonvirtual_long,
        "CallNonvirtualLongMethodV" => va_list_unsupported,
        "CallNonvirtualLongMethodA" => CallNonvirtualLongMethodA,
        "CallNonvirtualFloatMethod" => va_call_nonvirtual_float,
        "CallNonvirtualFloatMethodV" => va_list_unsupported,
        "CallNonvirtualFloatMethodA" => CallNonvirtualFloatMethodA,
        "CallNonvirtualDoubleMethod" => va_call_nonvirtual_double,
        "CallNonvirtualDoubleMethodV" => va_list_unsupported,
        "CallNonvirtualDoubleMethodA" => CallNonvirtualDoubleMethodA,
        "CallNonvirtualVoidMethod" => va_call_nonvirtual_void,
        "CallNonvirtualVoidMethodV" => va_list_unsupported,
        "CallNonvirtualVoidMethodA" => CallNonvirtualVoidMethodA,
        "CallStaticObjectMethod" => va_call_static_object,
        "CallStaticObjectMethodV" => va_list_unsupported,
        "CallStaticObjectMethodA" => CallStaticObjectMethodA,
        "CallStaticBooleanMethod" => va_call_static_boolean,
        "CallStaticBooleanMethodV" => va_list_unsupported,
        "CallStaticBooleanMethodA" => CallStaticBooleanMethodA,
        "CallStaticByteMethod" => va_call_static_byte,
        "CallStaticByteMethodV" => va_list_unsupported,
        "CallStaticByteMethodA" => CallStaticByteMethodA,
        "CallStaticCharMethod" => va_call_static_char,
        "CallStaticCharMethodV" => va_list_unsupported,
        "CallStaticCharMethodA" => CallStaticCharMethodA,
        "CallStaticShortMethod" => va_call_static_short,
        "CallStaticShortMethodV" => va_list_unsupported,
        "CallStaticShortMethodA" => CallStaticShortMethodA,
        "CallStaticIntMethod" => va_call_static_int,
        "CallStaticIntMethodV" => va_list_unsupported,
        "CallStaticIntMethodA" => CallStaticIntMethodA,
        "CallStaticLongMethod" => va_call_static_long,
        "CallStaticLongMethodV" => va_list_unsupported,
        "CallStaticLongMethodA" => CallStaticLongMethodA,
        "CallStaticFloatMethod" => va_call_static_float,
        "CallStaticFloatMethodV" => va_list_unsupported,
        "CallStaticFloatMethodA" => CallStaticFloatMethodA,
        "CallStaticDoubleMethod" => va_call_static_double,
        "CallStaticDoubleMethodV" => va_list_unsupported,
        "CallStaticDoubleMethodA" => CallStaticDoubleMethodA,
        "CallStaticVoidMethod" => va_call_static_void,
        "CallStaticVoidMethodV" => va_list_unsupported,
        "CallStaticVoidMethodA" => CallStaticVoidMethodA,

        // ---- 字符串 ----
        "NewString" => NewString,
        "GetStringLength" => GetStringLength,
        "GetStringChars" => GetStringChars,
        "ReleaseStringChars" => ReleaseStringChars,
        "NewStringUTF" => NewStringUTF,
        "GetStringUTFLength" => GetStringUTFLength,
        "GetStringUTFChars" => GetStringUTFChars,
        "ReleaseStringUTFChars" => ReleaseStringUTFChars,
        "GetStringRegion" => GetStringRegion,
        "GetStringUTFRegion" => GetStringUTFRegion,
        "GetStringCritical" => GetStringCritical,
        "ReleaseStringCritical" => ReleaseStringCritical,
        "GetStringUTFLengthAsLong" => GetStringUTFLengthAsLong,

        // ---- 数组 ----
        "GetArrayLength" => GetArrayLength,
        "NewObjectArray" => NewObjectArray,
        "GetObjectArrayElement" => GetObjectArrayElement,
        "SetObjectArrayElement" => SetObjectArrayElement,
        "NewBooleanArray" => NewBooleanArray,
        "NewByteArray" => NewByteArray,
        "NewCharArray" => NewCharArray,
        "NewShortArray" => NewShortArray,
        "NewIntArray" => NewIntArray,
        "NewLongArray" => NewLongArray,
        "NewFloatArray" => NewFloatArray,
        "NewDoubleArray" => NewDoubleArray,
        "GetBooleanArrayElements" => GetBooleanArrayElements,
        "GetByteArrayElements" => GetByteArrayElements,
        "GetCharArrayElements" => GetCharArrayElements,
        "GetShortArrayElements" => GetShortArrayElements,
        "GetIntArrayElements" => GetIntArrayElements,
        "GetLongArrayElements" => GetLongArrayElements,
        "GetFloatArrayElements" => GetFloatArrayElements,
        "GetDoubleArrayElements" => GetDoubleArrayElements,
        "ReleaseBooleanArrayElements" => ReleaseBooleanArrayElements,
        "ReleaseByteArrayElements" => ReleaseByteArrayElements,
        "ReleaseCharArrayElements" => ReleaseCharArrayElements,
        "ReleaseShortArrayElements" => ReleaseShortArrayElements,
        "ReleaseIntArrayElements" => ReleaseIntArrayElements,
        "ReleaseLongArrayElements" => ReleaseLongArrayElements,
        "ReleaseFloatArrayElements" => ReleaseFloatArrayElements,
        "ReleaseDoubleArrayElements" => ReleaseDoubleArrayElements,
        "GetBooleanArrayRegion" => GetBooleanArrayRegion,
        "GetByteArrayRegion" => GetByteArrayRegion,
        "GetCharArrayRegion" => GetCharArrayRegion,
        "GetShortArrayRegion" => GetShortArrayRegion,
        "GetIntArrayRegion" => GetIntArrayRegion,
        "GetLongArrayRegion" => GetLongArrayRegion,
        "GetFloatArrayRegion" => GetFloatArrayRegion,
        "GetDoubleArrayRegion" => GetDoubleArrayRegion,
        "SetBooleanArrayRegion" => SetBooleanArrayRegion,
        "SetByteArrayRegion" => SetByteArrayRegion,
        "SetCharArrayRegion" => SetCharArrayRegion,
        "SetShortArrayRegion" => SetShortArrayRegion,
        "SetIntArrayRegion" => SetIntArrayRegion,
        "SetLongArrayRegion" => SetLongArrayRegion,
        "SetFloatArrayRegion" => SetFloatArrayRegion,
        "SetDoubleArrayRegion" => SetDoubleArrayRegion,
        "GetPrimitiveArrayCritical" => GetPrimitiveArrayCritical,
        "ReleasePrimitiveArrayCritical" => ReleasePrimitiveArrayCritical,

        // ---- 注册 / 监视器 / VM ----
        "RegisterNatives" => RegisterNatives,
        "UnregisterNatives" => UnregisterNatives,
        "MonitorEnter" => MonitorEnter,
        "MonitorExit" => MonitorExit,
        "GetJavaVM" => GetJavaVM,
        "GetModule" => GetModule,
        "IsVirtualThread" => IsVirtualThread,
        "NewDirectByteBuffer" => NewDirectByteBuffer,
        "GetDirectBufferAddress" => GetDirectBufferAddress,
        "GetDirectBufferCapacity" => GetDirectBufferCapacity,
    }
}

static NATIVE_INTERFACE: OnceLock<Box<[usize; SLOT_NAMES.len()]>> = OnceLock::new();

/// `JNINativeInterface_`：整个进程共用一张
pub(crate) fn native_interface() -> *const c_void {
    let t = NATIVE_INTERFACE.get_or_init(|| {
        let mut t = Box::new([0usize; SLOT_NAMES.len()]);
        for (i, name) in SLOT_NAMES.iter().enumerate() {
            t[i] = slot_fn(name);
        }
        t
    });
    t.as_ptr() as *const c_void
}

/// 已经真实现的槽位个数（诊断/报告用：不是 0 也不是那几个 stub）
pub(crate) fn implemented_slots() -> usize {
    let mut n = 0;
    for name in SLOT_NAMES.iter() {
        let f = slot_fn(name);
        if f != unimplemented_slot as *const () as usize && f != va_list_unsupported as *const () as usize {
            n += 1;
        }
    }
    n
}

/// 没实现的槽位名单（报告用）
pub(crate) fn missing_slots() -> Vec<&'static str> {
    let mut v = Vec::new();
    for name in SLOT_NAMES.iter() {
        if slot_fn(name) == unimplemented_slot as *const () as usize {
            v.push(*name);
        }
    }
    v
}
