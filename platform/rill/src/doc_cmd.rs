//! `rill doc` subcommands (document-format.md §9): compile, inspect.

use std::process::ExitCode;

use rill_doc::{Dimension, NO_STYLE, Node, compile, decode};

pub fn run(args: &[String]) -> ExitCode {
    let result = match args.split_first().map(|(c, r)| (c.as_str(), r)) {
        Some(("compile", rest)) => cmd_compile(rest),
        Some(("inspect", rest)) => cmd_inspect(rest),
        _ => {
            eprintln!("usage: rill doc compile <src.kdl> --output <out.rill>");
            eprintln!("       rill doc inspect <file.rill>");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rill doc: {message}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_compile(args: &[String]) -> Result<(), String> {
    let (mut input, mut output) = (None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                output = Some(args.get(i + 1).ok_or("--output needs a value")?.clone());
                i += 2;
            }
            other if input.is_none() => {
                input = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let input = input.ok_or("compile needs a source file")?;
    let output = output.ok_or("compile needs --output <file>")?;

    let source = std::fs::read_to_string(&input).map_err(|e| format!("{input}: {e}"))?;
    let compiled = compile(&source).map_err(|e| e.to_string())?;
    for note in &compiled.notes {
        eprintln!("{note}");
    }
    std::fs::write(&output, &compiled.bytes).map_err(|e| format!("{output}: {e}"))?;
    let doc = decode(&compiled.bytes).map_err(|e| e.to_string())?;
    println!(
        "{output}: {} bytes — {} nodes, {} styles, {} strings",
        compiled.bytes.len(),
        doc.nodes.len(),
        doc.styles.len(),
        doc.strings.len()
    );
    Ok(())
}

fn cmd_inspect(args: &[String]) -> Result<(), String> {
    let [file] = args else { return Err("inspect needs a .rill file".into()) };
    let bytes = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
    let doc = decode(&bytes).map_err(|e| e.to_string())?;

    println!("Document: {file} ({} bytes)", bytes.len());
    println!("Strings: {}", doc.strings.len());
    println!("Styles: {}", doc.styles.len());
    for (i, s) in doc.styles.iter().enumerate() {
        let mut props = Vec::new();
        let show = |c: rill_doc::ColorRef| match c {
            rill_doc::ColorRef::Literal(c) => c.to_string(),
            rill_doc::ColorRef::Token(idx) => format!("{} (token)", doc.string(idx)),
        };
        if let Some(c) = s.color {
            props.push(format!("color={}", show(c)));
        }
        if let Some(c) = s.background {
            props.push(format!("background={}", show(c)));
        }
        if let Some(v) = s.font_size {
            props.push(format!("size={v}"));
        }
        if let Some(v) = s.font_weight {
            props.push(format!("weight={v}"));
        }
        if let Some(v) = s.corner_radius {
            props.push(format!("corner={v}"));
        }
        if let Some(idx) = s.font_family {
            props.push(format!("font={:?}", doc.string(idx)));
        }
        println!("  [{i}] {:?}: {}", doc.string(s.name_idx), props.join(" "));
    }
    if !doc.states.is_empty() {
        println!("States: {}", doc.states.len());
        for (i, st) in doc.states.iter().enumerate() {
            println!("  [{i}] {:?} = {:?}", doc.string(st.name_idx), st.initial);
        }
    }
    if !doc.actions.is_empty() {
        println!("Actions: {}", doc.actions.len());
        for (i, a) in doc.actions.iter().enumerate() {
            println!("  [{i}] {a:?}");
        }
    }
    println!("Nodes: {} (root: {})", doc.nodes.len(), doc.root);
    print_tree(&doc, doc.root, 1);
    Ok(())
}

fn dim(d: Dimension) -> String {
    match d {
        Dimension::Auto => "auto".into(),
        Dimension::Px(v) => format!("{v}px"),
        Dimension::Fill(v) => format!("fill:{v}"),
    }
}

fn print_tree(doc: &rill_doc::Document, index: u32, depth: usize) {
    let node = &doc.nodes[index as usize];
    let indent = "  ".repeat(depth);
    let style_of = |style: &u16| {
        if *style == NO_STYLE {
            String::new()
        } else {
            format!(" style={:?}", doc.string(doc.styles[*style as usize].name_idx))
        }
    };
    let described = match node {
        Node::Text { style, value } => {
            format!("Text {:?}{}", doc.string(*value), style_of(style))
        }
        Node::Image { style, source } => {
            format!("Image {:?}{}", doc.string(*source), style_of(style))
        }
        Node::Icon { style, name, size } => {
            format!("Icon {:?} size={}{}", doc.string(*name), dim(*size), style_of(style))
        }
        Node::Row { style, gap, padding, target: _, children } => format!(
            "Row gap={} padding={} ({} children){}",
            dim(*gap), dim(*padding), children.len(), style_of(style)
        ),
        Node::Column { style, gap, padding, target: _, children } => format!(
            "Column gap={} padding={} ({} children){}",
            dim(*gap), dim(*padding), children.len(), style_of(style)
        ),
        Node::Rectangle { style, width, height } => {
            format!("Rectangle {}×{}{}", dim(*width), dim(*height), style_of(style))
        }
        Node::Spacer { style, size } => format!("Spacer {}{}", dim(*size), style_of(style)),
        Node::Link { style, label, target } => format!(
            "Link {:?} → {}{}",
            doc.string(*label), doc.string(*target), style_of(style)
        ),
        Node::Scroll { style, .. } => format!("Scroll{}", style_of(style)),
        Node::Chrome { style, .. } => format!("Chrome (window titlebar){}", style_of(style)),
        Node::Button { style, label, icon: _, action } => format!(
            "Button {:?} → action {}{}",
            doc.string(*label), action, style_of(style)
        ),
        Node::TextInput { style, bind, placeholder, action, multiline } => format!(
            "TextInput bind=state[{}] placeholder={:?}{}{}{}",
            bind,
            doc.string(*placeholder),
            if *multiline { " multiline" } else { "" },
            if *action != NO_STYLE { format!(" on_enter=action {action}") } else { String::new() },
            style_of(style)
        ),
        Node::Code { style, bind, lang } => format!(
            "Code bind=state[{}] lang={:?}{}",
            bind,
            doc.string(*lang),
            style_of(style)
        ),
        Node::When { state, invert, .. } => format!(
            "When state[{}]{}", state, if *invert { " (inverted)" } else { "" }
        ),
        Node::Menu { items } => format!(
            "Menu ({} item{})",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ),
        Node::Keys { target } => format!("Keys → {:?}", doc.string(*target)),
        Node::Page { color } => match color {
            rill_doc::ColorRef::Literal(c) => {
                format!("Page background #{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
            }
            rill_doc::ColorRef::Token(idx) => {
                format!("Page background {:?} (token)", doc.string(*idx))
            }
        },
        Node::Live { target, interval } => {
            format!("Live {:?} every {interval}ms", doc.string(*target))
        }
        Node::Closing { target } => format!("Closing → {:?}", doc.string(*target)),
        Node::Sensitive { tier } => format!("Sensitive tier={tier} (records at T{tier})"),
        Node::Key { key, target, action } => format!(
            "Key {:?}{}",
            doc.string(*key),
            if *target != NO_STYLE {
                format!(" target={:?}", doc.string(*target))
            } else {
                format!(" action {action}")
            }
        ),
        Node::Slider { style, bind, min, max, step, action } => format!(
            "Slider bind=state[{}] {min}..{max} step={step}{}{}",
            bind,
            if *action != NO_STYLE { format!(" → action {action}") } else { String::new() },
            style_of(style)
        ),
        Node::UnknownIgnorable { node_type } => {
            format!("(unknown ignorable node {node_type:#06x} — skipped)")
        }
    };
    println!("{indent}[{index}] {described}");
    for &child in node.children() {
        print_tree(doc, child, depth + 1);
    }
}
