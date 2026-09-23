//! The TeX subset a Rill document can show (`specs/knowledge-math.md`).
//!
//! Wikipedia keeps formulas as TeX source (`specs/knowledge.md` §4.3), and
//! the document format has no math node. What it *does* have decides how
//! far a formula can get, and the deciding constraint is in the layout:
//! a wrapping row is a grid, not a text flow — "children keep their own
//! width and start a new line when the next one will not fit"
//! (`rill-ui/src/layout.rs`). Children are atomic, and a `text` node takes
//! no inline runs. So a prose paragraph broken into `[text, math, text]`
//! children would wrap at *fragment* boundaries instead of word ones.
//!
//! That draws the line this crate is built around:
//!
//! ```text
//! $$…$$  display, already its own paragraph  → structural nodes (later)
//! $…$    inline, inside flowing prose        → Unicode, one text node
//! neither parses                             → the TeX source, marked
//! ```
//!
//! Inline math stays inside the single `text` node that holds its
//! paragraph, so the paragraph keeps ordinary word wrap. Display math has
//! no surrounding prose and can afford a node tree.
//!
//! This crate is the shared half: [`segments`] splits a paragraph,
//! [`parse`] builds an [`Expr`], and [`to_unicode`] renders the subset
//! that needs only one dimension. The node emitter lives above it, in
//! presentation code, once corpus coverage says which constructs earn it.

/// One piece of a paragraph: prose, or a formula's TeX source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    /// `$…$` in flowing prose.
    Inline(&'a str),
    /// `$$…$$`, which the walker emits as a paragraph of its own.
    Display(&'a str),
}

/// A parsed formula. Deliberately small: every node here is something the
/// document format can show, either today through Unicode or later through
/// rows, columns and a `rect` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Row(Vec<Expr>),
    Num(String),
    /// A single variable letter. Italic by convention, which the format
    /// cannot express yet (`specs/knowledge-math.md` §5).
    Var(String),
    /// An operator, delimiter or named symbol, already as its character.
    Sym(String),
    /// `\text{…}` and friends: prose inside a formula, spaces kept.
    Text(String),
    Group(Box<Expr>),
    Frac(Box<Expr>, Box<Expr>),
    Sqrt(Box<Expr>),
    Script { base: Box<Expr>, sup: Option<Box<Expr>>, sub: Option<Box<Expr>> },
}

/// Longest `$…$` span considered for inline math. Past this a lone `$` is
/// far likelier to be a currency symbol whose partner is a later price.
pub const MAX_INLINE_TEX: usize = 256;

// ---------------------------------------------------------------- scanning

/// Split a paragraph into prose and formulas.
///
/// `$$…$$` is unambiguous — no currency amount doubles the sign. A single
/// `$` is not: "it cost $5 million and $3 billion" holds two of them with
/// ordinary prose between. [`looks_like_math`] is what separates the two,
/// and it is deliberately conservative: a missed formula renders as the
/// prose it already was, while a false positive would eat a sentence.
pub fn segments(text: &str) -> Vec<Segment<'_>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut plain) = (0usize, 0usize);
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"$$") {
            if let Some(rel) = find(&b[i + 2..], b"$$") {
                push_text(&mut out, &text[plain..i]);
                out.push(Segment::Display(&text[i + 2..i + 2 + rel]));
                i = i + 2 + rel + 2;
                plain = i;
            } else {
                i += 2;
            }
            continue;
        }
        match inline_end(text, i) {
            Some(end) => {
                push_text(&mut out, &text[plain..i]);
                out.push(Segment::Inline(&text[i + 1..end]));
                i = end + 1;
                plain = i;
            }
            None => i += 1,
        }
    }
    push_text(&mut out, &text[plain..]);
    out
}

/// The byte index of the `$` closing an inline formula opened at `open`.
fn inline_end(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let limit = (open + 1 + MAX_INLINE_TEX).min(b.len());
    let rel = b[open + 1..limit].iter().position(|&c| c == b'$')?;
    let end = open + 1 + rel;
    looks_like_math(&text[open + 1..end]).then_some(end)
}

