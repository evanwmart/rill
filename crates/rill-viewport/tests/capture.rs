//! A page that takes the keyboard, and a page that reloads itself.
//!
//! Both are what a terminal is made of, and neither is a host feature: the
//! document declares them, so the checks here are about the viewport reading
//! declarations rather than about terminals.

use rill_ui::{LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, Fetcher, KeyResult, Source};

struct FixedMeasurer;

impl TextMeasurer for FixedMeasurer {
    fn measure(
        &mut self,
        text: &str,
        font_size: f32,
        _weight: u16,
        _family: &str,
        _max_width: f32,
    ) -> LineMetrics {
        LineMetrics { width: text.chars().count() as f32 * font_size * 0.6, height: font_size * 1.4 }
    }
}

fn view(src: &str, name: &str) -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-capture-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "test".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
    view
}

const CAPTURING: &str = r##"
column {
    keys target="/term/key"
    text "screen"
    link "unreachable" target="/elsewhere"
}
"##;

/// A capturing page gets the keys that would otherwise have moved focus or
/// activated something — that is the whole point — and the host keeps
/// `ctrl+shift+*`, so there is always a way back out.
#[test]
fn a_capturing_page_takes_the_keyboard_but_not_the_reserved_combinations() {
    let mut v = view(CAPTURING, "keys");
    assert!(v.captures_keys(), "the page asked for the keyboard");

    // Tab would normally move focus and Enter would then follow the button's
    // link. Under capture both go to the page, so no link ever comes back.
    assert!(matches!(v.on_key("tab", None, false, false, false), KeyResult::Handled));
    assert!(
        matches!(v.on_key("enter", None, false, false, false), KeyResult::Handled),
        "Enter went to the page, not to a focused button"
    );

    // Ctrl+Shift is the host's, whatever the page wants: it falls through
    // capture untouched, so an unbound one is simply not consumed.
    assert_eq!(
        v.on_key("q", None, true, true, false),
        KeyResult::Ignored,
        "ctrl+shift is never swallowed by a capturing page"
    );
}

/// Without the declaration nothing changes: Tab still walks focus. The
/// capture is a property of the document, not a mode the viewer is left in.
#[test]
fn a_page_that_did_not_ask_keeps_the_ordinary_bindings() {
    let src = r##"
column {
    text "screen"
    link "one" target="/a"
}
"##;
    let mut v = view(src, "nokeys");
    assert!(!v.captures_keys());
    v.on_key("tab", None, false, false, false);
    assert_eq!(
        v.on_key("enter", None, false, false, false),
        KeyResult::Link("/a".into()),
        "Tab moved focus and Enter followed the button, as they always did"
    );
}

// ---- live pages that fail, and pages the user is halfway through ---------
//
// These use `Source::Local` rather than `Generated`, because the questions
// are about what happens when a live *fetch* goes wrong or brings back
// something different: a generated source re-serves the same bytes forever
// and can never fail.

struct Served {
    dir: std::path::PathBuf,
}

