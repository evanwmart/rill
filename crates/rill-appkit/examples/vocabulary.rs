//! The kit's whole vocabulary on one page — the living style guide.
//!
//! Prints the KDL for a page using every atom, arranged as the component
//! spec lays them out: SidebarHeader + TopToolbar, Sidebar, ContentPane
//! with SortBar / ListView / forms / verbs / empty state. Compile and
//! render it whenever [`rill_appkit::STYLES`] moves:
//!
//! ```sh
//! cargo run -p rill-appkit --example vocabulary > /tmp/vocab.kdl
//! cargo run -p rill -- doc compile /tmp/vocab.kdl --output /tmp/vocab.rill
//! RILL_DOC=/tmp/vocab.rill DOC_PREVIEW=vocab.ppm \
//!     cargo test -p rill-vector -- --ignored render_document
//! ```

use rill_appkit as kit;
use rill_appkit::{FileRow, Place, Shell};

fn main() {
    let places = [
        Place {
            label: "Library".into(),
            target: "/demo".into(),
            icon: "home-fill".into(),
            current: false,
        },
        Place {
            label: "Projects".into(),
            target: "/demo/projects".into(),
            icon: "folder-fill".into(),
            current: true,
        },
        Place {
            label: "Archive".into(),
            target: "/demo/archive".into(),
            icon: "folder-fill".into(),
            current: false,
        },
    ];

    let strip = kit::sidebar_header(
        &(kit::icon_slot("home", "navigate \"/demo\"")
            + &kit::search_field(
                "q",
                "Search\u{2026}",
                "submit \"/demo/filter\" { field \"q\" from=\"q\" }",
            )),
    ) + &kit::toolbar(
        &(kit::location_bar("loc", "submit \"/demo/go\" { field \"loc\" from=\"loc\" }")
            + &kit::text_button("New item", "toggle \"mk\"")
            + &kit::danger_button("Delete", "submit \"/demo/rm\" { field \"loc\" from=\"loc\" }")
            + &kit::toolbar_button("list", "toggle \"view\"")),
    );

    let sorts = kit::sort_bar(
        &(kit::sort_control("Name", true, false, "toggle \"view\"")
            + &kit::sort_control("Type", false, false, "toggle \"view\"")
            + &kit::sort_control("Size", false, false, "toggle \"view\"")
            + &kit::sort_control("Modified", false, false, "toggle \"view\"")),
    );

    let pick =
        |i: u32| Some(("dots-vertical", format!("submit \"/demo/pick/{i}\" {{ field \"q\" from=\"q\" }}")));
    let rows = [
        FileRow {
            selected: false,
            icon: ("folder", "ico"),
            title: "designs",
            target: "/demo/projects",
            title_style: "file-name--dir",
            cells: &[("Folder".into(), "cell-kind"), ("\u{2014}".into(), "cell-meta")],
            trailing: pick(0),
            menu: None,
        },
        FileRow {
            selected: true,
            icon: ("file", "ico-dim"),
            title: "roadmap-draft.md",
            target: "/demo/projects",
            title_style: "file-name",
            cells: &[("MD".into(), "cell-kind"), ("4 KB".into(), "cell-meta")],
            trailing: pick(1),
            menu: None,
        },
        FileRow {
            selected: false,
            icon: ("world", "ico-dim"),
            title: "a-name-long-enough-to-prove-the-ellipsis-works.txt",
            target: "/demo/projects",
            title_style: "file-name",
            cells: &[("TXT".into(), "cell-kind"), ("812 B".into(), "cell-meta")],
            trailing: pick(2),
            menu: None,
        },
    ];

    let form = kit::when(
        "mk",
        &kit::panel_row(
            &(kit::input(
                "name",
                "field",
                "Item name\u{2026}",
                "submit \"/demo/new\" { field \"name\" from=\"name\" }",
            ) + &kit::cta_button("Create", "submit \"/demo/new\" { field \"name\" from=\"name\" }")),
        ),
    );

    let mut body = sorts;
    body.push_str(&kit::list_view(
        &rows.iter().map(kit::file_row).collect::<String>(),
    ));
    body.push_str(&form);
    body.push_str(&kit::empty_note("2 hidden"));
    body.push_str("\t\t\tspacer\n");

    let m = kit::Metrics::default();
    let kdl = kit::shell(&Shell {
        metrics: m,
        states: "state \"loc\" initial=\"/projects\"\nstate \"q\" initial=\"\"\n\
                 state \"name\" initial=\"\"\nstate \"mk\" initial=#true\n\
                 state \"view\" initial=#false\n",
        titlebar: &strip,
        places: &places,
        footer: Some(("About this page", "/demo")),
        sidebar_top_gap: m.sidebar_align_gap() as u32,
        extra_styles: "",
        content_style: None,
        body: &body,
        rail_body: None,
        scroll_content: true,
    });
    println!("{kdl}");
}
