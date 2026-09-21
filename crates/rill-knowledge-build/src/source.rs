//! What a source hands the chunker: one article at a time, with the
//! structure the pack needs and nothing the pack does not.
//!
//! Bodies come from the Kiwix ZIM's Parsoid HTML (sections, paragraphs,
//! lists survive there); per-article metadata — the popularity score that
//! decides the cut, the Wikidata item, the opening text — comes from the
//! CirrusSearch dump, joined by title. See `specs/knowledge.md` §2.

/// One article, structured. Section 0 is the lead (no heading).
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// MediaWiki page id (stable across dumps; the join key with the
    /// CirrusSearch record once titles have been matched).
    pub page_id: u64,
    pub title: String,
    /// Wikidata item (`Q937`), when the wiki has one for the page.
    pub wikibase_item: Option<String>,
    /// CirrusSearch `popularity_score` (page-view share). Decides the cut.
    pub popularity: f64,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// `None` for the lead section.
    pub heading: Option<String>,
    /// 2 for `h2`, 3 for `h3`, … ; 1 for the lead.
    pub level: u8,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(String),
    /// One entry per list item, already flattened to text.
    List(Vec<String>),
}

impl Document {
    /// Every word in the body, for size estimates and the chunker's budget.
    pub fn word_count(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|s| s.blocks.iter())
            .map(|b| match b {
                Block::Paragraph(p) => p.split_whitespace().count(),
                Block::List(items) => items.iter().map(|i| i.split_whitespace().count()).sum(),
            })
            .sum()
    }
}
