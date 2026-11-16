//! 方法体字节码发射器：字节缓冲、栈深跟踪、标签/回填、栈图帧、异常表、局部变量验证类型。

use crate::Attribute;
use super::constpool::CpBuilder;
use super::error::*;
use super::types::*;

/// 方法体字节码发射器（带栈深度跟踪）
pub(crate) struct Emit {
    pub(crate) bytes: Vec<u8>,
    pub(crate) depth: u16,
    pub(crate) max_depth: u16,
    pub(crate) max_locals: u16,
    /// 标签 id -> 代码偏移
    pub(crate) labels: Vec<Option<usize>>,
    /// (操作数位置, 标签 id)，收尾时回填
    pub(crate) patches: Vec<(usize, usize)>,
    /// 需要 StackMapTable 帧的位置、局部变量快照、以及**操作数栈**里那一个引用类型
    /// （`u16::MAX` = 空栈；否则是常量池 Class 索引 —— 只有异常 handler 入口才会非空）
    pub(crate) frames: Vec<(usize, Vec<u16>, u16)>,
    /// 异常表：(start 标签, end 标签, handler 标签, catch_type)
    /// catch_type = 0 表示「任意」（finally 的 catch-all），否则是常量池 Class 索引
    pub(crate) exc_table: Vec<(usize, usize, usize, u16)>,
    /// 常量池里 "StackMapTable" 的索引
    pub(crate) stack_map_name: u16,
    /// 每个局部变量槽的验证类型：0 = int，否则是类索引（对象/数组）
    pub(crate) local_types: Vec<u16>,
    /// 脏槽池（`SCRATCH_POOL_BASE .. +SCRATCH_POOL_LEN`）里哪些正被占用。
    /// 用位图而不是栈：兄弟表达式（`f(a<b, c<d)`）会同时占着，释放顺序不定。
    pub(crate) scratch_inuse: [bool; SCRATCH_POOL_LEN as usize],
}

impl Emit {
    pub(crate) fn new(max_locals: u16) -> Self {
        Emit {
            bytes: Vec::new(),
            depth: 0,
            max_depth: 0,
            max_locals,
            labels: Vec::new(),
            patches: Vec::new(),
            frames: Vec::new(),
            exc_table: Vec::new(),
            stack_map_name: 0,
            local_types: vec![u16::MAX; max_locals as usize],
            scratch_inuse: [false; SCRATCH_POOL_LEN as usize],
        }
    }

    /// 借一个脏槽（值先算进槽里，再把栈恢复成空）。
    ///
    /// 跳转点的栈图恒声明「栈为空」，所以任何会分叉的表达式都必须在空栈上开算；
    /// 前面已经压了东西（`println(…)` 的 PrintStream、`a + (b<c)` 的 a）时，
    /// 就先把待消费的东西算进脏槽，等分叉过去再取回来。
    pub(crate) fn alloc_scratch(&mut self) -> Result<u8, CompileError> {
        let i = match self.scratch_inuse.iter().position(|u| !*u) {
            Some(i) => i,
            None => {
                return err(format!(
                    "表达式嵌套过深：脏槽池只有 {SCRATCH_POOL_LEN} 个"
                ))
            }
        };
        self.scratch_inuse[i] = true;
        let slot = SCRATCH_POOL_BASE + i as u8;
        if self.max_locals < slot as u16 + 1 {
            self.max_locals = slot as u16 + 1;
        }
        Ok(slot)
    }

    pub(crate) fn free_scratch(&mut self, slot: u8) {
        if slot >= SCRATCH_POOL_BASE {
            let i = (slot - SCRATCH_POOL_BASE) as usize;
            if i < self.scratch_inuse.len() {
                self.scratch_inuse[i] = false;
            }
        }
    }

