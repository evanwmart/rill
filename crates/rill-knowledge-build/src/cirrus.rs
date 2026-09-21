//! The CirrusSearch content dump: one JSON document per article, with the
//! metadata the pack needs and a `text` field that is flat (no paragraphs,
//! no headings) and therefore ignored for bodies. Gzip for the legacy
//! series (`other/cirrussearch/`), bzip2 for the weekly successor
//! (`other/cirrus_search_index/…/index_name=…/*.json.bz2`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Meta {
    pub page_id: u64,
    pub title: String,
    pub wikibase_item: Option<String>,
    pub popularity: f64,
    /// Words in the dump's flat `text`, for cross-checking a walked body.
    pub text_words: u32,
    /// Words in `opening_text` (the lead), the cleaner cross-check.
    pub opening_words: u32,
}

#[derive(Deserialize)]
struct Record {
    title: String,
    page_id: u64,
    #[serde(default)]
    namespace: i64,
    #[serde(default)]
    wikibase_item: Option<String>,
    #[serde(default)]
    popularity_score: Option<f64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    opening_text: Option<String>,
}

fn open(path: &Path) -> io::Result<Box<dyn Read>> {
    let file = File::open(path)?;
    let mut magic = [0u8; 3];
    let n = File::open(path)?.read(&mut magic)?;
    Ok(if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else if n >= 3 && &magic == b"BZh" {
        Box::new(bzip2::read::MultiBzDecoder::new(file))
    } else {
        Box::new(file)
    })
}

/// Every namespace-0 article's metadata, in dump order.
pub fn read(path: &Path) -> io::Result<Vec<Meta>> {
    let reader = BufReader::with_capacity(1 << 20, open(path)?);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("{\"index\"") || line.is_empty() {
            continue;
        }
        let r: Record = serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if r.namespace != 0 {
            continue;
        }
        out.push(Meta {
            page_id: r.page_id,
            title: r.title,
            wikibase_item: r.wikibase_item.filter(|q| !q.is_empty()),
            popularity: r.popularity_score.unwrap_or(0.0),
            text_words: r.text.as_deref().map(|t| t.split_whitespace().count() as u32).unwrap_or(0),
            opening_words: r.opening_text.as_deref().map(|t| t.split_whitespace().count() as u32).unwrap_or(0),
        });
    }
    Ok(out)
}

/// `"1/3"`, `"0.25"`, `"1"` → a fraction in (0, 1].
pub fn parse_fraction(s: &str) -> Option<f64> {
    let f = match s.split_once('/') {
        Some((a, b)) => a.trim().parse::<f64>().ok()? / b.trim().parse::<f64>().ok()?,
        None => s.trim().parse().ok()?,
    };
    (f > 0.0 && f <= 1.0 && f.is_finite()).then_some(f)
}

/// The top `fraction` of articles by popularity (ties broken by page id,
/// so the cut is deterministic), the Main Page excluded. Returns the kept
/// metas in rank order and the popularity at the cut.
pub fn cut(mut metas: Vec<Meta>, fraction: f64) -> (Vec<Meta>, f64) {
    metas.retain(|m| m.title != "Main Page");
    metas.sort_by(|a, b| b.popularity.partial_cmp(&a.popularity).unwrap_or(std::cmp::Ordering::Equal).then(a.page_id.cmp(&b.page_id)));
    let keep = ((metas.len() as f64) * fraction).floor() as usize;
    metas.truncate(keep);
    let threshold = metas.last().map(|m| m.popularity).unwrap_or(0.0);
    (metas, threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64, title: &str, pop: f64) -> Meta {
        Meta { page_id: id, title: title.into(), wikibase_item: None, popularity: pop, text_words: 0, opening_words: 0 }
    }

    #[test]
    fn cut_is_top_fraction_by_popularity_without_main_page() {
        let metas = vec![m(1, "Main Page", 9.0), m(2, "a", 0.5), m(3, "b", 0.5), m(4, "c", 0.1), m(5, "d", 0.9), m(6, "e", 0.0), m(7, "f", 0.2)];
        let (kept, thr) = cut(metas, 1.0 / 3.0);
        assert_eq!(kept.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["d", "a"]);
        assert_eq!(thr, 0.5);
    }

    #[test]
    fn fractions() {
        assert_eq!(parse_fraction("1/3"), Some(1.0 / 3.0));
        assert_eq!(parse_fraction("0.5"), Some(0.5));
        assert_eq!(parse_fraction("2"), None);
        assert_eq!(parse_fraction("x"), None);
    }
}