/// Whether the text between two `$` is a formula rather than prose that
/// happens to sit between two currency symbols.
///
/// A control sequence, a script marker or a math operator settles it. With
/// none of those, only a very short alphanumeric run counts (`$x$`, `$ab$`)
/// — enough for a bare variable, too little for "5 million and ".
///
/// The ASCII hyphen is *not* a signal: "$50,000-$60,000" is a price range,
/// not a subtraction. `$a-b$` is missed as a result, and renders as the
/// source it already was.
pub fn looks_like_math(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_INLINE_TEX || s.contains('\n') {
        return false;
    }
    if s.contains('\\') || s.contains('^') || s.contains('_') {
        return true;
    }
    if s.chars().any(|c| "=+*/<>()[]|\u{2264}\u{2265}\u{2260}\u{00b1}\u{00d7}\u{00f7}\u{2212}".contains(c)) {
        return true;
    }
    s.chars().count() <= 3 && s.chars().all(char::is_alphanumeric)
}

fn push_text<'a>(out: &mut Vec<Segment<'a>>, s: &'a str) {
    if !s.is_empty() {
        out.push(Segment::Text(s));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ----------------------------------------------------------------- parsing

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Cmd(String),
    Char(char),
    Space,
    LBrace,
    RBrace,
    Sup,
    Sub,
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn next(&mut self) -> Option<Tok> {
        let v = self.t.get(self.i).cloned();
        if v.is_some() {
            self.i += 1;
        }
        v
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.i += 1;
            return true;
        }
        false
    }

    /// Math mode ignores source whitespace; `\,` and friends are the way to
    /// ask for a space, and they arrive as commands.
    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(Tok::Space)) {
            self.i += 1;
        }
    }
}

fn lex(tex: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut it = tex.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                let mut name = String::new();
                while let Some(&n) = it.peek() {
                    if n.is_ascii_alphabetic() {
                        name.push(n);
                        it.next();
                    } else {
                        break;
                    }
                }
                // `\,` `\;` `\!` `\%`: a control symbol is one character.
                if name.is_empty() && let Some(n) = it.next() {
                    name.push(n);
                }
                out.push(Tok::Cmd(name));
            }
            '{' => out.push(Tok::LBrace),
            '}' => out.push(Tok::RBrace),
            '^' => out.push(Tok::Sup),
            '_' => out.push(Tok::Sub),
            c if c.is_whitespace() => out.push(Tok::Space),
            c => out.push(Tok::Char(c)),
        }
    }
    out
}

/// Parse a formula's TeX source. `None` when anything in it falls outside
/// the subset — the caller then shows the source, never a half-rendering.
pub fn parse(tex: &str) -> Option<Expr> {
    let mut p = P { t: lex(tex), i: 0 };
    let e = seq(&mut p, false)?;
    p.skip_space();
    p.peek().is_none().then_some(e)
}

fn seq(p: &mut P, until_brace: bool) -> Option<Expr> {
    let mut items: Vec<Expr> = Vec::new();
    loop {
        p.skip_space();
        match p.peek() {
            None => {
                if until_brace {
                    return None; // unbalanced `{`
                }
                break;
            }
            Some(Tok::RBrace) => {
                if !until_brace {
                    return None; // unbalanced `}`
                }
                p.i += 1;
                break;
            }
            _ => {}
        }
        let base = atom(p)?;
        items.push(scripts(p, base)?);
    }
    Some(match items.len() {
        1 => items.pop()?,
        _ => Expr::Row(items),
    })
}

