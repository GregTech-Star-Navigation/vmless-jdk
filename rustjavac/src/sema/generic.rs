//! 泛型签名（`Signature` 属性的原文）→ 调用点能代回的具体类型。
//!
//! 为什么需要它：擦除之后 `List<String>.get(int)` 的**字节码**返回
//! `Ljava/lang/Object;`，而源码里 `l.get(0).isEmpty()` 的接收者是 `String`。
//! 只看描述符，接收者就成了 `Object`，于是 `isEmpty` 报「未找到实例方法」——
//! java.base 里这一类占「未找到实例方法」桶的四成。
//!
//! 做法：类文件里那个 `Signature` 属性保留着擦除前的类型（`()TE;` 之类），
//! 解析成 [`SigTy`] 小树；调用点上把**接收者的类型实参**按声明类的类型参数位置
//! 代进去，就得到源码里的静态类型。
//!
//! 只做静态类型，不改变任何字节码 —— 擦除后的描述符仍然由类型表决定。

use crate::ast::Type;

/// 泛型签名里的一个类型
#[derive(Clone, Debug, PartialEq)]
pub enum SigTy {
    /// 基本类型 / `void`（描述符片段，如 `I`、`V`）
    Base(String),
    /// 类型变量（`E` / `T` / `K`）
    Var(String),
    /// 类或接口：内部名（已按 `$` 拼好）+ 类型实参
    Class(String, Vec<SigTy>),
    /// 数组（元素类型）
    Array(Box<SigTy>),
}

/// 方法签名：形参 + 返回。类型参数表只用来认出「返回的是方法自己的类型变量」，
/// 那种情况一律退化成 `Object`（调用点上拿不到，擦除也是这样）。
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MethodSig {
    pub params: Vec<SigTy>,
    pub ret: SigTy,
}

impl Default for SigTy {
    fn default() -> Self {
        SigTy::Base("V".to_string())
    }
}

impl SigTy {
    /// 这棵树里有没有类型变量（**不含**通配 `*` 落成的 Object）
    pub fn has_var(&self) -> bool {
        match self {
            SigTy::Var(_) => true,
            SigTy::Array(t) => t.has_var(),
            SigTy::Class(_, args) => args.iter().any(|a| a.has_var()),
            SigTy::Base(_) => false,
        }
    }
}

/// 解析器：在签名字节上顺序扫描
struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn new(s: &'a str) -> Self {
        P { b: s.as_bytes(), i: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// 读到这三个字符之一（不含）为止
    fn until(&mut self, stops: &[u8]) -> &'a str {
        let start = self.i;
        while let Some(c) = self.peek() {
            if stops.contains(&c) {
                break;
            }
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i]).unwrap_or("")
    }

    fn ident(&mut self) -> String {
        self.until(&[b':', b';', b'<', b'>', b'.', b'/']).to_string()
    }

    /// `<E:Ljava/lang/Object;>`
    ///
    /// 界可以是**空的**（`<T::Ljava/lang/Comparable<TT;>;>` 里第一个界就是空），
    /// 所以每次只吃 `:` 而不假定后面一定有类型；并且**每一轮都必须前进**，
    /// 否则遇到不认识的字符会死循环（编译整个 java.base 时就是卡在这里）。
    fn type_params(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.eat(b'<') {
            return out;
        }
        while let Some(c) = self.peek() {
            if c == b'>' {
                self.i += 1;
                break;
            }
            let start = self.i;
            let name = self.ident();
            if !name.is_empty() {
                out.push(name);
            }
            if self.eat(b':') && !matches!(self.peek(), Some(b'>') | Some(b':') | None) {
                self.skip_type();
            }
            while self.eat(b':') {
                if !matches!(self.peek(), Some(b'>') | Some(b':') | None) {
                    self.skip_type();
                }
            }
            if self.i == start {
                self.i += 1; // 兜底：绝不空转
            }
        }
        out
    }

    /// 跳过一段类型（`parse_type` 的丢弃版，用来掏界与 throws）
    fn skip_type(&mut self) {
        match self.peek() {
            Some(b'[') => {
                self.i += 1;
                self.skip_type()
            }
            Some(b'T') => {
                self.i += 1;
                self.until(&[b';']);
                self.eat(b';');
            }
            Some(b'L') => {
                let _ = self.parse_class();
            }
            Some(_) => self.i += 1,
            None => {}
        }
    }

    fn type_args(&mut self) -> Vec<SigTy> {
        let mut out = Vec::new();
        if !self.eat(b'<') {
            return out;
        }
        while let Some(c) = self.peek() {
            if c == b'>' {
                self.i += 1;
                break;
            }
            match c {
                // 无界通配 `*`：擦除后就是 Object
                b'*' => {
                    self.i += 1;
                    out.push(SigTy::Base("Ljava/lang/Object;".to_string()));
                }
                // `+T` / `-T`：协变/逆变界，静态类型取界本身
                b'+' | b'-' => {
                    self.i += 1;
                    out.push(self.parse_type());
                }
                _ => out.push(self.parse_type()),
            }
        }
        out
    }

    /// `Ljava/util/Map<TK;TV;>.Entry<TK;TV;>;` → 内部名 `java/util/Map$Entry`
    fn parse_class(&mut self) -> SigTy {
        self.eat(b'L');
        let first = self.until(&[b'<', b';', b'.']);
        let mut name = first.to_string();
        let mut args = self.type_args();
        while self.eat(b'.') {
            let seg = self.until(&[b'<', b';', b'.']);
            name.push('$');
            name.push_str(seg);
            let inner = self.type_args();
            // 里层实参才对应真名上的类型参数（`Outer<A>.Inner<B>` 的 B）
            if !inner.is_empty() {
                args = inner;
            }
        }
        self.eat(b';');
        SigTy::Class(name, args)
    }

    fn parse_type(&mut self) -> SigTy {
        match self.peek() {
            Some(b'[') => {
                self.i += 1;
                SigTy::Array(Box::new(self.parse_type()))
            }
            Some(b'T') => {
                self.i += 1;
                let n = self.until(&[b';']).to_string();
                self.eat(b';');
                SigTy::Var(n)
            }
            Some(b'L') => self.parse_class(),
            // `*` 出现在类型位置时同样当 Object
            Some(b'*') => {
                self.i += 1;
                SigTy::Base("Ljava/lang/Object;".to_string())
            }
            Some(c) => {
                self.i += 1;
                SigTy::Base((c as char).to_string())
            }
            None => SigTy::Base("V".to_string()),
        }
    }
}

