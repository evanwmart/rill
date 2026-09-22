//! Parsoid HTML → sections, paragraphs, lists. A tag walker, not a DOM:
//! Parsoid output is well-formed and every piece of article text lives in
//! a `<section data-mw-section-id>`, so the walker only has to track a
//! tag stack, one text sink, and what to skip. What it skips (from a
//! survey of 600 top-third articles): navbox / infobox / sidebar tables,
//! every other table for now (wikitables are counted, not walked),
//! citation markers and reference lists, `<style>`/`<script>`, figures,
//! hatnotes, and whole sections whose heading is boilerplate
//! (References, Other websites, …).

use crate::source::{Block, Section};

/// Headings whose sections carry no article text (case-insensitive).
const BOILERPLATE_SECTIONS: &[&str] = &[
    "references",
    "other websites",
    "related pages",
    "notes",
    "sources",
    "external links",
    "see also",
    "notes and references",
    "references and notes",
    "further reading",
    "footnotes",
];

const VOID: &[&str] = &["br", "img", "hr", "meta", "link", "input", "wbr", "source", "area", "base", "col", "embed", "param", "track"];

#[derive(Debug, Default)]
pub struct Walk {
    pub sections: Vec<Section>,
    pub skipped_wikitables: usize,
    pub skipped_other_tables: usize,
    /// Tags still open when the input ended, outermost first. Empty for
    /// well-formed input; a diagnostic when a walk comes back empty.
    pub unclosed: Vec<String>,
    /// Whether the input ended inside a skipped element.
    pub ended_skipping: bool,
}

enum Tok<'a> {
    Open { name: &'a str, attrs: &'a str, self_closing: bool },
    Close(&'a str),
    Text(&'a str),
}

struct Tokens<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Tok<'a>;

    fn next(&mut self) -> Option<Tok<'a>> {
        let s = self.s;
        if self.at >= s.len() {
            return None;
        }
        let rest = &s[self.at..];
        if !rest.starts_with('<') {
            let end = rest.find('<').map(|i| self.at + i).unwrap_or(s.len());
            let t = &s[self.at..end];
            self.at = end;
            return Some(Tok::Text(t));
        }
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map(|i| self.at + i + 3).unwrap_or(s.len());
            self.at = end;
            return self.next();
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let end = rest.find('>').map(|i| self.at + i + 1).unwrap_or(s.len());
            self.at = end;
            return self.next();
        }
        // A tag. Find its end, honouring quoted attribute values.
        let bytes = rest.as_bytes();
        let mut i = 1;
        let mut quote = 0u8;
        while i < bytes.len() {
            let c = bytes[i];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            i += 1;
        }
        let tag_end = self.at + i.min(bytes.len());
        let inner = &s[self.at + 1..tag_end.min(s.len())];
        self.at = (tag_end + 1).min(s.len());
        if let Some(name) = inner.strip_prefix('/') {
            return Some(Tok::Close(name.trim()));
        }
        let self_closing = inner.ends_with('/');
        let inner = inner.trim_end_matches('/');
        let name_end = inner.find(|c: char| c.is_ascii_whitespace()).unwrap_or(inner.len());
        let (name, attrs) = inner.split_at(name_end);
        Some(Tok::Open { name, attrs, self_closing })
    }
}

/// The value of attribute `key` in a tag's attribute text.
/// The TeX a formula was written in, without the `{\\displaystyle …}`
/// wrapper Parsoid adds and with whitespace collapsed.
pub fn tex_source(alttext: &str) -> String {
    let t = alttext.trim();
    let inner = t.strip_prefix("{\\displaystyle").and_then(|r| r.strip_suffix('}')).unwrap_or(t);
    inner.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(i) = rest.find(key) {
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = &rest[i + key.len()..];
        let after_t = after.trim_start();
        if before_ok && after_t.starts_with('=') {
            let v = after_t[1..].trim_start();
            return Some(if let Some(q) = v.strip_prefix('"') {
                &q[..q.find('"').unwrap_or(q.len())]
            } else if let Some(q) = v.strip_prefix('\'') {
                &q[..q.find('\'').unwrap_or(q.len())]
            } else {
                &v[..v.find(|c: char| c.is_ascii_whitespace() || c == '>').unwrap_or(v.len())]
            });
        }
        rest = &rest[i + key.len()..];
    }
    None
}