fn scripts(p: &mut P, base: Expr) -> Option<Expr> {
    let (mut sup, mut sub) = (None, None);
    loop {
        p.skip_space();
        if p.eat(&Tok::Sup) {
            if sup.is_some() {
                return None; // double superscript
            }
            sup = Some(Box::new(arg(p)?));
        } else if p.eat(&Tok::Sub) {
            if sub.is_some() {
                return None;
            }
            sub = Some(Box::new(arg(p)?));
        } else {
            break;
        }
    }
    Some(match (&sup, &sub) {
        (None, None) => base,
        _ => Expr::Script { base: Box::new(base), sup, sub },
    })
}

/// One argument: a braced group, or the single token after the command.
/// `\frac12` is 1/2, so a bare digit argument takes exactly one digit.
fn arg(p: &mut P) -> Option<Expr> {
    p.skip_space();
    if p.eat(&Tok::LBrace) {
        return seq(p, true).map(|e| Expr::Group(Box::new(e)));
    }
    match p.next()? {
        Tok::Char(c) if c.is_ascii_digit() => Some(Expr::Num(c.to_string())),
        Tok::Char(c) => Some(char_atom(c)),
        Tok::Cmd(name) => cmd(p, &name),
        _ => None,
    }
}

fn atom(p: &mut P) -> Option<Expr> {
    match p.next()? {
        Tok::LBrace => seq(p, true).map(|e| Expr::Group(Box::new(e))),
        Tok::RBrace | Tok::Sup | Tok::Sub | Tok::Space => None,
        Tok::Cmd(name) => cmd(p, &name),
        Tok::Char(c) if c.is_ascii_digit() => {
            let mut s = String::from(c);
            loop {
                let d = match p.peek() {
                    Some(Tok::Char(d)) if d.is_ascii_digit() || *d == '.' => *d,
                    _ => break,
                };
                s.push(d);
                p.i += 1;
            }
            Some(Expr::Num(s))
        }
        Tok::Char(c) => Some(char_atom(c)),
    }
}

fn char_atom(c: char) -> Expr {
    match c {
        c if c.is_alphabetic() => Expr::Var(c.to_string()),
        // The hyphen-minus in TeX source is a minus sign; U+2212 is what a
        // reader should see.
        '-' => Expr::Sym("\u{2212}".into()),
        '*' => Expr::Sym("\u{2217}".into()),
        c => Expr::Sym(c.to_string()),
    }
}

fn cmd(p: &mut P, name: &str) -> Option<Expr> {
    match name {
        "frac" | "dfrac" | "tfrac" => {
            let n = arg(p)?;
            let d = arg(p)?;
            Some(Expr::Frac(Box::new(n), Box::new(d)))
        }
        // `\sqrt[3]{x}` is a cube root; rendered as a square root it
        // would be a wrong formula shown confidently, so refuse it.
        "sqrt" => {
            p.skip_space();
            if p.peek() == Some(&Tok::Char('[')) {
                return None;
            }
            Some(Expr::Sqrt(Box::new(arg(p)?)))
        }
        "text" | "textrm" | "mathrm" | "mathbf" | "mathit" | "mbox" | "operatorname" => text_arg(p).map(Expr::Text),
        // Delimiters do not stretch here; `\left(` is just `(`.
        "left" | "right" => {
            p.skip_space();
            match p.next()? {
                Tok::Char('.') => Some(Expr::Sym(String::new())),
                Tok::Char(c) => Some(Expr::Sym(c.to_string())),
                Tok::Cmd(n) => lookup(&n).map(|s| Expr::Sym(s.to_string())),
                _ => None,
            }
        }
        "," | ":" | ";" | " " => Some(Expr::Sym(" ".into())),
        "!" => Some(Expr::Sym(String::new())),
        "quad" | "qquad" => Some(Expr::Sym("  ".into())),
        // Presentation directives with no bearing on what is shown here.
        "displaystyle" | "textstyle" | "limits" | "nolimits" | "left." | "right." => Some(Expr::Sym(String::new())),
        _ => lookup(name).map(|s| Expr::Sym(s.to_string())),
    }
}

