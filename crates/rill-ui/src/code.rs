//! The code surface's highlighter: comments, strings, numbers, keywords —
//! the four classes that carry most of what colour does for code. Written
//! rather than imported on purpose: a grammar engine is a heavy dependency
//! for an 80%-of-the-value job, and these classes are stable across every
//! language a config-editing desktop meets.
//!
//! Classes map to *theme tokens*, not colours: keywords wear the accent,
//! strings the terminal's green, comments the muted text. Code dressed by
//! the rice, and re-themed with it — the same move the terminal palette
//! made. A theme that names no ANSI colours degrades to plain text colour,
//! which is exactly what un-highlighted code already was.

/// What a span of source *is*. The token each class wears is the whole of
/// the styling policy, in one place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Plain,
    Comment,
    String,
    Number,
    Keyword,
}

impl Class {
    pub fn token(self) -> &'static str {
        match self {
            Class::Plain => "text",
            Class::Comment => "text-muted",
            Class::String => "ansi-green",
            Class::Number => "ansi-cyan",
            Class::Keyword => "accent",
        }
    }
}

/// The language, chosen by file extension. `None` means "highlight nothing",
/// which is the correct amount of colour for a language we cannot lex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lang {
    keywords: &'static [&'static str],
    /// Line comment starter(s).
    line_comment: &'static [&'static str],
    /// Block comment pair, if the language has one.
    block_comment: Option<(&'static str, &'static str)>,
    /// Whether single quotes delimit strings (off for Rust, where they are
    /// lifetimes and chars and colouring them as strings reads as a bug).
    single_quotes: bool,
}

const RUST: Lang = Lang {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
        "false", "type", "unsafe", "use", "where", "while",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    single_quotes: false,
};

const C_LIKE: Lang = Lang {
    keywords: &[
        "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for",
        "goto", "if", "return", "sizeof", "static", "struct", "switch", "typedef", "union",
        "void", "while", "class", "new", "delete", "true", "false", "null", "nullptr",
        "function", "var", "let", "of", "in", "import", "export", "async", "await", "this",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    single_quotes: true,
};

const PYTHON: Lang = Lang {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in",
        "is", "lambda", "None", "not", "or", "pass", "raise", "return", "True", "False", "try",
        "while", "with", "yield", "self",
    ],
    line_comment: &["#"],
    block_comment: None,
    single_quotes: true,
};

const SHELL: Lang = Lang {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
        "function", "in", "return", "exit", "local", "export", "set", "source",
    ],
    line_comment: &["#"],
    block_comment: None,
    single_quotes: true,
};

/// TOML and KDL share enough shape for four classes: `#`/`//` comments,
/// quoted strings, numbers, and bare keys left plain.
const CONFIG: Lang = Lang {
    keywords: &["true", "false", "#true", "#false", "null", "state", "style", "column", "row"],
    line_comment: &["#", "//"],
    block_comment: None,
    single_quotes: true,
};

pub fn lang_of(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => RUST,
        "c" | "h" | "cpp" | "hpp" | "js" | "ts" | "jsx" | "tsx" | "java" | "go" | "wgsl"
        | "glsl" | "css" => C_LIKE,
        "py" => PYTHON,
        "sh" | "bash" | "zsh" => SHELL,
        "toml" | "kdl" | "ini" | "conf" | "yaml" | "yml" | "json" => CONFIG,
        _ => return None,
    })
}

/// Carried across lines: are we inside a block comment?
#[derive(Default, Clone, Copy)]
pub struct LineState {
    in_block_comment: bool,
}

