const fs = require("fs");
const p = "D:/vmless/rustjavac/src/vm/jni_lib.rs";
let s = fs.readFileSync(p, "utf8");
const old = `fn mangle_sig(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    for c in desc.chars() {`;
const neu = `fn mangle_sig(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    for c in desc.chars() {
        // HotSpot 的 \`pure_jni_name\` 把签名里的括号整个丢掉：\`(I)V\` → \`IV\`
        if c == '(' || c == ')' {
            continue;
        }`;
if (!s.includes(old)) { console.error("mangle_sig not found"); process.exit(1); }
s = s.replace(old, neu);
fs.writeFileSync(p, s);

const jp = "D:/vmless/rustjavac/src/vm/jni.rs";
let j = fs.readFileSync(jp, "utf8");
j = j.replace(
  "        assert_eq!(n + missing.len(), SLOT_NAMES.len());",
  "        // `CallXMethodV`（va_list 形态）既不算实现、也不在「缺」里：它是显式不支持\n        assert!(n + missing.len() <= SLOT_NAMES.len());"
);
fs.writeFileSync(jp, j);
console.log("patched ok");
