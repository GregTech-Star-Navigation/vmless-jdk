const fs = require("fs");
const p = "D:/vmless/rustjavac/src/vm/jni.rs";
let s = fs.readFileSync(p, "utf8");

const start = s.indexOf("pub(crate) fn find_class(name: &str) -> jclass {");
const end = s.indexOf("\njni!(GetObjectClass", start);
if (start < 0 || end < 0) { console.error("not found"); process.exit(1); }

const repl = `pub(crate) fn find_class(name: &str) -> jclass {
    // JNI 传的是内部名（\`java/lang/String\`），数组是 \`[I\` / \`[Ljava/lang/String;\`
    let internal = name;
    let (loaded, has_cp) = unsafe {
        with_vm(|vm| {
            let ok = if internal.starts_with('[') || internal.len() == 1 {
                true
            } else {
                vm.ensure_class_loaded(internal)
            };
            let has_cp = vm.sh.cp.lock().map(|g| g.is_some()).unwrap_or(false);
            (ok, has_cp)
        })
    }
    .unwrap_or((false, false));
    // 独立模式（classpath 上没有 java.base）下 java.* 是内建替身：它们没有 .class，
    // 但"类存在"这件事必须成立，否则 native 库连 \`FindClass("java/lang/RuntimeException")\`
    // 都失败。真字节码模式下找不到就是真找不到 —— 照规范报错。
    if !loaded && has_cp && !internal.starts_with('[') && internal.len() != 1 {
        let h = unsafe { with_vm(|vm| throw_new(vm, "java/lang/NoClassDefFoundError", internal)) }.unwrap_or(0);
        unsafe { set_pending(h) };
        return std::ptr::null_mut();
    }
    match unsafe { with_vm(|vm| vm.class_obj(internal)) } {
        Some(h) if h != 0 => h2p(h),
        _ => std::ptr::null_mut(),
    }
}

`;
s = s.slice(0, start) + repl + s.slice(end + 1);
fs.writeFileSync(p, s);
console.log("find_class patched");