/// `\text{…}`: the one place source spaces survive.
fn text_arg(p: &mut P) -> Option<String> {
    p.skip_space();
    if !p.eat(&Tok::LBrace) {
        return match p.next()? {
            Tok::Char(c) => Some(c.to_string()),
            _ => None,
        };
    }
    let mut s = String::new();
    let mut depth = 1usize;
    loop {
        match p.next()? {
            Tok::LBrace => depth += 1,
            Tok::RBrace => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Tok::Char(c) => s.push(c),
            Tok::Space => s.push(' '),
            Tok::Sup => s.push('^'),
            Tok::Sub => s.push('_'),
            Tok::Cmd(n) => s.push_str(lookup(&n)?),
        }
    }
    Some(s)
}

fn lookup(name: &str) -> Option<&'static str> {
    SYMBOLS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

// ----------------------------------------------------------------- Unicode

/// Render an expression as a single run of text, or `None` when it needs a
/// second dimension the format cannot give it inline.
pub fn to_unicode(e: &Expr) -> Option<String> {
    Some(match e {
        Expr::Row(items) => {
            let mut s = String::new();
            for i in items {
                s.push_str(&to_unicode(i)?);
            }
            s
        }
        Expr::Num(n) => n.clone(),
        Expr::Var(v) => v.clone(),
        Expr::Sym(s) | Expr::Text(s) => s.clone(),
        Expr::Group(g) => to_unicode(g)?,
        Expr::Sqrt(x) => format!("\u{221a}{}", parenthesised(x)?),
        Expr::Frac(n, d) => match vulgar(n, d) {
            Some(v) => v.to_string(),
            None => format!("{}/{}", parenthesised(n)?, parenthesised(d)?),
        },
        Expr::Script { base, sup, sub } => {
            let mut s = to_unicode(base)?;
            // Subscript first: x₁² is the conventional order.
            if let Some(sub) = sub {
                s.push_str(&shift(&to_unicode(sub)?, SUBSCRIPT)?);
            }
            if let Some(sup) = sup {
                s.push_str(&shift(&to_unicode(sup)?, SUPERSCRIPT)?);
            }
            s
        }
    })
}

/// Parse and render in one step: the inline tier.
pub fn inline(tex: &str) -> Option<String> {
    to_unicode(&parse(tex)?)
}

/// Every character lifted into the script table, or `None` if one is not
/// there — half a superscript is worse than none.
fn shift(s: &str, table: &[(char, char)]) -> Option<String> {
    s.chars().map(|c| table.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)).collect()
}

/// A compound operand needs brackets once it is flattened to one line:
/// `\sqrt{b^2-4ac}` is `\u{221a}(b\u{00b2}\u{2212}4ac)`, not `\u{221a}b\u{00b2}\u{2212}4ac`.
fn parenthesised(e: &Expr) -> Option<String> {
    let s = to_unicode(e)?;
    Some(if simple(e) { s } else { format!("({s})") })
}

fn simple(e: &Expr) -> bool {
    match e {
        Expr::Num(_) | Expr::Var(_) | Expr::Sym(_) | Expr::Script { .. } => true,
        Expr::Group(g) => simple(g),
        Expr::Row(items) => items.first().is_none_or(simple) && items.len() <= 1,
        _ => false,
    }
}

fn vulgar(n: &Expr, d: &Expr) -> Option<&'static str> {
    let digit = |e: &Expr| match e {
        Expr::Group(g) => match &**g {
            Expr::Num(n) if n.len() == 1 => Some(n.clone()),
            _ => None,
        },
        Expr::Num(n) if n.len() == 1 => Some(n.clone()),
        _ => None,
    };
    let (n, d) = (digit(n)?, digit(d)?);
    VULGAR.iter().find(|(a, b, _)| *a == n && *b == d).map(|(_, _, v)| *v)
}

