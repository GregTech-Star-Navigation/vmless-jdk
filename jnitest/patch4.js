const fs = require("fs");
const p = "D:/vmless/rustjavac/src/vm/jni.rs";
let s = fs.readFileSync(p, "utf8");

// slot_fn 改成返回 Option<usize>：地址比较不可靠（LLVM 会把"返回 0"的相同函数合并）
const oldHead = `fn slot_fn(name: &str) -> usize {
    macro_rules! m {
        ($($n:literal => $f:ident),* $(,)?) => {
            match name {
                $($n => $f as *const () as usize,)*
                _ => unimplemented_slot as *const () as usize,
            }
        };
    }`;
const newHead = `fn slot_fn(name: &str) -> Option<usize> {
    macro_rules! m {
        ($($n:literal => $f:ident),* $(,)?) => {
            match name {
                $($n => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }`;
if (!s.includes(oldHead)) { console.error("slot_fn head not found"); process.exit(1); }
s = s.replace(oldHead, newHead);

// 表格构建 / 诊断改成按 Option 判
s = s.replace(
  `        for (i, name) in SLOT_NAMES.iter().enumerate() {
            t[i] = slot_fn(name);
        }`,
  `        for (i, name) in SLOT_NAMES.iter().enumerate() {
            t[i] = slot_fn(name).unwrap_or(unimplemented_slot as *const () as usize);
        }`
);

const oldImpl = `pub(crate) fn implemented_slots() -> usize {
    let mut n = 0;
    for name in SLOT_NAMES.iter() {
        let f = slot_fn(name);
        if f != unimplemented_slot as *const () as usize && f != va_list_unsupported as *const () as usize {
            n += 1;
        }
    }
    n
}`;
const newImpl = `pub(crate) fn implemented_slots() -> usize {
    let mut n = 0;
    for name in SLOT_NAMES.iter() {
        match slot_fn(name) {
            Some(f) if f != va_list_unsupported as *const () as usize => n += 1,
            _ => {}
        }
    }
    n
}`;
if (!s.includes(oldImpl)) { console.error("implemented_slots not found"); process.exit(1); }
s = s.replace(oldImpl, newImpl);

const oldMissing = `pub(crate) fn missing_slots() -> Vec<&'static str> {
    let mut v = Vec::new();
    for name in SLOT_NAMES.iter() {
        if slot_fn(name) == unimplemented_slot as *const () as usize {
            v.push(*name);
        }
    }
    v
}`;
const newMissing = `pub(crate) fn missing_slots() -> Vec<&'static str> {
    SLOT_NAMES.iter().filter(|n| slot_fn(n).is_none()).copied().collect()
}

/// 显式声明「不支持」的槽位（CallXMethodV 这些 va_list 形态）
pub(crate) fn unsupported_slots() -> Vec<&'static str> {
    SLOT_NAMES
        .iter()
        .filter(|n| matches!(slot_fn(n), Some(f) if f == va_list_unsupported as *const () as usize))
        .copied()
        .collect()
}`;
if (!s.includes(oldMissing)) { console.error("missing_slots not found"); process.exit(1); }
s = s.replace(oldMissing, newMissing);

fs.writeFileSync(p, s);
console.log("slot_fn -> Option applied");
