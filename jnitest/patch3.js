const fs = require("fs");

// 1) va shim：xmm 用 movq 逐 8 字节存（否则 xmm1 会落在 xmm0 的高半部分上）
let j = fs.readFileSync("D:/vmless/rustjavac/src/vm/jni.rs", "utf8");
const oldXmm = [
  '                    "movaps xmmword ptr [rsp + 0x40], xmm0",',
  '                    "movaps xmmword ptr [rsp + 0x50], xmm1",',
  '                    "movaps xmmword ptr [rsp + 0x60], xmm2",',
  '                    "movaps xmmword ptr [rsp + 0x70], xmm3",',
].join("\n");
const newXmm = [
  "                    // 8 字节一个槽位（和 va_core 的 spill[4..8] 一一对应）",
  '                    "movq qword ptr [rsp + 0x40], xmm0",',
  '                    "movq qword ptr [rsp + 0x48], xmm1",',
  '                    "movq qword ptr [rsp + 0x50], xmm2",',
  '                    "movq qword ptr [rsp + 0x58], xmm3",',
].join("\n");
if (!j.includes(oldXmm)) { console.error("xmm spill not found"); process.exit(1); }
j = j.replace(oldXmm, newXmm);
fs.writeFileSync("D:/vmless/rustjavac/src/vm/jni.rs", j);

// 2) is_static 在调用时从类表判定（绑定时还没有 native 上下文，判不出来）
let l = fs.readFileSync("D:/vmless/rustjavac/src/vm/jni_lib.rs", "utf8");
const oldIs = [
  "    let vmp: *mut Vm = vm;",
  "    // JNI 的第一个参数永远是 JNIEnv*（Win64 上就是 rcx），native 方法本体从第二个开始",
  "    let env = jni::env_ptr();",
  "    args.push(env as usize as u64);",
  "    kinds.push(0);",
  "    let first = if t.is_static {",
].join("\n");
const newIs = [
  "    let vmp: *mut Vm = vm;",
  "    // 静态还是实例，**在调用时**从类表判定：绑定时（require_native 里）还没有 native",
  "    // 上下文，resolve_method 拿不到 flags，只能猜 —— 猜错的话静态 native 收到的",
  "    // jclass 会是 null（回调到静态方法时会静默失效）。",
  "    let is_static = {",
  "        const ACC_STATIC: u16 = 0x0008;",
  "        match (*vmp).store.method_in_class(&t.class, &t.name, &t.desc) {",
  "            Some((ci, mi)) => (*vmp).store.classes[ci].methods[mi].access_flags & ACC_STATIC != 0,",
  "            None => t.is_static,",
  "        }",
  "    };",
  "    // JNI 的第一个参数永远是 JNIEnv*（Win64 上就是 rcx），native 方法本体从第二个开始",
  "    let env = jni::env_ptr();",
  "    args.push(env as usize as u64);",
  "    kinds.push(0);",
  "    let first = if is_static {",
].join("\n");
if (!l.includes(oldIs)) { console.error("invoke prologue not found"); process.exit(1); }
l = l.replace(oldIs, newIs);
fs.writeFileSync("D:/vmless/rustjavac/src/vm/jni_lib.rs", l);
console.log("patched both");
