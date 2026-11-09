//! Helpers shared by the case files.
//!
//! The font crate has no `Encoder` of its own: a backend decides what a shape becomes.
//! The default one, [`font::pathfinder_impl::OutlineEncoder`], is what the cases use, so
//! every glyph is observed as a pathfinder `Outline`, exactly like the old non-generic
//! `Glyph::path`.

#![allow(dead_code)]

pub use font::pathfinder_impl::{flatten, OutlineEncoder, PathBuilder};
pub use pathfinder_content::outline::Outline;

/// The outline of one glyph, with any `Shape::Compound` components composed.
pub fn outline<F>(font: &F, id: font::GlyphId) -> Outline
where
    F: font::Font<OutlineEncoder> + ?Sized,
{
    flatten(font, id).expect("glyph exists")
}

/// Every on-curve point of an outline, in contour order.
pub fn points(outline: &Outline) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    for contour in outline.contours() {
        for point in contour.points() {
            out.push((point.x(), point.y()));
        }
    }
    out
}