fn has_class(attrs: &str, class: &str) -> bool {
    attr(attrs, "class").is_some_and(|c| c.split_ascii_whitespace().any(|x| x == class))
}

fn class_contains(attrs: &str, needle: &str) -> bool {
    attr(attrs, "class").is_some_and(|c| c.split_ascii_whitespace().any(|x| x.contains(needle)))
}

/// Decode the entities Parsoid emits (it writes UTF-8 directly, so the
/// named set is the XML five plus nbsp); numeric forms in full.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let name = &after[..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix('#')
                .and_then(|n| n.strip_prefix(['x', 'X']).map(|h| u32::from_str_radix(h, 16).ok()).unwrap_or_else(|| n.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse whitespace runs to one space and trim.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[derive(Default)]
struct ListState {
    items: Vec<String>,
    cur: Option<String>,
}

struct Walker {
    out: Walk,
    /// Open elements, by tag name.
    stack: Vec<String>,
    /// `Some(depth)`: ignore everything until the stack is back to `depth`.
    skip_until: Option<usize>,
    /// Index into `out.sections` of each open section, innermost last.
    sec_stack: Vec<usize>,
    para: String,
    heading: Option<(u8, String)>,
    lists: Vec<ListState>,
    /// The formula wrapper being entered is display-mode (its own line).
    math_display: bool,
}

impl Walker {
    fn skipping(&self) -> bool {
        self.skip_until.is_some()
    }

    fn skip_from_here(&mut self) {
        if self.skip_until.is_none() {
            self.skip_until = Some(self.stack.len());
        }
    }

    fn cur_section(&mut self) -> Option<&mut Section> {
        let i = *self.sec_stack.last()?;
        self.out.sections.get_mut(i)
    }

    fn flush_para(&mut self) {
        let text = normalize(&self.para);
        self.para.clear();
        if text.is_empty() {
            return;
        }
        if let Some(sec) = self.cur_section() {
            sec.blocks.push(Block::Paragraph(text));
        }
    }

    fn finish_item(&mut self) {
        if let Some(list) = self.lists.last_mut()
            && let Some(cur) = list.cur.take()
        {
            let t = normalize(&cur);
            if !t.is_empty() {
                list.items.push(t);
            }
        }
    }

    fn open(&mut self, name: &str, attrs: &str, self_closing: bool) {
        let name = name.to_ascii_lowercase();
        let void = VOID.contains(&name.as_str()) || self_closing;
        if self.skipping() {
            if !void {
                self.stack.push(name);
            }
            return;
        }
        // Parsoid wraps every formula in `span.mwe-math-element`, inline
        // ones also `-inline`; the `<math>` inside carries the TeX source
        // as `alttext`. The source is what survives — MathML and the
        // fallback image do not — as `$…$` in the prose or `$$…$$` as its
        // own paragraph, so a formula stays readable, searchable, and
        // renderable later by anything that knows TeX.
        if name == "span" && class_contains(attrs, "mwe-math-element") {
            self.math_display = !class_contains(attrs, "mwe-math-element-inline");
        }
        if name == "math" {
            if let Some(tex) = attr(attrs, "alttext").map(decode_entities).map(|t| tex_source(&t)).filter(|t| !t.is_empty()) {
                if self.math_display || attr(attrs, "display") == Some("block") {
                    self.flush_para();
                    self.sink().push_str(&format!("$${tex}$$"));
                    self.flush_para();
                } else {
                    let sink = self.sink();
                    if !sink.is_empty() && !sink.ends_with(' ') {
                        sink.push(' ');
                    }
                    sink.push_str(&format!("${tex}$"));
                }
            }
            self.skip_from_here();
            if !void {
                self.stack.push(name);
            }
            return;
        }
        let skip = match name.as_str() {
            "style" | "script" | "figure" | "noscript" | "svg" => true,
            // Layout tables (multi-column bodies) carry article text and
            // are walked as flow; data and furniture tables are skipped.
            "table" if attr(attrs, "role") == Some("presentation") || has_class(attrs, "multicol") => false,
            "table" => {
                if has_class(attrs, "wikitable") {
                    self.out.skipped_wikitables += 1;
                } else {
                    self.out.skipped_other_tables += 1;
                }
                true
            }
            "sup" => has_class(attrs, "reference"),
            "ol" | "ul" => has_class(attrs, "references") || class_contains(attrs, "mw-references"),
            "div" => has_class(attrs, "hatnote") || attr(attrs, "role") == Some("note") || class_contains(attrs, "mw-references"),
            "span" => has_class(attrs, "mw-editsection"),
            _ => false,
        };
        if skip {
            self.skip_from_here();
            if !void {
                self.stack.push(name);
            }
            return;
        }
        match name.as_str() {
            "br" => self.sink().push(' '),
            "section" => {
                self.flush_para();
                let level = if attr(attrs, "data-mw-section-id") == Some("0") { 1 } else { 2 };
                self.out.sections.push(Section { heading: None, level, blocks: Vec::new() });
                self.sec_stack.push(self.out.sections.len() - 1);
            }
            "h2" | "h3" | "h4" | "h5" | "h6" if !self.sec_stack.is_empty() => {
                self.flush_para();
                let level = name.as_bytes()[1] - b'0';
                self.heading = Some((level, String::new()));
            }
            "p" | "blockquote" | "pre" => self.flush_para(),
            "ul" | "ol" | "dl" => {
                self.flush_para();
                self.lists.push(ListState::default());
            }
            "li" | "dt" | "dd" => {
                self.finish_item();
                if let Some(list) = self.lists.last_mut() {
                    list.cur = Some(String::new());
                }
            }
            _ => {}
        }
        if !void {
            self.stack.push(name);
        }
    }

    /// Where text goes right now.
    fn sink(&mut self) -> &mut String {
        if let Some((_, h)) = self.heading.as_mut() {
            return h;
        }
        if let Some(list) = self.lists.last_mut()
            && let Some(cur) = list.cur.as_mut()
        {
            return cur;
        }
        &mut self.para
    }

    fn text(&mut self, t: &str) {
        if self.skipping() || self.sec_stack.is_empty() {
            return;
        }
        if t.trim().is_empty() {
            if !t.is_empty() {
                self.sink().push(' ');
            }
            return;
        }
        let decoded = decode_entities(t);
        self.sink().push_str(&decoded);
    }

    /// The semantic side of popping `name` off the stack.
    fn close_one(&mut self, name: &str) {
        if self.skipping() {
            return;
        }
        match name {
            "section" => {
                self.flush_para();
                while !self.lists.is_empty() {
                    self.close_list();
                }
                self.sec_stack.pop();
            }
            "h2" | "h3" | "h4" | "h5" | "h6" => {
                if let Some((level, text)) = self.heading.take() {
                    let text = normalize(&text);
                    let boilerplate = BOILERPLATE_SECTIONS.contains(&text.to_ascii_lowercase().as_str());
                    if let Some(sec) = self.cur_section() {
                        sec.heading = Some(text);
                        sec.level = level;
                    }
                    if boilerplate {
                        // The section was opened before its heading was known;
                        // drop it and everything until it closes.
                        if let Some(i) = self.sec_stack.pop()
                            && i + 1 == self.out.sections.len()
                        {
                            self.out.sections.pop();
                        }
                        // Skip until the enclosing <section> closes: its
                        // depth is one below the heading's div/h2 chain, so
                        // find it on the stack.
                        if let Some(depth) = self.stack.iter().rposition(|t| t == "section") {
                            self.skip_until = Some(depth);
                        }
                    }
                }
            }
            "p" | "blockquote" | "pre" => self.flush_para(),
            "ul" | "ol" | "dl" => self.close_list(),
            "li" | "dt" | "dd" => self.finish_item(),
            _ => {}
        }
    }

    fn close_list(&mut self) {
        self.finish_item();
        let Some(list) = self.lists.pop() else { return };
        if list.items.is_empty() {
            return;
        }
        if let Some(parent) = self.lists.last_mut() {
            // A nested list folds into the item that holds it.
            let joined = list.items.join("; ");
            match parent.cur.as_mut() {
                Some(cur) => {
                    let end = cur.trim_end().len();
                    cur.truncate(end);
                    cur.push_str("; ");
                    cur.push_str(&joined);
                }
                None => parent.items.push(joined),
            }
        } else if let Some(sec) = self.cur_section() {
            sec.blocks.push(Block::List(list.items));
        }
    }

    fn close(&mut self, name: &str) {
        let name = name.to_ascii_lowercase();
        let Some(pos) = self.stack.iter().rposition(|t| *t == name) else { return };
        while self.stack.len() > pos {
            let top = self.stack.pop().unwrap();
            self.close_one(&top);
            if self.skip_until == Some(self.stack.len()) {
                self.skip_until = None;
            }
        }
    }
}

pub fn walk(html: &str) -> Walk {
    let mut w = Walker {
        out: Walk::default(),
        stack: Vec::new(),
        skip_until: None,
        sec_stack: Vec::new(),
        para: String::new(),
        heading: None,
        lists: Vec::new(),
        math_display: false,
    };
    for tok in (Tokens { s: html, at: 0 }) {
        match tok {
            Tok::Open { name, attrs, self_closing } => w.open(name, attrs, self_closing),
            Tok::Close(name) => w.close(name),
            Tok::Text(t) => w.text(t),
        }
    }
    w.out.ended_skipping = w.skip_until.is_some();
    w.out.unclosed = w.stack.clone();
    while let Some(top) = w.stack.pop() {
        w.close_one(&top);
    }
    w.out.sections.retain(|s| !s.blocks.is_empty());
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r##"<html><body><div id="content">
<section data-mw-section-id="0"><div class="hatnote">For other uses, see <a href="X">X</a>.</div>
<table class="infobox"><tbody><tr><td>Born 1900</td></tr></tbody></table>
<p id="a">Alpha &amp; beta <b>bold</b><sup class="mw-ref reference"><a href="#c"><span>[1]</span></a></sup>.  Second sentence.</p>
<figure><figcaption>A caption</figcaption></figure>
</section>
<section data-mw-section-id="1"><div class="mw-heading mw-heading2"><h2 id="Early_life">Early&#160;life</h2></div>
<p>Born in <a href="Q">Q</a>.</p>
<ul><li>one</li><li>two <ul><li>nested a</li><li>nested b</li></ul></li><li></li></ul>
<table class="wikitable"><tr><td>cell</td></tr></table>
<section data-mw-section-id="2"><div class="mw-heading mw-heading3"><h3>Sub</h3></div><p>Sub text</p></section>
</section>
<section data-mw-section-id="3"><div class="mw-heading mw-heading2"><h2>References</h2></div>
<ol class="mw-references references"><li>ref one</li></ol><p>stray</p>
</section>
<section data-mw-section-id="4"><div class="mw-heading mw-heading2"><h2>Later</h2></div>
<p>After references.</p>
<table class="nowraplinks navbox"><tr><td><ul><li>navlink</li></ul></td></tr></table>
</section>
</div><div class="zim-footer">This article is issued from Wikipedia.</div></body></html>"##;

    #[test]
    fn walks_sections_paragraphs_lists_and_skips_noise() {
        let w = walk(FIXTURE);
        let heads: Vec<_> = w.sections.iter().map(|s| (s.heading.clone(), s.level)).collect();
        assert_eq!(
            heads,
            vec![
                (None, 1),
                (Some("Early life".into()), 2),
                (Some("Sub".into()), 3),
                (Some("Later".into()), 2),
            ]
        );
        assert_eq!(
            w.sections[0].blocks,
            vec![Block::Paragraph("Alpha & beta bold. Second sentence.".into())]
        );
        assert_eq!(
            w.sections[1].blocks,
            vec![
                Block::Paragraph("Born in Q.".into()),
                Block::List(vec!["one".into(), "two; nested a; nested b".into()]),
            ]
        );
        assert_eq!(w.sections[2].blocks, vec![Block::Paragraph("Sub text".into())]);
        assert_eq!(w.sections[3].blocks, vec![Block::Paragraph("After references.".into())]);
        assert_eq!(w.skipped_wikitables, 1);
        assert_eq!(w.skipped_other_tables, 2);
    }

    #[test]
    fn entities_and_whitespace() {
        assert_eq!(decode_entities("a &amp; b &#39;c&#x27; &nbsp;d &unknown; &#x1F600;"), "a & b 'c'  d &unknown; 😀");
        assert_eq!(normalize("  a \n\t b  "), "a b");
    }

    #[test]
    fn attributes() {
        assert_eq!(attr(r#" class="a b" id='x' data-mw-section-id="0""#, "id"), Some("x"));
        assert_eq!(attr(r#" data-mw-section-id="0""#, "id"), None);
        assert!(has_class(r#" class="mw-ref reference""#, "reference"));
    }

    #[test]
    fn formulas_survive_as_tex() {
        let html = r##"<section data-mw-section-id="0"><p>If the sides are <i>a</i> and <i>b</i>, then <span class="mwe-math-element mwe-math-element-inline" typeof="mw:Extension/math"><span class="mwe-math-mathml-inline" style="display: none;"><math xmlns="http://www.w3.org/1998/Math/MathML" alttext="{\displaystyle a^{2}+b^{2}=c^{2}}"><semantics><mrow><mi>a</mi></mrow><annotation encoding="application/x-tex">{\displaystyle a^{2}+b^{2}=c^{2}}</annotation></semantics></math></span><img src="x.svg" class="mwe-math-fallback-image-inline" alt="{\displaystyle a^{2}+b^{2}=c^{2}}"></span> holds.</p>
<p>The roots are</p><span class="mwe-math-element" typeof="mw:Extension/math"><span class="mwe-math-mathml-display" style="display: none;"><math xmlns="http://www.w3.org/1998/Math/MathML" display="block" alttext="{\displaystyle x={\frac {-b\pm {\sqrt {b^{2}-4ac}}}{2a}}}"><semantics><mrow/></semantics></math></span><img src="y.svg" alt="z"></span><p>for α &gt; 0 and Ω.</p></section>"##;
        let w = walk(html);
        let paras: Vec<String> = w.sections.iter().flat_map(|s| s.blocks.iter()).map(|b| match b { Block::Paragraph(p) => p.clone(), Block::List(i) => i.join("; ") }).collect();
        assert_eq!(paras[0], "If the sides are a and b, then $a^{2}+b^{2}=c^{2}$ holds.");
        assert_eq!(paras[1], "The roots are");
        assert_eq!(paras[2], "$$x={\\frac {-b\\pm {\\sqrt {b^{2}-4ac}}}{2a}}$$");
        assert_eq!(paras[3], "for α > 0 and Ω.");
        assert_eq!(tex_source("{\\displaystyle  E = m c^{2} }"), "E = m c^{2}");
        assert_eq!(tex_source("x+1"), "x+1");
    }
}