/// One line into classified spans. Byte ranges cover the line exactly, in
/// order — the renderer's contract.
pub fn spans(line: &str, lang: Lang, state: &mut LineState) -> Vec<(Class, std::ops::Range<usize>)> {
    let bytes = line.as_bytes();
    let mut out: Vec<(Class, std::ops::Range<usize>)> = Vec::new();
    let mut i = 0;
    let push =
        |class: Class, range: std::ops::Range<usize>, out: &mut Vec<(Class, std::ops::Range<usize>)>| {
        if range.is_empty() {
            return;
        }
        // Merge with the previous span when the class repeats.
        if let Some((last, r)) = out.last_mut()
            && *last == class
            && r.end == range.start
        {
            r.end = range.end;
            return;
        }
        out.push((class, range));
    };

    while i < bytes.len() {
        // Inside a block comment: everything until its end.
        if state.in_block_comment {
            let (_, close) = lang.block_comment.unwrap_or(("", "*/"));
            match line[i..].find(close) {
                Some(at) => {
                    push(Class::Comment, i..i + at + close.len(), &mut out);
                    state.in_block_comment = false;
                    i += at + close.len();
                }
                None => {
                    push(Class::Comment, i..bytes.len(), &mut out);
                    return out;
                }
            }
            continue;
        }
        // A line comment eats the rest.
        if lang.line_comment.iter().any(|c| line[i..].starts_with(c)) {
            push(Class::Comment, i..bytes.len(), &mut out);
            return out;
        }
        // A block comment opens.
        if let Some((open, _)) = lang.block_comment
            && line[i..].starts_with(open)
        {
            state.in_block_comment = true;
            push(Class::Comment, i..i + open.len(), &mut out);
            i += open.len();
            continue;
        }
        let c = bytes[i];
        // Strings, with escapes.
        if c == b'"' || (c == b'\'' && lang.single_quotes) {
            let quote = c;
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            push(Class::String, i..j.min(bytes.len()), &mut out);
            i = j.min(bytes.len());
            continue;
        }
        // Numbers: a digit-led run of digit-ish chars.
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'.' || bytes[j] == b'_')
            {
                j += 1;
            }
            push(Class::Number, i..j, &mut out);
            i = j;
            continue;
        }
        // Words: keyword or plain.
        if c.is_ascii_alphabetic() || c == b'_' || c == b'#' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let word = &line[i..j];
            let class =
                if lang.keywords.contains(&word) { Class::Keyword } else { Class::Plain };
            push(class, i..j, &mut out);
            i = j;
            continue;
        }
        // Anything else — punctuation, whitespace, non-ASCII — one plain
        // byte at a time; the merge above coalesces runs. Non-ASCII is
        // stepped over whole so ranges stay on char boundaries.
        let step = line[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
        push(Class::Plain, i..i + step, &mut out);
        i += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(line: &str, lang: Lang) -> Vec<(Class, String)> {
        let mut st = LineState::default();
        spans(line, lang, &mut st)
            .into_iter()
            .map(|(c, r)| (c, line[r].to_string()))
            .collect()
    }

    #[test]
    fn rust_line_reads_as_rust() {
        let got = classes("let x = 42; // answer", RUST);
        assert!(got.contains(&(Class::Keyword, "let".into())));
        assert!(got.contains(&(Class::Number, "42".into())));
        assert!(got.contains(&(Class::Comment, "// answer".into())));
    }

    #[test]
    fn strings_swallow_their_escapes_and_comments() {
        let got = classes(r#"print("// not a comment \" still")"#, PYTHON);
        assert!(
            got.iter()
                .any(|(c, t)| *c == Class::String && t.contains("// not a comment")),
            "{got:?}"
        );
    }

    #[test]
    fn block_comments_carry_across_lines() {
        let mut st = LineState::default();
        let _ = spans("before /* opens", RUST, &mut st);
        assert!(st.in_block_comment);
        let line2 = spans("still inside */ after", RUST, &mut st);
        assert!(!st.in_block_comment);
        assert_eq!(line2[0].0, Class::Comment);
        assert!(line2.iter().any(|(c, _)| *c == Class::Plain), "{line2:?}");
    }

    #[test]
    fn spans_cover_the_line_exactly_in_order() {
        for line in ["fn main() { let s = \"x\"; }", "  # comment", "你好 = 1"] {
            let mut st = LineState::default();
            let sp = spans(line, CONFIG, &mut st);
            let mut at = 0;
            for (_, r) in &sp {
                assert_eq!(r.start, at, "gap in {line:?}: {sp:?}");
                at = r.end;
            }
            assert_eq!(at, line.len(), "spans do not cover {line:?}");
        }
    }

    #[test]
    fn rust_single_quotes_are_not_strings() {
        let got = classes("let a: &'static str = f('x');", RUST);
        assert!(
            !got.iter().any(|(c, t)| *c == Class::String && t.starts_with('\'')),
            "a lifetime was coloured as a string: {got:?}"
        );
    }
}
