use std::collections::HashMap;
use std::iter;
use std::ops::Deref;
use crate::opentype::post::parse_post;
use crate::{Font, Glyph, R, IResultExt, GlyphId, Name, Info, VMetrics, FontError};
use crate::parsers::{iterator, parse};
use pdf_encoding::Encoding;
use nom::{
    number::complete::{be_u8, be_i8, be_i16, be_u16},
    bytes::complete::take,
    sequence::tuple
};
use pathfinder_content::outline::{Outline, Contour};
use pathfinder_geometry::{vector::Vector2F, transform2d::Transform2F, rect::RectF};
use crate::opentype::{
    parse_tables, parse_head, parse_maxp, parse_loca,
    parse_hhea, parse_hmtx, parse_name, Hmtx, Tables,
    cmap::{CMap, parse_cmap},
    kern::{parse_kern},
    gpos::KernTable,
    os2::parse_os2,
};
use pathfinder_geometry::{transform2d::Matrix2x2F};
use itertools::Itertools;

/// Where one component of a composite glyph (OT §glyf, "Composite glyph description") sits.
#[derive(Copy, Clone, Debug)]
pub enum ComponentOffset {
    /// `ARGS_ARE_XY_VALUES` is set: the two arguments are an x/y offset.
    Vector(Vector2F),
    /// Otherwise the arguments are point numbers (§glyf): the component's point `child` is
    /// moved onto point `parent` of the outline built from the preceding components.
    MatchPoints { parent: u16, child: u16 },
}

/// One component of a composite glyph: glyph id, the 2×2 transform that applies to its
/// coordinates, and where the component is placed.
#[derive(Copy, Clone, Debug)]
pub struct Component {
    pub gid: u32,
    pub matrix: Matrix2x2F,
    pub offset: ComponentOffset,
    /// `SCALED_COMPONENT_OFFSET` (0x0800): the placement offset is part of the component's
    /// own design space and is scaled by `matrix`, as Apple's rasterizer does it. Without the
    /// flag — and without `UNSCALED_COMPONENT_OFFSET` (0x1000) too, which only exists to say
    /// so explicitly — the offset is in the composite's coordinates and applied after the
    /// matrix, the Microsoft default (MS §glyf, "Composite glyph description").
    pub scaled_offset: bool,
}

#[derive(Clone)]
pub enum Shape {
    /// The outline and, beside it, the glyph's own points exactly as `glyf` numbers them
    /// (continuously over all contours, no implied or closing points): the outline holds the
    /// converted curves, which cannot be mapped back to the point numbers that composite
    /// glyphs place their components by.
    Simple(Outline, Vec<Vector2F>),
    Compound(Vec<Component>),
    Empty
}

#[derive(Clone)]
pub struct TrueTypeFont {
    shapes: Vec<Shape>,
    pub cmap: Option<CMap>,
    encoding: Option<Encoding>,
    hmtx: Hmtx,
    num_glyphs: u32,
    units_per_em: u16,
    bbox: RectF,
    kern: KernTable,
    name: Name,
    info: Info,
    vmetrics: Option<VMetrics>,
    pub name_map: HashMap<String, u16>,
}