    /// 取一个槽当前的验证类型（借脏槽前存下来，还的时候恢复 —— 否则后续兄弟分支的
    /// 合并帧会声明一个「那条路径上根本没赋值」的类型，HotSpot 判 VerifyError）
    pub(crate) fn local_type_of(&self, slot: u8) -> u16 {
        self.local_types.get(slot as usize).copied().unwrap_or(u16::MAX)
    }

    /// 确保 `max_locals` 覆盖 `SCRATCH_SLOT`（只增不减）。
    ///
    /// `wide` 时槽里可能放 long/double：它占两个槽，而栈图里的 ITEM_Long 会被
    /// HotSpot 展开成两个，`max_locals` 少一个就报 ClassFormatError: bad type array size。
    pub(crate) fn reserve_scratch_slot(&mut self, wide: bool) {
        let need = SCRATCH_SLOT as u16 + 1 + if wide { 1 } else { 0 };
        if self.max_locals < need {
            self.max_locals = need;
        }
    }

    pub(crate) fn push(&mut self, n: u16) {
        self.depth += n;
        if self.depth > self.max_depth {
            self.max_depth = self.depth;
        }
    }

    pub(crate) fn pop(&mut self, n: u16) {
        self.depth = self.depth.saturating_sub(n);
    }

    pub(crate) fn op(&mut self, b: u8) {
        self.bytes.push(b);
    }

