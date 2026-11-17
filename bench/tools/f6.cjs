const fs=require("fs");
let s=fs.readFileSync("summarize.mjs","utf8");
const anchor = "L.push('- 原始 `P2` 走 JIT 会**整体退回解释器**";
if(!s.includes(anchor)){console.log("MISS");process.exit(1);}
s=s.replace(anchor, `L.push('- **口径提醒**：`+"`run_jit`"+` 目前是「一处不支持就整个程序退回解释器」（`+"`invokedynamic` / `java.lang.invoke` / `synchronized`"+` 三种），');
L.push('  这个策略正在改（改成逐方法回退），所以**凡是有 lambda / 字符串拼接 / 同步块的程序，`run_jit` 的数字都偏低**。');
L.push('  本文的 JIT 数字来自 `P2j`（刻意避开这三样），同一形状在 `run_jdk`（真字节码解释路径）下的对照值见上一节。');
L.push('- 原始 `P2` 走 JIT 会**整体退回解释器**`);
fs.writeFileSync("summarize.mjs",s);
console.log("OK");
