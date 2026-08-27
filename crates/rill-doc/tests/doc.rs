//! Conformance tests for the document format, mirroring
//! `specs/document-format.md`: deterministic compilation, style flattening,
//! and the strict decode matrix.

use rill_doc::{
    Color, ColorRef, Dimension, DocError, Document, NO_STYLE, Node, compile, decode, encode,
};

const PAGE: &str = r##"
style "heading" size=24 weight="bold" color="#e8e8f0"
style "card" background="#26263a" corner=8
style "serif" font="serif"

column gap=16 padding=12 {
    text "Hello from Rill" style="heading serif"
    image "/assets/moon.webp"
    rect width=320 height=2 style="card"
    link "Open notes" target="/private/notes"
    scroll {
        column gap=4 {
            text "line one"
            spacer 8
            text "line two" style="serif"
        }
    }
}
"##;

#[test]
fn deterministic_compilation() {
    let a = compile(PAGE).unwrap();
    let b = compile(PAGE).unwrap();
    assert_eq!(a.bytes, b.bytes); // the milestone-8 exit condition
}

#[test]
fn compile_decode_roundtrip_structure() {
    let compiled = compile(PAGE).unwrap();
    let doc = decode(&compiled.bytes).unwrap();

    // Root is the outer column, emitted last (post-order).
    assert_eq!(doc.root as usize, doc.nodes.len() - 1);
    let Node::Column { gap, padding, children, .. } = &doc.nodes[doc.root as usize] else {
        panic!("root should be a column");
    };
    assert_eq!(*gap, Dimension::Px(16.0));
    assert_eq!(*padding, Dimension::Px(12.0));
    assert_eq!(children.len(), 5);

    // First child: text with the flattened "heading+serif" combo.
    let Node::Text { style, value } = &doc.nodes[children[0] as usize] else {
        panic!("first child should be text");
    };
    assert_eq!(doc.string(*value), "Hello from Rill");
    let combo = &doc.styles[*style as usize];
    assert_eq!(doc.string(combo.name_idx), "heading+serif");
    assert_eq!(combo.font_size, Some(24.0));
    assert_eq!(combo.font_weight, Some(700));
    assert_eq!(combo.color, Some(ColorRef::Literal(Color { r: 0xE8, g: 0xE8, b: 0xF0, a: 0xFF })));
    assert_eq!(combo.font_family.map(|i| doc.string(i)), Some("serif"));

    // Unstyled node → NO_STYLE.
    let Node::Image { style, source } = &doc.nodes[children[1] as usize] else {
        panic!("second child should be image");
    };
    assert_eq!(*style, NO_STYLE);
    assert_eq!(doc.string(*source), "/assets/moon.webp");

    // String table is sorted and deduplicated.
    let mut sorted = doc.strings.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(doc.strings, sorted);

    // Re-encoding the decoded document is byte-identical (canonical form).
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);
}

#[test]
fn style_layering_last_wins_with_notes() {
    let src = r##"
style "card" color="#111111" corner=8
style "highlight" color="#ffd866"

column {
    text "plain" style="card"
    text "hot" style="card highlight"
}
"##;
    let compiled = compile(src).unwrap();
    assert_eq!(compiled.notes.len(), 1);
    assert!(compiled.notes[0].contains("\"highlight\" overrides color"));

    let doc = decode(&compiled.bytes).unwrap();
    assert_eq!(doc.styles.len(), 2); // "card" and "card+highlight"
    let combined = doc
        .styles
        .iter()
        .find(|s| doc.string(s.name_idx) == "card+highlight")
        .unwrap();
    assert_eq!(combined.color, Some(ColorRef::Literal(Color { r: 0xFF, g: 0xD8, b: 0x66, a: 0xFF })));
    assert_eq!(combined.corner_radius, Some(8.0)); // card's survives
}

#[test]
fn theme_token_colors_compile_and_roundtrip() {
    // A bare identifier is a semantic token; a `#hex` stays a literal. Both
    // survive a canonical encode/decode round-trip.
    let src = r##"
style "brand" color="accent" background="surface-raised"
style "fixed" color="#101018"

column {
    text "themed" style="brand"
    text "hard" style="fixed"
}
"##;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();

    let brand = doc.styles.iter().find(|s| doc.string(s.name_idx) == "brand").unwrap();
    let Some(ColorRef::Token(idx)) = brand.color else { panic!("accent should be a token") };
    assert_eq!(doc.string(idx), "accent");
    let Some(ColorRef::Token(bg)) = brand.background else { panic!("surface token") };
    assert_eq!(doc.string(bg), "surface-raised");

    let fixed = doc.styles.iter().find(|s| doc.string(s.name_idx) == "fixed").unwrap();
    assert_eq!(fixed.color, Some(ColorRef::Literal(Color { r: 0x10, g: 0x10, b: 0x18, a: 0xFF })));

    // Canonical: re-encoding is byte-identical.
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);
}