/// 类的类型参数名：`<E:Ljava/lang/Object;>Ljava/lang/Object;…` → `["E"]`
pub fn class_tparams(sig: &str) -> Vec<String> {
    P::new(sig).type_params()
}

/// 方法签名：`<T:Ljava/lang/Object;>(TT;)TT;` → 形参/返回的 [`SigTy`]
pub fn method_sig(sig: &str) -> Option<MethodSig> {
    let mut p = P::new(sig);
    let _ = p.type_params();
    if !p.eat(b'(') {
        return None;
    }
    let mut params = Vec::new();
    while let Some(c) = p.peek() {
        if c == b')' {
            p.i += 1;
            break;
        }
        let before = p.i;
        params.push(p.parse_type());
        // 解析没前进就说明签名里有不认识的字符：放弃，别死循环
        if p.i == before {
            return None;
        }
    }
    let ret = if p.peek() == Some(b'V') {
        p.i += 1;
        SigTy::Base("V".to_string())
    } else {
        p.parse_type()
    };
    Some(MethodSig { params, ret })
}

/// 把签名里的类型变量按 `map` 代回，得到一个可直接用的 [`Type`]。
///
/// 代不回去的类型变量（方法自己的 `<U>`、或者调用点没类型实参）落成 `Object` ——
/// 那正是擦除后的样子，不会比现状更差。
pub fn subst(t: &SigTy, map: &dyn Fn(&str) -> Option<Type>) -> Type {
    match t {
        SigTy::Base(d) => crate::codegen::types::type_from_desc(d),
        SigTy::Var(n) => map(n).unwrap_or_else(object),
        SigTy::Array(inner) => Type::Array(Box::new(subst(inner, map))),
        SigTy::Class(name, args) => Type::Named {
            path: name.split('/').map(|s| s.to_string()).collect(),
            args: args.iter().map(|a| subst(a, map)).collect(),
        },
    }
}

fn object() -> Type {
    Type::Named { path: vec!["java".into(), "lang".into(), "Object".into()], args: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_method_sig() {
        // `List.get(int)`：`(I)TE;`
        let s = method_sig("(I)TE;").expect("parse");
        assert_eq!(s.params, vec![SigTy::Base("I".into())]);
        assert_eq!(s.ret, SigTy::Var("E".into()));
    }

    #[test]
    fn parses_map_entry_sig() {
        // `Map.entrySet()`：`()Ljava/util/Set<Ljava/util/Map$Entry<TK;TV;>;>;`
        let s = method_sig("()Ljava/util/Set<Ljava/util/Map$Entry<TK;TV;>;>;").expect("parse");
        match s.ret {
            SigTy::Class(name, args) => {
                assert_eq!(name, "java/util/Set");
                assert_eq!(args.len(), 1);
                match &args[0] {
                    SigTy::Class(inner, ia) => {
                        assert_eq!(inner, "java/util/Map$Entry");
                        assert_eq!(ia, &vec![SigTy::Var("K".into()), SigTy::Var("V".into())]);
                    }
                    other => panic!("内层不是类: {other:?}"),
                }
            }
            other => panic!("返回不是类: {other:?}"),
        }
    }

    #[test]
    fn parses_type_params_and_wildcards() {
        let s = method_sig("<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)TR;")
            .expect("parse");
        assert_eq!(s.params.len(), 1);
        assert_eq!(s.ret, SigTy::Var("R".into()));
        assert_eq!(class_tparams("<E:Ljava/lang/Object;>Ljava/lang/Object;"), vec!["E".to_string()]);
    }

    /// 回归：空界（`<T::L…;>`）+ 嵌套泛型界曾让解析器**死循环**，整个 java.base
    /// 编译卡在第一个带这种签名的类上。每轮必须前进是硬要求。
    #[test]
    fn empty_bound_terminates() {
        let tp = class_tparams("<T::Ljava/lang/Comparable<TT;>;>Ljava/lang/Object;");
        assert_eq!(tp, vec!["T".to_string()]);
        // 乱码也不能空转
        assert!(class_tparams("<>>>>>>>>>>>>>>>>").len() < 100);
        assert!(method_sig("<T:/((((((").is_none() || true);
    }

    #[test]
    fn substitutes_receiver_arg() {
        let s = method_sig("(I)TE;").expect("parse");
        let string = Type::Named { path: vec!["java".into(), "lang".into(), "String".into()], args: vec![] };
        let f = |n: &str| if n == "E" { Some(string.clone()) } else { None };
        assert_eq!(subst(&s.ret, &f), string);
    }
}
