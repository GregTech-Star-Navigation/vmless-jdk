macro_rules! field_ops {
    ($(($gf:ident, $sf:ident, $gsf:ident, $ssf:ident, $ty:ty, |$v:ident: Slot| $rd:expr, |$w:ident: $wk:ident| $mk:expr);)*) => {
        $(
            jni!($gf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let $v: Slot = get_field_slot(obj, fid);
                $rd
            });

            jni!($sf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let $w: $ty = val;
                set_field_slot(obj, fid, $mk);
            });

            jni!($gsf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let $v: Slot = get_static_slot(fid);
                $rd
            });

            jni!($ssf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let $w: $ty = val;
                set_static_slot(fid, $mk);
            });
        )*
    };
}

field_ops! {
    (GetObjectField, SetObjectField, GetStaticObjectField, SetStaticObjectField, jobject,
     |v: Slot| Slot::I(v.as_i32()),
     |w: jobject| Slot::I(w as usize as i32));
    (GetBooleanField, SetBooleanField, GetStaticBooleanField, SetStaticBooleanField, jboolean,
     |v: Slot| u8::from(v.as_i32() != 0),
     |w: jboolean| Slot::I(i32::from(w)));
    (GetByteField, SetByteField, GetStaticByteField, SetStaticByteField, jbyte,
     |v: Slot| v.as_i32() as i8,
     |w: jbyte| Slot::I(w as i32));
    (GetCharField, SetCharField, GetStaticCharField, SetStaticCharField, jchar,
     |v: Slot| v.as_i32() as u16,
     |w: jchar| Slot::I(w as i32));
    (GetShortField, SetShortField, GetStaticShortField, SetStaticShortField, jshort,
     |v: Slot| v.as_i32() as i16,
     |w: jshort| Slot::I(w as i32));
    (GetIntField, SetIntField, GetStaticIntField, SetStaticIntField, jint,
     |v: Slot| v.as_i32(),
     |w: jint| Slot::I(w));
    (GetLongField, SetLongField, GetStaticLongField, SetStaticLongField, jlong,
     |v: Slot| v.as_i64(),
     |w: jlong| Slot::L(w));
    (GetFloatField, SetFloatField, GetStaticFloatField, SetStaticFloatField, jfloat,
     |v: Slot| v.as_f32(),
     |w: jfloat| Slot::Fl(w));
    (GetDoubleField, SetDoubleField, GetStaticDoubleField, SetStaticDoubleField, jdouble,
     |v: Slot| v.as_f64(),
     |w: jdouble| Slot::F(w));
}