#[test]
fn compiler_rejections() {
    let cases: &[(&str, &str)] = &[
        ("text \"a\"\ntext \"b\"", "more than one root"),
        ("style \"s\" color=\"#123456\"", "no root UI node"),
        ("bogus \"x\"", "unknown node"),
        ("text \"a\" style=\"nope\"", "unknown style"),
        ("text \"a\" frobnicate=1", "unknown property"),
        ("image \"relative/path\"", "must start with"),
        ("link \"x\" target=\"/a/../b\"", "'..' segment"),
        ("column gap=\"wide\" { text \"a\" }", "finite number or \"auto\""),
        ("style \"s\" color=\"Red\"\ntext \"a\" style=\"s\"", "theme token"),
        ("style \"s\" weight=1001\ntext \"a\" style=\"s\"", "1–1000"),
        ("scroll { text \"a\"; text \"b\" }", "exactly one child"),
        ("style \"dup\" size=1\nstyle \"dup\" size=2\ntext \"a\"", "defined twice"),
    ];
    for (src, expected) in cases {
        let e = compile(src).expect_err(&format!("should reject: {src}"));
        assert!(
            e.to_string().contains(expected),
            "for {src:?}: expected error containing {expected:?}, got {e}"
        );
    }
}

fn valid_bytes() -> Vec<u8> {
    compile("column { text \"hi\" }").unwrap().bytes
}

#[test]
fn decoder_strictness() {
    let good = valid_bytes();
    decode(&good).unwrap();

    // Bad magic / version / reserved.
    let mut b = good.clone();
    b[0] = b'X';
    assert!(decode(&b).is_err());
    let mut b = good.clone();
    // Any version this build does not speak — not a hardcoded number, which
    // silently became the *current* version when the format last moved.
    b[4] = rill_doc::VERSION.wrapping_add(1);
    assert!(decode(&b).unwrap_err().to_string().contains("version"));
    let mut b = good.clone();
    b[28] = 1;
    assert!(decode(&b).unwrap_err().to_string().contains("reserved"));

    // Total-size mismatch (truncation and padding both rejected).
    assert!(decode(&good[..good.len() - 1]).is_err());
    let mut b = good.clone();
    b.push(0);
    assert!(decode(&b).is_err());
}

/// Hand-build documents to hit the decoder's structural rules that a correct
/// compiler can never produce.
#[test]
fn decoder_rejects_non_trees_and_bad_refs() {
    // Child index ≥ parent index (forward reference).
    let doc = Document {
        strings: vec!["hi".into()],
        styles: vec![],
        states: vec![],
        actions: vec![],
        nodes: vec![
            Node::Column { style: NO_STYLE, target: NO_STYLE, gap: Dimension::Px(0.0), padding: Dimension::Px(0.0), children: vec![1] },
            Node::Text { style: NO_STYLE, value: 0 },
        ],
        root: 0,
        warnings: Vec::new(),
    };
    assert!(encode(&doc).unwrap_err().to_string().contains("not less than parent"));

    // Node referenced twice (DAG, not tree).
    let doc = Document {
        strings: vec!["hi".into()],
        styles: vec![],
        states: vec![],
        actions: vec![],
        nodes: vec![
            Node::Text { style: NO_STYLE, value: 0 },
            Node::Column { style: NO_STYLE, target: NO_STYLE, gap: Dimension::Px(0.0), padding: Dimension::Px(0.0), children: vec![0, 0] },
        ],
        root: 1,
        warnings: Vec::new(),
    };
    assert!(encode(&doc).unwrap_err().to_string().contains("not a tree"));

    // Orphan node (never referenced, not root).
    let doc = Document {
        strings: vec!["hi".into()],
        styles: vec![],
        states: vec![],
        actions: vec![],
        nodes: vec![
            Node::Text { style: NO_STYLE, value: 0 },
            Node::Text { style: NO_STYLE, value: 0 },
        ],
        root: 1,
        warnings: Vec::new(),
    };
    assert!(encode(&doc).unwrap_err().to_string().contains("not a tree"));

    // String index out of range.
    let doc = Document {
        strings: vec![],
        styles: vec![],
        states: vec![],
        actions: vec![],
        nodes: vec![Node::Text { style: NO_STYLE, value: 7 }],
        root: 0,
        warnings: Vec::new(),
    };
    assert!(encode(&doc).unwrap_err().to_string().contains("out of range"));

    // Invalid image source path caught at decode.
    let doc = Document {
        strings: vec!["not-a-path".into()],
        styles: vec![],
        states: vec![],
        actions: vec![],
        nodes: vec![Node::Image { style: NO_STYLE, source: 0 }],
        root: 0,
        warnings: Vec::new(),
    };
    assert!(encode(&doc).unwrap_err().to_string().contains("image source"));
}

