//! Client side of `rill_stream_v1` (protocols/rill-stream-v1.xml).
#![allow(missing_docs, clippy::all)]

// The generated modules reference these names via `super::`.
#[allow(clippy::single_component_path_imports)]
use wayland_client;

pub mod __interfaces {
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!("../../protocols/rill-stream-v1.xml");
}
use self::__interfaces::*;
use wayland_client::protocol::*;

wayland_scanner::generate_client_code!("../../protocols/rill-stream-v1.xml");