const VULGAR: &[(&str, &str, &str)] = &[
    ("1", "2", "\u{00bd}"),
    ("1", "3", "\u{2153}"),
    ("2", "3", "\u{2154}"),
    ("1", "4", "\u{00bc}"),
    ("3", "4", "\u{00be}"),
    ("1", "5", "\u{2155}"),
    ("2", "5", "\u{2156}"),
    ("3", "5", "\u{2157}"),
    ("4", "5", "\u{2158}"),
    ("1", "6", "\u{2159}"),
    ("5", "6", "\u{215a}"),
    ("1", "8", "\u{215b}"),
    ("3", "8", "\u{215c}"),
    ("5", "8", "\u{215d}"),
    ("7", "8", "\u{215e}"),
];

const SUPERSCRIPT: &[(char, char)] = &[
    ('0', '\u{2070}'), ('1', '\u{00b9}'), ('2', '\u{00b2}'), ('3', '\u{00b3}'), ('4', '\u{2074}'), ('5', '\u{2075}'), ('6', '\u{2076}'), ('7', '\u{2077}'), ('8', '\u{2078}'), ('9', '\u{2079}'),
    ('+', '\u{207a}'), ('\u{2212}', '\u{207b}'), ('-', '\u{207b}'), ('=', '\u{207c}'), ('(', '\u{207d}'), (')', '\u{207e}'),
    ('a', '\u{1d43}'), ('b', '\u{1d47}'), ('c', '\u{1d9c}'), ('d', '\u{1d48}'), ('e', '\u{1d49}'), ('f', '\u{1da0}'), ('g', '\u{1d4d}'), ('h', '\u{02b0}'),
    ('i', '\u{2071}'), ('j', '\u{02b2}'), ('k', '\u{1d4f}'), ('l', '\u{02e1}'), ('m', '\u{1d50}'), ('n', '\u{207f}'), ('o', '\u{1d52}'), ('p', '\u{1d56}'),
    ('r', '\u{02b3}'), ('s', '\u{02e2}'), ('t', '\u{1d57}'), ('u', '\u{1d58}'), ('v', '\u{1d5b}'), ('w', '\u{02b7}'), ('x', '\u{02e3}'), ('y', '\u{02b8}'), ('z', '\u{1dbb}'),
];

const SUBSCRIPT: &[(char, char)] = &[
    ('0', '\u{2080}'), ('1', '\u{2081}'), ('2', '\u{2082}'), ('3', '\u{2083}'), ('4', '\u{2084}'), ('5', '\u{2085}'), ('6', '\u{2086}'), ('7', '\u{2087}'), ('8', '\u{2088}'), ('9', '\u{2089}'),
    ('+', '\u{208a}'), ('\u{2212}', '\u{208b}'), ('-', '\u{208b}'), ('=', '\u{208c}'), ('(', '\u{208d}'), (')', '\u{208e}'),
    ('a', '\u{2090}'), ('e', '\u{2091}'), ('h', '\u{2095}'), ('i', '\u{1d62}'), ('j', '\u{2c7c}'), ('k', '\u{2096}'), ('l', '\u{2097}'), ('m', '\u{2098}'),
    ('n', '\u{2099}'), ('o', '\u{2092}'), ('p', '\u{209a}'), ('r', '\u{1d63}'), ('s', '\u{209b}'), ('t', '\u{209c}'), ('u', '\u{1d64}'), ('v', '\u{1d65}'), ('x', '\u{2093}'),
];

