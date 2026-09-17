//! The exhibit label: a document that is mostly words, in three languages,
//! at two sizes, that does not change.
//!
//! No `live` here. A label is read once and holds; the glass fetches it,
//! caches it by hash, and could show it from the cache with the server
//! gone. The axis this one proves is *semantic*: the label is text with
//! structure — title, maker, date, medium, credit, the essay — not a
//! bitmap of text, so a screen reader, a search index, or a translator
//! gets the words, and the same document re-flows to the large-print
//! setting without a second asset.
//!
//! Language and size are paths, because a document's identity is its
//! address: `/museum/fr` is the French label, `/museum/fr/large` the
//! large-print one, and the links at the foot of each page are ordinary
//! navigations a finger or a keyboard can take.

use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

pub struct Label;

struct Text {
    lang_name: &'static str,
    title: &'static str,
    maker: &'static str,
    dates: &'static str,
    medium: &'static str,
    credit: &'static str,
    essay: &'static [&'static str],
    large_print: &'static str,
    normal_print: &'static str,
    gallery: &'static str,
}

const LANGS: &[(&str, Text)] = &[
    (
        "en",
        Text {
            lang_name: "English",
            title: "Under the Wave off Kanagawa",
            maker: "Katsushika Hokusai",
            dates: "Japanese, 1760–1849 · c. 1830–32",
            medium: "Woodblock print; ink and colour on paper",
            credit: "From the series Thirty-six Views of Mount Fuji · Gallery 214",
            essay: &[
                "Three boats and their crews pitch beneath a wave whose crest breaks into claws. Far behind them, small and still, stands Mount Fuji — the subject of the series, here almost an afterthought against the sea.",
                "The deep blue is Prussian blue, a synthetic pigment newly imported from Europe and cheaper than the mineral blues before it. Its intensity is part of why the print was a popular success in its own time and why it still reads from across a room.",
                "Hokusai was about seventy when he designed it. He signed the series as \"Iitsu, formerly Hokusai\", one of more than thirty names he used across a working life of some seventy years.",
            ],
            large_print: "Large print",
            normal_print: "Standard print",
            gallery: "Please do not touch the works",
        },
    ),
    (
        "fr",
        Text {
            lang_name: "Français",
            title: "Sous la vague au large de Kanagawa",
            maker: "Katsushika Hokusai",
            dates: "Japonais, 1760–1849 · vers 1830–1832",
            medium: "Estampe sur bois ; encre et couleurs sur papier",
            credit: "De la série Trente-six vues du mont Fuji · Salle 214",
            essay: &[
                "Trois barques et leurs équipages tanguent sous une vague dont la crête se brise en griffes. Loin derrière, petit et immobile, se dresse le mont Fuji — sujet de la série, ici presque effacé par la mer.",
                "Le bleu profond est du bleu de Prusse, pigment synthétique fraîchement importé d'Europe et moins coûteux que les bleus minéraux qui l'ont précédé. Son intensité explique en partie le succès populaire de l'estampe à son époque, et sa lisibilité d'un bout à l'autre d'une salle.",
                "Hokusai avait environ soixante-dix ans lorsqu'il la conçut. Il signa la série « Iitsu, anciennement Hokusai », l'un des plus de trente noms qu'il utilisa au cours de quelque soixante-dix années de travail.",
            ],
            large_print: "Gros caractères",
            normal_print: "Caractères standard",
            gallery: "Merci de ne pas toucher les œuvres",
        },
    ),
    (
        "es",
        Text {
            lang_name: "Español",
            title: "Bajo la ola de Kanagawa",
            maker: "Katsushika Hokusai",
            dates: "Japonés, 1760–1849 · c. 1830–1832",
            medium: "Grabado en madera; tinta y color sobre papel",
            credit: "De la serie Treinta y seis vistas del monte Fuji · Sala 214",
            essay: &[
                "Tres barcas y sus tripulaciones cabecean bajo una ola cuya cresta se rompe en garras. Muy atrás, pequeño y quieto, se alza el monte Fuji: el tema de la serie, aquí casi un detalle frente al mar.",
                "El azul profundo es azul de Prusia, un pigmento sintético recién importado de Europa y más barato que los azules minerales anteriores. Su intensidad explica en parte el éxito popular de la estampa en su época, y que aún se lea desde el otro extremo de una sala.",
                "Hokusai tenía unos setenta años cuando la diseñó. Firmó la serie como «Iitsu, antes Hokusai», uno de los más de treinta nombres que usó a lo largo de unos setenta años de trabajo.",
            ],
            large_print: "Letra grande",
            normal_print: "Letra estándar",
            gallery: "Por favor, no toque las obras",
        },
    ),
];

