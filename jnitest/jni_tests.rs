
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_table_matches_jni_h_ordering() {
        // 槽位顺序的自检：`SLOT_NAMES` 是从 JDK 的 `include/jni.h` 里按声明顺序抓的，
        // 这里钉住几个关键锚点（2008 年以来没变过的位置）—— 它们一错就会让 native
        // 库调到别的函数上，而且不会有任何报错。
        assert_eq!(SLOT_NAMES.len(), 236);
        assert_eq!(SLOT_NAMES[4], "GetVersion");
        assert_eq!(SLOT_NAMES[6], "FindClass");
        assert_eq!(SLOT_NAMES[21], "NewGlobalRef");
        assert_eq!(SLOT_NAMES[33], "GetMethodID");
        assert_eq!(SLOT_NAMES[49], "CallIntMethod");
        assert_eq!(SLOT_NAMES[94], "GetFieldID");
        assert_eq!(SLOT_NAMES[163], "NewString");
        assert_eq!(SLOT_NAMES[169], "GetStringUTFChars");
        assert_eq!(SLOT_NAMES[179], "NewIntArray");
        assert_eq!(SLOT_NAMES[187], "GetIntArrayElements");
        assert_eq!(SLOT_NAMES[195], "ReleaseIntArrayElements");
        assert_eq!(SLOT_NAMES[215], "RegisterNatives");
        assert_eq!(SLOT_NAMES[219], "GetJavaVM");
        assert_eq!(SLOT_NAMES[232], "GetObjectRefType");
    }

    #[test]
    fn table_has_no_null_slots() {
        // 空槽位会让 native 库在调用时直接跳到 0 —— 比"调到错的函数"更难查
        let t = SLOT_NAMES.len();
        let p = native_interface() as *const usize;
        for i in 0..t {
            let f = unsafe { *p.add(i) };
            assert_ne!(f, 0, "槽位 {i}（{}）是空指针", SLOT_NAMES[i]);
        }
    }

    #[test]
    fn reports_implemented_vs_missing_slots() {
        let n = implemented_slots();
        let missing = missing_slots();
        // 至少这几个是必须真实现的（验收项直接依赖它们）
        for must in [
            "FindClass",
            "GetMethodID",
            "NewGlobalRef",
            "DeleteGlobalRef",
            "ThrowNew",
            "ExceptionCheck",
            "NewStringUTF",
            "GetStringUTFChars",
            "GetIntArrayElements",
            "ReleaseIntArrayElements",
            "RegisterNatives",
            "GetJavaVM",
        ] {
            assert!(!missing.contains(&must), "{must} 没实现");
        }
        assert_eq!(n + missing.len(), SLOT_NAMES.len());
        println!("实现的槽位 {n} / {}，没实现 {} 个", SLOT_NAMES.len(), missing.len());
    }

    #[test]
    fn global_ref_table_returns_to_baseline() {
        // 句柄不会失效（没有 GC），但"建了没删"必须能被这张表量出来
        let base = global_ref_count();
        let fake = 42usize as jobject;
        let env = std::ptr::null_mut();
        for _ in 0..1000 {
            let g = unsafe { NewGlobalRef(env, fake) };
            assert_eq!(g, fake);
            unsafe { DeleteGlobalRef(env, g) };
        }
        assert_eq!(global_ref_count(), base, "全局引用表没有回到基线 = 泄漏");
        // 追加两次，确认计数确实是每次都涨（不是"表其实没记"）
        unsafe {
            NewGlobalRef(env, fake);
            NewGlobalRef(env, fake);
        }
        assert_eq!(global_ref_count(), base + 2);
        unsafe {
            DeleteGlobalRef(env, fake);
            DeleteGlobalRef(env, fake);
        }
        assert_eq!(global_ref_count(), base);
    }

    // ---- 调用垫片（Win64 调用约定） ----

    extern "C" fn c_add(a: i64, b: i64) -> i64 {
        a + b
    }
    extern "C" fn c_mix(a: i32, x: f32, y: f64, b: i64) -> f64 {
        a as f64 + x as f64 + y + b as f64
    }
    extern "C" fn c_many(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64) -> i64 {
        a + b + c + d + e + f + g
    }

    #[test]
    fn calls_c_with_integer_args() {
        let (rax, _) = unsafe { call_native_ffi(c_add as usize, &[2, 3], &[0, 0]) };
        assert_eq!(rax as i64, 5);
    }

    #[test]
    fn calls_c_with_mixed_float_args() {
        // 浮点参数必须进 xmm —— 这条是 GLFW 的 cursorPos 回调（两个 jdouble）的前提
        let (_, xmm) = unsafe {
            call_native_ffi(
                c_mix as usize,
                &[1, (1.5f32).to_bits() as u64, (0.25f64).to_bits(), 10],
                &[0, 1, 2, 0],
            )
        };
        let r = f64::from_bits(xmm);
        assert!((r - 12.75).abs() < 1e-9, "得到 {r}");
    }

    #[test]
    fn calls_c_with_stack_args() {
        let (rax, _) = unsafe { call_native_ffi(c_many as usize, &[1, 2, 3, 4, 5, 6, 7], &[0; 7]) };
        assert_eq!(rax as i64, 28);
    }
}
