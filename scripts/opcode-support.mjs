#!/usr/bin/env node
// 从源码抽出「实际支持哪些 JVM 指令」，输出 Markdown 表格（docs/支持矩阵.md 的指令表就是它生成的）。
//
//   node scripts/opcode-support.mjs            # 打印表格
//   node scripts/opcode-support.mjs --stats    # 只打印统计
//
// 口径：解释器看 rustjavac/src/vm/exec.rs，JIT 看 rustjavac/src/jit/translate.rs，
// 取各自的 `match op { ... }` 分支（含 `0x15..=0x19` 这种范围式分支），展开成 opcode 集合。
// 只是「分派里有这个分支」，不代表语义完全正确 —— 语义以 samples/ 的三路对照为准。

import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..');

// JVM 规范（JVMS §6）的 202 条指令，按 opcode 顺序
const NAMES = `nop aconst_null iconst_m1 iconst_0 iconst_1 iconst_2 iconst_3 iconst_4 iconst_5 lconst_0 lconst_1 fconst_0 fconst_1 fconst_2 dconst_0 dconst_1
bipush sipush ldc ldc_w ldc2_w iload lload fload dload aload iload_0 iload_1 iload_2 iload_3 lload_0 lload_1 lload_2 lload_3
fload_0 fload_1 fload_2 fload_3 dload_0 dload_1 dload_2 dload_3 aload_0 aload_1 aload_2 aload_3
iaload laload faload daload aaload baload caload saload
istore lstore fstore dstore astore istore_0 istore_1 istore_2 istore_3 lstore_0 lstore_1 lstore_2 lstore_3
fstore_0 fstore_1 fstore_2 fstore_3 dstore_0 dstore_1 dstore_2 dstore_3 astore_0 astore_1 astore_2 astore_3
iastore lastore fastore dastore aastore bastore castore sastore
pop pop2 dup dup_x1 dup_x2 dup2 dup2_x1 dup2_x2 swap
iadd ladd fadd dadd isub lsub fsub dsub imul lmul fmul dmul idiv ldiv fdiv ddiv irem lrem frem drem
ineg lneg fneg dneg ishl lshl ishr lshr iushr lushr iand land ior lor ixor lxor iinc
i2l i2f i2d l2i l2f l2d f2i f2l f2d d2i d2l d2f i2b i2c i2s
lcmp fcmpl fcmpg dcmpl dcmpg
ifeq ifne iflt ifge ifgt ifle if_icmpeq if_icmpne if_icmplt if_icmpge if_icmpgt if_icmple if_acmpeq if_acmpne goto jsr ret
tableswitch lookupswitch ireturn lreturn freturn dreturn areturn return
getstatic putstatic getfield putfield invokevirtual invokespecial invokestatic invokeinterface invokedynamic
new newarray anewarray arraylength athrow checkcast instanceof monitorenter monitorexit wide multianewarray
ifnull ifnonnull goto_w jsr_w`.split(/\s+/).filter(Boolean);

const GROUPS = [
  ['常量与栈', 0x00, 0x0f],
  ['常量与栈', 0x10, 0x14],
  ['局部变量', 0x15, 0x2d],
  ['数组访问', 0x2e, 0x35],
  ['局部变量', 0x36, 0x4e],
  ['数组访问', 0x4f, 0x56],
  ['常量与栈', 0x57, 0x5f],
  ['算术与位运算', 0x60, 0x84],
  ['类型转换', 0x85, 0x93],
  ['比较', 0x94, 0x98],
  ['跳转与分支', 0x99, 0xab],
  ['返回', 0xac, 0xb1],
  ['字段与调用', 0xb2, 0xba],
  ['对象与数组', 0xbb, 0xc1],
  ['同步', 0xc2, 0xc4],
  ['对象与数组', 0xc5, 0xc9],
];

// 只扫 `match op { … }` 块内部：块外的十六进制常量（掩码、标志位）不算指令
function opsOf(file) {
  const src = fs.readFileSync(file, 'utf8');
  const ops = new Set();
  let from = 0;
  for (;;) {
    const at = src.indexOf('match op {', from);
    if (at < 0) break;
    const open = src.indexOf('{', at);
    let depth = 0;
    let j = open;
    for (; j < src.length; j++) {
      if (src[j] === '{') depth++;
      else if (src[j] === '}') { depth--; if (depth === 0) break; }
    }
    for (const line of src.slice(open, j).split(/\r?\n/)) {
      if (!/^\s*0x[0-9a-fA-F]{2}/.test(line) || !line.includes('=>')) continue;
      const range = line.match(/0x([0-9a-fA-F]{2})\s*\.\.=\s*0x([0-9a-fA-F]{2})/);
      if (range) {
        for (let k = parseInt(range[1], 16); k <= parseInt(range[2], 16); k++) ops.add(k);
        continue;
      }
      for (const h of (line.split('=>')[0].match(/0x[0-9a-fA-F]{2}/g) || [])) ops.add(parseInt(h.slice(2), 16));
    }
    from = open + 1;
  }
  return ops;
}

const vmOps = opsOf(path.join(ROOT, 'rustjavac/src/vm/exec.rs'));
const jitOps = opsOf(path.join(ROOT, 'rustjavac/src/jit/translate.rs'));
const all = NAMES.map((_, i) => i);
const stats = {
  total: NAMES.length,
  vm: vmOps.size,
  jit: jitOps.size,
  both: all.filter(i => vmOps.has(i) && jitOps.has(i)).length,
  vmOnly: all.filter(i => vmOps.has(i) && !jitOps.has(i)),
  jitOnly: all.filter(i => !vmOps.has(i) && jitOps.has(i)),
  none: all.filter(i => !vmOps.has(i) && !jitOps.has(i)),
};
const nm = i => NAMES[i];

if (process.argv.includes('--stats')) {
  console.log(`总指令 ${stats.total} · 解释器 ${stats.vm} · JIT ${stats.jit} · 两边都有 ${stats.both}`);
  console.log(`只有解释器（${stats.vmOnly.length}）: ${stats.vmOnly.map(nm).join(' ')}`);
  console.log(`只有 JIT（${stats.jitOnly.length}）: ${stats.jitOnly.map(nm).join(' ')}`);
  console.log(`都没有（${stats.none.length}）: ${stats.none.map(nm).join(' ')}`);
  process.exit(0);
}

let out = '| # | opcode | 助记符 | 解释器 | JIT |\n|---|---|---|---|---|\n';
for (const [name, a, b] of GROUPS) {
  out += `| | | **${name}** | | |\n`;
  for (let i = a; i <= b; i++) {
    out += `| ${i} | \`0x${i.toString(16).padStart(2, '0')}\` | \`${nm(i)}\` | ${vmOps.has(i) ? '✅' : '❌'} | ${jitOps.has(i) ? '✅' : '❌'} |\n`;
  }
}
out += `\n**合计**：${stats.total} 条指令里，解释器实现 ${stats.vm} 条、JIT 实现 ${stats.jit} 条、两边都有 ${stats.both} 条。\n`;
process.stdout.write(out);