#[test]
fn unknown_node_types_critical_vs_ignorable() {
    let good = valid_bytes();
    // The single text node's type field: find it. Layout: header(32) +
    // string table + node table. One string "hi": 2 + 2 bytes. Then text
    // node: type u16 at offset 36.
    let text_type_offset = 32 + 2 + 2;
    assert_eq!(
        u16::from_be_bytes([good[text_type_offset], good[text_type_offset + 1]]),
        0x0001
    );

    // Unknown CRITICAL type → reject with the newer-viewer message.
    let mut b = good.clone();
    b[text_type_offset] = 0x7F;
    b[text_type_offset + 1] = 0xFF;
    let e = decode(&b).unwrap_err();
    assert!(e.to_string().contains("newer viewer"), "{e}");

    // Unknown IGNORABLE type → skipped placeholder, document still loads.
    // (0x8002: 0x8001 is assigned now — Closing — and decodes for real.)
    let mut b = good;
    b[text_type_offset] = 0x80;
    b[text_type_offset + 1] = 0x02;
    let doc = decode(&b).unwrap();
    assert!(matches!(doc.nodes[0], Node::UnknownIgnorable { node_type: 0x8002 }));
}

#[test]
fn non_finite_dimension_rejected() {
    let good = compile("column gap=16 { text \"hi\" }").unwrap().bytes;
    // gap dimension value: find f32 bytes of 16.0 (0x41800000) and NaN them.
    let pattern = 16.0f32.to_be_bytes();
    let pos = good
        .windows(4)
        .position(|w| w == pattern)
        .expect("gap literal present");
    let mut b = good;
    b[pos..pos + 4].copy_from_slice(&f32::NAN.to_be_bytes());
    let e = decode(&b).unwrap_err();
    assert!(e.to_string().contains("non-finite"), "{e}");
}

#[test]
fn errors_are_doc_errors() {
    // Compile errors surface as DocError with useful text.
    let e: DocError = compile("").unwrap_err();
    assert!(e.to_string().contains("no root"));
}

#[test]
fn interactive_document_roundtrips() {
    use rill_doc::{ActionValue, DocAction};
    let src = r#"
state "title" initial=""
state "show_form" initial=#false

column gap=8 padding=16 {
    button "New note" { toggle "show_form" }
    when "show_form" {
        column gap=6 {
            text_input bind="title" placeholder="Note title…"
            button "Create" {
                submit "/notes/actions/create" {
                    field "title" from="title"
                }
            }
            button "Clear" { set "title" "" }
        }
    }
    unless "show_form" { text "form hidden" }
    button "Go home" { navigate "/notes" }
}
"#;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();

    assert_eq!(doc.states.len(), 2);
    assert_eq!(doc.string(doc.states[0].name_idx), "title");
    assert_eq!(doc.states[0].initial, ActionValue::Str(String::new()));
    assert_eq!(doc.states[1].initial, ActionValue::Bool(false));
    assert_eq!(doc.actions.len(), 4);
    assert!(matches!(doc.actions[0], DocAction::Toggle { state: 1 }));
    let DocAction::Submit { fields, .. } = &doc.actions[1] else {
        panic!("expected submit, got {:?}", doc.actions[1]);
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(doc.string(fields[0].0), "title");
    assert_eq!(fields[0].1, 0); // state slot 0 = "title"
    assert!(matches!(doc.actions[2], DocAction::SetState { state: 0, ref value } if *value == ActionValue::Str(String::new())));
    assert!(matches!(doc.actions[3], DocAction::Navigate { .. }));

    // Canonical: decode → encode is byte-identical.
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);
}

