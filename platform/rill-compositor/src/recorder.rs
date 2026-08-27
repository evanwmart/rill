//! Session recording: the compositor as the authority on what the desktop
//! looked like, written out as `.rillrec` semantic events (rill_ui::recording).
//!
//! Two kinds of hook feed this. Frames are *transient* — a command-stream blob
//! exists only between attach and latch — so they are pushed in at the latch
//! point, verbatim, exactly as the client sent them. Everything else (geometry,
//! stacking, titles, appearance and disappearance) is *state*, so the render
//! loop hands over the whole window list once a tick and the recorder diffs it,
//! emitting an event only where something actually changed. Diffing beats
//! hooking each mutation site: move grabs, resize anchoring, map/unmap, reflow
//! and raise are many places to instrument and easy to add a new one to
//! without noticing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rill_ui::recording::{RecEvent, Stamped, write_event, write_header};

/// One window as the recorder last saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub title: String,
    /// False for pixel windows — they can't be recorded semantically and
    /// replay as placeholders.
    pub vector: bool,
    /// The surface's latched sensitivity tier. The `.rillrec` demo format
    /// ignores it; the history writer stamps this window's events with it.
    pub tier: u8,
    /// The app id, for history's window record and the owner's tier pins.
    /// The `.rillrec` demo format ignores this too.
    pub app: String,
}

pub struct Recorder {
    out: BufWriter<File>,
    path: PathBuf,
    start: Instant,
    windows: HashMap<u32, Snapshot>,
    /// Stacking order, bottom → top, as last written.
    order: Vec<u32>,
    pointer: Option<(f32, f32)>,
    /// First write error, if any. A recording that hits a full disk stops
    /// recording; it does not report the same failure once a frame. (A
    /// per-iteration log on a dead output is exactly what cost us a day —
    /// see the rill-vector spin fix.)
    failed: Option<String>,
}