impl TrueTypeFont {
    pub fn parse(data: &[u8]) -> Result<Self, FontError> {
        let tables = parse_tables(data)?;
        TrueTypeFont::parse_glyf(tables)
    }
    pub fn parse_glyf(tables: Tables<impl Deref<Target=[u8]>>) -> Result<Self, FontError> {
        let head = parse_head(expect!(tables.get(b"head"), "no head"))?;
        let maxp = parse_maxp(expect!(tables.get(b"maxp"), "no maxp"))?;
        let loca = parse_loca(expect!(tables.get(b"loca"), "no loca"), &head, &maxp)?;
        let glyf = expect!(tables.get(b"glyf"), "no glyf");
        let hhea = parse_hhea(expect!(tables.get(b"hhea"), "no hhea"))?;
        let hmtx = parse_hmtx(expect!(tables.get(b"hmtx"), "no hmtx"), &hhea, &maxp)?;
        
        let shapes = parse_shapes(&loca, glyf)?;
        
        TrueTypeFont::from_shapes_and_metrics(tables, shapes, hmtx)
    }
    pub fn from_shapes_and_metrics(tables: Tables<impl Deref<Target=[u8]>>, shapes: Vec<Shape>, hmtx: Hmtx) -> Result<TrueTypeFont, FontError> {
        let head = parse_head(expect!(tables.get(b"head"), "no head"))?;
        // OT §maxp: this is the authoritative glyph count; `loca` and `post` have to agree
        // with it, and it must not silently differ from what the shapes contain.
        let maxp = parse_maxp(expect!(tables.get(b"maxp"), "no maxp"))?;
        if shapes.len() != maxp.num_glyphs as usize {
            warn!("glyf/loca describe {} glyphs, maxp says {}", shapes.len(), maxp.num_glyphs);
        }
        let (cmap, encoding) = match tables.get(b"cmap").map(|data| parse_cmap(data)).transpose()? {
            Some((cmap, encoding)) => (Some(cmap), Some(encoding)),
            None => (None, None)
        };
        let name = tables.get(b"name").map(|data| parse_name(data)).transpose()?.unwrap_or_default();
        let os2 = tables.get(b"OS/2").map(|data| parse_os2(data)).transpose()?;
        // OT §OS/2: with `fsSelection` bit 7 (USE_TYPO_METRICS) the `sTypo*` values are
        // the line metrics; without it applications lay out with `hhea`.
        let hhea = tables.get(b"hhea").map(parse_hhea).transpose()?;
        let vmetrics = os2.as_ref().and_then(|os2| os2.typo_metrics())
            .or_else(|| hhea.map(|hhea| hhea.into()));

        let post = t!(tables.get(b"post").map(parse_post).transpose());
        let mut name_map = HashMap::new();
        if let Some(post) = post {
            name_map.extend(post.names.into_iter().enumerate().map(|(i, name)| (name.into(), i as u16)));
        }
        
        Ok(TrueTypeFont {
            shapes,
            cmap,
            encoding,
            hmtx,
            num_glyphs: maxp.num_glyphs as u32,
            units_per_em: head.units_per_em,
            bbox: head.bbox(),
            kern: tables.get(b"kern").map(|data| parse_kern(data)).transpose()?.unwrap_or_default(),
            name,
            info: Info {
                weight: os2.as_ref().map(|t| t.weight),
                x_height: os2.as_ref().and_then(|t| t.x_height).map(|height| height as f32),
                cap_height: os2.as_ref().and_then(|t| t.cap_height).map(|height| height as f32),
                unicode_ranges: os2.as_ref().map(|t| t.unicode_ranges).unwrap_or_default(),
            },
            vmetrics,
            name_map,
        })
    }
    fn get_path(&self, idx: u32) -> Option<Outline> {
        get_outline(&self.shapes, idx)
    }
}
impl Font for TrueTypeFont {
    fn num_glyphs(&self) -> u32 {
        // OT §maxp, not `shapes.len()`: both `OpenTypeFont` and `TrueTypeFont` must answer
        // the same number for the same font.
        self.num_glyphs
    }
    fn font_matrix(&self) -> Transform2F {
        let scale = 1.0 / self.units_per_em as f32;
        Transform2F::from_scale(Vector2F::splat(scale.into()))
    }
    fn glyph(&self, id: GlyphId) -> Option<Glyph> {
        if id.0 > u16::max_value() as u32 {
            return None;
        }
        
        debug!("get gid {:?}", id);
        let path = self.get_path(id.0)?;
        let metrics = self.hmtx.metrics_for_gid(id.0 as u16)?;
        
        Some(Glyph {
            path,
            metrics
        })
    }
    fn is_empty_glyph(&self, gid: GlyphId) -> bool {
        if gid.0 > u16::max_value() as u32 {
            return true;
        }
        match self.shapes.get(gid.0 as usize) {
            None => true,
            Some(Shape::Empty) => true,
            _ => false
        }
    }
    fn gid_for_codepoint(&self, codepoint: u32) -> Option<GlyphId> {
        match self.cmap {
            Some(ref cmap) => cmap.get_codepoint(codepoint).map(GlyphId),
            None => None
        }
    }
    fn gid_for_unicode_codepoint(&self, codepoint: u32) -> Option<GlyphId> {
        match (self.cmap.as_ref(), self.encoding) {
            (Some(cmap), Some(Encoding::Unicode)) => cmap.get_codepoint(codepoint).map(GlyphId),
            (Some(cmap), _) => self.gid_for_codepoint(codepoint),
            _ => None
        }
    }
    fn encoding(&self) -> Option<Encoding> {
        self.encoding
    }
    fn bbox(&self) -> Option<RectF> {
        Some(self.bbox)
    }
    fn kerning(&self, left: GlyphId, right: GlyphId) -> f32 {
        self.kern.get(left.0 as u16, right.0 as u16).unwrap_or(0) as f32
    }
    fn name(&self) -> &Name {
        &self.name
    }
    fn info(&self) -> &Info {
        &self.info
    }
    fn vmetrics(&self) -> Option<VMetrics> {
        self.vmetrics
    }
}