/// A slider is a value control: bound to a Num slot, carrying its range in
/// the document, firing its (optional) action on release.
#[test]
fn slider_roundtrips_and_guards_its_range() {
    use rill_doc::{ActionValue, Node};
    let src = r#"
state "decay" initial=0.62

column gap=8 {
    slider bind="decay" min=0.1 max=3.0 step=0.01 {
        submit "/studio/actions/shader/decay" {
            field "value" from="decay"
        }
    }
}
"#;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();
    assert_eq!(doc.states[0].initial, ActionValue::Num(0.62));
    let slider = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Slider { bind, min, max, step, action, .. } => {
                Some((*bind, *min, *max, *step, *action))
            }
            _ => None,
        })
        .expect("a slider node");
    assert_eq!(slider, (0, 0.1, 3.0, 0.01, 0));
    // Canonical: decode → encode is byte-identical.
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);

    // A slider without an action is a purely local control.
    let local = compile("state \"v\" initial=1\ncolumn { slider bind=\"v\" min=0 max=2 }").unwrap();
    decode(&local.bytes).unwrap();

    let rejects: &[(&str, &str)] = &[
        ("state \"v\" initial=\"\"\ncolumn { slider bind=\"v\" min=0 max=1 }", "not a number"),
        ("state \"v\" initial=1\ncolumn { slider bind=\"v\" min=2 max=1 }", "not below max"),
        ("state \"v\" initial=1\ncolumn { slider bind=\"v\" min=0 max=1 step=5 }", "inside min..max"),
        ("state \"v\" initial=1\ncolumn { slider bind=\"v\" }", "must be a finite number"),
        ("column { slider min=0 max=1 }", "bind=\"state\" is required"),
    ];
    for (src, expected) in rejects {
        let e = compile(src).expect_err(&format!("should reject: {src}"));
        assert!(e.to_string().contains(expected), "for {src:?}: got {e}");
    }
}

#[test]
fn interactive_rejections() {
    let cases: &[(&str, &str)] = &[
        ("column { button \"x\" { toggle \"nope\" } }", "unknown state"),
        ("state \"s\" initial=\"\"\ncolumn { button \"x\" { toggle \"s\" } }", "not a bool"),
        ("state \"b\" initial=#true\ncolumn { text_input bind=\"b\" }", "not a string"),
        ("state \"b\" initial=#true\ncolumn { button \"x\" { set \"b\" 5 } }", "does not match"),
        ("state \"b\" initial=#true\ncolumn { when \"b\" { text \"a\"; text \"b\" } }", "exactly one child"),
        ("column { button \"x\" }", "action child"),
        ("state \"s\" initial=\"\"\nstate \"s\" initial=\"\"\ncolumn { text \"x\" }", "defined twice"),
        ("column { button \"x\" { submit \"nope\" } }", "must start with"),
    ];
    for (src, expected) in cases {
        let e = compile(src).expect_err(&format!("should reject: {src}"));
        assert!(
            e.to_string().contains(expected),
            "for {src:?}: expected {expected:?}, got {e}"
        );
    }
}

#[test]
fn pick_file_action_roundtrips() {
    use rill_doc::DocAction;
    let src = r##"
state "body" initial=""
column {
    text_input bind="body"
    button "Import" { pick_file into="body" }
}
"##;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();
    assert_eq!(doc.actions.len(), 1);
    assert!(matches!(doc.actions[0], DocAction::PickFile { into: 0 }));
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);

    // pick_file into a non-string slot is rejected.
    let bad = r##"
state "n" initial=#false
column { button "x" { pick_file into="n" } }
"##;
    assert!(compile(bad).unwrap_err().to_string().contains("not a string"));
}


#[test]
fn kdl_escape_survives_hostile_strings() {
    use rill_doc::kdl_escape;
    // Strings that would break out of, or inject into, a naive `"{}"` template.
    let hostile = [
        r#"say "hi""#,                       // embedded quotes
        "line1\nlink \"x\" target=\"/evil\"", // newline + node injection attempt
        "tab\there",
        "back\\slash",
        "ctrl\u{0007}bell",
        "}}} column { text \"pwned\"",        // brace break-out attempt
    ];
    for s in hostile {
        // Interpolating the escaped literal must produce compilable KDL whose
        // single text node holds exactly the original string.
        let src = format!("column {{ text {} }}", kdl_escape(s));
        let doc = decode(&compile(&src).unwrap().bytes).unwrap();
        // Exactly one text node under the column (no injected siblings).
        let texts: Vec<&str> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Text { value, .. } => Some(doc.string(*value)),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "hostile {s:?} injected extra nodes");
        // The decoded text equals the original, except control chars → space.
        let expected: String = s
            .chars()
            .map(|c| if (c as u32) < 0x20 && !matches!(c, '\n' | '\t' | '\r') { ' ' } else { c })
            .collect();
        assert_eq!(texts[0], expected, "hostile {s:?} round-trip mismatch");
    }
}

