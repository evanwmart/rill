//! W1.4b — the glyph atlas: swash-rasterized coverage masks packed into one
//! GPU texture (D3's "our own atlas").
//!
//! Coverage-only (R8): a glyph's color is applied in the shader, so one atlas
//! entry serves every text color. Shelf packing with 1px padding against
//! sampler bleed. Color glyphs (emoji) collapse to their alpha channel for now
//! — monochrome, but correctly shaped; a color atlas page can come later.

use std::collections::HashMap;

use cosmic_text::{CacheKey, CacheKeyFlags, FontSystem, SwashCache, SwashContent, SwashImage};
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform, Vector};

use crate::text::WGHT_AXIS;

/// A glyph is cached per (shaping key, rendered weight): the same glyph at
/// the same size in Regular and in Bold is two different rasters, and
/// cosmic-text's `CacheKey` cannot tell them apart because it does not know
/// the axis moved. Weight `0` means "no variation applied".
type GlyphKey = (CacheKey, u16);

/// Square atlas dimension. 1024² of unique glyph coverage is far beyond what
/// a UI frame needs; see [`GlyphAtlas::slot`] for the overflow policy.
pub(crate) const ATLAS_SIZE: u32 = 1024;

/// Where a rasterized glyph lives in the atlas, plus its raster offsets
/// (`left`/`top` are the swash placement: bearing from the pen position).
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub left: i32,
    pub top: i32,
}

struct Shelf {
    y: u32,
    height: u32,
    used: u32,
}

pub(crate) struct GlyphAtlas {
    texture: wgpu::Texture,
    shelves: Vec<Shelf>,
    next_y: u32,
    /// `None` = rasterized to nothing (whitespace) — cached so we don't retry.
    map: HashMap<GlyphKey, Option<Slot>>,
    swash: SwashCache,
    /// Our own scaler, for the variable-weight path. cosmic-text's
    /// `SwashCache` builds its scaler without variation settings, so it can
    /// only ever produce a font's default instance.
    context: ScaleContext,
}

impl GlyphAtlas {
    pub fn new(device: &wgpu::Device) -> (GlyphAtlas, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas = GlyphAtlas {
            texture,
            shelves: Vec::new(),
            next_y: 0,
            map: HashMap::new(),
            swash: SwashCache::new(),
            context: ScaleContext::new(),
        };
        (atlas, view)
    }

    /// Atlas slot for `key`, rasterizing and uploading on first sight. `None`
    /// for glyphs with no coverage (spaces) or that fail to rasterize.
    ///
    /// Overflow policy: if packing fails the atlas resets and retries once —
    /// crude but bounded, same spirit as the shaper-cache clear. A frame
    /// needing more than the full atlas of *unique* glyphs would draw stale
    /// slots for that frame; unrealistic for UI text, revisit with eviction.
    /// `wght` is the weight to move the font's variation axis to, or `0` to
    /// render its default instance (the right answer for a family that ships
    /// static cuts, where cosmic-text already picked the correct face).
    pub fn slot(
        &mut self,
        fs: &mut FontSystem,
        queue: &wgpu::Queue,
        key: CacheKey,
        wght: u16,
    ) -> Option<Slot> {
        if let Some(hit) = self.map.get(&(key, wght)) {
            return *hit;
        }
        let slot = self.rasterize(fs, queue, key, wght);
        self.map.insert((key, wght), slot);
        slot
    }

    fn rasterize(
        &mut self,
        fs: &mut FontSystem,
        queue: &wgpu::Queue,
        key: CacheKey,
        wght: u16,
    ) -> Option<Slot> {
        let image = match wght {
            0 => self.swash.get_image_uncached(fs, key)?,
            w => variable_image(fs, &mut self.context, key, w)?,
        };
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None;
        }
        // Coverage bytes: Mask is A8 as-is; Color (emoji) collapses to alpha;
        // subpixel masks take one channel (we don't do subpixel AA).
        let data: Vec<u8> = match image.content {
            SwashContent::Mask => image.data,
            SwashContent::Color => image.data.as_chunks::<4>().0.iter().map(|px| px[3]).collect(),
            SwashContent::SubpixelMask => image.data.as_chunks::<4>().0.iter().map(|px| px[1]).collect(),
        };

        let (x, y) = match self.pack(w, h) {
            Some(pos) => pos,
            None => {
                // Full: reset and retry once (see `slot` docs).
                self.shelves.clear();
                self.next_y = 0;
                self.map.clear();
                self.pack(w, h)?
            }
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        Some(Slot { x, y, w, h, left: image.placement.left, top: image.placement.top })
    }

    /// Shelf packing with 1px padding on the trailing edges.
    fn pack(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (pw, ph) = (w + 1, h + 1);
        if pw > ATLAS_SIZE || ph > ATLAS_SIZE {
            return None; // glyph larger than the atlas — give up on it
        }
        for shelf in &mut self.shelves {
            if ph <= shelf.height && shelf.used + pw <= ATLAS_SIZE {
                let pos = (shelf.used, shelf.y);
                shelf.used += pw;
                return Some(pos);
            }
        }
        if self.next_y + ph <= ATLAS_SIZE {
            let pos = (0, self.next_y);
            self.shelves.push(Shelf { y: self.next_y, height: ph, used: pw });
            self.next_y += ph;
            return Some(pos);
        }
        None
    }
}

/// Rasterize one glyph with the font's `wght` axis moved to `weight`.
///
/// This is cosmic-text's own `swash_image` with one line added — the
/// `variations` call — because that is the only thing it is missing and
/// there is no way to reach into `SwashCache` to supply it. Everything else
/// (hinting, source order, subpixel offset, the fake-italic skew) is copied
/// deliberately so a glyph rendered through this path lands in exactly the
/// same place as one rendered through the default path.
///
/// The alternative was shipping static instances of a font we already ship:
/// more bytes in every binary, a build step to generate them, and still only
/// the handful of weights someone thought to generate. Moving the axis costs
/// nothing on disk and gives every weight in the font's range.
fn variable_image(
    fs: &mut FontSystem,
    context: &mut ScaleContext,
    key: CacheKey,
    weight: u16,
) -> Option<SwashImage> {
    let font = fs.get_font(key.font_id)?;
    let font_ref = font.as_swash();
    // Clamp into the axis's own range: asking for 900 of a font that stops
    // at 700 must give its heaviest, not an extrapolation.
    let axis = font_ref.variations().find_by_tag(WGHT_AXIS)?;
    let value = (weight as f32).clamp(axis.min_value(), axis.max_value());

    let mut scaler = context
        .builder(font_ref)
        .size(f32::from_bits(key.font_size_bits))
        .hint(true)
        .variations([(WGHT_AXIS, value)])
        .build();

    let offset = Vector::new(key.x_bin.as_float(), key.y_bin.as_float());
    Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ])
    .format(Format::Alpha)
    .offset(offset)
    .transform(if key.flags.contains(CacheKeyFlags::FAKE_ITALIC) {
        Some(Transform::skew(Angle::from_degrees(14.0), Angle::from_degrees(0.0)))
    } else {
        None
    })
    .render(&mut scaler, key.glyph_id)
}