#[inline]
fn vec_i8(i: &[u8]) -> R<Vector2F> {
    let (i, x) = be_i8(i)?;
    let (i, y) = be_i8(i)?;
    Ok((i, Vector2F::new(x as f32, y as f32)))
}
#[inline]
fn vec_i16(i: &[u8]) -> R<Vector2F> {
    let (i, x) = be_i16(i)?;
    let (i, y) = be_i16(i)?;
    Ok((i, Vector2F::new(x as f32, y as f32)))
}
#[inline]
fn fraction_i16(i: &[u8]) -> R<f32> {
    let (i, s) = be_i16(i)?;
    Ok((i, s as f32 / 16384.0))
}

pub fn parse_shapes(loca: &[u32], data: &[u8]) -> Result<Vec<Shape>, FontError> {
    // OT §loca: the last offset is the length of `glyf`; anything larger points outside it.
    // Only the glyphs past the end are affected and they are read as empty below, so a font
    // that overshoots still renders the rest.
    if let Some(&end) = loca.last() {
        if end as usize > data.len() {
            warn!("loca: last offset {} is past glyf's {} bytes — those glyphs stay empty",
                end, data.len());
        }
    }
    let mut shapes = Vec::with_capacity(loca.len() - 1);
    for (i, (start, end)) in loca.iter().cloned().tuple_windows().enumerate() {
        // OT §glyf: one corrupt glyph record must not cost the whole font. `parse_glyph_shape`
        // is allowed to read past its slice, so a bad record is just an empty glyph here.
        let shape = match data.get(start as usize .. end as usize) {
            Some(slice) => match parse_glyph_shape(slice) {
                Ok(shape) => shape,
                Err(e) => {
                    warn!("glyph {}: {:?} — treated as an empty glyph", i, e);
                    Shape::Empty
                }
            },
            None => {
                warn!("glyph {}: loca range {}..{} outside glyf — treated as an empty glyph", i, start, end);
                Shape::Empty
            }
        };
        shapes.push(shape);
    }
    Ok(shapes)
}
// the following code is borrowed from stb-truetype and modified heavily

fn parse_glyph_shape(data: &[u8]) -> Result<Shape, FontError> {
    if data.len() == 0 {
        return Ok(Shape::Empty);
    }
    let (i, number_of_contours) = be_i16(data)?;
    
    let (i, _) = take(8usize)(i)?;
    //debug!("n_contours: {}", number_of_contours);
    match number_of_contours {
        0 => Ok(Shape::Empty),
        n if n >= 0 => glyph_shape_positive_contours(i, number_of_contours as usize),
        -1 => compound(i).get(),
        n => error!("Contour format {} not supported.", n)
    }
}