/// A document written by a newer build must still render here: the property
/// this build has never heard of is skipped, the rest of the style survives,
/// and the skip is reported rather than swallowed.
///
/// Forged by setting an unassigned bit and appending its payload, which is
/// exactly what a future writer would emit.
#[test]
fn unknown_style_properties_are_skipped_not_fatal() {
    let src = r##"
        style "big" size=20 weight="bold"
        column { text "hello" style="big" }
    "##;
    let good = rill_doc::compile(src).unwrap().bytes;
    let doc = rill_doc::decode(&good).unwrap();
    assert!(doc.warnings.is_empty(), "nothing unknown in our own output");

    // Find the style record: name_idx(2) bitmap(4) len(2) payload(len).
    let at = good
        .windows(4)
        .position(|w| {
            // The bitmap for size|weight is 0x0000000C; the first occurrence
            // after the header is the style we just wrote.
            w == 0x0000000Cu32.to_be_bytes()
        })
        .expect("style bitmap");
    let mut forged = good.clone();
    // The lowest bit no property has claimed *yet* — asked for, not guessed.
    // A guessed bit eventually becomes a real property, and then this test
    // forges a valid document and asserts nothing.
    let free = (0..32)
        .map(|b| 1u32 << b)
        .find(|b| rill_doc::KNOWN_STYLE_BITS & b == 0)
        .expect("a free style bit");
    let bitmap = u32::from_be_bytes(forged[at..at + 4].try_into().unwrap()) | free;
    forged[at..at + 4].copy_from_slice(&bitmap.to_be_bytes());
    let len_at = at + 4;
    let len = u16::from_be_bytes([forged[len_at], forged[len_at + 1]]);
    forged[len_at..len_at + 2].copy_from_slice(&(len + 1).to_be_bytes());
    forged.insert(len_at + 2 + len as usize, 0x7f);
    // The header carries the total size; a forged byte has to be accounted
    // for, exactly as a real writer would.
    let total = forged.len() as u32;
    forged[8..12].copy_from_slice(&total.to_be_bytes());

    let doc = rill_doc::decode(&forged).expect("a newer document still decodes");
    assert_eq!(doc.styles[0].font_size, Some(20.0), "known properties survive");
    assert_eq!(doc.styles[0].font_weight, Some(700), "and all of them, not just the first");
    assert_eq!(doc.warnings.len(), 1, "the skip is reported");
    assert!(doc.warnings[0].contains(&format!("{free:#010x}")), "says which bit: {}", doc.warnings[0]);
}

/// A document from an older format layout must be refused by name, not
/// misparsed. Skew used to surface as "string index 256 out of range" — a
/// message that sends you looking for a bug in the document instead of
/// rebuilding a stale binary.
#[test]
fn an_older_format_version_is_refused_clearly() {
    let good = rill_doc::compile(r##"column { text "x" }"##).unwrap().bytes;
    let mut old = good.clone();
    old[4] = rill_doc::VERSION - 1;
    let err = rill_doc::decode(&old).expect_err("an older layout must not be parsed");
    let message = err.to_string();
    assert!(message.contains("version"), "says it is a version problem: {message}");
    assert!(message.contains(&(rill_doc::VERSION - 1).to_string()), "names it: {message}");
}

/// A page can ask for the whole keyboard, and for a clock to reload itself
/// on. Both are declarations like every other affordance — nothing about
/// them is viewer configuration — so both survive the codec unchanged.
#[test]
fn keyboard_capture_and_live_refresh_roundtrip() {
    let src = r##"
column {
    keys target="/term/key"
    live target="/term/screen" every=50
    text "screen"
}
"##;
    let compiled = compile(src).expect("compiles");
    let doc = decode(&compiled.bytes).expect("decodes");
    assert_eq!(encode(&doc).unwrap(), compiled.bytes, "survives a re-encode unchanged");

    let capture = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Keys { target } => Some(doc.string(*target).to_string()),
            _ => None,
        })
        .expect("a keys node");
    assert_eq!(capture, "/term/key");

    let live = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Live { target, interval } => {
                Some((doc.string(*target).to_string(), *interval))
            }
            _ => None,
        })
        .expect("a live node");
    assert_eq!(live, ("/term/screen".to_string(), 50));

    // A reload faster than a client can paint is a busy loop, not a view.
    let hot = r##"column { live target="/x" every=4 }"##;
    assert!(compile(hot).unwrap_err().to_string().contains("floor"));
    // Capture goes to exactly one place, and it must be a path.
    let kids = r##"column { keys target="/k" { text "no" } }"##;
    assert!(compile(kids).unwrap_err().to_string().contains("no children"));
    let bare = r##"column { keys }"##;
    assert!(compile(bare).unwrap_err().to_string().contains("target"));
}