    pub(crate) fn op_u2(&mut self, b: u8, idx: u16) {
        self.bytes.push(b);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub(crate) fn iload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x1A + slot);
        } else {
            self.op(0x15);
            self.bytes.push(slot);
        }
        self.push(1);
        Ok(())
    }

    pub(crate) fn const_int(&mut self, v: i32) {
        match v {
            -1..=5 => self.op((0x03 + v) as u8),
            -128..=127 => {
                self.op(0x10);
                self.bytes.push(v as u8);
            }
            -32768..=32767 => {
                self.op(0x11);
                self.bytes.extend_from_slice(&(v as i16).to_be_bytes());
            }
            _ => {
                // 简化：大整数走 sipush 范围；超出直接截断
                self.op(0x11);
                self.bytes.extend_from_slice(&(v as i16).to_be_bytes());
            }
        }
        self.push(1);
    }

    pub(crate) fn istore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x3B + slot);
        } else {
            self.op(0x36);
            self.bytes.push(slot);
        }
        self.pop(1);
        Ok(())
    }

    /// long 的读取（lload）
    pub(crate) fn lload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x1E + slot); } else { self.op(0x16); self.bytes.push(slot); }
        self.push(2);
        Ok(())
    }

    /// long 的存储（lstore）
    pub(crate) fn lstore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x3F + slot); } else { self.op(0x37); self.bytes.push(slot); }
        self.pop(2);
        Ok(())
    }

    /// double 的读取（dload）
    pub(crate) fn dload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x26 + slot); } else { self.op(0x18); self.bytes.push(slot); }
        self.push(2);
        Ok(())
    }

    /// double 的存储（dstore）
    pub(crate) fn dstore(&mut self, slot: u8) -> Result<(), CompileError> {
        // 注意：0x38 是 fstore，dstore 是 0x39
        if slot <= 3 { self.op(0x47 + slot); } else { self.op(0x39); self.bytes.push(slot); }
        self.pop(2);
        Ok(())
    }

    /// float 的读取（fload）
    pub(crate) fn fload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x22 + slot); } else { self.op(0x17); self.bytes.push(slot); }
        self.push(1);
        Ok(())
    }

    /// float 的存储（fstore）
    pub(crate) fn fstore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x43 + slot); } else { self.op(0x38); self.bytes.push(slot); }
        self.pop(1);
        Ok(())
    }

    /// 压一个 float 常量：0.0/1.0/2.0 走 fconst_*，其余走 ldc_w（常量池 Float 项）
    pub(crate) fn const_float(&mut self, v: f32, cp: &mut CpBuilder) {
        match v {
            0.0 if !v.is_sign_negative() => self.op(0x0B), // fconst_0
            1.0 => self.op(0x0C),                          // fconst_1
            2.0 => self.op(0x0D),                          // fconst_2
            _ => {
                let idx = cp.float(v);
                self.op_u2(0x13, idx); // ldc_w
            }
        }
        self.push(1);
    }

    /// 按种类发射 load
    pub(crate) fn load_slot(&mut self, slot: u8, kind: u8) -> Result<(), CompileError> {
        match kind {
            K_LONG => self.lload(slot),
            K_DOUBLE => self.dload(slot),
            K_FLOAT => self.fload(slot),
            K_REF => self.aload(slot),
            _ => self.iload(slot),
        }
    }

    /// 按种类发射 store
    pub(crate) fn store_slot(&mut self, slot: u8, kind: u8) -> Result<(), CompileError> {
        match kind {
            K_LONG => self.lstore(slot),
            K_DOUBLE => self.dstore(slot),
            K_FLOAT => self.fstore(slot),
            K_REF => self.astore(slot),
            _ => self.istore(slot),
        }
    }

    /// 引用类型的存储（astore）
    pub(crate) fn astore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x4B + slot);
        } else {
            self.op(0x3A);
            self.bytes.push(slot);
        }
        self.pop(1);
        Ok(())
    }

    /// 引用类型的读取（aload）
    pub(crate) fn aload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x2A + slot);
        } else {
            self.op(0x19);
            self.bytes.push(slot);
        }
        self.push(1);
        Ok(())
    }

    pub(crate) fn new_label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    pub(crate) fn set_local_type(&mut self, slot: usize, ty: u16) {
        if slot >= self.local_types.len() {
            self.local_types.resize(slot + 1, u16::MAX);
        }
        self.local_types[slot] = ty;
    }

    pub(crate) fn bind(&mut self, label: usize) {
        self.bind_t(label, None);
    }

    /// 绑定标签，并可选地把脏槽（`SCRATCH_SLOT`）的验证类型单独写进这一帧。
    ///
    /// 脏槽必须先写后读，所以「跳到假分支」那一帧里它还是 Top（未赋值），
    /// 「汇合点」那一帧里才是 Integer —— 全局的 `local_types` 只能有一个值，
    /// 表达不了这种差别，只能在绑定这一帧时单独覆盖。
    pub(crate) fn bind_t(&mut self, label: usize, scratch_ty: Option<u16>) {
        let off = self.bytes.len();
        self.labels[label] = Some(off);
        let mut snap = self.local_types.clone();
        if let Some(t) = scratch_ty {
            let s = SCRATCH_SLOT as usize;
            if snap.len() <= s {
                snap.resize(s + 1, u16::MAX);
            }
            snap[s] = t;
        }
        self.frames.push((off, snap, u16::MAX));
    }

    /// 绑定一个异常 handler 的入口标签。
    ///
    /// 与普通标签的唯一区别：这一帧的**操作数栈里有一个异常对象**（JVM 要求）。
    /// locals 用调用方给的快照（try 入口处的那份）——handler 可能从保护区里任意
    /// 一条指令跳进来，声明得越少越安全（少声明的槽按 Top 处理，HotSpot 不追究）。
    pub(crate) fn bind_handler(&mut self, label: usize, locals: Vec<u16>, stack_ty: u16) {
        let off = self.bytes.len();
        self.labels[label] = Some(off);
        self.frames.push((off, locals, stack_ty));
    }

    /// 登记一条异常表项（标签在 `resolve` 之后再换算成偏移）
    pub(crate) fn exc(&mut self, start: usize, end: usize, handler: usize, catch_type: u16) {
        self.exc_table.push((start, end, handler, catch_type));
    }

    /// 只记下标签的代码偏移，**不**产生栈图帧。
    ///
    /// 异常表的「保护区结束」这类边界不是跳转目标，不该有帧：一旦和紧随其后的
    /// handler 入口落在同一偏移上，`resolve` 的 dedup 会把 handler 那份（栈上带一个
    /// 异常对象）丢掉，HotSpot 就报 "stack size doesn't match stackmap"。
    pub(crate) fn place(&mut self, label: usize) {
        self.labels[label] = Some(self.bytes.len());
    }

    pub(crate) fn branch(&mut self, opcode: u8, label: usize) {
        // 记录**操作码**位置：JVM 的跳转偏移是相对操作码地址的
        let pos = self.bytes.len();
        self.op(opcode);
        self.bytes.extend_from_slice(&[0, 0]);
        self.patches.push((pos, label));
    }

    pub(crate) fn goto(&mut self, label: usize) {
        self.branch(0xA7, label);
    }

    /// 把栈顶已经算好的比较结果用 `jump_false`（条件为假时跳）收尾成 0/1 压回栈上。
    ///
    /// 跳到 l_false 的是「假」分支（写 0），落空的是「真」分支（写 1）；
    /// 两个分支各自 `iconst` + `istore` 到脏槽，再在 L_end 汇合。
    ///
    /// 只在操作数栈为空的位置可用：本项目的 StackMapTable 一律声明「栈为 0」
    /// （`Emit::stack_map`），所以任何跳转点都不能压着别的值。
    pub(crate) fn bool_from_branch(&mut self, jump_false: u8) -> Result<(), CompileError> {
        if self.depth != 0 {
            return err("比较结果作为表达式时，前面不能有未消费的操作数（操作数栈必须为空）");
        }
        let s = SCRATCH_SLOT;
        if self.max_locals < s as u16 + 1 {
            self.max_locals = s as u16 + 1;
        }
        let l_false = self.new_label();
        let l_end = self.new_label();
        self.branch(jump_false, l_false);
        self.const_int(1);
        let _ = self.istore(s);
        self.goto(l_end);
        self.bind_t(l_false, Some(u16::MAX)); // 假分支上脏槽还没存过 → ITEM_Top
        self.const_int(0);
        let _ = self.istore(s);
        self.bind_t(l_end, Some(0)); // 汇合点上两条路径都存过 → ITEM_Integer
        let _ = self.iload(s);
        Ok(())
    }

    pub(crate) fn resolve(&mut self) -> Result<(), CompileError> {
        let patches = std::mem::take(&mut self.patches);
        for (pos, label) in patches {
            let target = self
                .labels
                .get(label)
                .and_then(|o| *o)
                .ok_or_else(|| CompileError { msg: format!("标签 {label} 未绑定") })?;
            let off = target as i32 - pos as i32;
            let off = i16::try_from(off).map_err(|_| CompileError { msg: "跳转距离超出 16 位".into() })?;
            self.bytes[pos + 1..pos + 3].copy_from_slice(&off.to_be_bytes());
        }
        self.frames.sort_unstable_by_key(|(o, _, _)| *o);
        self.frames.dedup_by_key(|(o, _, _)| *o);
        Ok(())
    }

    /// 生成 StackMapTable（全部用 full_frame）
    ///
    /// locals 数组里 **一个变量占一个条目**：long/double 只写一个
    /// ITEM_Long/ITEM_Double，紧跟的第二个槽由 HotSpot 自己补 Top
    /// （stackMapTable.cpp 的 full_frame 分支会按 is_category2 自行展开，
    /// 再多写一个 ITEM_Top 就会让展开后的槽数超出 max_locals，
    /// 报 ClassFormatError: bad type array size）。
    pub(crate) fn stack_map(&self) -> Option<Vec<u8>> {
        if self.frames.is_empty() {
            return None;
        }
        let mut info = Vec::new();
        info.extend_from_slice(&(self.frames.len() as u16).to_be_bytes());
        let mut prev: Option<usize> = None;
        for (off, snapshot, stack_ty) in &self.frames {
            let off = *off;
            let stack_ty = *stack_ty;
            let delta = match prev {
                None => off as u16,
                Some(p) => (off - p - 1) as u16,
            };
            prev = Some(off);
            let mut entries: Vec<u8> = Vec::new();
            let mut slot = 0usize;
            let mut n_entries = 0u16;
            while slot < self.max_locals as usize {
                let t = *snapshot.get(slot).unwrap_or(&u16::MAX);
                if t == 0xFFFE {
                    entries.push(4); // ITEM_Long，吃掉第 2 槽
                    slot += 2;
                } else if t == 0xFFFD {
                    entries.push(3); // ITEM_Double，吃掉第 2 槽
                    slot += 2;
                } else if t == 0xFFFC {
                    entries.push(2); // ITEM_Float，占 1 槽
                    slot += 1;
                } else if t == u16::MAX {
                    entries.push(0); // ITEM_Top（尚未初始化）
                    slot += 1;
                } else if t == 0 {
                    entries.push(1); // ITEM_Integer
                    slot += 1;
                } else {
                    entries.push(7); // ITEM_Object
                    entries.extend_from_slice(&t.to_be_bytes());
                    slot += 1;
                }
                n_entries += 1;
            }
            info.push(255); // full_frame
            info.extend_from_slice(&delta.to_be_bytes());
            info.extend_from_slice(&n_entries.to_be_bytes());
            info.extend_from_slice(&entries);
            // 操作数栈：默认空；异常 handler 入口只有一个异常对象
            if stack_ty == u16::MAX {
                info.extend_from_slice(&0u16.to_be_bytes());
            } else {
                info.extend_from_slice(&1u16.to_be_bytes());
                info.push(7); // ITEM_Object
                info.extend_from_slice(&stack_ty.to_be_bytes());
            }
        }
        Some(info)
    }

    pub(crate) fn to_attribute(&self, name_index: u16) -> Attribute {
        let mut info = Vec::new();
        info.extend_from_slice(&self.max_depth.to_be_bytes());
        info.extend_from_slice(&self.max_locals.to_be_bytes());
        info.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        info.extend_from_slice(&self.bytes);
        // 异常表：标签在这里才换算成代码偏移（`resolve` 之前标签是空的）
        info.extend_from_slice(&(self.exc_table.len() as u16).to_be_bytes());
        for (start, end, handler, catch_type) in &self.exc_table {
            for l in [start, end, handler] {
                let off = self
                    .labels
                    .get(*l)
                    .and_then(|o| *o)
                    .ok_or_else(|| CompileError { msg: format!("异常表的标签 {l} 未绑定") });
                // to_attribute 不能返回 Result：标签缺失按 0 写，靠单元测试与 java 校验暴露
                info.extend_from_slice(&(off.unwrap_or(0) as u16).to_be_bytes());
            }
            info.extend_from_slice(&catch_type.to_be_bytes());
        }
        match self.stack_map() {
            Some(sm) => {
                info.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
                info.extend_from_slice(&self.stack_map_name.to_be_bytes());
                info.extend_from_slice(&(sm.len() as u32).to_be_bytes());
                info.extend_from_slice(&sm);
            }
            None => info.extend_from_slice(&0u16.to_be_bytes()),
        }
        Attribute { name_index, info }
    }
}

/// 「比较 → 0/1」用的脏槽：放在局部变量表的最高固定位置。
/// 用前先用 `set_local_type(.., 0)` 声明成 int，`max_locals` 会跟着抬到 251。
pub(crate) const SCRATCH_SLOT: u8 = 250;

/// 临时脏槽池的起点与容量（240..249）：把「会分叉的子表达式」先算进槽里用。
/// 与 `SCRATCH_SLOT`（比较结果专用）分开，互不踩。
pub(crate) const SCRATCH_POOL_BASE: u8 = 240;
pub(crate) const SCRATCH_POOL_LEN: u8 = 10;