pub fn compound(mut input: &[u8]) -> R<Shape> {
    // Compound shapes
    let mut parts = Vec::new();
    loop {
        let (flags, gidx) = parse(&mut input, tuple((be_u16, be_u16)))?;
        // OT §glyf: with ARGS_ARE_XY_VALUES the two arguments are an x/y offset, otherwise they
        // are unsigned point numbers (one into the composite built so far, one into the
        // component) and the component is placed by matching them.
        let offset = if flags & 2 != 0 {
            if flags & 1 != 0 {
                // shorts
                ComponentOffset::Vector(parse(&mut input, vec_i16)?)
            } else {
                ComponentOffset::Vector(parse(&mut input, vec_i8)?)
            }
        } else if flags & 1 != 0 {
            let (parent, child) = parse(&mut input, tuple((be_u16, be_u16)))?;
            ComponentOffset::MatchPoints { parent, child }
        } else {
            let (parent, child) = parse(&mut input, tuple((be_u8, be_u8)))?;
            ComponentOffset::MatchPoints { parent: parent as u16, child: child as u16 }
        };
        let mut transform = Transform2F::default();
        if flags & (1 << 3) != 0 {
            // WE_HAVE_A_SCALE
            let scale = parse(&mut input, fraction_i16)?;
            transform.matrix = Matrix2x2F::from_scale(Vector2F::splat(scale));
        } else if flags & (1 << 6) != 0 {
            // WE_HAVE_AN_X_AND_YSCALE
            let (sx, sy) = parse(&mut input, tuple((fraction_i16, fraction_i16)))?;
            let s = Vector2F::new(sx, sy);
            transform.matrix = Matrix2x2F::from_scale(s);
        } else if flags & (1 << 7) != 0 {
            // WE_HAVE_A_TWO_BY_TWO
            let (a, b, c, d) = parse(&mut input, tuple((fraction_i16, fraction_i16, fraction_i16, fraction_i16)))?;
            transform.matrix = Matrix2x2F::row_major(a, b, c, d);
        }

        // Get indexed glyph.
        parts.push(Component {
            gid: gidx as u32,
            matrix: transform.matrix,
            offset,
            scaled_offset: flags & 0x0800 != 0,
        });
        // More components ?
        if flags & 0x20 == 0 {
            break;
        }
    }
    Ok((input, Shape::Compound(parts)))
}

/// A composite glyph may nest; the spec also forbids a glyph that contains itself. Both are
/// bounded here so a malformed font cannot loop or overflow the stack.
const MAX_COMPONENT_DEPTH: usize = 16;

/// The depth cap alone is not enough: one level may hold many components, so a chain of
/// composites that each name two components still asks for `2 ^ 16` expanded outlines. The
/// expansion as a whole is therefore bounded too, counted over every level.
const MAX_COMPONENT_EXPANSIONS: usize = 10_000;

/// Counting expansions is not enough either: each one copies the outline it names, so one big
/// simple glyph named ten thousand times is still hundreds of millions of points. The points
/// copied for one glyph are capped as well (a real glyph has a few thousand).
const MAX_COMPONENT_POINTS: usize = 1_000_000;

/// What is left of `MAX_COMPONENT_EXPANSIONS` for the glyph being expanded.
struct ComponentBudget {
    left: usize,
    points_left: usize,
    reported: bool,
}

impl ComponentBudget {
    fn new() -> ComponentBudget {
        ComponentBudget { left: MAX_COMPONENT_EXPANSIONS, points_left: MAX_COMPONENT_POINTS, reported: false }
    }

    /// Whether one more component may be expanded; reports the truncation once.
    fn take(&mut self) -> bool {
        match self.left.checked_sub(1) {
            Some(left) => {
                self.left = left;
                true
            }
            None => {
                if !self.reported {
                    self.reported = true;
                    warn!("glyf: composite glyphs expand more than {} outlines; dropping the rest",
                          MAX_COMPONENT_EXPANSIONS);
                }
                false
            }
        }
    }
}

impl ComponentBudget {
    /// Whether an outline of `points` points may be copied; reports the truncation once.
    fn take_points(&mut self, points: usize) -> bool {
        match self.points_left.checked_sub(points) {
            Some(left) => {
                self.points_left = left;
                true
            }
            None => {
                if !self.reported {
                    self.reported = true;
                    warn!("glyf: composite glyphs copy more than {} points; dropping the rest",
                          MAX_COMPONENT_POINTS);
                }
                false
            }
        }
    }
}

pub fn get_outline(shapes: &[Shape], idx: u32) -> Option<Outline> {
    let mut budget = ComponentBudget::new();
    expand_outline(shapes, idx, &mut Vec::new(), &mut budget).map(|(outline, _points)| outline)
}

