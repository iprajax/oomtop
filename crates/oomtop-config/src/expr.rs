//! A tiny, safe expression language for custom columns (UX §12.6):
//! `expr = "footprint / host.mem.total * 100"`.
//!
//! - Values are numbers (`f64`); comparisons and logic yield `1` / `0`.
//! - Numbers may carry units: bytes `2G`, `500M`, `1.5GiB`, `2GB` (uppercase first letter), durations `30m`,
//!   `5h`, `500ms`, `2d` (lowercase) → seconds.
//! - Operators (loosest first): `c ? a : b` · `||`/`or` · `&&`/`and` · `!`/`not` · `< <= > >= == !=` ·
//!   `+ -` · `* / %` · unary `-` · `^` (right-assoc).
//! - Functions: `min(a, b, …)`, `max(…)`, `abs(x)`, `round(x)`, `floor(x)`, `ceil(x)`, `clamp(x, lo, hi)`,
//!   `if(c, a, b)`, `coalesce(a, b, …)` (first available), `pct(a, b)` (= a / b × 100).
//! - Identifiers name entity fields (see [`crate::layout::ENTITY_FIELDS`]); dots allowed (`host.mem.total`).
//!
//! **Sandboxed by construction:** there is no I/O, no variables, no loops and no recursion beyond the parse
//! tree; input is limited to [`MAX_LEN`] bytes and [`MAX_DEPTH`] nesting. A field that is unavailable, a
//! division by zero or a non-finite result evaluates to `None` — rendered as "–", never as zero.

use crate::layered::closest;
use std::fmt;

pub const MAX_LEN: usize = 512;
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct ExprError {
    /// Byte offset into the expression.
    pub pos: usize,
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at column {}: {}", self.pos + 1, self.message)
    }
}

impl std::error::Error for ExprError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Min,
    Max,
    Abs,
    Round,
    Floor,
    Ceil,
    Clamp,
    If,
    Coalesce,
    Pct,
}

impl Func {
    fn parse(s: &str) -> Option<Func> {
        Some(match s {
            "min" => Func::Min,
            "max" => Func::Max,
            "abs" => Func::Abs,
            "round" => Func::Round,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            "clamp" => Func::Clamp,
            "if" => Func::If,
            "coalesce" => Func::Coalesce,
            "pct" => Func::Pct,
            _ => return None,
        })
    }
    fn arity(self) -> (usize, usize) {
        match self {
            Func::Min | Func::Max | Func::Coalesce => (1, 16),
            Func::Abs | Func::Round | Func::Floor | Func::Ceil => (1, 1),
            Func::Clamp | Func::If => (3, 3),
            Func::Pct => (2, 2),
        }
    }
}

pub const FUNCTIONS: &[&str] = &[
    "min", "max", "abs", "round", "floor", "ceil", "clamp", "if", "coalesce", "pct",
];

/// A compiled expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Field(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Question,
    Colon,
}

