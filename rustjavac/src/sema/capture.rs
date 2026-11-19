//! 匿名类的**捕获**（JLS 8.1.3 / 15.9.5）：外层局部变量（`val$`）与外层实例（`this$0`）。
//!
//! `new X(args){ … }` 在解析阶段被合成成一个嵌套类型 `Outer$AnonN`（见
//! `front::parser::expr`），`sema::nested::flatten` 再把它拍平成顶层类型。
//! 拍平之后，匿名类体是一个**独立的类**，看不见外层方法的局部变量与 `this` —— 所以要
//! 在拍平之后补上 javac 的那一步：
//!
//! 1. 合成类里加 `val$x` 字段（类型取自局部变量的声明类型），
//! 2. 构造器最前面加形参 `$c0…$cN`，字段初始化式写成 `val$x = $cN`（字段初始化式由
//!    `codegen::class` 在每个构造器里、`super(...)` 之后、按成员顺序执行，所以捕获
//!    字段先于用户自己写的字段 —— 顺序与 javac 一致，别的初始化式也看得到捕获值），
//! 3. 创建点 `new Outer$AnonN(a…)` 改成 `new Outer$AnonN(caps…, a…)`，
//! 4. 匿名类体里的 `Ident(x)` 改写成 `this.val$x`。
//!
//! **外层实例（`this$0`）**：体里用到直接外层的实例字段/实例方法，或写了 `Outer.this`
//! 时，合成类再加一个 `this$0` 字段 + 构造器第一个形参 `$this`，创建点把外层 `this`
//! 传进去；体里对应的名字改写成 `this.this$0.f` / `this.this$0.m(…)` / `this.this$0`。
//! 只支持**直接外层**（`A$Anon0` → A）：多层 `this$0` 与 `Outer.super.m()` 仍然明确报错，
//! 静态上下文里拿不到 `this` 也报错（都不猜）。
//!
//! 作用域栈按**程序顺序**走语句树：`Block` 压栈/出栈，`LocalVar` / `For` 初值 /
//! `ForEach` / `catch` 变量入栈。走到创建点时，`自由名 ∩ 活跃局部变量` 就是捕获集。
//!
//! **effectively final 是硬要求**：被捕获的局部变量只要在方法里被重新赋值过，就明确
//! 报错（javac 同样的措辞），绝不放宽 —— 放宽的后果是「编译通过、运行时读到旧值」，
//! 比明确失败难查得多。判定内层用的名字是不是外层的**实例成员**只看本编译单元里
//! 声明的类型（字段表/方法表）；继承来的成员看不到，宁可报错也不猜。
//! 用到时报明确的错（错误消息里带合成类名，能一眼看出是匿名类这一路）。

use crate::ast::*;
use std::collections::{HashMap, HashSet};

/// 合成字段/形参前缀。`val$x` 是 javac 的命名，留在字节码里便于与 javac 对照。
const CAP: &str = "val$";

/// 拍平之后、擦除之前跑一遍。`cu` 里的类型全是顶层类型（`Outer$AnonN` 也在其中）。
pub fn capture_unit(cu: &mut CompilationUnit) -> Result<(), String> {
    let mut fields = collect_fields(cu);
    // 「这个名字是类型，不是变量」：本单元声明的类型 + import 进来的简单名。
    // 只用于**避免误报**（`Foo.bar()` 里的 `Foo` 命中外层同名实例字段时，
    // 那是在用类名，不是在用字段）。
    let mut type_names: HashSet<String> = cu.types.iter().map(|t| t.name.clone()).collect();
    for imp in &cu.imports {
        if imp.is_static {
            continue;
        }
        if let Some((_, simple)) = imp.name.rsplit_once('.') {
            type_names.insert(simple.to_string());
        }
    }

    // 合成类先摘出来：创建点要改它的字段/构造器，改完还要再走一遍它的体
    // （匿名类里套匿名类 —— 内层的自由名可能指到外层的捕获字段）。
    let mut anon: HashMap<String, TypeDecl> = HashMap::new();
    let mut taken: Vec<(usize, String)> = Vec::new();
    for i in 0..cu.types.len() {
        if is_anon(&cu.types[i].name) {
            let td = std::mem::replace(&mut cu.types[i], placeholder());
            taken.push((i, td.name.clone()));
            anon.insert(td.name.clone(), td);
        }
    }
    // `this$0` 支持：要认出「这个名字是外层类的实例成员」，得先有全部类型的方法表
    // （字段表 `fields` 上面已经收集了，静态性也在里面）。
    let methods = collect_methods(cu);
    let mut outer: HashMap<String, String> = HashMap::new();

    let mut w = Cap {
        anon: &mut anon,
        fields: &mut fields,
        type_names: &type_names,
        scopes: Vec::new(),
        da: HashMap::new(),
        assigned: HashSet::new(),
        pending: Vec::new(),
        next_id: 0,
        class: String::new(),
        outer: &mut outer,
        methods: &methods,
        in_static: false,
    };
    for i in 0..cu.types.len() {
        if cu.types[i].name.is_empty() {
            continue; // 被摘出去的合成类的占位
        }
        let mut td = std::mem::replace(&mut cu.types[i], placeholder());
        let r = w.walk_type(&mut td);
        cu.types[i] = td;
        r?;
    }
    // 合成类放回原位：`cu.types` 的顺序保持不变（后面各阶段按源码顺序处理）
    for (i, name) in taken {
        if let Some(td) = w.anon.remove(&name) {
            cu.types[i] = td;
        }
    }
    Ok(())
}