impl Served {
    fn new(name: &str) -> Served {
        let dir = std::env::temp_dir()
            .join(format!("viewport-live-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("serve dir");
        Served { dir }
    }

    /// Write the page that lives at `/page`.
    fn put(&self, src: &str) {
        let bytes = rill_doc::compile(src).expect("compiles").bytes;
        std::fs::write(self.dir.join("page"), bytes).expect("write page");
    }

    fn remove(&self) {
        std::fs::remove_file(self.dir.join("page")).expect("remove page");
    }

    fn view(&self) -> AppView {
        let fetcher = Fetcher::new(self.dir.clone(), None, self.dir.clone()).expect("fetcher");
        let mut view = AppView::new(
            fetcher,
            Source::Local { dir: self.dir.clone(), path: "/page".into() },
        );
        settle(&mut view);
        view
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn state_str(view: &AppView, name: &str) -> Option<String> {
    match view.state_value(name) {
        Some(rill_ui::ActionValue::Str(s)) => Some(s),
        _ => None,
    }
}

fn state_bool(view: &AppView, name: &str) -> Option<bool> {
    match view.state_value(name) {
        Some(rill_ui::ActionValue::Bool(b)) => Some(b),
        _ => None,
    }
}

fn settle(view: &mut AppView) {
    for _ in 0..400 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
}

/// Wait out the live interval, then let one tick run to completion.
fn tick(view: &mut AppView) {
    std::thread::sleep(std::time::Duration::from_millis(40));
    view.poll();
    settle(view);
}

const CLOCK: &str = r##"
column {
    live target="/page" every=20
    text "tick"
}
"##;

/// A failed live tick must not cost the page. The error document carries no
/// `live` node, so replacing the page with one used to withdraw the clock —
/// and a widget whose server blinked stayed frozen on an error until somebody
/// reloaded it by hand.
#[test]
fn a_failed_live_tick_keeps_the_page_and_the_clock() {
    let served = Served::new("survives");
    served.put(CLOCK);
    let mut v = served.view();
    assert!(v.live_interval().is_some(), "the page declared a clock");
    assert!(!v.live_stale());

    // The server goes away mid-session.
    served.remove();
    tick(&mut v);
    assert!(v.live_stale(), "the view knows its content is older than intended");
    assert!(
        v.live_interval().is_some(),
        "the clock survived the failure — otherwise nothing would ever retry"
    );
    assert!(v.error().is_none(), "a blink is not an error page");

    // And it comes back on its own when the server does.
    served.put(CLOCK);
    for _ in 0..40 {
        tick(&mut v);
        if !v.live_stale() {
            break;
        }
    }
    assert!(!v.live_stale(), "the page recovered without anyone reloading it");
}

/// A live page that fails on its *first* load has no page to keep, so it
/// still shows the error. Nothing about the fix hides a genuine failure.
#[test]
fn a_live_page_that_never_loaded_still_reports_the_failure() {
    let served = Served::new("first");
    let v = served.view(); // nothing written: the first fetch fails
    assert!(v.error().is_some(), "a page that never arrived is an error");
}

const FORM: &str = r##"
state "draft" initial=""
state "shown" initial=#false
column {
    live target="/page" every=20
    text_input bind="draft"
    button "toggle" { toggle "shown" }
}
"##;

/// The correctness fix: a live refresh rebuilds state from the new
/// document's initials, which is right for everything the server owns and
/// wrong for the one thing it does not — what the user is in the middle of
/// typing. A terminal refreshes twenty times a second; a form on the same
/// clock used to be unusable.
#[test]
fn typing_survives_a_live_refresh_and_untouched_slots_take_the_server_value() {
    let served = Served::new("staged");
    served.put(FORM);
    let mut v = served.view();

    // Focus the input and type. (Tab reaches it: it is the first focusable.)
    v.on_key("tab", None, false, false, false);
    for ch in ["h", "i"] {
        v.on_key(ch, Some(ch), false, false, false);
    }
    assert_eq!(state_str(&v, "draft").as_deref(), Some("hi"), "typed into the slot");

    // The page re-serves itself, unchanged, on its clock.
    tick(&mut v);
    assert_eq!(
        state_str(&v, "draft").as_deref(),
        Some("hi"),
        "the half-typed value survived the refresh"
    );

    // A slot nobody has touched takes whatever the server now says.
    let moved = FORM.replace("state \"draft\" initial=\"\"", "state \"draft\" initial=\"server\"")
        .replace("state \"shown\" initial=#false", "state \"shown\" initial=#true");
    served.put(&moved);
    tick(&mut v);
    assert_eq!(
        state_str(&v, "draft").as_deref(),
        Some("hi"),
        "still the user's — they are mid-edit, and the server does not own this"
    );
    assert_eq!(
        state_bool(&v, "shown"),
        Some(true),
        "the untouched slot took the server's new value"
    );
}

/// Navigating away is a different world: nothing staged against the old page
/// leaks into the new one.
#[test]
fn navigation_drops_staged_values() {
    let served = Served::new("nav");
    served.put(FORM);
    let mut v = served.view();
    v.on_key("tab", None, false, false, false);
    v.on_key("x", Some("x"), false, false, false);
    assert_eq!(state_str(&v, "draft").as_deref(), Some("x"));

    // Same document, reached as a navigation rather than a refresh.
    v.navigate("/page");
    settle(&mut v);
    assert_eq!(
        state_str(&v, "draft").as_deref(),
        Some(""),
        "a fresh navigation starts from the document's declared initials"
    );
}

/// A fetch in flight is *pending*, not *changed*. The distinction is the
/// whole reason `poll` returns two facts instead of one: hosts repaint on
/// `changed`, and a fetch that has not arrived yet has altered nothing on
/// screen. Conflating them cost 2.4 client commits per real update on the
/// Pi, each one a full composite for content that had not landed.
#[test]
fn a_fetch_in_flight_is_pending_but_not_changed() {
    let served = Served::new("pending");
    served.put(CLOCK);
    let mut v = served.view();

    // Wait out the clock, then step once: this starts a fetch and cannot
    // possibly have received it in the same call.
    std::thread::sleep(std::time::Duration::from_millis(40));
    let started = v.poll();
    assert!(!started.changed, "nothing on screen changed by starting a fetch");
    assert!(started.pending, "but there is work outstanding");

    // Let it land. The step that applies it is the one that changed.
    let mut saw_change = false;
    for _ in 0..400 {
        let p = v.poll();
        if p.changed {
            saw_change = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(saw_change, "the arriving document is a change");

    // And once settled, a page between ticks asks for nothing at all.
    v.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
    let quiet = v.poll();
    assert!(!quiet.changed && !quiet.pending, "settled between ticks: {quiet:?}");
}

/// Smooth scrolling is the other half of the split: the content moves, so it
/// *is* a change and must keep frames coming, or motion would stutter.
#[test]
fn scroll_easing_reports_changed_so_motion_keeps_its_frames() {
    let served = Served::new("easing");
    // A page tall enough to scroll.
    let mut tall = String::from("column {\n");
    for i in 0..200 {
        tall.push_str(&format!("    text \"line {i}\"\n"));
    }
    tall.push('}');
    served.put(&tall);
    let mut v = served.view();
    v.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);

    // Negative delta scrolls *down*: `scroll_by` subtracts, so a positive
    // value from a scroll offset of zero clamps straight back to zero.
    v.scroll_by(-600.0);
    let p = v.poll();
    assert!(p.changed, "easing toward the target moves the picture: {p:?}");
}

/// A live page hands the host its cadence, and drops it again the moment the
/// page stops asking — otherwise a document could leave a client polling
/// forever after navigating away.
#[test]
fn a_live_page_publishes_its_interval_and_gives_it_back() {
    let src = r##"
column {
    live target="/term" every=50
    text "screen"
}
"##;
    let mut v = view(src, "live");
    assert_eq!(v.live_interval(), Some(std::time::Duration::from_millis(50)));

    let quiet = rill_doc::compile("column { text \"still\" }").expect("compiles").bytes;
    v.reload_keep_focus(Source::Generated { label: "quiet".into(), bytes: quiet });
    for _ in 0..200 {
        v.poll();
        if !v.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    v.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
    assert_eq!(v.live_interval(), None, "the clock belonged to the page, not the viewer");
}

/// The host loop, simulated: sleep exactly as long as the view says, poll,
/// repeat. A page asking for 20ms should tick ~50 times a second; the
/// workstation measured a widget asking for 80ms ticking at half its rate,
/// and the loop's sleep is the only thing between the two.
#[test]
fn a_live_page_ticks_at_the_rate_it_asked_for() {
    let served = Served::new("cadence");
    served.put(
        "column {\n    live target=\"/page\" every=20\n    text \"tick\"\n}",
    );
    let mut v = served.view();

    let started = std::time::Instant::now();
    let mut ticks = 0;
    while started.elapsed() < std::time::Duration::from_millis(600) {
        // Exactly what platform/rill-vector's loop does: ask the view how long it
        // may sleep, sleep that long, then step.
        let wait = v
            .next_tick_in()
            .map(|d| d.as_millis().clamp(1, 100) as u64)
            .unwrap_or(100);
        std::thread::sleep(std::time::Duration::from_millis(wait));
        let p = v.poll();
        if p.changed {
            ticks += 1;
        }
        // A pending fetch is checked promptly rather than slept through.
        if p.pending {
            std::thread::sleep(std::time::Duration::from_millis(2));
            if v.poll().changed {
                ticks += 1;
            }
        }
    }
    let rate = ticks as f64 / started.elapsed().as_secs_f64();
    assert!(
        rate > 30.0,
        "a 20ms page should tick ~50/s, measured {rate:.1}/s ({ticks} ticks)"
    );
}

/// The document's declared tier reaches the host through the view — the leg
/// rill-vector reads before deciding what `set_tier` to send with the frame.
#[test]
fn the_view_reports_the_documents_tier() {
    let plain = rill_doc::compile("column { text \"hello\" }").unwrap().bytes;
    let mut v = AppView::new(
        Fetcher::new(std::env::temp_dir(), None, std::env::temp_dir()).unwrap(),
        Source::Generated { label: "t".into(), bytes: plain },
    );
    for _ in 0..100 {
        v.poll();
        if !v.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(v.tier(), 0, "an undeclared page is routine");

    let sealed =
        rill_doc::compile("column { text \"recovery phrase\"; sensitive tier=2 }").unwrap().bytes;
    let mut v = AppView::new(
        Fetcher::new(std::env::temp_dir(), None, std::env::temp_dir()).unwrap(),
        Source::Generated { label: "t2".into(), bytes: sealed },
    );
    for _ in 0..100 {
        v.poll();
        if !v.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(v.tier(), 2, "the declaration reached the host");
}
