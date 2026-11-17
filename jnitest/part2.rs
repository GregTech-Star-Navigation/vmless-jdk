
jni!(GetStaticMethodID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jmethodID, std::ptr::null_mut(), {
    let _ = env;
    get_method_id(clazz, name, sig, true)
});

unsafe fn get_method_id(clazz: jclass, name: *const c_char, sig: *const c_char, want_static: bool) -> jmethodID {
    if name.is_null() || sig.is_null() {
        return std::ptr::null_mut();
    }
    let mname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let mdesc = CStr::from_ptr(sig).to_string_lossy().into_owned();
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    let owner = with_vm(|vm| find_method_in_chain(vm, &cls, &mname, &mdesc));
    let Some(Some(owner)) = owner else {
        let h = with_vm(|vm| throw_new(vm, "java/lang/NoSuchMethodError", &format!("{cls}.{mname}{mdesc}")))
            .unwrap_or(0);
        set_pending(h);
        return std::ptr::null_mut();
    };
    intern_method_id(MethodId { class: owner, name: mname, desc: mdesc, is_static: want_static, _priv: () })
}

jni!(GetFieldID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jfieldID, std::ptr::null_mut(), {
    let _ = env;
    get_field_id(clazz, name, sig, false)
});

jni!(GetStaticFieldID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jfieldID, std::ptr::null_mut(), {
    let _ = env;
    get_field_id(clazz, name, sig, true)
});

unsafe fn get_field_id(clazz: jclass, name: *const c_char, sig: *const c_char, want_static: bool) -> jfieldID {
    if name.is_null() || sig.is_null() {
        return std::ptr::null_mut();
    }
    let fname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let fdesc = CStr::from_ptr(sig).to_string_lossy().into_owned();
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    let owner = with_vm(|vm| find_field_owner(vm, &cls, &fname, &fdesc));
    let Some(Some(owner)) = owner else {
        let h = with_vm(|vm| throw_new(vm, "java/lang/NoSuchFieldError", &format!("{cls}.{fname}:{fdesc}")))
            .unwrap_or(0);
        set_pending(h);
        return std::ptr::null_mut();
    };
    let slot = with_vm(|vm| vm.store.field_index(&owner, &fname)).unwrap_or(None).unwrap_or(0);
    intern_field_id(FieldId { class: owner, name: fname, desc: fdesc, slot, is_static: want_static })
}

// ============================================================ 字段读写

unsafe fn get_field_slot(obj: jobject, fid: jfieldID) -> Slot {
    let Some(f) = field_id(fid) else { return Slot::I(0) };
    let h = p2h(obj);
    if h == 0 || f.is_static {
        return Slot::I(0);
    }
    with_vm(|vm| vm.sh.heap.get(h, f.slot).unwrap_or(Slot::I(0))).unwrap_or(Slot::I(0))
}

unsafe fn set_field_slot(obj: jobject, fid: jfieldID, v: Slot) {
    let Some(f) = field_id(fid) else { return };
    let h = p2h(obj);
    if h == 0 || f.is_static {
        return;
    }
    let is_ref = f.desc.starts_with('L') || f.desc.starts_with('[');
    with_vm(|vm| {
        if is_ref {
            vm.sh.heap.set_ref(h, f.slot, v);
        } else {
            vm.sh.heap.set(h, f.slot, v);
        }
    });
}

unsafe fn get_static_slot(fid: jfieldID) -> Slot {
    let Some(f) = field_id(fid) else { return Slot::I(0) };
    if !f.is_static {
        return Slot::I(0);
    }
    with_vm(|vm| {
        vm.sh
            .statics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(f.class.clone(), f.name.clone()))
            .copied()
            .unwrap_or(Slot::I(0))
    })
    .unwrap_or(Slot::I(0))
}

unsafe fn set_static_slot(fid: jfieldID, v: Slot) {
    let Some(f) = field_id(fid) else { return };
    if !f.is_static {
        return;
    }
    with_vm(|vm| {
        vm.sh
            .statics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((f.class.clone(), f.name.clone()), v);
    });
}

/// 生成「按类型读写字段」的 24 个函数：
/// 实例读 / 实例写 / 静态读 / 静态写，每种基本类型 + Object
macro_rules! field_ops {
    ($(($gf:ident, $sf:ident, $gsf:ident, $ssf:ident, $ty:ty, $mk:expr, $rd:expr);)*) => {
        $(
            jni!($gf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let v = get_field_slot(obj, fid);
                let f: fn(Slot) -> $ty = $rd;
                f(v)
            });

            jni!($sf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let f: fn($ty) -> Slot = $mk;
                set_field_slot(obj, fid, f(val));
            });

            jni!($gsf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let v = get_static_slot(fid);
                let f: fn(Slot) -> $ty = $rd;
                f(v)
            });

            jni!($ssf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let f: fn($ty) -> Slot = $mk;
                set_static_slot(fid, f(val));
            });
        )*
    };
}

field_ops! {
    (GetObjectField, SetObjectField, GetStaticObjectField, SetStaticObjectField, jobject,
     |v: Slot| Slot::I(v.as_i32()), |v: Slot| v.as_i32() as jobject);
    (GetBooleanField, SetBooleanField, GetStaticBooleanField, SetStaticBooleanField, jboolean,
     |v: Slot| Slot::I(i32::from(v)), |v: Slot| u8::from(v.as_i32() != 0));
    (GetByteField, SetByteField, GetStaticByteField, SetStaticByteField, jbyte,
     |v: Slot| Slot::I(v.as_i32() as i32), |v: Slot| v.as_i32() as i8);
    (GetCharField, SetCharField, GetStaticCharField, SetStaticCharField, jchar,
     |v: Slot| Slot::I(v.as_i32() as i32), |v: Slot| v.as_i32() as u16);
    (GetShortField, SetShortField, GetStaticShortField, SetStaticShortField, jshort,
     |v: Slot| Slot::I(v.as_i32() as i32), |v: Slot| v.as_i32() as i16);
    (GetIntField, SetIntField, GetStaticIntField, SetStaticIntField, jint,
     |v: Slot| Slot::I(v.as_i32()), |v: Slot| v.as_i32());
    (GetLongField, SetLongField, GetStaticLongField, SetStaticLongField, jlong,
     |v: Slot| Slot::L(v.as_i64()), |v: Slot| v.as_i64());
    (GetFloatField, SetFloatField, GetStaticFloatField, SetStaticFloatField, jfloat,
     |v: Slot| Slot::Fl(v.as_f32()), |v: Slot| v.as_f32());
    (GetDoubleField, SetDoubleField, GetStaticDoubleField, SetStaticDoubleField, jdouble,
     |v: Slot| Slot::F(v.as_f64()), |v: Slot| v.as_f64());
}