/// A page can name its goodbye: an action the host fires when the window
/// closes. It is the first node in the *ignorable* type half — a viewer
/// that predates it must skip the node and render the rest, which is the
/// whole point of the critical/ignorable split.
#[test]
fn closing_declaration_roundtrips_and_is_ignorable() {
    let src = r##"
column {
    closing target="/term/7/close"
    text "screen"
}
"##;
    let compiled = compile(src).expect("compiles");
    let doc = decode(&compiled.bytes).expect("decodes");
    assert_eq!(encode(&doc).unwrap(), compiled.bytes, "survives a re-encode unchanged");

    let goodbye = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Closing { target } => Some(doc.string(*target).to_string()),
            _ => None,
        })
        .expect("a closing node");
    assert_eq!(goodbye, "/term/7/close");
    // The ignorable half, so an old viewer skips it instead of rejecting
    // the whole page.
    assert!(doc.nodes.iter().any(|n| n.type_code() >= rill_doc::IGNORABLE_TYPE_START));

    // A declaration, not a container, and the target must be a path.
    let kids = r##"column { closing target="/x" { text "no" } }"##;
    assert!(compile(kids).unwrap_err().to_string().contains("no children"));
    let bare = r##"column { closing }"##;
    assert!(compile(bare).unwrap_err().to_string().contains("target"));
}

/// A page can say what is behind it. Documents normally take the desktop's
/// page colour and should; a page that *is* a surface says otherwise, and a
/// fully transparent one means "paint nothing here".
#[test]
fn a_page_can_declare_its_own_background() {
    let src = r##"
column {
    page background="#00000000"
    text "clear"
}
"##;
    let compiled = compile(src).expect("compiles");
    let doc = decode(&compiled.bytes).expect("decodes");
    assert_eq!(encode(&doc).unwrap(), compiled.bytes, "survives a re-encode unchanged");

    let color = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Page { color } => Some(*color),
            _ => None,
        })
        .expect("a page node");
    assert_eq!(color, rill_doc::ColorRef::Literal(rill_doc::Color { r: 0, g: 0, b: 0, a: 0 }));

    // A token works too — a page that wants the desktop's own surface.
    let token = r##"column { page background="surface" }"##;
    let doc = decode(&compile(token).expect("compiles").bytes).unwrap();
    assert!(doc.nodes.iter().any(|n| matches!(n, Node::Page { color: rill_doc::ColorRef::Token(_) })));

    // It declares, it does not contain.
    let kids = r##"column { page background="surface" { text "no" } }"##;
    assert!(compile(kids).unwrap_err().to_string().contains("no children"));
    let bare = r##"column { page }"##;
    assert!(compile(bare).unwrap_err().to_string().contains("background"));
}

/// A key binding is a page affordance like a link or a button: it compiles,
/// survives the codec roundtrip, and carries exactly one meaning.
#[test]
fn key_bindings_roundtrip_and_reject_ambiguity() {
    let src = r##"
column {
    key "down" { submit "/nav/next" }
    key "enter" target="/open"
    text "body"
}
"##;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);
    let keys: Vec<_> = doc
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::Key { key, target, action } => {
                Some((doc.string(*key).to_string(), *target, *action))
            }
            _ => None,
        })
        .collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].0, "down");
    assert_eq!(keys[0].1, NO_STYLE, "action form has no target");
    assert_ne!(keys[0].2, NO_STYLE);
    assert_eq!(keys[1].0, "enter");
    assert_ne!(keys[1].1, NO_STYLE);
    assert_eq!(keys[1].2, NO_STYLE, "target form has no action");

    // Both meanings, no meaning, and a non-canonical combo are all rejected.
    let both = r##"column { key "a" target="/x" { toggle "t" } }"##;
    assert!(compile(both).unwrap_err().to_string().contains("exactly one"));
    let neither = r##"column { key "a" }"##;
    assert!(compile(neither).unwrap_err().to_string().contains("exactly one"));
    let combo = r##"column { key "shift+ctrl+a" target="/x" }"##;
    assert!(compile(combo).unwrap_err().to_string().contains("modifiers"));
}

