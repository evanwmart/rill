//! Resize against a *real* shell.
//!
//! Every reflow bug in this terminal so far was found by reasoning backwards
//! from a photograph of a window. This drives the real thing instead: a real
//! bash on a real pty, its bytes through the real parser, resized the way
//! the live app resizes — the grid immediately, the shell's signal only once
//! the window has been still.
//!
//! Ignored by default: it spawns a shell and waits on it in real time.

use std::time::{Duration, Instant};

use term_app::testing::{Harness, PROMPT};

fn settle(h: &mut Harness) {
    let deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < deadline {
        h.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A drag in and back out leaves one prompt per command, and the output
/// above it intact.
#[test]
#[ignore = "spawns a real shell"]
fn dragging_a_real_shell_leaves_one_prompt() {
    let mut h = Harness::new(24, 110);
    settle(&mut h);
    h.run("printf 'alpha %s\\n' one two three");
    settle(&mut h);

    for w in [96, 72, 54, 40, 30, 40, 54, 72, 96, 110, 130, 88, 44, 130] {
        h.resize(24, w);
        for _ in 0..8 {
            h.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    settle(&mut h);

    let lines = h.transcript();
    let shown = lines.join("\n");
    // One prompt for the command that was run, one waiting at the end.
    let prompts = lines.iter().filter(|l| l.contains(PROMPT.trim_end())).count();
    assert_eq!(prompts, 2, "expected 2 prompts, found {prompts}:\n{shown}");
    for want in ["alpha one", "alpha two", "alpha three"] {
        assert!(
            lines.iter().any(|l| l.trim() == want),
            "output line {want:?} did not survive the drag:\n{shown}"
        );
    }
}

/// The reported case, run for real: a screenful of positioned, coloured
/// output (fastfetch is what the photograph showed), then a drag in and
/// back out. Nothing may duplicate.
#[test]
#[ignore = "spawns a real shell"]
fn dragging_after_a_screenful_of_output_leaves_one_prompt() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v fastfetch"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("no fastfetch on this machine; skipping");
        return;
    }
    let mut h = Harness::new(30, 150);
    settle(&mut h);
    h.run("fastfetch");
    settle(&mut h);
    settle(&mut h);

    for w in [130, 100, 70, 44, 32, 44, 70, 100, 130, 150] {
        h.resize(30, w);
        for _ in 0..8 {
            h.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    settle(&mut h);

    let lines = h.transcript();
    let shown = lines.join("\n");
    let prompts = lines.iter().filter(|l| l.contains(PROMPT.trim_end())).count();
    assert_eq!(prompts, 2, "expected 2 prompts, found {prompts}:\n{shown}");
    // And no row carries the prompt twice — the glue in the photograph.
    for l in &lines {
        assert!(
            l.matches(PROMPT.trim_end()).count() <= 1,
            "a row carries the prompt more than once: {l:?}"
        );
    }
}


/// vim, for real: it probes the terminal at startup (device attributes,
/// cursor position) and waits on the answers, then sets the alternate
/// screen and DECCKM. Before the terminal answered probes, vim stalled;
/// while arrows were CSI-encoded under DECCKM, they typed letters. This is
/// the test that says "a modal editor is usable in this terminal".
#[test]
#[ignore = "spawns a real shell"]
fn vim_starts_probes_and_takes_arrow_keys() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v vim"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("no vim on this machine; skipping");
        return;
    }
    let mut h = Harness::new(24, 90);
    settle(&mut h);
    h.run("vim -u NONE -N");
    // Wait for the editor to arrive: alternate screen up.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.on_alt() && Instant::now() < deadline {
        h.pump();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(h.on_alt(), "vim never reached the alternate screen");
    settle(&mut h);
    let drawn = h.screen_lines();
    assert!(
        drawn.iter().filter(|l| l.trim() == "~").count() > 5,
        "vim's empty-buffer tildes are not on screen:\n{}",
        drawn.join("\n")
    );

    // Type a few lines, leave insert mode, and move with arrows — under
    // DECCKM these go as SS3, and getting that wrong types letters.
    h.pty_write(b"ialpha\rbravo\rcharlie\x1b");
    settle(&mut h);
    h.key("up");
    h.key("up");
    h.pty_write(b"Iup-");
    h.pty_write(b"\x1b");
    settle(&mut h);
    let lines = h.screen_lines();
    assert!(
        lines.iter().any(|l| l == "up-alpha"),
        "two Ups from charlie land on alpha:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines.iter().any(|l| l.contains("OA") || l.contains("[A")),
        "an arrow key typed its own escape bytes:\n{}",
        lines.join("\n")
    );

    // Leave vim; the shell comes back off the alternate screen.
    h.pty_write(b":q!\r");
    settle(&mut h);
    assert!(!h.on_alt(), "vim left but the alternate screen stayed");
}