fn placeholder() -> TypeDecl {
    TypeDecl {
        kind: TypeKind::Class,
        name: String::new(),
        modifiers: Vec::new(),
        type_params: Vec::new(),
        extends: Vec::new(),
        implements: Vec::new(),
        members: Vec::new(),
    }
}

/// 合成类的名字：`Outer$Anon0` / `Anon3`。取最后一段判 `Anon<数字>`。
fn is_anon(name: &str) -> bool {
    let last = name.rsplit('$').next().unwrap_or(name);
    match last.strip_prefix("Anon") {
        Some(d) => !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// 全部类型的字段表：类名 → (字段名 → (类型, 是否 static))。
fn collect_fields(cu: &CompilationUnit) -> HashMap<String, HashMap<String, (Type, bool)>> {
    let mut out = HashMap::new();
    for t in &cu.types {
        let mut m = HashMap::new();
        for mem in &t.members {
            if let Member::Field(f) = mem {
                let st = f.modifiers.iter().any(|x| x == "static");
                m.insert(f.name.clone(), (f.ty.clone(), st));
            }
        }
        out.insert(t.name.clone(), m);
    }
    out
}

/// 全部类型的方法表：类名 → (方法名 → 是否 static)
fn collect_methods(cu: &CompilationUnit) -> HashMap<String, HashMap<String, bool>> {
    let mut out = HashMap::new();
    for t in &cu.types {
        let mut m = HashMap::new();
        for mem in &t.members {
            if let Member::Method(md) = mem {
                let st = md.modifiers.iter().any(|x| x == "static");
                m.insert(md.name.clone(), st);
            }
        }
        out.insert(t.name.clone(), m);
    }
    out
}

// ---------------------------------------------------------------- 作用域栈
struct Local {
    name: String,
    ty: Type,
    /// 声明点的身份。同名变量在不同作用域里是**不同的声明**，重新赋值只污染自己那一个。
    id: u32,
}

struct Cap<'a> {
    /// 合成类（按名字），创建点上取出来改、改完放回去
    anon: &'a mut HashMap<String, TypeDecl>,
    /// 全部类型的字段表；给合成类加 `val$x` 时同步更新
    fields: &'a mut HashMap<String, HashMap<String, (Type, bool)>>,
    /// 本编译单元里「当类型用的名字」（避免把类名当外层实例字段误报）
    type_names: &'a HashSet<String>,
    /// 作用域栈：每个元素是一层作用域里声明的局部变量
    scopes: Vec<Vec<Local>>,
    /// 声明 id → **当前点是否已确定赋值**（JLS 16 的 DA）。effectively final 的判据是
    /// javac 那一条：**对「已确定赋值」的变量再赋值**才算重赋值。所以
    /// `int x; if (c) x = 1; else x = 2;`（每条路径只赋一次）仍然是 effectively final，
    /// 而 `int x = 1; x = 2;` / `x = 1; x = 2;` 不是。分支处按「任一路径有值即视为有值」
    /// 合并 —— 这一点的保守方向是**多报错**，不会把重赋值放过去。
    da: HashMap<u32, bool>,
    /// 「对已确定赋值的变量又赋了一次值」的声明 id
    assigned: HashSet<u32>,
    /// 本方法里被捕获的变量 (声明 id, 变量名, 合成类名) —— 方法走完统一验 effectively final
    pending: Vec<(u32, String, String)>,
    next_id: u32,
    /// 当前正在走的类型（创建点的外层类 = 匿名类的直接外层）
    class: String,
    /// 需要 `this$0` 的合成类：类名 → 直接外层类型
    outer: &'a mut HashMap<String, String>,
    /// 全部类型的方法表：类名 → (方法名 → 是否 static)
    methods: &'a HashMap<String, HashMap<String, bool>>,
    /// 当前在 static 上下文里（静态方法 / 静态初始化块）：`this` 不可用
    in_static: bool,
}

impl<'a> Cap<'a> {
    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// 在当前作用域声明一个局部变量，返回它的声明 id。
    /// `has_init`：声明处带初始化式（`int x = 1`）或本来就有值（形参 / catch / foreach）
    /// —— 这种变量一出生就是「已确定赋值」。
    fn declare(&mut self, name: &str, ty: Type, has_init: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.da.insert(id, has_init);
        let l = Local { name: name.to_string(), ty, id };
        match self.scopes.last_mut() {
            Some(top) => top.push(l),
            None => self.scopes.push(vec![l]),
        }
        id
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        for s in self.scopes.iter().rev() {
            for l in s.iter().rev() {
                if l.name == name {
                    return Some(l);
                }
            }
        }
        None
    }

    /// 一次赋值。只认活跃的声明：字段/类名不算。
    /// 对「已经确定赋值」的变量再赋值 → 它不是 final / effectively final。
    fn mark(&mut self, name: &str) {
        if let Some(id) = self.lookup(name).map(|l| l.id) {
            if *self.da.get(&id).unwrap_or(&false) {
                self.assigned.insert(id);
            } else {
                self.da.insert(id, true);
            }
        }
    }

    /// 在一份「当前 DA 状态」的副本上走一段代码，返回走完之后的状态（自己那份复原）。
    /// 分支 / 循环 / try / switch 靠它做 DA 合并。
    fn branch(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<HashMap<u32, bool>, String> {
        let snap = self.da.clone();
        f(self)?;
        Ok(std::mem::replace(&mut self.da, snap))
    }

    /// 把分支走完的 DA 状态并回当前状态：任一路径赋过值就算「有值」
    fn merge_da(&mut self, after: HashMap<u32, bool>) {
        for (k, v) in after {
            if v {
                self.da.insert(k, true);
            }
        }
    }

    /// 当前类（含外层链）里有没有这个字段
    fn chain_field(&self, name: &str) -> Option<(Type, bool)> {
        let mut cur = Some(self.class.clone());
        while let Some(c) = cur {
            if let Some(v) = self.fields.get(&c).and_then(|m| m.get(name)) {
                return Some(v.clone());
            }
            cur = c.rsplit_once('$').map(|(p, _)| p.to_string());
        }
        None
    }

    /// 这个名字是不是 `owner` 上**声明的实例字段**（不含继承）
    fn is_instance_field(&self, owner: &str, name: &str) -> bool {
        self.fields.get(owner).and_then(|m| m.get(name)).map_or(false, |(_, st)| !st)
    }

    /// 这个名字是不是 `owner` 上**声明的实例方法**
    fn is_instance_method(&self, owner: &str, name: &str) -> bool {
        self.methods.get(owner).and_then(|m| m.get(name)).copied().unwrap_or(false) == false
            && self.methods.get(owner).map_or(false, |m| m.contains_key(name))
    }

    /// 静态字段 / 静态方法：codegen 自己解析得到，不需要 this$0
    fn is_static_member(&self, owner: &str, name: &str) -> bool {
        self.fields.get(owner).and_then(|m| m.get(name)).map_or(false, |(_, st)| *st)
            || self.methods.get(owner).and_then(|m| m.get(name)).copied().unwrap_or(false)
    }

    // ------------------------------------------------------------ 类型 / 方法

    fn walk_type(&mut self, td: &mut TypeDecl) -> Result<(), String> {
        let saved = std::mem::replace(&mut self.class, td.name.clone());
        let saved_static = self.in_static;
        self.in_static = false;
        let mut err = None;
        for m in td.members.iter_mut() {
            let r = match m {
                Member::Field(f) => match &mut f.init {
                    Some(e) => self.scoped(|s| s.expr(e)),
                    None => Ok(()),
                },
                Member::Method(md) => match &mut md.body {
                    Some(b) => {
                        let ps: Vec<(String, Type)> =
                            md.params.iter().map(|p| (p.name.clone(), p.ty.clone())).collect();
                        let st = md.modifiers.iter().any(|x| x == "static");
                        let prev = self.in_static;
                        self.in_static = st;
                        let r = self.method_body(b, &ps);
                        self.in_static = prev;
                        r
                    }
                    None => Ok(()),
                },
                // 实例初始化块：codegen 目前不编（既有的缺口），但里面可能有创建点，
                // 照样走一遍 —— 不能因为「这段没人编」就漏掉里面的匿名类
                Member::Init { body, .. } => self.scoped(|s| s.stmt(body)),
                Member::EnumConst { args, .. } => self.scoped(|s| {
                    for a in args.iter_mut() {
                        s.expr(a)?;
                    }
                    Ok(())
                }),
                Member::Type(_) => Ok(()),
            };
            if let Err(e) = r {
                err = Some(e);
                break;
            }
        }
        self.in_static = saved_static;
        self.class = saved;
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 不带形参的独立代码区域（字段初始化式、实例初始化块）：用一份空作用域走
    fn scoped(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), String> {
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_pending = std::mem::take(&mut self.pending);
        self.scopes.push(Vec::new());
        let r = f(self).and_then(|_| self.check_pending());
        self.scopes = saved_scopes;
        self.pending = saved_pending;
        r
    }

    /// 一个方法体：形参入栈、走语句树、收口验 effectively final
    fn method_body(&mut self, body: &mut Stmt, params: &[(String, Type)]) -> Result<(), String> {
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_pending = std::mem::take(&mut self.pending);
        self.scopes.push(Vec::new());
        for (n, t) in params {
            self.declare(n, t.clone(), true);
        }
        let r = self.stmt(body).and_then(|_| self.check_pending());
        self.scopes = saved_scopes;
        self.pending = saved_pending;
        r
    }

    /// 被捕获的局部变量必须 final / effectively final
    fn check_pending(&mut self) -> Result<(), String> {
        for (id, name, cls) in std::mem::take(&mut self.pending) {
            if self.assigned.contains(&id) {
                return Err(format!(
                    "local variables referenced from an inner class must be final or effectively final: \
                     `{name}`（匿名类 `{cls}` 捕获它，但它在方法里被重新赋值过）"
                ));
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 语句

    fn stmt(&mut self, s: &mut Stmt) -> Result<(), String> {
        match s {
            Stmt::Block(items) => {
                self.push_scope();
                for it in items.iter_mut() {
                    self.stmt(it)?;
                }
                self.pop_scope();
            }
            Stmt::Labeled { body, .. } => self.stmt(body)?,
            Stmt::LocalVar { ty, name, init } => {
                // 字段类型要按初值把 `var` 补出来；但**不改 AST 里的声明类型**
                // （codegen 会自己推，换成 Object 反而更差）
                let t = cap_ty(ty, init.as_ref());
                self.declare(name, t, init.is_some());
                if let Some(e) = init {
                    self.expr(e)?;
                }
            }
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e)?,
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e)?;
                }
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond)?;
                let a = self.branch(|s| s.stmt(then))?;
                let b = match els {
                    Some(e) => self.branch(|s| s.stmt(e))?,
                    None => self.da.clone(),
                };
                self.merge_da(a);
                self.merge_da(b);
            }
            Stmt::While { cond, body } => {
                self.expr(cond)?;
                let after = self.branch(|s| s.stmt(body))?;
                self.merge_da(after);
            }
            Stmt::DoWhile { body, cond } => {
                let after = self.branch(|s| s.stmt(body))?;
                self.expr(cond)?;
                self.merge_da(after);
            }
            Stmt::For { init, cond, update, body } => {
                self.push_scope();
                for i in init.iter_mut() {
                    self.stmt(i)?;
                }
                if let Some(c) = cond {
                    self.expr(c)?;
                }
                for u in update.iter_mut() {
                    self.expr(u)?;
                }
                let after = self.branch(|s| s.stmt(body))?;
                self.merge_da(after);
                self.pop_scope();
            }
            Stmt::ForEach { ty, name, iter, body } => {
                self.expr(iter)?;
                self.push_scope();
                let t = cap_ty(ty, Some(iter));
                self.declare(name, t, true);
                let after = self.branch(|s| s.stmt(body))?;
                self.merge_da(after);
                self.pop_scope();
            }
            Stmt::Switch { selector, cases } => {
                self.expr(selector)?;
                self.push_scope();
                for c in cases.iter_mut() {
                    for l in c.labels.iter_mut().flatten() {
                        self.expr(l)?;
                    }
                    let body = &mut c.body;
                    let after = self.branch(|s| {
                        for it in body.iter_mut() {
                            s.stmt(it)?;
                        }
                        Ok(())
                    })?;
                    self.merge_da(after);
                }
                self.pop_scope();
            }
            Stmt::Yield(e) => self.expr(e)?,
            Stmt::Sync { lock, body } => {
                self.expr(lock)?;
                self.stmt(body)?;
            }
            Stmt::Try { body, catches, finally } => {
                let after = self.branch(|s| s.stmt(body))?;
                self.merge_da(after);
                for c in catches.iter_mut() {
                    self.push_scope();
                    let t = c.types.first().cloned().unwrap_or_else(object_ty);
                    let name = c.name.clone();
                    self.declare(&name, t, true);
                    let cb = &mut c.body;
                    let after = self.branch(|s| s.stmt(cb))?;
                    self.merge_da(after);
                    self.pop_scope();
                }
                if let Some(f) = finally {
                    self.stmt(f)?;
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 表达式

    fn expr(&mut self, e: &mut Expr) -> Result<(), String> {
        if let Expr::Ident(n) = e {
            // 局部变量/形参优先：同名局部变量遮蔽了捕获字段
            if self.lookup(n).is_some() {
                return Ok(());
            }
            let field = format!("{CAP}{n}");
            if self.fields.get(&self.class).map_or(false, |m| m.contains_key(&field)) {
                *e = Expr::Field(Box::new(Expr::This), field);
                return Ok(());
            }
            // 外层实例字段（`fd` 在 `FileInputStream` 里）→ `this.this$0.fd`
            if let Some(o) = self.outer.get(&self.class).cloned() {
                if self.is_instance_field(&o, n) && !self.is_instance_field(&self.class, n) {
                    *e = Expr::Field(Box::new(this_outer()), n.clone());
                }
            }
            return Ok(());
        }
        // `Outer.this` / 裸实例方法调用 —— 需要 this$0 的合成类里都要挂到外层实例上
        {
            let cur_class = self.class.clone();
            if let Some(o) = self.outer.get(&cur_class).cloned() {
                let mut rewrite_this = false;
                let mut outer_call = false;
                match &*e {
                    Expr::Field(recv, name) if name == "this" => {
                        let simple =
                            o.rsplit(|c| c == '$' || c == '/').next().unwrap_or("").to_string();
                        if let Expr::Ident(x) = &**recv {
                            if *x == simple || *x == o {
                                rewrite_this = true;
                            }
                        }
                    }
                    Expr::Call { recv: None, name, .. }
                        if !self.is_instance_method(&cur_class, name)
                            && self.is_instance_method(&o, name) => outer_call = true,
                    _ => {}
                }
                if rewrite_this {
                    *e = this_outer();
                    return Ok(());
                }
                if outer_call {
                    if let Expr::Call { recv, .. } = e {
                        *recv = Some(Box::new(this_outer()));
                    }
                }
            }
        }
        match e {
            Expr::New { ty, args } => {
                let name = match ty {
                    Type::Named { path, args: g } if path.len() == 1 && g.is_empty() => {
                        Some(path[0].clone())
                    }
                    _ => None,
                };
                // 实参先走一遍：里面可能有要改写的外层捕获字段
                for a in args.iter_mut() {
                    self.expr(a)?;
                }
                if let Some(n) = name {
                    self.anon_site(&n, args)?;
                }
            }
            Expr::ArrayNew { dims, .. } => {
                for d in dims.iter_mut() {
                    self.expr(d)?;
                }
            }
            Expr::ArrayInit(items) => {
                for i in items.iter_mut() {
                    self.expr(i)?;
                }
            }
            Expr::ArrayAccess { array, index } => {
                self.expr(array)?;
                self.expr(index)?;
            }
            Expr::Field(recv, _) => self.expr(recv)?,
            Expr::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    self.expr(r)?;
                }
                for a in args.iter_mut() {
                    self.expr(a)?;
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs)?;
                self.expr(rhs)?;
            }
            Expr::Unary { op, expr } => {
                if matches!(op, UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec) {
                    if let Expr::Ident(n) = &**expr {
                        let n = n.clone();
                        self.mark(&n);
                    }
                }
                self.expr(expr)?;
            }
            Expr::Assign { lhs, rhs, .. } => {
                if let Expr::Ident(n) = &**lhs {
                    let n = n.clone();
                    self.mark(&n);
                }
                self.expr(lhs)?;
                self.expr(rhs)?;
            }
            Expr::Cond { cond, then, els } => {
                self.expr(cond)?;
                self.expr(then)?;
                self.expr(els)?;
            }
            Expr::Cast { expr, .. } => self.expr(expr)?,
            Expr::InstanceOf { expr, ty, binding } => {
                self.expr(expr)?;
                // 模式变量只在 `instanceof` 为真的分支可见；这里退化成「声明在外面」，
                // 方向是**少捕获**（真的漏了，codegen 会明确报错，不会静默算错）
                if let Some(b) = binding {
                    let t = cap_ty(ty, None);
                    let b = b.clone();
                    self.declare(&b, t, true);
                }
            }
            Expr::Lambda { params, body } => {
                let ps = params.clone();
                self.push_scope();
                for p in ps {
                    self.declare(&p, object_ty(), true);
                }
                self.expr(body)?;
                self.pop_scope();
            }
            Expr::LambdaStmt { params, body } => {
                let ps = params.clone();
                self.push_scope();
                for p in ps {
                    self.declare(&p, object_ty(), true);
                }
                self.stmt(body)?;
                self.pop_scope();
            }
            Expr::MethodRef { recv, .. } => self.expr(recv)?,
            Expr::SwitchExpr(inner) => self.stmt(inner)?,
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ 匿名类创建点

    /// 走到 `new Outer$AnonN(args)`：定捕获集、改合成类、补实参
    fn anon_site(&mut self, ty_name: &str, args: &mut Vec<Expr>) -> Result<(), String> {
        if !self.anon.contains_key(ty_name) {
            return Ok(());
        }
        let fr = free_names(&self.anon[ty_name], self.type_names);
        let outer = enclosing_type(ty_name);
        // 外层实例成员：匿名类体里用到外层的实例字段/实例方法，或 `X.this` ——
        // javac 的做法是给合成类加一个 `this$0` 字段，创建点把外层 `this` 传进去。
        // 只支持**直接外层**（`A$Anon0` 里的 A）：再往上的外层要多层 this$0，
        // 那类写法在 java.base 里几乎没有，遇到就继续明确报错（不猜）。
        let mut need_outer = false;
        if let Some(o) = &outer {
            if fr.outer_this {
                need_outer = true;
            }
        } else if fr.outer_this {
            return Err(unsupported_outer_this(ty_name, "`X.this` / `X.super`"));
        }
        // 捕获集：自由名里、当前作用域下活跃的局部变量
        let mut caps: Vec<(String, Type)> = Vec::new();
        let mut args2: Vec<Expr> = Vec::new();
        for n in &fr.names {
            if let Some((id, ty)) = self.lookup(n).map(|l| (l.id, l.ty.clone())) {
                self.pending.push((id, n.clone(), ty_name.to_string()));
                caps.push((n.clone(), ty));
                args2.push(Expr::Ident(n.clone()));
                continue;
            }
            // 外层也是匿名类：值已经在它的 `val$n` 字段里，再往下传一层
            let cap_field = format!("{CAP}{n}");
            let outer_cap = self
                .fields
                .get(&self.class)
                .and_then(|m| m.get(&cap_field))
                .map(|(t, _)| t.clone());
            if let Some(ty) = outer_cap {
                caps.push((n.clone(), ty));
                args2.push(Expr::Field(Box::new(Expr::This), cap_field));
                continue;
            }
            if self.type_names.contains(n) {
                continue;
            }
            // 直接外层的**实例**成员 → 走 this$0（不再报错）
            if let Some(o) = &outer {
                if self.is_instance_field(o, n) || self.is_instance_method(o, n) {
                    need_outer = true;
                    continue;
                }
                // 静态成员：codegen 能按名字解析，不用 this$0
                if self.is_static_member(o, n) {
                    continue;
                }
            }
            // 更外层（祖父级）的实例字段：多层 this$0 不支持，仍明确报错
            if let Some((_, is_static)) = self.chain_field(n) {
                if !is_static {
                    return Err(unsupported_outer_this(
                        ty_name,
                        &format!("外层实例字段 `{n}`（不在直接外层 `{}` 上）", outer.as_deref().unwrap_or("?")),
                    ));
                }
            }
        }
        if need_outer && self.in_static {
            return Err(unsupported_outer_this(ty_name, "创建点在一个 static 上下文里，拿不到外层 this"));
        }

        let Some(mut td) = self.anon.remove(ty_name) else { return Ok(()) };
        if !caps.is_empty() {
            add_captures(&mut td, &caps)?;
            let fm = self.fields.entry(ty_name.to_string()).or_default();
            for (n, ty) in &caps {
                fm.insert(format!("{CAP}{n}"), (ty.clone(), false));
            }
        }
        if need_outer {
            let o = outer.clone().unwrap_or_default();
            add_outer_this(&mut td, &o)?;
            let fm = self.fields.entry(ty_name.to_string()).or_default();
            fm.insert("this$0".to_string(), (named_ty(&o), false));
            // 体里的改写（`Ident(外层字段)` / `X.this` / 裸调用）靠这张表
            self.outer.insert(ty_name.to_string(), o);
        }
        // 走合成类的体：改 `Ident(cap)` → `this.val$cap`，以及里面的嵌套匿名类创建点
        let r = self.walk_type(&mut td);
        self.outer.remove(ty_name);
        self.anon.insert(ty_name.to_string(), td);
        r?;

        let mut new_args: Vec<Expr> = Vec::new();
        if need_outer {
            new_args.push(Expr::This); // 参数顺序：$this, $c0…, 用户实参
        }
        new_args.extend(args2);
        if !new_args.is_empty() {
            new_args.extend(args.drain(..));
            *args = new_args;
        }
        Ok(())
    }
}

fn unsupported_outer_this(ty_name: &str, what: &str) -> String {
    format!(
        "匿名类暂时不支持捕获外层 this（合成类 `{ty_name}` 用了 {what}）：\
         把要用的值先赋给一个局部变量再捕获，或改用 lambda"
    )
}

/// `A$B$Anon0` → `A$B`（直接外层类型的二进制名）
fn enclosing_type(name: &str) -> Option<String> {
    name.rsplit_once('$').map(|(p, _)| p.to_string())
}

/// `this.this$0`（外层实例）
fn this_outer() -> Expr {
    Expr::Field(Box::new(Expr::This), "this$0".to_string())
}

fn named_ty(internal: &str) -> Type {
    Type::Named {
        path: internal.split('/').map(|s| s.to_string()).collect(),
        args: Vec::new(),
    }
}

/// 给合成类加 `this$0` 字段与构造器形参（形参在最前面，字段初始化式就是那个形参）
fn add_outer_this(td: &mut TypeDecl, outer: &str) -> Result<(), String> {
    let cname = td.name.clone();
    let ty = named_ty(outer);
    let mut found = false;
    for m in td.members.iter_mut() {
        if let Member::Method(md) = m {
            if md.ret.is_none() && md.name == cname {
                md.params.insert(
                    0,
                    Param { ty: ty.clone(), name: "$this".to_string(), varargs: false },
                );
                found = true;
                break;
            }
        }
    }
    if !found {
        return Err(format!("合成类 `{cname}` 找不到构造器（匿名类应有解析阶段生成的构造器）"));
    }
    td.members.insert(
        0,
        Member::Field(FieldDecl {
            modifiers: Vec::new(),
            ty,
            name: "this$0".to_string(),
            init: Some(Expr::Ident("$this".to_string())),
        }),
    );
    Ok(())
}

/// 给合成类加 `val$x` 字段 + 构造器形参（字段初始化式就是形参，见模块注释）
fn add_captures(td: &mut TypeDecl, caps: &[(String, Type)]) -> Result<(), String> {
    let cname = td.name.clone();
    // 构造器：解析阶段合成的那个（名字 == 类型名、没有返回类型）
    let mut ci = None;
    for (i, m) in td.members.iter().enumerate() {
        if let Member::Method(md) = m {
            if md.ret.is_none() && md.name == cname {
                ci = Some(i);
                break;
            }
        }
    }
    let Some(ci) = ci else {
        return Err(format!("合成类 `{cname}` 找不到构造器（匿名类应有解析阶段生成的构造器）"));
    };
    let mut params: Vec<Param> = Vec::with_capacity(caps.len() + 1);
    for (i, (_, ty)) in caps.iter().enumerate() {
        params.push(Param { ty: ty.clone(), name: format!("$c{i}"), varargs: false });
    }
    if let Member::Method(md) = &mut td.members[ci] {
        params.extend(md.params.drain(..));
        md.params = params;
    }
    // 字段放在最前面：字段初始化式的执行顺序就是成员顺序 —— 捕获字段必须先于
    // 用户写的字段（`new X(){ int v = outerLocal; }` 读到的必须是捕获值）
    for (i, (n, ty)) in caps.iter().enumerate().rev() {
        td.members.insert(
            0,
            Member::Field(FieldDecl {
                modifiers: Vec::new(),
                ty: ty.clone(),
                name: format!("{CAP}{n}"),
                init: Some(Expr::Ident(format!("$c{i}"))),
            }),
        );
    }
    Ok(())
}

// ------------------------------------------------------------ 自由名收集

struct Free {
    /// 匿名类体里用到、但自己没声明的名字（按首次出现排序）
    names: Vec<String>,
    /// 用了 `X.this` / `X.super`（外层 this，需要 this$0）
    outer_this: bool,
}

struct FreeWalk<'a> {
    own: HashSet<String>,
    type_names: &'a HashSet<String>,
    scopes: Vec<Vec<String>>,
    names: Vec<String>,
    seen: HashSet<String>,
    outer_this: bool,
}

impl<'a> FreeWalk<'a> {
    fn bound(&self, n: &str) -> bool {
        if self.own.contains(n) || self.type_names.contains(n) {
            return true;
        }
        self.scopes.iter().rev().any(|s| s.iter().any(|x| x == n))
    }
    fn add(&mut self, n: &str) {
        if self.bound(n) {
            return;
        }
        if self.seen.insert(n.to_string()) {
            self.names.push(n.to_string());
        }
    }
    fn push(&mut self) {
        self.scopes.push(Vec::new());
    }
    fn pop(&mut self) {
        self.scopes.pop();
    }
    fn declare(&mut self, n: &str) {
        match self.scopes.last_mut() {
            Some(s) => s.push(n.to_string()),
            None => self.scopes.push(vec![n.to_string()]),
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Block(items) => {
                self.push();
                for it in items {
                    self.stmt(it);
                }
                self.pop();
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::LocalVar { name, init, .. } => {
                self.declare(name);
                if let Some(e) = init {
                    self.expr(e);
                }
            }
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond);
                self.stmt(body);
            }
            Stmt::DoWhile { body, cond } => {
                self.stmt(body);
                self.expr(cond);
            }
            Stmt::For { init, cond, update, body } => {
                self.push();
                for i in init {
                    self.stmt(i);
                }
                if let Some(c) = cond {
                    self.expr(c);
                }
                for u in update {
                    self.expr(u);
                }
                self.stmt(body);
                self.pop();
            }
            Stmt::ForEach { name, iter, body, .. } => {
                self.expr(iter);
                self.push();
                self.declare(name);
                self.stmt(body);
                self.pop();
            }
            Stmt::Switch { selector, cases } => {
                self.expr(selector);
                self.push();
                for c in cases {
                    for l in c.labels.iter().flatten() {
                        self.expr(l);
                    }
                    for s in &c.body {
                        self.stmt(s);
                    }
                }
                self.pop();
            }
            Stmt::Yield(e) => self.expr(e),
            Stmt::Sync { lock, body } => {
                self.expr(lock);
                self.stmt(body);
            }
            Stmt::Try { body, catches, finally } => {
                self.stmt(body);
                for c in catches {
                    self.push();
                    self.declare(&c.name);
                    self.stmt(&c.body);
                    self.pop();
                }
                if let Some(f) = finally {
                    self.stmt(f);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Ident(n) => self.add(n),
            Expr::New { args, .. } => {
                for a in args {
                    self.expr(a);
                }
            }
            Expr::ArrayNew { dims, .. } => {
                for d in dims {
                    self.expr(d);
                }
            }
            Expr::ArrayInit(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            Expr::ArrayAccess { array, index } => {
                self.expr(array);
                self.expr(index);
            }
            Expr::Field(recv, name) => {
                if (name == "this" || name == "super") && matches!(**recv, Expr::Ident(_)) {
                    self.outer_this = true;
                }
                self.expr(recv);
            }
            Expr::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    self.expr(r);
                }
                for a in args {
                    self.expr(a);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Unary { expr, .. } => self.expr(expr),
            Expr::Assign { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Cond { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            Expr::Cast { expr, .. } => self.expr(expr),
            Expr::InstanceOf { expr, binding, .. } => {
                self.expr(expr);
                if let Some(b) = binding {
                    self.declare(b);
                }
            }
            Expr::Lambda { params, body } => {
                self.push();
                for p in params {
                    self.declare(p);
                }
                self.expr(body);
                self.pop();
            }
            Expr::LambdaStmt { params, body } => {
                self.push();
                for p in params {
                    self.declare(p);
                }
                self.stmt(body);
                self.pop();
            }
            Expr::MethodRef { recv, .. } => self.expr(recv),
            Expr::SwitchExpr(inner) => self.stmt(inner),
            _ => {}
        }
    }
}

fn free_names(td: &TypeDecl, type_names: &HashSet<String>) -> Free {
    let mut own = HashSet::new();
    for m in &td.members {
        if let Member::Field(f) = m {
            own.insert(f.name.clone());
        }
    }
    let mut w = FreeWalk {
        own,
        type_names,
        scopes: Vec::new(),
        names: Vec::new(),
        seen: HashSet::new(),
        outer_this: false,
    };
    for m in &td.members {
        match m {
            Member::Field(f) => {
                if let Some(e) = &f.init {
                    w.expr(e);
                }
            }
            Member::Method(md) => {
                w.push();
                for p in &md.params {
                    w.declare(&p.name);
                }
                if let Some(b) = &md.body {
                    w.stmt(b);
                }
                w.pop();
            }
            Member::Init { body, .. } => w.stmt(body),
            Member::EnumConst { args, .. } => {
                for a in args {
                    w.expr(a);
                }
            }
            Member::Type(_) => {}
        }
    }
    Free { names: w.names, outer_this: w.outer_this }
}

/// 捕获字段的类型：`var` 声明的局部变量要按初值把类型补出来
/// （`Type::Var` 的描述符是 `I`，直接当字段类型会编成 int 槽 —— 参见 codegen/stmt.rs）
fn cap_ty(ty: &Type, init: Option<&Expr>) -> Type {
    match ty {
        Type::Var => infer_var(init),
        t => t.clone(),
    }
}

fn infer_var(init: Option<&Expr>) -> Type {
    match init {
        Some(Expr::New { ty: Type::Var, .. }) | Some(Expr::Cast { ty: Type::Var, .. }) => object_ty(),
        Some(Expr::New { ty, .. }) => ty.clone(),
        Some(Expr::Cast { ty, .. }) => ty.clone(),
        Some(Expr::StrLit(_)) | Some(Expr::TextBlock(_)) => {
            Type::Named { path: vec!["String".into()], args: Vec::new() }
        }
        _ => object_ty(),
    }
}

fn object_ty() -> Type {
    Type::Named { path: vec!["Object".into()], args: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    fn run(src: &str) -> Result<CompilationUnit, String> {
        let cu = parse_compilation_unit(src).expect("parse");
        let mut cu = crate::sema::flatten_unit(&cu);
        capture_unit(&mut cu)?;
        Ok(cu)
    }

    fn anon_of<'a>(cu: &'a CompilationUnit, name: &str) -> &'a TypeDecl {
        cu.types.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("没有 {name}"))
    }

    #[test]
    fn captures_local_into_anon_field() {
        let cu = run(
            r#"
            class A {
                void m() {
                    final int id = 3;
                    Runnable r = new Runnable() {
                        public void run() { System.out.println(id); }
                    };
                    r.run();
                }
            }
        "#,
        )
        .expect("capture");
        let a = anon_of(&cu, "A$Anon0");
        let f = a
            .members
            .iter()
            .find_map(|m| match m {
                Member::Field(f) => Some(f),
                _ => None,
            })
            .expect("val$id 字段");
        assert_eq!(f.name, "val$id");
        assert_eq!(f.ty, Type::Prim("int"));
        let ctor = a
            .members
            .iter()
            .find_map(|m| match m {
                Member::Method(md) if md.ret.is_none() => Some(md),
                _ => None,
            })
            .expect("构造器");
        assert_eq!(ctor.params.len(), 1);
        assert_eq!(ctor.params[0].name, "$c0");
        // 体内 `id` 已经改写成 `this.val$id`
        let src = format!("{:?}", a.members);
        assert!(src.contains("val$id"), "体内应改写：{src}");
    }

    #[test]
    fn rejects_reassigned_local() {
        let err = run(
            r#"
            class A {
                void m() {
                    int n = 0;
                    Runnable r = new Runnable() {
                        public void run() { System.out.println(n); }
                    };
                    n = 5;
                    r.run();
                }
            }
        "#,
        )
        .expect_err("应报错");
        assert!(err.contains("effectively final"), "{err}");
    }

    /// `int x; if (c) x = 1; else x = 2;` —— 无初始化式、每条路径只赋一次：
    /// javac 认它是 effectively final（JLS 4.12.4），这里必须放行。
    #[test]
    fn allows_blank_local_assigned_once_per_path() {
        run(
            r#"
            class A {
                interface I { int f(); }
                void m(boolean c) {
                    int x;
                    if (c) x = 1; else x = 2;
                    I i = new I() { public int f() { return x; } };
                    System.out.println(i.f());
                }
            }
        "#,
        )
        .expect("无初始化式 + 分支各赋一次应当放行");
    }

    /// 但有初始化式之后再赋值就不是 effectively final 了
    #[test]
    fn rejects_init_then_assign() {
        let err = run(
            r#"
            class A {
                void m(boolean c) {
                    int x = 1;
                    if (c) x = 2;
                    Runnable r = new Runnable() { public void run() { System.out.println(x); } };
                    r.run();
                }
            }
        "#,
        )
        .expect_err("有初始化式再赋值应报错");
        assert!(err.contains("effectively final"), "{err}");
    }

    /// 依次赋两次（不是分支）也不是 effectively final
    #[test]
    fn rejects_two_assignments_in_sequence() {
        let err = run(
            r#"
            class A {
                void m() {
                    int x;
                    x = 1;
                    x = 2;
                    Runnable r = new Runnable() { public void run() { System.out.println(x); } };
                    r.run();
                }
            }
        "#,
        )
        .expect_err("顺序赋两次应报错");
        assert!(err.contains("effectively final"), "{err}");
    }

    #[test]
    fn synthesizes_outer_this_for_field_access() {
        // `count = 1` 在外层实例上：合成 `this$0` 字段 + 构造器形参，
        // 创建点补一个 `this`，体里改写成 `this.this$0.count = 1`
        let cu = run(
            r#"
            class A {
                int count;
                void m() {
                    Runnable r = new Runnable() {
                        public void run() { count = 1; }
                    };
                    r.run();
                }
            }
        "#,
        )
        .expect("应支持外层实例字段（this$0）");
        let anon = cu.types.iter().find(|t| t.name == "A$Anon0").expect("合成类");
        let has_this0 = anon.members.iter().any(|m| matches!(m, Member::Field(f) if f.name == "this$0"));
        assert!(has_this0, "合成类应有 this$0 字段");
        let Member::Method(md) = anon.members.iter().find(|m| matches!(m, Member::Method(md) if md.ret.is_none())).expect("构造器") else { panic!("构造器") };
        assert_eq!(md.params.first().map(|p| p.name.as_str()), Some("$this"), "构造器第一个形参应是 $this");
    }

    #[test]
    fn no_capture_when_shadowed_by_anon_field() {
        let cu = run(
            r#"
            class A {
                void m() {
                    final int id = 3;
                    Runnable r = new Runnable() {
                        int id = 9;
                        public void run() { System.out.println(id); }
                    };
                    r.run();
                }
            }
        "#,
        )
        .expect("capture");
        let a = anon_of(&cu, "A$Anon0");
        assert!(
            !a.members.iter().any(|m| matches!(m, Member::Field(f) if f.name.starts_with("val$"))),
            "遮蔽了就不该捕获"
        );
    }
}
