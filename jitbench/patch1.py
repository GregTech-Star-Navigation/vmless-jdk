import io
p = 'src/jit/translate.rs'
s = io.open(p, encoding='utf-8').read()

subs = [
("""                0x15 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    stack.push(v);
                }""",
"""                0x15 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    stack.push(v);
                }"""),
("""                0x17 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F32, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x17 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F32);
                    stack.push(v);
                }"""),
("""                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F32, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F32);
                    stack.push(v);
                }"""),
("""                0x43..=0x46 => {
                    // fstore_0..3
                    let slot = (op - 0x43) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }""",
"""                0x43..=0x46 => {
                    // fstore_0..3
                    let slot = (op - 0x43) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::F32, v);
                }"""),
("""                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    stack.push(v);
                }""",
"""                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    stack.push(v);
                }"""),
("""                0x36 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_local(&mut fb, locals_ptr, slot, v);
                }""",
"""                0x36 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, v);
                }"""),
("""                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_local(&mut fb, locals_ptr, slot, v);
                }""",
"""                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, v);
                }"""),
("""                    let v = load_local(&mut fb, locals_ptr, slot);
                    let nv = fb.ins().iadd_imm(v, delta);
                    store_local(&mut fb, locals_ptr, slot, nv);""",
"""                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    let nv = fb.ins().iadd_imm(v, delta);
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, nv);"""),
("""                0x18 => {
                    // dload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x18 => {
                    // dload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F64);
                    stack.push(v);
                }"""),
("""                0x26..=0x29 => {
                    // dload_0..3
                    let slot = (op - 0x26) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x26..=0x29 => {
                    // dload_0..3
                    let slot = (op - 0x26) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F64);
                    stack.push(v);
                }"""),
("""                0x38 | 0x39 => {
                    // fstore(0x38) / dstore(0x39)：都只是把栈顶值按值存进 8 字节槽
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }""",
"""                0x38 | 0x39 => {
                    // fstore(0x38) / dstore(0x39)：都只是把栈顶值按值存进 8 字节槽
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    // 按**值本身的类型**存（不按操作码猜），与旧行为一致
                    let ty = fb.func.dfg.value_type(v);
                    lvars.store(&mut fb, locals_ptr, slot, ty, v);
                }"""),
("""                0x47..=0x4a => {
                    // dstore_0..3
                    let slot = (op - 0x47) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }""",
"""                0x47..=0x4a => {
                    // dstore_0..3
                    let slot = (op - 0x47) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::F64, v);
                }"""),
("""                0x16 => {
                    // lload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x16 => {
                    // lload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I64);
                    stack.push(v);
                }"""),
("""                0x1e..=0x21 => {
                    // lload_0..3
                    let slot = (op - 0x1e) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }""",
"""                0x1e..=0x21 => {
                    // lload_0..3
                    let slot = (op - 0x1e) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I64);
                    stack.push(v);
                }"""),
("""                0x37 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }""",
"""                0x37 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I64, v);
                }"""),
("""                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }""",
"""                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I64, v);
                }"""),
]
n = 0
for a, b in subs:
    if a not in s:
        print("MISS:", a.strip().splitlines()[0])
        continue
    s = s.replace(a, b, 1)
    n += 1
io.open(p, 'w', encoding='utf-8').write(s)
print("replaced", n, "of", len(subs))