/// The outline of glyph `idx` and its points in `glyf` numbering. For a composite the numbering
/// is that of its components' points, in order, each transformed like its outline (OT §glyf:
/// a component is placed by matching one of its points to one of the points of the composite
/// built so far).
fn expand_outline(shapes: &[Shape], idx: u32, stack: &mut Vec<u32>, budget: &mut ComponentBudget)
    -> Option<(Outline, Vec<Vector2F>)>
{
    if stack.contains(&idx) {
        warn!("glyf: composite glyph {} contains itself", idx);
        return Some((Outline::new(), Vec::new()));
    }
    if stack.len() >= MAX_COMPONENT_DEPTH {
        warn!("glyf: composite glyphs nested deeper than {}", MAX_COMPONENT_DEPTH);
        return Some((Outline::new(), Vec::new()));
    }
    stack.push(idx);
    let result = match shapes.get(idx as usize) {
        None => None,
        Some(&Shape::Empty) => Some((Outline::new(), Vec::new())),
        Some(&Shape::Simple(ref path, ref points)) => {
            let count: usize = path.contours().iter().map(|contour| contour.points().len()).sum();
            if budget.take_points(count) {
                Some((path.clone(), points.clone()))
            } else {
                Some((Outline::new(), Vec::new()))
            }
        }
        Some(&Shape::Compound(ref parts)) => {
            let mut outline = Outline::new();
            let mut points: Vec<Vector2F> = Vec::new();
            for part in parts {
                if !budget.take() {
                    break;
                }
                let (mut child, mut child_points) = match expand_outline(shapes, part.gid, stack, budget) {
                    Some(child) => child,
                    None => {
                        warn!("glyf: glyph {} references missing glyph {}", idx, part.gid);
                        continue;
                    }
                };
                // The 2×2 part applies before the placement offset, except for a component
                // that asks for its offset to be scaled as well.
                child.transform(&Transform2F { matrix: part.matrix, vector: Vector2F::zero() });
                for point in &mut child_points {
                    *point = part.matrix * *point;
                }
                let offset = match part.offset {
                    ComponentOffset::Vector(v) if part.scaled_offset => part.matrix * v,
                    ComponentOffset::Vector(v) => v,
                    ComponentOffset::MatchPoints { parent, child: child_point } => {
                        match (points.get(parent as usize), child_points.get(child_point as usize)) {
                            (Some(&p), Some(&c)) => p - c,
                            _ => {
                                warn!("glyf: glyph {} matches points {} and {} that do not exist",
                                      idx, parent, child_point);
                                Vector2F::zero()
                            }
                        }
                    }
                };
                child.transform(&Transform2F::from_translation(offset));
                points.extend(child_points.into_iter().map(|point| point + offset));
                outline.push_outline(child);
            }
            Some((outline, points))
        }
    };
    stack.pop();
    result
}