fn lex(src: &str) -> Result<Vec<(usize, Tok)>, ExprError> {
    let err = |pos, m: &str| ExprError {
        pos,
        message: m.to_string(),
    };
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            // exponent
            if i < b.len()
                && (b[i] == b'e' || b[i] == b'E')
                && b.get(i + 1)
                    .is_some_and(|d| d.is_ascii_digit() || *d == b'-' || *d == b'+')
                && !b.get(i + 1).is_some_and(|d| d.is_ascii_alphabetic())
            {
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            let num_end = i;
            while i < b.len() && (b[i] as char).is_ascii_alphabetic() {
                i += 1;
            }
            // compound durations: 1h30m, 2m30s
            if i > num_end && b[num_end].is_ascii_lowercase() {
                while i < b.len() && b[i].is_ascii_digit() {
                    let mut j = i;
                    while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                        j += 1;
                    }
                    let k = j;
                    while j < b.len() && b[j].is_ascii_lowercase() {
                        j += 1;
                    }
                    if j == k {
                        break;
                    }
                    i = j;
                }
            }
            let num: f64 = src[start..num_end]
                .replace('_', "")
                .parse()
                .map_err(|_| err(start, "bad number"))?;
            let unit = &src[num_end..i];
            let v = if unit.is_empty() {
                num
            } else if unit.starts_with(|c: char| c.is_ascii_uppercase()) {
                oomtop_core::units::parse_bytes(&src[start..i]).map_err(|_| {
                    err(
                        num_end,
                        &format!("unknown byte unit {unit:?} (K, M, G, T, KiB, GB…)"),
                    )
                })? as f64
            } else {
                oomtop_core::units::parse_duration_s(&src[start..i]).map_err(|_| {
                    err(
                        num_end,
                        &format!("unknown duration unit {unit:?} (ms, s, m, h, d)"),
                    )
                })?
            };
            out.push((start, Tok::Num(v)));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            let word = src[start..i].trim_end_matches('.');
            i = start + word.len();
            out.push((
                start,
                match word {
                    "and" => Tok::Op("&&"),
                    "or" => Tok::Op("||"),
                    "not" => Tok::Op("!"),
                    "true" => Tok::Num(1.0),
                    "false" => Tok::Num(0.0),
                    w => Tok::Ident(w.to_string()),
                },
            ));
            continue;
        }
        let two = src.get(i..i + 2).unwrap_or("");
        let tok = match two {
            "<=" | ">=" | "==" | "!=" | "&&" | "||" => {
                i += 2;
                Tok::Op(match two {
                    "<=" => "<=",
                    ">=" => ">=",
                    "==" => "==",
                    "!=" => "!=",
                    "&&" => "&&",
                    _ => "||",
                })
            }
            _ => {
                i += c.len_utf8();
                match c {
                    '+' => Tok::Op("+"),
                    '-' => Tok::Op("-"),
                    '*' => Tok::Op("*"),
                    '/' => Tok::Op("/"),
                    '%' => Tok::Op("%"),
                    '^' => Tok::Op("^"),
                    '<' => Tok::Op("<"),
                    '>' => Tok::Op(">"),
                    '!' => Tok::Op("!"),
                    '=' => Tok::Op("=="),
                    '(' => Tok::LParen,
                    ')' => Tok::RParen,
                    ',' => Tok::Comma,
                    '?' => Tok::Question,
                    ':' => Tok::Colon,
                    _ => return Err(err(start, &format!("unexpected character {c:?}"))),
                }
            }
        };
        out.push((start, tok));
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<(usize, Tok)>,
    i: usize,
    depth: usize,
    len: usize,
    fields: Option<&'a [&'a str]>,
}