/// A menu is affordance data like a link: it compiles, round-trips, and an
/// item carries a label plus exactly one meaning.
#[test]
fn menus_roundtrip_and_reject_malformed_items() {
    let src = r##"
column {
    row target="/open" {
        text "entry"
        menu {
            item "Open" target="/open"
            item "Star" icon="star" { submit "/star" }
            separator
            item "Delete" icon="trash" danger=#true { submit "/rm" }
        }
    }
}
"##;
    let compiled = compile(src).unwrap();
    let doc = decode(&compiled.bytes).unwrap();
    assert_eq!(encode(&doc).unwrap(), compiled.bytes);
    let menu = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Menu { items } => Some(items),
            _ => None,
        })
        .expect("menu node");
    assert_eq!(menu.len(), 4);
    assert_eq!(doc.string(menu[0].label), "Open");
    assert!(menu[2].separator);
    assert!(menu[3].danger);
    assert_ne!(menu[3].action, NO_STYLE);

    let both = r##"column { menu { item "X" target="/x" { submit "/y" } } }"##;
    assert!(compile(both).unwrap_err().to_string().contains("exactly one"));
    let neither = r##"column { menu { item "X" } }"##;
    assert!(compile(neither).unwrap_err().to_string().contains("exactly one"));
    let empty = r##"column { menu { } }"##;
    assert!(compile(empty).unwrap_err().to_string().contains("1..=32"));
}

/// A chain of single-child columns is a legal tree of any length as far as
/// node count and reference counts are concerned, so depth needs its own
/// bound: every consumer (resolve, measure, layout, paint) walks by recursion,
/// and a 65k-deep document would overflow the stack — taking the whole viewer
/// down instead of failing one request.
#[test]
fn nesting_deeper_than_the_limit_is_refused() {
    // A chain `depth` levels tall: a text leaf under `depth - 1` columns.
    let chain = |depth: u32| -> Document {
        let mut nodes = vec![Node::Text { style: NO_STYLE, value: 0 }];
        for i in 1..depth {
            nodes.push(Node::Column {
                style: NO_STYLE,
                gap: Dimension::Auto,
                padding: Dimension::Auto,
                target: NO_STYLE,
                children: vec![i - 1],
            });
        }
        Document {
            strings: vec!["x".into()],
            styles: vec![],
            states: vec![],
            actions: vec![],
            root: depth - 1,
            nodes,
            warnings: vec![],
        }
    };

    // Encode self-verifies by decoding, so this covers both directions.
    let at_limit = encode(&chain(rill_doc::MAX_DEPTH)).expect("256 deep is legal");
    assert_eq!(decode(&at_limit).unwrap().nodes.len(), rill_doc::MAX_DEPTH as usize);

    let over = encode(&chain(rill_doc::MAX_DEPTH + 1)).unwrap_err();
    assert!(over.0.contains("deeper than"), "{}", over.0);
}