impl Recorder {
    /// Begin a recording of an output of this size.
    pub fn start(path: &Path, width: u32, height: u32) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = BufWriter::new(File::create(path)?);
        write_header(&mut out, width, height)?;
        Ok(Self {
            out,
            path: path.to_path_buf(),
            start: Instant::now(),
            windows: HashMap::new(),
            order: Vec::new(),
            pointer: None,
            failed: None,
        })
    }

    /// Milliseconds since the recording began, saturating — a recording long
    /// enough to overflow u32 (49 days) has bigger problems than a wrapped
    /// timestamp.
    fn now(&self) -> u32 {
        self.start.elapsed().as_millis().min(u32::MAX as u128) as u32
    }

    fn emit(&mut self, event: RecEvent) {
        if self.failed.is_some() {
            return;
        }
        let stamped = Stamped { t_ms: self.now(), event };
        if let Err(e) = write_event(&mut self.out, &stamped) {
            self.failed = Some(e.to_string());
        }
    }

    /// A latched command-stream frame, stored as the client sent it. The blob
    /// is already a valid `rill_ui::stream` encoding — the compositor decoded
    /// it to display it — so replay needs no re-encoding and loses nothing.
    pub fn frame(&mut self, id: u32, bytes: Vec<u8>) {
        self.emit(RecEvent::Frame { id, bytes });
    }

    /// Hand over the live window list, bottom → top. Emits upserts for what
    /// changed, `Closed` for what went away, and the stacking order when it
    /// differs.
    pub fn sync(&mut self, live: &[Snapshot]) {
        for snap in live {
            if self.windows.get(&snap.id) != Some(snap) {
                self.windows.insert(snap.id, snap.clone());
                self.emit(RecEvent::Window {
                    id: snap.id,
                    x: snap.x,
                    y: snap.y,
                    w: snap.w,
                    h: snap.h,
                    title: snap.title.clone(),
                    vector: snap.vector,
                });
            }
        }
        let gone: Vec<u32> = self
            .windows
            .keys()
            .copied()
            .filter(|id| !live.iter().any(|s| s.id == *id))
            .collect();
        for id in gone {
            self.windows.remove(&id);
            self.emit(RecEvent::Closed { id });
        }
        let order: Vec<u32> = live.iter().map(|s| s.id).collect();
        if order != self.order {
            self.order = order.clone();
            self.emit(RecEvent::Order { ids: order });
        }
    }

    /// Pointer position, deduplicated — an idle pointer writes nothing.
    pub fn pointer(&mut self, x: f32, y: f32) {
        if !x.is_finite() || !y.is_finite() || self.pointer == Some((x, y)) {
            return;
        }
        self.pointer = Some((x, y));
        self.emit(RecEvent::Pointer { x, y });
    }

    /// Close the recording. Returns the path, plus the first write error if
    /// one happened — the file is still there and still decodes up to the
    /// last whole event, which is the point of an append-only log.
    pub fn finish(mut self) -> (PathBuf, Option<String>) {
        if let Err(e) = self.out.flush()
            && self.failed.is_none()
        {
            self.failed = Some(e.to_string());
        }
        (self.path, self.failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_ui::recording::decode;

    fn snap(id: u32, x: i32, title: &str) -> Snapshot {
        Snapshot { id, x, y: 0, w: 100, h: 80, title: title.into(), vector: true, tier: 0, app: String::new() }
    }

    /// The diff emits on change and stays silent otherwise — the property the
    /// whole design rests on, since the render loop calls sync() every tick.
    #[test]
    fn sync_emits_only_real_changes() {
        let dir = std::env::temp_dir().join(format!("rillrec-test-{}", std::process::id()));
        let path = dir.join("a.rillrec");
        let mut rec = Recorder::start(&path, 1280, 800).unwrap();

        rec.sync(&[snap(1, 10, "One")]);
        rec.sync(&[snap(1, 10, "One")]); // unchanged — silent
        rec.sync(&[snap(1, 10, "One"), snap(2, 20, "Two")]); // new window + order
        rec.sync(&[snap(1, 15, "One"), snap(2, 20, "Two")]); // window 1 moved
        rec.sync(&[snap(2, 20, "Two")]); // window 1 closed + order
        rec.pointer(5.0, 6.0);
        rec.pointer(5.0, 6.0); // same spot — silent
        let (path, failed) = rec.finish();
        assert!(failed.is_none(), "write failed: {failed:?}");

        let (w, h, events) = decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((w, h), (1280, 800));
        let shapes: Vec<String> = events
            .iter()
            .map(|s| match &s.event {
                RecEvent::Window { id, x, .. } => format!("window{id}@{x}"),
                RecEvent::Closed { id } => format!("closed{id}"),
                RecEvent::Order { ids } => format!("order{ids:?}"),
                RecEvent::Pointer { .. } => "pointer".into(),
                RecEvent::Frame { id, .. } => format!("frame{id}"),
            })
            .collect();
        assert_eq!(
            shapes,
            [
                "window1@10",
                "order[1]",
                "window2@20",
                "order[1, 2]",
                "window1@15",
                "closed1",
                "order[2]",
                "pointer",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Frames go in verbatim: what the client sent is what replay gets.
    #[test]
    fn frames_are_stored_verbatim() {
        let dir = std::env::temp_dir().join(format!("rillrec-frame-{}", std::process::id()));
        let path = dir.join("b.rillrec");
        let blob = rill_ui::stream::encode(&[rill_ui::DrawCommand::Rect {
            rect: rill_ui::Rect { x: 1.0, y: 2.0, w: 3.0, h: 4.0 },
            color: rill_ui::Color { r: 9, g: 8, b: 7, a: 255 },
            corner_radius: 2.0,
        }])
        .unwrap();

        let mut rec = Recorder::start(&path, 800, 600).unwrap();
        rec.frame(7, blob.clone());
        let (path, failed) = rec.finish();
        assert!(failed.is_none());

        let (_, _, events) = decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].event {
            RecEvent::Frame { id, bytes } => {
                assert_eq!(*id, 7);
                assert_eq!(bytes, &blob, "frame blob changed in transit");
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
