//! The corpus as documents: read the dump, take the cut, resolve every
//! title in the ZIM, walk the bodies in cluster order (each cluster is
//! decompressed once), then order the result by popularity so document 0
//! is the most-read article. `probe-source` is the diagnostic twin of this
//! loop; this one just loads.

use std::io;
use std::path::Path;

use crate::{cirrus, html, source::Document, zim};

pub struct Loaded {
    pub docs: Vec<Document>,
    pub dump_articles: usize,
    pub cut_threshold: f64,
    pub missing: usize,
}

pub fn load(zim_path: &Path, dump_path: &Path, fraction: f64, limit: usize) -> io::Result<Loaded> {
    let metas = cirrus::read(dump_path)?;
    let dump_articles = metas.len();
    let (cut, cut_threshold) = cirrus::cut(metas, fraction);
    let mut z = zim::Zim::open(zim_path)?;

    let mut jobs: Vec<(u32, u32, usize)> = Vec::new();
    let mut missing = 0usize;
    for (mi, m) in cut.iter().enumerate().take(limit) {
        let url = m.title.replace(' ', "_");
        match z.lookup(b'C', &url).and_then(|idx| z.resolve(idx)).and_then(|(idx, _)| z.entry(idx)) {
            Some(e) => {
                if let zim::Kind::Item { cluster, blob } = e.kind {
                    jobs.push((cluster, blob, mi));
                }
            }
            None => missing += 1,
        }
    }
    jobs.sort_unstable();

    let mut docs = Vec::with_capacity(jobs.len());
    let mut cur: Option<(u32, zim::Cluster)> = None;
    for (cluster, blob, mi) in jobs {
        if cur.as_ref().is_none_or(|(c, _)| *c != cluster) {
            cur = Some((cluster, z.cluster(cluster)?));
        }
        let bytes = cur.as_ref().expect("cluster loaded").1.blob(blob).unwrap_or(&[]);
        let walked = html::walk(&String::from_utf8_lossy(bytes));
        let m = &cut[mi];
        docs.push(Document {
            page_id: m.page_id,
            title: m.title.clone(),
            wikibase_item: m.wikibase_item.clone(),
            popularity: m.popularity,
            sections: walked.sections,
        });
    }
    // Popularity descending, page id as the tiebreak: deterministic, and
    // the document table reads top-down.
    docs.sort_by(|a, b| b.popularity.partial_cmp(&a.popularity).unwrap_or(std::cmp::Ordering::Equal).then(a.page_id.cmp(&b.page_id)));
    Ok(Loaded { docs, dump_articles, cut_threshold, missing })
}