/// The same bound applies to *source*, and has to be checked before parsing:
/// the KDL parser recurses per `{` and overflows its stack a few thousand
/// levels down, which aborts the process rather than returning a parse error.
/// Braces that only look like nesting — inside strings and comments — must not
/// count, or ordinary pages would be refused.
#[test]
fn deeply_nested_source_is_refused_before_the_parser_sees_it() {
    let nest = |n: usize| {
        let mut s = "column {\n".repeat(n);
        s.push_str("text \"x\"\n");
        s.push_str(&"}\n".repeat(n));
        s
    };
    assert!(compile(&nest(8)).is_ok());
    let e = compile(&nest(400)).unwrap_err();
    assert!(e.0.contains("nested deeper than"), "{}", e.0);

    // Braces the depth scan must skip: quoted, escaped, raw-string, and
    // commented. None of these open a level.
    let tricky = r####"
// { { { a line comment full of braces
/* { { {
   /* nested block comment { { */
   still in a comment { { */
column gap=4 {
    text "{{{{ literal braces in a string }}}}"
    text "an escaped quote \" followed by {{{{"
    text #"a raw string: {{{{ "quoted" inside }}}}"#
    text ##"deeper raw hashes {{{{"##
    button "ok" { navigate "/" }
}
"####;
    let doc = decode(&compile(tricky).expect("tricky braces compile").bytes).unwrap();
    assert_eq!(doc.nodes.len(), 6, "4 texts + button + column");
}

/// Writes seed inputs for `cargo fuzz run doc_decode`. Ignored: run
/// explicitly with `cargo test -p rill-doc --test doc -- --ignored
/// write_fuzz` when the corpus needs refreshing (the corpus is committed).
///
/// The point is *node-type coverage*. A fuzzer mutating bytes will find
/// the shape of a document it has already seen far sooner than it will
/// invent one, so every node type this build knows about wants a seed —
/// including the ignorable half, whose whole contract is that an unknown
/// type is skipped rather than fatal.
#[test]
#[ignore]
fn write_fuzz_corpus() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/doc_decode");
    std::fs::create_dir_all(dir).unwrap();
    let write = |name: &str, source: &str| {
        let bytes = compile(source).unwrap_or_else(|e| panic!("{name}: {e}"));
        std::fs::write(format!("{dir}/seed-{name}"), bytes.bytes).unwrap();
    };

    write("page", PAGE);
    write("declarations", r##"
column {
    page background="#00000000"
    keys target="/term/7/key"
    live target="/term/7/fit/{w}x{h}" every=50
    closing target="/term/7/close"
    key "f2" target="/rename"
    text "screen"
}
"##);
    write("interactive", r##"
state "name" initial=""
state "on" initial=#true
state "level" initial=3.0

column gap=8 padding=16 {
    text_input bind="name" placeholder="who?"
    slider bind="level" min=0.0 max=10.0 step=1.0 {
        submit "/save" { field "level" from="level" }
    }
    button "Save" { submit "/save" { field "name" from="name" } }
    button "Toggle" { toggle "on" }
    button "Clear" { set "name" "" }
    button "Home" { navigate "/" }
    when "on" { text "shown" }
    unless "on" { text "hidden" }
}
"##);
    write("menu-and-icons", r##"
column {
    icon "gear" size=16
    row target="/open" {
        text "Row with a menu"
        menu {
            item "Open" target="/open"
            item "Star" icon="star" { submit "/star" }
            separator
            item "Delete" icon="trash" danger=#true { submit "/rm" }
        }
    }
    titlebar { text "my titlebar" }
}
"##);
    write("nested-scroll", r##"
column gap=2 padding=1 {
    scroll { column { text "a"; text "b"; spacer size=4; rect width=2 height=2 } }
}
"##);
}

/// The tier chain's first leg: `sensitive tier=N` compiles, round-trips, and
/// sits in the *critical* type half — a viewer too old to know it must
/// refuse the document, because skipping it would record the page at T0, a
/// fail-open on a classification control (specs/history.md decision 4).
#[test]
fn sensitive_declares_and_is_critical() {
    let compiled = rill_doc::compile("column { text \"secrets\"; sensitive tier=2 }").unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let node = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            rill_doc::Node::Sensitive { tier } => Some(*tier),
            _ => None,
        })
        .expect("the declaration survived the round trip");
    assert_eq!(node, 2);
    // Critical, deliberately: the whole point of the type-code choice.
    let code = doc
        .nodes
        .iter()
        .find(|n| matches!(n, rill_doc::Node::Sensitive { .. }))
        .map(|n| n.type_code())
        .unwrap();
    assert!(
        code < rill_doc::IGNORABLE_TYPE_START,
        "sensitive landed in the ignorable half ({code:#06x}) — an old viewer would \
         skip it and record at T0"
    );
}

/// Only raising exists in the tier vocabulary: tier=0 is what an undeclared
/// page already is, and an unknown tier must fail the compile, not pass
/// through as a number nothing downstream classifies.
#[test]
fn sensitive_rejects_the_tiers_that_are_not_a_raise() {
    let page = |decl: &str| rill_doc::compile(&format!("column {{ {decl} }}"));
    assert!(page("sensitive tier=0").is_err(), "tier=0 is a no-op, refused");
    assert!(page("sensitive tier=3").is_err(), "tier=3 is unknown, refused");
    assert!(page("sensitive").is_err(), "tier is required");
}