#[derive(Copy, Clone, Debug)]
struct FlagData {
    flags: u8,
    p: (i32, i32)
}
fn parse_coord(short: bool, same_or_pos: bool) -> impl Fn(&[u8]) -> R<i16> {
    move |i| match (short, same_or_pos) {
        (true, true) => {
            let (i, dx) = be_u8(i)?;
            Ok((i, dx as i16))
        }
        (true, false) => {
            let (i, dx) = be_u8(i)?;
            Ok((i, - (dx as i16)))
        }
        (false, false) => {
            let (i, dx) = be_i16(i)?;
            Ok((i, dx))
        }
        (false, true) => Ok((i, 0))
    }
}
fn mid(a: Vector2F, b: Vector2F) -> Vector2F {
    (a + b) * 0.5
}
fn glyph_shape_positive_contours(i: &[u8], number_of_contours: usize) -> Result<Shape, FontError> {
    let (i, point_indices) = take(2 * number_of_contours)(i)?;
    let (i, num_instructions) = be_u16(i)?;
    let (mut i, _instructions) = take(num_instructions)(i)?;
    
    // total number of points
    let n = 1 + be_u16(slice!(point_indices, 2 * number_of_contours - 2 ..)).get()? as usize;

    let mut flag_data = Vec::with_capacity(n);

    // first load flags
    while flag_data.len() < n {
        let flags = parse(&mut i, be_u8)?;
        let flag = FlagData { flags, p: (0, 0) };
        
        if flags & 8 != 0 {
            let flagcount = parse(&mut i, be_u8)?;
            let num = (n - flag_data.len()).min(flagcount as usize + 1);
            flag_data.extend(iter::repeat(flag).take(num));
        } else {
            flag_data.push(flag);
        }
    }
    require_eq!(flag_data.len(), n);
    
    // now load x coordinates
    let mut x_coord: i32 = 0;
    for &mut FlagData { flags, ref mut p } in flag_data.iter_mut() {
        x_coord += parse(&mut i, parse_coord(flags & 2 != 0, flags & 16 != 0))? as i32;
        p.0 = x_coord;
    }

    // now load y coordinates
    let mut y_coord: i32 = 0;
    for &mut FlagData { flags, ref mut p } in flag_data.iter_mut() {
        y_coord += parse(&mut i, parse_coord(flags & 4 != 0, flags & 32 != 0))? as i32;
        p.1 = y_coord;
    }

    let glyph_points: Vec<Vector2F> = flag_data.iter()
        .map(|&FlagData { p, .. }| Vector2F::new(p.0 as f32, p.1 as f32))
        .collect();
    let mut points = flag_data.iter().map(|&FlagData { flags, p }| 
        (flags & 1 != 0, Vector2F::new(p.0 as f32, p.1 as f32))
    );
    let mut start = 0;
    let mut outline = Outline::new();
    for end in iterator(point_indices, be_u16) {
        let n_points = end + 1 - start;
        start += n_points;
        
        if let Some(contour) = contour((&mut points).take(n_points as usize)) {
            outline.push_contour(contour);
        }
    }
    
    Ok(Shape::Simple(outline, glyph_points))
}

