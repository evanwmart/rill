//! The policy path pattern language (security.md §6). Complete semantics:
//! literal segments match exactly; `*` matches exactly one segment; `**` is
//! only valid as the final segment and matches any remaining suffix,
//! including the empty one. Anything else is a parse error.

use crate::AuthError;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Lit(String),
    Any,
    Rest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    segs: Vec<Seg>,
    source: String,
}

impl Pattern {
    pub fn parse(source: &str) -> Result<Pattern, AuthError> {
        let err = |m: &str| AuthError::new(format!("pattern {source:?}: {m}"));
        if !source.starts_with('/') {
            return Err(err("must start with '/'"));
        }
        let mut segs = Vec::new();
        if source != "/" {
            let raw: Vec<&str> = source[1..].split('/').collect();
            for (i, seg) in raw.iter().enumerate() {
                match *seg {
                    "" => return Err(err("empty segment")),
                    "*" => segs.push(Seg::Any),
                    "**" => {
                        if i != raw.len() - 1 {
                            return Err(err("'**' is only valid as the final segment"));
                        }
                        segs.push(Seg::Rest);
                    }
                    lit if lit.contains('*') => {
                        return Err(err("'*' must be a whole segment"));
                    }
                    lit => segs.push(Seg::Lit(lit.to_string())),
                }
            }
        }
        Ok(Pattern { segs, source: source.to_string() })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Match a request path (already validated by the protocol codec —
    /// leading `/`, no empty/`.`/`..` segments).
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> =
            if path == "/" { Vec::new() } else { path[1..].split('/').collect() };
        Self::match_segs(&self.segs, &parts)
    }

    fn match_segs(segs: &[Seg], parts: &[&str]) -> bool {
        match segs.split_first() {
            None => parts.is_empty(),
            Some((Seg::Rest, _)) => true, // final by construction; any suffix
            Some((seg, rest_segs)) => match parts.split_first() {
                None => false,
                Some((part, rest_parts)) => {
                    let hit = match seg {
                        Seg::Lit(lit) => lit == part,
                        Seg::Any => true,
                        Seg::Rest => unreachable!("handled above"),
                    };
                    hit && Self::match_segs(rest_segs, rest_parts)
                }
            },
        }
    }

    /// True if `self` matches every path `other` matches — used by the
    /// startup lint to flag unreachable (shadowed) rules. Conservative in
    /// `false` (a missed shadow is a spurious non-warning, never a wrong
    /// access decision).
    pub fn covers(&self, other: &Pattern) -> bool {
        Self::cover_segs(&self.segs, &other.segs)
    }

    fn cover_segs(a: &[Seg], b: &[Seg]) -> bool {
        match (a.split_first(), b.split_first()) {
            (None, None) => true,
            (Some((Seg::Rest, _)), _) => true,
            (None, Some(_)) | (Some(_), None) => false,
            (Some((sa, ra)), Some((sb, rb))) => {
                let seg_covers = match (sa, sb) {
                    (Seg::Any, Seg::Rest) => false, // b allows longer suffixes
                    (Seg::Any, _) => true,
                    (Seg::Lit(x), Seg::Lit(y)) => x == y,
                    (Seg::Lit(_), _) => false,
                    (Seg::Rest, _) => unreachable!("handled above"),
                };
                seg_covers && Self::cover_segs(ra, rb)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    fn p(s: &str) -> Pattern {
        Pattern::parse(s).unwrap()
    }

    #[test]
    fn parse_rejects_bad_patterns() {
        for bad in ["", "public/**", "/a//b", "/a/**/b", "/a*", "/*x", "/**x"] {
            assert!(Pattern::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn literal_matching() {
        assert!(p("/").matches("/"));
        assert!(!p("/").matches("/a"));
        assert!(p("/a/b").matches("/a/b"));
        assert!(!p("/a/b").matches("/a"));
        assert!(!p("/a/b").matches("/a/b/c"));
        assert!(!p("/a/b").matches("/a/x"));
    }

    #[test]
    fn star_is_exactly_one_segment() {
        assert!(p("/public/*").matches("/public/index"));
        assert!(!p("/public/*").matches("/public"));
        assert!(!p("/public/*").matches("/public/a/b"));
        assert!(p("/*/index").matches("/site/index"));
    }

    #[test]
    fn double_star_is_any_suffix_including_empty() {
        assert!(p("/public/**").matches("/public"));
        assert!(p("/public/**").matches("/public/a"));
        assert!(p("/public/**").matches("/public/a/b/c"));
        assert!(!p("/public/**").matches("/private/a"));
        assert!(p("/**").matches("/"));
        assert!(p("/**").matches("/anything/at/all"));
    }

    #[test]
    fn covers_for_shadow_lint() {
        assert!(p("/**").covers(&p("/private/**")));
        assert!(p("/private/**").covers(&p("/private/notes")));
        assert!(p("/a/*").covers(&p("/a/b")));
        assert!(!p("/a/*").covers(&p("/a/**"))); // ** reaches deeper
        assert!(!p("/private/**").covers(&p("/public/**")));
        assert!(!p("/a/b").covers(&p("/a/*")));
    }
}