/// Named symbols, as the character a reader should see. Greek first, then
/// relations, arrows, operators and the few letterlike constants Simple
/// English articles reach for.
const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "\u{3b1}"), ("beta", "\u{3b2}"), ("gamma", "\u{3b3}"), ("delta", "\u{3b4}"), ("epsilon", "\u{3b5}"), ("varepsilon", "\u{3b5}"), ("zeta", "\u{3b6}"),
    ("eta", "\u{3b7}"), ("theta", "\u{3b8}"), ("vartheta", "\u{3d1}"), ("iota", "\u{3b9}"), ("kappa", "\u{3ba}"), ("lambda", "\u{3bb}"), ("mu", "\u{3bc}"),
    ("nu", "\u{3bd}"), ("xi", "\u{3be}"), ("pi", "\u{3c0}"), ("varpi", "\u{3d6}"), ("rho", "\u{3c1}"), ("varrho", "\u{3f1}"), ("sigma", "\u{3c3}"),
    ("varsigma", "\u{3c2}"), ("tau", "\u{3c4}"), ("upsilon", "\u{3c5}"), ("phi", "\u{3c6}"), ("varphi", "\u{3d5}"), ("chi", "\u{3c7}"), ("psi", "\u{3c8}"), ("omega", "\u{3c9}"),
    ("Gamma", "\u{393}"), ("Delta", "\u{394}"), ("Theta", "\u{398}"), ("Lambda", "\u{39b}"), ("Xi", "\u{39e}"), ("Pi", "\u{3a0}"), ("Sigma", "\u{3a3}"),
    ("Upsilon", "\u{3a5}"), ("Phi", "\u{3a6}"), ("Psi", "\u{3a8}"), ("Omega", "\u{3a9}"),
    ("pm", "\u{b1}"), ("mp", "\u{2213}"), ("times", "\u{d7}"), ("div", "\u{f7}"), ("cdot", "\u{22c5}"), ("ast", "\u{2217}"), ("star", "\u{22c6}"), ("circ", "\u{2218}"),
    ("le", "\u{2264}"), ("leq", "\u{2264}"), ("ge", "\u{2265}"), ("geq", "\u{2265}"), ("ne", "\u{2260}"), ("neq", "\u{2260}"), ("equiv", "\u{2261}"),
    ("approx", "\u{2248}"), ("sim", "\u{223c}"), ("simeq", "\u{2243}"), ("cong", "\u{2245}"), ("propto", "\u{221d}"), ("ll", "\u{226a}"), ("gg", "\u{226b}"),
    ("to", "\u{2192}"), ("rightarrow", "\u{2192}"), ("leftarrow", "\u{2190}"), ("leftrightarrow", "\u{2194}"), ("Rightarrow", "\u{21d2}"), ("Leftarrow", "\u{21d0}"),
    ("Leftrightarrow", "\u{21d4}"), ("mapsto", "\u{21a6}"),
    ("infty", "\u{221e}"), ("partial", "\u{2202}"), ("nabla", "\u{2207}"), ("forall", "\u{2200}"), ("exists", "\u{2203}"), ("neg", "\u{ac}"),
    ("in", "\u{2208}"), ("notin", "\u{2209}"), ("subset", "\u{2282}"), ("supset", "\u{2283}"), ("subseteq", "\u{2286}"), ("supseteq", "\u{2287}"),
    ("cup", "\u{222a}"), ("cap", "\u{2229}"), ("emptyset", "\u{2205}"), ("setminus", "\u{2216}"),
    ("sum", "\u{2211}"), ("prod", "\u{220f}"), ("int", "\u{222b}"), ("oint", "\u{222e}"),
    ("angle", "\u{2220}"), ("perp", "\u{22a5}"), ("parallel", "\u{2225}"), ("deg", "\u{b0}"), ("degree", "\u{b0}"), ("prime", "\u{2032}"),
    ("ldots", "\u{2026}"), ("dots", "\u{2026}"), ("cdots", "\u{22ef}"),
    ("aleph", "\u{2135}"), ("hbar", "\u{210f}"), ("ell", "\u{2113}"), ("Re", "\u{211c}"), ("Im", "\u{2111}"),
    ("lbrace", "{"), ("rbrace", "}"), ("langle", "\u{27e8}"), ("rangle", "\u{27e9}"), ("vert", "|"), ("%", "%"), ("$", "$"), ("&", "&"), ("#", "#"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_prose_from_formulas() {
        let p = "If the sides are a and b, then $a^{2}+b^{2}=c^{2}$ holds.";
        assert_eq!(
            segments(p),
            vec![Segment::Text("If the sides are a and b, then "), Segment::Inline("a^{2}+b^{2}=c^{2}"), Segment::Text(" holds.")]
        );
        assert_eq!(segments("$$x=1$$"), vec![Segment::Display("x=1")]);
        assert_eq!(segments("no math here"), vec![Segment::Text("no math here")]);
    }

    #[test]
    fn currency_is_not_mistaken_for_math() {
        // The killer case: two prices in one sentence put prose between
        // two dollar signs.
        let p = "It cost $5 million and $3 billion in total.";
        assert_eq!(segments(p), vec![Segment::Text(p)], "prices must survive as prose");
        assert!(!looks_like_math("5 million and "));
        assert!(!looks_like_math("50,000-"), "a price range is not a subtraction");
        assert!(looks_like_math("x"));
        assert!(looks_like_math("a^2"));
        assert!(looks_like_math("\\alpha"));
        assert!(!looks_like_math(""));
    }

    #[test]
    fn renders_the_formulas_the_walker_keeps() {
        // Exactly the two the ingest test pins (rill-knowledge-build).
        assert_eq!(inline("a^{2}+b^{2}=c^{2}").unwrap(), "a\u{b2}+b\u{b2}=c\u{b2}");
        assert_eq!(inline(r"x={\frac {-b\pm {\sqrt {b^{2}-4ac}}}{2a}}").unwrap(), "x=(\u{2212}b\u{b1}\u{221a}(b\u{b2}\u{2212}4ac))/(2a)");
    }

    #[test]
    fn greek_operators_and_scripts() {
        assert_eq!(inline(r"\alpha > 0").unwrap(), "\u{3b1}>0");
        assert_eq!(inline(r"\Omega").unwrap(), "\u{3a9}");
        assert_eq!(inline(r"E = mc^2").unwrap(), "E=mc\u{b2}");
        assert_eq!(inline("x_1").unwrap(), "x\u{2081}");
        assert_eq!(inline("x_i^2").unwrap(), "x\u{1d62}\u{b2}");
        assert_eq!(inline(r"\frac{1}{2}").unwrap(), "\u{bd}");
        assert_eq!(inline(r"\frac{a}{b}").unwrap(), "a/b");
        assert_eq!(inline(r"\sqrt{2}").unwrap(), "\u{221a}2");
        assert_eq!(inline(r"\text{if } x > 0").unwrap(), "if x>0");
        assert_eq!(inline(r"\left( a + b \right)").unwrap(), "(a+b)");
        // Sum limits land beside the sign rather than above and below it,
        // which is the ordinary inline convention anyway.
        assert_eq!(inline(r"\sum_{i=1}^{n} i").unwrap(), "\u{2211}\u{1d62}\u{208c}\u{2081}\u{207f}i");
    }

    #[test]
    fn unsupported_input_is_refused_rather_than_half_rendered() {
        // A script no Unicode table covers: better the source than "x" with
        // the exponent silently dropped.
        assert_eq!(inline("x^{q}"), None);
        assert_eq!(inline(r"\begin{matrix} a & b \end{matrix}"), None);
        assert_eq!(parse("{unbalanced"), None);
        assert_eq!(parse("unbalanced}"), None);
        assert_eq!(inline(r"\unknowncommand"), None);
        // A root index has no inline form; rendering it as a square root
        // would be worse than showing the source.
        assert_eq!(inline(r"\sqrt[3]{x}"), None);
    }

    #[test]
    fn parse_shapes() {
        assert_eq!(parse("2"), Some(Expr::Num("2".into())));
        assert_eq!(parse("x"), Some(Expr::Var("x".into())));
        assert_eq!(parse(r"\frac12"), Some(Expr::Frac(Box::new(Expr::Num("1".into())), Box::new(Expr::Num("2".into())))), "a bare digit argument is one digit");
    }
}