pub fn contour(points: impl Iterator<Item=(bool, Vector2F)>) -> Option<Contour> {
    let mut points = points.peekable();
    
    let (start_on, p) = points.next()?;
    let start_off = !start_on;
    let (s, sc) = if start_off {
        // if we start off with an off-curve point, then when we need to find a
        // point on the curve where we can start, and we
        // need to save some state for
        // when we wraparound.
        let sc = p;

        let (next_on, next_p) = match points.peek() {
            Some(&t) => t,
            None => return None,
        };

        let p = if !next_on {
            // next point is also a curve point, so interpolate an on-point curve
            mid(p, next_p)
        } else {
            // we're using point i+1 as the starting point, so skip it
            let _ = points.next();
            
            // otherwise just use the next point as our start point
            next_p
        };
        (p, Some(sc))
    } else {
        (p, None)
    };
    
    let mut contour = Contour::new();
    contour.push_endpoint(s);
    
    let mut c = None;
    for (on_curve, p) in points {
        if !on_curve {
            // if it's a curve
            if let Some(c) = c {
                // two off-curve control points in a row means interpolate an on-curve
                // midpoint
                contour.push_quadratic(c, mid(c, p));
            }
            c = Some(p);
        } else {
            if let Some(c) = c.take() {
                contour.push_quadratic(c, p);
            } else {
                contour.push_endpoint(p);
            }
        }
    }
    
    if let Some(sc) = sc {
        if let Some(c) = c {
            contour.push_quadratic(c, mid(c, sc));
        }
        contour.push_quadratic(sc, s);
    } else {
        if let Some(c) = c {
            contour.push_quadratic(c, s);
        } else {
            contour.push_endpoint(s);
        }
    }

    contour.close();
    Some(contour)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// One triangle.
    fn leaf() -> Shape {
        let mut outline = Outline::new();
        let contour = contour(
            vec![
                (true, Vector2F::new(0., 0.)),
                (true, Vector2F::new(100., 0.)),
                (true, Vector2F::new(100., 100.)),
            ].into_iter(),
        ).expect("triangle");
        outline.push_contour(contour);
        Shape::Simple(outline, Vec::new())
    }

    /// Glyph `n` is two components of glyph `n + 1`, the last one is a plain leaf: the
    /// expansion is `2 ^ depth` outlines.
    fn doubling_chain(depth: u32) -> Vec<Shape> {
        let component = |gid| Component {
            gid,
            matrix: Matrix2x2F::row_major(1., 0., 0., 1.),
            offset: ComponentOffset::Vector(Vector2F::zero()),
            scaled_offset: false,
        };
        let mut shapes = Vec::new();
        for level in 0 .. depth {
            shapes.push(Shape::Compound(vec![component(level + 1), component(level + 1)]));
        }
        shapes.push(leaf());
        shapes
    }

    #[test]
    fn point_matching_uses_the_glyf_numbering_with_implied_points() {
        // Audit R6: points 0..=3 are on, off, off, on. `contour()` inserts an implied midpoint
        // between the two off-curve points, so the converted outline has more points than the
        // glyph; point 3 of the glyph is the last one, whatever the outline holds.
        let original = vec![
            Vector2F::new(0., 0.), Vector2F::new(50., 100.), Vector2F::new(150., 100.), Vector2F::new(200., 0.),
        ];
        let mut outline = Outline::new();
        outline.push_contour(contour(
            original.iter().enumerate().map(|(n, &p)| (n == 0 || n == 3, p)),
        ).expect("contour"));
        let component = |gid, parent, child| Component {
            gid,
            matrix: Matrix2x2F::row_major(1., 0., 0., 1.),
            offset: ComponentOffset::MatchPoints { parent, child },
            scaled_offset: false,
        };
        let first = Component {
            gid: 1,
            matrix: Matrix2x2F::row_major(1., 0., 0., 1.),
            offset: ComponentOffset::Vector(Vector2F::zero()),
            scaled_offset: false,
        };
        // Place a second copy so that its point 0 lands on point 3 of the first: shift +200.
        let shapes = vec![
            Shape::Compound(vec![first, component(1, 3, 0)]),
            Shape::Simple(outline, original),
        ];
        let result = get_outline(&shapes, 0).expect("glyph 0");
        let max_x = result.contours().iter().flat_map(|c| c.points().iter().map(|p| p.x()))
            .fold(0f32, f32::max);
        assert_eq!(max_x, 400.0);
    }

    #[test]
    fn many_references_to_a_big_glyph_stop_at_the_point_budget() {
        // A composite naming one 1000-point glyph 5000 times is under the expansion cap but
        // would copy 5 million points.
        let mut points = Vec::new();
        for n in 0 .. 1000 {
            points.push((true, Vector2F::new(n as f32, (n % 7) as f32)));
        }
        let mut outline = Outline::new();
        outline.push_contour(contour(points.into_iter()).expect("big contour"));
        let component = Component {
            gid: 1,
            matrix: Matrix2x2F::row_major(1., 0., 0., 1.),
            offset: ComponentOffset::Vector(Vector2F::zero()),
            scaled_offset: false,
        };
        let shapes = vec![Shape::Compound(vec![component; 5000]), Shape::Simple(outline, Vec::new())];
        let copied: usize = get_outline(&shapes, 0).expect("glyph 0").contours().iter()
            .map(|contour| contour.points().len()).sum();
        assert!(copied > 0 && copied <= MAX_COMPONENT_POINTS + 1000, "copied {} points", copied);
    }

    #[test]
    fn a_doubling_composite_chain_expands_within_the_budget() {
        // Audit §3: the depth cap alone does not bound the work — a chain of composites that
        // each name two components asks for 2 ^ 14 outlines. Without the budget every leaf is
        // expanded; with it the expansion stops at MAX_COMPONENT_EXPANSIONS.
        let shapes = doubling_chain(14);
        let start = Instant::now();
        let outline = get_outline(&shapes, 0).expect("composite glyph 0");
        let contours = outline.contours().len();
        assert!(start.elapsed().as_secs() < 2, "expansion took {:?}", start.elapsed());
        assert!(contours > 0, "the budget must leave some of the glyph");
        assert!(
            contours <= MAX_COMPONENT_EXPANSIONS,
            "{} contours: the component budget did not stop the expansion",
            contours
        );
    }
}
