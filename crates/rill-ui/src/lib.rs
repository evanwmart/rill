//! The shared Rill UI core (plan § Application Phase 2).
//!
//! Pipeline: `rill-doc Document → resolved UI tree → layout → DrawCommand
//! list`. This crate is backend-agnostic: it needs a [`TextMeasurer`] from
//! the backend and emits [`DrawCommand`]s for the backend to paint. No
//! window, no GPU, no fonts here — that keeps layout unit-testable with a
//! mock measurer and identical across every backend.

pub mod icons;
pub mod code;
mod layout;
pub mod recording;
pub mod text;
mod tree;

pub use layout::{
    ImageSizer, LayoutOptions, LineMetrics, NoImages, TextMeasurer, layout_chrome, layout_document_with_scroll,
    layout_document,
};
pub use tree::{Defaults, ResolvedNode, ResolvedStyle, UiTree, resolve};

pub use rill_doc::Dimension;
// The draw vocabulary lives one crate down, so that things which *keep*
// frames (the history log) need not depend on the engine that makes them.
// Re-exported wholesale — `rill_ui::DrawCommand` and `rill_ui::stream::…`
// still name the same types they always did.
pub use rill_draw::{
    ActionValue, Color, DrawCommand, MenuItem, MIN_LIVE_INTERVAL_MS, Point, Rect, UiAction, stream,
};