impl Parser<'_> {
    fn pos(&self) -> usize {
        self.toks.get(self.i).map(|(p, _)| *p).unwrap_or(self.len)
    }
    fn err(&self, m: impl Into<String>) -> ExprError {
        ExprError {
            pos: self.pos(),
            message: m.into(),
        }
    }
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|(_, t)| t)
    }
    fn eat_op(&mut self, ops: &[&'static str]) -> Option<&'static str> {
        match self.peek() {
            Some(Tok::Op(o)) if ops.contains(o) => {
                let o = *o;
                self.i += 1;
                Some(o)
            }
            _ => None,
        }
    }
    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("expression nested too deeply"));
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr, ExprError> {
        self.enter()?;
        let c = self.or()?;
        let out = if self.peek() == Some(&Tok::Question) {
            self.i += 1;
            let a = self.expr()?;
            if self.peek() != Some(&Tok::Colon) {
                return Err(self.err("expected ':' in `cond ? a : b`"));
            }
            self.i += 1;
            let b = self.expr()?;
            Expr::Cond(Box::new(c), Box::new(a), Box::new(b))
        } else {
            c
        };
        self.depth -= 1;
        Ok(out)
    }
    fn or(&mut self) -> Result<Expr, ExprError> {
        let mut l = self.and()?;
        while self.eat_op(&["||"]).is_some() {
            l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, ExprError> {
        let mut l = self.not()?;
        while self.eat_op(&["&&"]).is_some() {
            l = Expr::Bin(BinOp::And, Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, ExprError> {
        if self.eat_op(&["!"]).is_some() {
            self.enter()?;
            let e = Expr::Not(Box::new(self.not()?));
            self.depth -= 1;
            return Ok(e);
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, ExprError> {
        let l = self.sum()?;
        if let Some(o) = self.eat_op(&["<", "<=", ">", ">=", "==", "!="]) {
            let op = match o {
                "<" => BinOp::Lt,
                "<=" => BinOp::Le,
                ">" => BinOp::Gt,
                ">=" => BinOp::Ge,
                "==" => BinOp::Eq,
                _ => BinOp::Ne,
            };
            return Ok(Expr::Bin(op, Box::new(l), Box::new(self.sum()?)));
        }
        Ok(l)
    }
    fn sum(&mut self) -> Result<Expr, ExprError> {
        let mut l = self.prod()?;
        while let Some(o) = self.eat_op(&["+", "-"]) {
            let op = if o == "+" { BinOp::Add } else { BinOp::Sub };
            l = Expr::Bin(op, Box::new(l), Box::new(self.prod()?));
        }
        Ok(l)
    }
    fn prod(&mut self) -> Result<Expr, ExprError> {
        let mut l = self.unary()?;
        while let Some(o) = self.eat_op(&["*", "/", "%"]) {
            let op = match o {
                "*" => BinOp::Mul,
                "/" => BinOp::Div,
                _ => BinOp::Rem,
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.unary()?));
        }
        Ok(l)
    }
    fn unary(&mut self) -> Result<Expr, ExprError> {
        if self.eat_op(&["-"]).is_some() {
            self.enter()?;
            let e = Expr::Neg(Box::new(self.unary()?));
            self.depth -= 1;
            return Ok(e);
        }
        if self.eat_op(&["+"]).is_some() {
            return self.unary();
        }
        self.pow()
    }
    fn pow(&mut self) -> Result<Expr, ExprError> {
        let base = self.atom()?;
        if self.eat_op(&["^"]).is_some() {
            self.enter()?;
            let e = Expr::Bin(BinOp::Pow, Box::new(base), Box::new(self.unary()?));
            self.depth -= 1;
            return Ok(e);
        }
        Ok(base)
    }
    fn atom(&mut self) -> Result<Expr, ExprError> {
        let pos = self.pos();
        let Some((_, t)) = self.toks.get(self.i).cloned() else {
            return Err(self.err("unexpected end of expression"));
        };
        self.i += 1;
        match t {
            Tok::Num(v) => Ok(Expr::Num(v)),
            Tok::LParen => {
                let e = self.expr()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(self.err("expected ')'"));
                }
                self.i += 1;
                Ok(e)
            }
            Tok::Ident(name) => {
                if self.peek() == Some(&Tok::LParen) {
                    let f = Func::parse(&name).ok_or_else(|| ExprError {
                        pos,
                        message: match closest(&name, FUNCTIONS.iter().copied()) {
                            Some(c) => format!("unknown function {name:?} — did you mean {c:?}?"),
                            None => format!("unknown function {name:?}"),
                        },
                    })?;
                    self.i += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.expr()?);
                            match self.peek() {
                                Some(Tok::Comma) => self.i += 1,
                                Some(Tok::RParen) => break,
                                _ => return Err(self.err("expected ',' or ')'")),
                            }
                        }
                    }
                    self.i += 1;
                    let (lo, hi) = f.arity();
                    if args.len() < lo || args.len() > hi {
                        return Err(ExprError {
                            pos,
                            message: format!(
                                "{name}() takes {lo}{} argument(s), got {}",
                                if hi > lo { "+" } else { "" },
                                args.len()
                            ),
                        });
                    }
                    return Ok(Expr::Call(f, args));
                }
                if let Some(known) = self.fields {
                    if !known.contains(&name.as_str()) {
                        return Err(ExprError {
                            pos,
                            message: match closest(&name, known.iter().copied()) {
                                Some(c) => format!("unknown field {name:?} — did you mean {c:?}?"),
                                None => format!("unknown field {name:?}"),
                            },
                        });
                    }
                }
                Ok(Expr::Field(name))
            }
            other => Err(ExprError {
                pos,
                message: format!("unexpected {}", tok_name(&other)),
            }),
        }
    }
}

fn tok_name(t: &Tok) -> String {
    match t {
        Tok::Num(_) => "number".into(),
        Tok::Ident(s) => format!("{s:?}"),
        Tok::Op(o) => format!("'{o}'"),
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::Comma => "','".into(),
        Tok::Question => "'?'".into(),
        Tok::Colon => "':'".into(),
    }
}

/// Parses an expression. With `fields`, unknown identifiers are rejected (with a suggestion).
pub fn compile(src: &str, fields: Option<&[&str]>) -> Result<Expr, ExprError> {
    if src.len() > MAX_LEN {
        return Err(ExprError {
            pos: MAX_LEN,
            message: format!("expression longer than {MAX_LEN} bytes"),
        });
    }
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(ExprError {
            pos: 0,
            message: "empty expression".into(),
        });
    }
    let mut p = Parser {
        toks,
        i: 0,
        depth: 0,
        len: src.len(),
        fields,
    };
    let e = p.expr()?;
    if p.i < p.toks.len() {
        return Err(p.err(format!("unexpected {}", tok_name(&p.toks[p.i].1))));
    }
    Ok(e)
}

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

fn truthy(v: f64) -> bool {
    v != 0.0
}

impl Expr {
    /// Evaluates with a field resolver (`None` = unavailable).
    pub fn eval(&self, field: &dyn Fn(&str) -> Option<f64>) -> Option<f64> {
        let b = |x: bool| if x { 1.0 } else { 0.0 };
        match self {
            Expr::Num(v) => Some(*v),
            Expr::Field(f) => field(f).and_then(finite),
            Expr::Neg(e) => e.eval(field).map(|v| -v),
            Expr::Not(e) => e.eval(field).map(|v| b(!truthy(v))),
            Expr::Cond(c, x, y) => {
                if truthy(c.eval(field)?) {
                    x.eval(field)
                } else {
                    y.eval(field)
                }
            }
            Expr::Bin(op, l, r) => {
                // short-circuit logic
                match op {
                    BinOp::And => {
                        let a = l.eval(field)?;
                        return if !truthy(a) {
                            Some(0.0)
                        } else {
                            r.eval(field).map(|v| b(truthy(v)))
                        };
                    }
                    BinOp::Or => {
                        let a = l.eval(field)?;
                        return if truthy(a) {
                            Some(1.0)
                        } else {
                            r.eval(field).map(|v| b(truthy(v)))
                        };
                    }
                    _ => {}
                }
                let (a, c) = (l.eval(field)?, r.eval(field)?);
                let v = match op {
                    BinOp::Add => a + c,
                    BinOp::Sub => a - c,
                    BinOp::Mul => a * c,
                    BinOp::Div => {
                        if c == 0.0 {
                            return None;
                        }
                        a / c
                    }
                    BinOp::Rem => {
                        if c == 0.0 {
                            return None;
                        }
                        a % c
                    }
                    BinOp::Pow => a.powf(c),
                    BinOp::Lt => b(a < c),
                    BinOp::Le => b(a <= c),
                    BinOp::Gt => b(a > c),
                    BinOp::Ge => b(a >= c),
                    BinOp::Eq => b((a - c).abs() < 1e-9),
                    BinOp::Ne => b((a - c).abs() >= 1e-9),
                    BinOp::And | BinOp::Or => unreachable!("handled above"),
                };
                finite(v)
            }
            Expr::Call(f, args) => {
                let all = || args.iter().map(|a| a.eval(field)).collect::<Option<Vec<f64>>>();
                let v = match f {
                    Func::Min => all()?.into_iter().fold(f64::INFINITY, f64::min),
                    Func::Max => all()?.into_iter().fold(f64::NEG_INFINITY, f64::max),
                    Func::Abs => args[0].eval(field)?.abs(),
                    Func::Round => args[0].eval(field)?.round(),
                    Func::Floor => args[0].eval(field)?.floor(),
                    Func::Ceil => args[0].eval(field)?.ceil(),
                    Func::Clamp => {
                        let (x, lo, hi) = (args[0].eval(field)?, args[1].eval(field)?, args[2].eval(field)?);
                        if lo > hi {
                            return None;
                        }
                        x.clamp(lo, hi)
                    }
                    Func::If => {
                        if truthy(args[0].eval(field)?) {
                            args[1].eval(field)?
                        } else {
                            args[2].eval(field)?
                        }
                    }
                    Func::Coalesce => return args.iter().find_map(|a| a.eval(field)),
                    Func::Pct => {
                        let (a, c) = (args[0].eval(field)?, args[1].eval(field)?);
                        if c == 0.0 {
                            return None;
                        }
                        a / c * 100.0
                    }
                };
                finite(v)
            }
        }
    }

    /// Field names referenced (sorted, unique).
    pub fn fields(&self) -> Vec<String> {
        fn walk(e: &Expr, out: &mut Vec<String>) {
            match e {
                Expr::Num(_) => {}
                Expr::Field(f) => out.push(f.clone()),
                Expr::Neg(x) | Expr::Not(x) => walk(x, out),
                Expr::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Expr::Cond(a, b, c) => {
                    walk(a, out);
                    walk(b, out);
                    walk(c, out);
                }
                Expr::Call(_, args) => args.iter().for_each(|a| walk(a, out)),
            }
        }
        let mut v = Vec::new();
        walk(self, &mut v);
        v.sort();
        v.dedup();
        v
    }
}

/// Validates a column format string: text with exactly one placeholder `{}`, `{:.N}`, `{:bytes}`, `{:dur}` or
/// `{:pct}`; `{{` / `}}` are literal braces.
pub fn check_format(fmt: &str) -> Result<(), String> {
    let mut count = 0;
    let mut rest = fmt;
    while let Some(i) = rest.find(['{', '}']) {
        let c = rest.as_bytes()[i];
        if rest[i..].starts_with("{{") || rest[i..].starts_with("}}") {
            rest = &rest[i + 2..];
            continue;
        }
        if c == b'}' {
            return Err("unmatched '}' (use '}}' for a literal brace)".into());
        }
        let end = rest[i..]
            .find('}')
            .ok_or_else(|| "unterminated '{'".to_string())?;
        let spec = &rest[i + 1..i + end];
        parse_spec(spec)?;
        count += 1;
        rest = &rest[i + end + 1..];
    }
    if count != 1 {
        return Err(format!(
            "format needs exactly one placeholder like {{:.0}}, found {count}"
        ));
    }
    Ok(())
}

enum Spec {
    Auto,
    Fixed(usize),
    Bytes,
    Dur,
    Pct,
}

fn parse_spec(spec: &str) -> Result<Spec, String> {
    match spec {
        "" | ":" => Ok(Spec::Auto),
        ":bytes" => Ok(Spec::Bytes),
        ":dur" => Ok(Spec::Dur),
        ":pct" => Ok(Spec::Pct),
        s => match s.strip_prefix(":.").and_then(|n| n.parse::<usize>().ok()) {
            Some(n) if n <= 6 => Ok(Spec::Fixed(n)),
            _ => Err(format!(
                "unknown format {{{s}}} (use {{}}, {{:.N}}, {{:bytes}}, {{:dur}} or {{:pct}})"
            )),
        },
    }
}

/// Renders a value with a column format. `None` renders as "–" (unavailable, never zero).
pub fn format_value(fmt: &str, v: Option<f64>) -> String {
    let render = |spec: &Spec| -> String {
        let Some(v) = v else { return "–".into() };
        match spec {
            Spec::Auto => {
                if (v - v.round()).abs() < 1e-9 && v.abs() < 1e15 {
                    format!("{}", v.round() as i64)
                } else {
                    let s = format!("{v:.2}");
                    s.trim_end_matches('0').trim_end_matches('.').to_string()
                }
            }
            Spec::Fixed(n) => format!("{v:.n$}"),
            Spec::Bytes => {
                if v < 0.0 {
                    format!("-{}", oomtop_core::units::format_bytes_short((-v) as u64))
                } else {
                    oomtop_core::units::format_bytes_short(v as u64)
                }
            }
            Spec::Dur => oomtop_core::units::format_duration(v.max(0.0) as u64),
            Spec::Pct => format!("{v:.0}%"),
        }
    };
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        if rest[i..].starts_with("{{") {
            out.push('{');
            rest = &rest[i + 2..];
            continue;
        }
        if rest[i..].starts_with("}}") {
            out.push('}');
            rest = &rest[i + 2..];
            continue;
        }
        match rest[i..].find('}') {
            Some(end) if rest.as_bytes()[i] == b'{' => {
                let spec = parse_spec(&rest[i + 1..i + end]).unwrap_or(Spec::Auto);
                out.push_str(&render(&spec));
                rest = &rest[i + end + 1..];
            }
            _ => {
                out.push_str(&rest[i..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env() -> HashMap<&'static str, Option<f64>> {
        HashMap::from([
            ("footprint", Some(2.0 * 1024.0 * 1024.0 * 1024.0)),
            ("host.mem.total", Some(24.0 * 1024.0 * 1024.0 * 1024.0)),
            ("cpu", Some(12.5)),
            ("gpu", None),
            ("idle_s", Some(7200.0)),
        ])
    }

    fn ev(src: &str) -> Option<f64> {
        let e = env();
        let fields: Vec<&str> = e.keys().copied().collect();
        compile(src, Some(&fields))
            .unwrap_or_else(|er| panic!("{src}: {er}"))
            .eval(&|f| e.get(f).copied().flatten())
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("1 + 2 * 3"), Some(7.0));
        assert_eq!(ev("(1 + 2) * 3"), Some(9.0));
        assert_eq!(ev("2 ^ 3 ^ 2"), Some(512.0));
        assert_eq!(ev("-2 ^ 2"), Some(-4.0));
        assert_eq!(ev("7 % 4"), Some(3.0));
        assert_eq!(ev("10 - 4 - 3"), Some(3.0));
        let share = ev("footprint / host.mem.total * 100").unwrap();
        assert!((share - 8.333).abs() < 0.01);
        assert_eq!(ev("footprint > 1G"), Some(1.0));
        assert_eq!(ev("footprint >= 2GiB and cpu < 50"), Some(1.0));
        assert_eq!(ev("idle_s > 1h30m"), Some(1.0));
        assert_eq!(ev("2GB"), Some(2e9));
        assert_eq!(ev("500ms"), Some(0.5));
        assert_eq!(ev("not (cpu > 50) || false"), Some(1.0));
        assert_eq!(ev("cpu > 10 ? 1 : 2"), Some(1.0));
        assert_eq!(ev("1.5e3"), Some(1500.0));
    }

    #[test]
    fn functions_and_unavailable() {
        assert_eq!(ev("max(1, cpu, 3)"), Some(12.5));
        assert_eq!(ev("min(4, 2, 9)"), Some(2.0));
        assert_eq!(ev("clamp(cpu, 0, 10)"), Some(10.0));
        assert_eq!(ev("round(2.5)"), Some(3.0));
        assert_eq!(ev("pct(cpu, 50)"), Some(25.0));
        assert_eq!(ev("gpu + 1"), None, "unavailable propagates");
        assert_eq!(ev("coalesce(gpu, 0)"), Some(0.0));
        assert_eq!(
            ev("if(cpu > 100, gpu, 5)"),
            Some(5.0),
            "only the chosen branch is evaluated"
        );
        assert_eq!(ev("1 / 0"), None);
        assert_eq!(ev("0 && gpu"), Some(0.0), "short-circuit");
    }

    #[test]
    fn errors_are_located_and_helpful() {
        let fields = ["footprint", "cpu"];
        let e = compile("footprnt / 2", Some(&fields)).unwrap_err();
        assert!(e.message.contains("did you mean \"footprint\"?"), "{e}");
        assert_eq!(e.pos, 0);
        let e = compile("cpu + ", Some(&fields)).unwrap_err();
        assert!(e.message.contains("unexpected end"), "{e}");
        let e = compile("mx(cpu)", Some(&fields)).unwrap_err();
        assert!(e.message.contains("did you mean \"max\"?"), "{e}");
        assert!(compile("abs(1, 2)", None).is_err());
        assert!(compile("cpu $ 2", None)
            .unwrap_err()
            .to_string()
            .starts_with("at column 5"));
        assert!(compile("(1", None).is_err());
        assert!(compile("1 2", None).is_err());
        assert!(compile("2Q", None).is_err());
        assert!(compile("", None).is_err());
        assert!(compile(&"(".repeat(40), None)
            .unwrap_err()
            .message
            .contains("deeply"));
        assert!(compile(&"1+".repeat(300), None)
            .unwrap_err()
            .message
            .contains("longer"));
        assert_eq!(
            compile("a.b + c", None).unwrap().fields(),
            vec!["a.b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn formats() {
        assert_eq!(format_value("{:.0}%", Some(8.33)), "8%");
        assert_eq!(format_value("{}", Some(3.0)), "3");
        assert_eq!(format_value("{}", Some(1.23456)), "1.23");
        assert_eq!(
            format_value("{:bytes}", Some(9.9 * 1024.0 * 1024.0 * 1024.0)),
            "9.9G"
        );
        assert_eq!(format_value("idle {:dur}", Some(13200.0)), "idle 3h40m");
        assert_eq!(format_value("{:pct}", Some(42.4)), "42%");
        assert_eq!(format_value("{{{}}}", Some(1.0)), "{1}");
        assert_eq!(format_value("{:.1} GB", None), "– GB");
        assert!(check_format("{:.0}%").is_ok());
        assert!(check_format("{:bytes}").is_ok());
        assert!(check_format("no placeholder").is_err());
        assert!(check_format("{} {}").is_err());
        assert!(check_format("{:x}").is_err());
        assert!(check_format("{").is_err());
        assert!(check_format("}").is_err());
    }
}