impl Label {
    fn text(lang: &str) -> Option<&'static Text> {
        LANGS.iter().find(|(l, _)| *l == lang).map(|(_, t)| t)
    }

    fn page(lang: &str, large: bool) -> Result<Vec<u8>, Status> {
        let t = Label::text(lang).ok_or(Status::NotFound)?;
        // Two type scales from one document. The large one is what a
        // visitor with low vision asks for at the desk; here it is a link.
        let k = if large { 1.45 } else { 1.0 };
        let sz = |base: f32| (base * k).round();
        let mut kdl = format!(
            "style \"label\" padding-x=140 padding-y=96 gap={gap} width=\"fill\" height=\"fill\" background=\"#f6f1e7\"\n\
             style \"rule\" background=\"#1f1a14\" corner=0\n\
             style \"title\" size={title} weight=800 color=\"#1f1a14\" wrap=#true\n\
             style \"maker\" size={maker} weight=700 color=\"#1f1a14\"\n\
             style \"dates\" size={meta} weight=500 color=\"#5b5247\"\n\
             style \"medium\" size={meta} weight=500 color=\"#5b5247\"\n\
             style \"essay\" size={essay} weight=400 color=\"#2b2520\" wrap=#true\n\
             style \"credit\" size={credit} weight=500 color=\"#7a6f62\"\n\
             style \"nav\" gap=36 valign=\"center\" width=\"fill\"\n\
             style \"nav-on\" size={nav} weight=800 color=\"#1f1a14\" underline=#false\n\
             style \"nav-off\" size={nav} weight=500 color=\"#7a6f62\" underline=#false\n\
             style \"nav-size\" size={nav} weight=700 color=\"#8a3b12\" underline=#false\n\
             style \"notice\" size={credit} weight=500 color=\"#7a6f62\"\n\n\
             column style=\"label\" {{\n\
             \tpage background=\"#f6f1e7\"\n",
            gap = sz(28.0),
            title = sz(72.0),
            maker = sz(44.0),
            meta = sz(30.0),
            essay = sz(34.0),
            credit = sz(24.0),
            nav = sz(28.0),
        );
        let q = rill_doc::kdl_escape;
        kdl.push_str(&format!("\ttext {} style=\"title\"\n", q(t.title)));
        kdl.push_str(&format!("\ttext {} style=\"maker\"\n", q(t.maker)));
        kdl.push_str(&format!("\ttext {} style=\"dates\"\n", q(t.dates)));
        kdl.push_str(&format!("\ttext {} style=\"medium\"\n", q(t.medium)));
        kdl.push_str(&format!("\trect style=\"rule\" width=120 height={}\n", sz(4.0)));
        for para in t.essay {
            kdl.push_str(&format!("\ttext {} style=\"essay\"\n", q(para)));
        }
        kdl.push_str(&format!("\ttext {} style=\"credit\"\n", q(t.credit)));
        kdl.push_str("\tspacer\n");
        let suffix = if large { "/large" } else { "" };
        kdl.push_str("\trow style=\"nav\" {\n");
        for (l, other) in LANGS {
            let style = if *l == lang { "nav-on" } else { "nav-off" };
            kdl.push_str(&format!(
                "\t\tlink {} target=\"/museum/{l}{suffix}\" style=\"{style}\"\n",
                q(other.lang_name)
            ));
        }
        kdl.push_str("\t\tspacer\n");
        let (size_label, size_target) = if large {
            (t.normal_print, format!("/museum/{lang}"))
        } else {
            (t.large_print, format!("/museum/{lang}/large"))
        };
        kdl.push_str(&format!(
            "\t\tlink {} target=\"{size_target}\" style=\"nav-size\"\n",
            q(size_label)
        ));
        kdl.push_str("\t}\n");
        kdl.push_str(&format!("\ttext {} style=\"notice\"\n", q(t.gallery)));
        kdl.push_str("}\n");
        rill_appkit::compile_page("signage-app", &kdl)
    }

    /// `/museum` → English; `/museum/<lang>[/large]`.
    fn route(path: &str) -> Option<(&str, bool)> {
        let rest = path.strip_prefix("/museum")?;
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            return Some(("en", false));
        }
        let (lang, large) = match rest.split_once('/') {
            Some((lang, "large")) => (lang, true),
            Some(_) => return None,
            None => (rest, false),
        };
        Some((lang, large))
    }
}

impl AppHandler for Label {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        let (lang, large) = Label::route(path)?;
        Label::page(lang, large).ok()
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        // Fixed content: one revision for the life of the process.
        Label::route(path).map(|_| 1)
    }

    fn action(
        &self,
        _path: &str,
        _fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        Err(Status::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_compiles_at_both_sizes() {
        for (lang, t) in LANGS {
            for large in [false, true] {
                let bytes = Label::page(lang, large).unwrap_or_else(|e| panic!("{lang}: {e:?}"));
                let doc = rill_doc::decode(&bytes).expect("decodes");
                assert!(doc.strings.iter().any(|s| s == t.title));
                // No live node: a label holds.
                assert!(!doc.nodes.iter().any(|n| matches!(n, rill_doc::Node::Live { .. })));
            }
        }
    }

    #[test]
    fn routes() {
        assert_eq!(Label::route("/museum"), Some(("en", false)));
        assert_eq!(Label::route("/museum/"), Some(("en", false)));
        assert_eq!(Label::route("/museum/fr"), Some(("fr", false)));
        assert_eq!(Label::route("/museum/es/large"), Some(("es", true)));
        assert_eq!(Label::route("/museum/es/huge"), None);
        assert!(Label.get("/museum/xx", &Identity::Anonymous).is_none());
    }
}
