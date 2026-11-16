//! The mosaic's colours, read back as the numbers they stand for.
//!
//! The reflectivity is the one layer that has to arrive as a picture. MRMS is a
//! raster product and NCEP publishes no vector form of it: GeoServer advertises
//! GeoJSON and vector tiles on the mosaic and both answer with a single feature
//! holding the coverage's bounding rectangle. The workspace advertises no
//! feature types, and the coverage's own bands are named `RED_BAND` and
//! `GREEN_BAND` - it is stored already rendered.
//!
//! That does not mean the numbers are lost. The service renders through a
//! *continuous* ramp, and the ramp is a bijection: measured against the same
//! sweep fetched as GRIB2 from `mrms.ncep.noaa.gov`, every colour it draws maps
//! to exactly one reflectivity, with a scatter of 0.000 dBZ across 153 distinct
//! colours and 40,000 sampled cells. So the picture can be read back into the
//! field it was drawn from, exactly, and that is what [`RAMP`] is.
//!
//! Why bother. A colour-banded picture cannot be magnified: stretching it
//! blends one band into the next, which is mush, and the app used to blur it on
//! purpose on top of that to dissolve the mosaic's cell blocks. The result was
//! soft at every zoom and the map underneath it was buried. Read the picture
//! back into numbers and the order of operations can be put right - interpolate
//! the *reflectivity*, then colour it - which gives band edges that are exactly
//! where the data says and as sharp as the screen can draw, at any
//! magnification. It is the difference between a photograph of a chart and the
//! chart.
//!
//! Asking the service to do this for us was the first thing tried. WMS takes an
//! `SLD_BODY` and a linear grey ramp would have made the encoding exact by
//! construction and immune to a restyle; NCEP answers "Dynamic style usage is
//! forbidden". So the table is learned rather than agreed, and because it is
//! learned it is also *checked* - see [`to_dbz`], which hands the picture back
//! untouched rather than guess if the colours stop matching.

use image::RgbaImage;

/// Every colour the mosaic draws, and the reflectivity it stands for.
///
/// Packed `0xRRGGBB` and sorted, so a pixel is a binary search. The second
/// number is dBZ in halves, which is the precision the service quantises to and
/// keeps the table in integers.
///
/// Learned from one sweep, cross-checked against that sweep's GRIB2: 153
/// colours, none of them ambiguous, covering -9.0 to 76.5 dBZ. The gaps are all
/// at the extremes - below -6.5 and above 65.5 - where a nearest match is both
/// rare and harmless.
///
/// To rebuild it, fetch a sweep as GRIB2 and the same sweep as WMS on a bbox
/// aligned to the 0.01 degree grid at one pixel per cell, and pair them.
/// Alignment is the whole trick: ask for a finer pixel than the data and
/// GeoServer interpolates, and an interpolated colour is a blend that sits
/// between two ramp entries and matches neither.
pub const RAMP: [(u32, i16); 153] = [
    (0x05D5D8,  153),
    (0x05E4E8,  151),
    (0x095E09,   70),
    (0x09620A,   69),
    (0x09660A,   68),
    (0x0A6B0B,   67),
    (0x0A6F0B,   66),
    (0x0A730C,   65),
    (0x0A770D,   64),
    (0x0A7B0D,   63),
    (0x0B800E,   62),
    (0x0B840E,   61),
    (0x0B880F,   60),
    (0x0B9010,   59),
    (0x0C9810,   58),
    (0x0C9F11,   57),
    (0x0CA711,   56),
    (0x0DAF12,   55),
    (0x0DB712,   54),
    (0x0DBF13,   53),
    (0x0DC613,   52),
    (0x0ECE14,   51),
    (0x0ED614,   50),
    (0x15D622,   49),
    (0x1CD630,   48),
    (0x226B08,   71),
    (0x22D63F,   47),
    (0x29D64D,   46),
    (0x30D65B,   45),
    (0x37A5F4,  147),
    (0x37D669,   44),
    (0x3A7807,   72),
    (0x3ED677,   43),
    (0x435E9F,   20),
    (0x44D686,   42),
    (0x4660A0,   19),
    (0x4666A4,   21),
    (0x486EA9,   22),
    (0x488EF5,  146),
    (0x4963A1,   18),
    (0x4B76AD,   23),
    (0x4BD694,   41),
    (0x4D65A2,   17),
    (0x4E7EB2,   24),
    (0x5068A3,   16),
    (0x5186B7,   25),
    (0x52D6A2,   40),
    (0x536AA4,   15),
    (0x538606,   73),
    (0x538DBC,   26),
    (0x53D2A7,   39),
    (0x54CEAB,   38),
    (0x566CA4,   14),
    (0x5695C1,   27),
    (0x56CAB0,   37),
    (0x57C6B4,   36),
    (0x58C2B9,   35),
    (0x596FA5,   13),
    (0x599DC5,   28),
    (0x59BDBD,   34),
    (0x5AB9C1,   33),
    (0x5BA5CA,   29),
    (0x5CB5C6,   32),
    (0x5D71A6,   12),
    (0x5DB1CB,   31),
    (0x5EADCF,   30),
    (0x6074A7,   11),
    (0x6376A8,   10),
    (0x687AA9,    9),
    (0x6B9305,   74),
    (0x6D7DAB,    8),
    (0x7281AC,    7),
    (0x7785AD,    6),
    (0x7A47F8,  143),
    (0x7C89AF,    5),
    (0x808CB0,    4),
    (0x84A005,   75),
    (0x8590B1,    3),
    (0x8A94B2,    2),
    (0x8F97B4,    1),
    (0x949BB5,    0),
    (0x979EB5,   -1),
    (0x9AA0B5,   -2),
    (0x9CA3B5,   -3),
    (0x9DAD04,   76),
    (0x9FA6B5,   -4),
    (0xA2A9B5,   -5),
    (0xA5ABB4,   -6),
    (0xA8AEB4,   -7),
    (0xAAB1B4,   -8),
    (0xADB3B4,   -9),
    (0xB0B6B4,  -10),
    (0xB10000,  110),
    (0xB3B9B4,  -11),
    (0xB40CFC,  139),
    (0xB5BA03,   77),
    (0xB7BCB4,  -12),
    (0xB90000,  109),
    (0xB91A1A,  111),
    (0xBABFB4,  -13),
    (0xC10000,  108),
    (0xC13333,  112),
    (0xC1C5B4,  -15),
    (0xC80000,  107),
    (0xC84D4D,  113),
    (0xCBCEB4,  -18),
    (0xCEC802,   78),
    (0xD00000,  106),
    (0xD06666,  114),
    (0xD80000,  105),
    (0xD88080,  115),
    (0xE00000,  104),
    (0xE09999,  116),
    (0xE6D501,   79),
    (0xE80000,  103),
    (0xE8B3B3,  117),
    (0xEF0000,  102),
    (0xEFCCCC,  118),
    (0xF70000,  101),
    (0xF769FF,  131),
    (0xF7E6E6,  119),
    (0xFF0000,  100),
    (0xFF1200,   99),
    (0xFF2300,   98),
    (0xFF3500,   97),
    (0xFF4700,   96),
    (0xFF5900,   95),
    (0xFF6A00,   94),
    (0xFF75FF,  130),
    (0xFF7C00,   93),
    (0xFF83FF,  129),
    (0xFF8E00,   92),
    (0xFF91FF,  128),
    (0xFF9EFF,  127),
    (0xFF9F00,   91),
    (0xFFACFF,  126),
    (0xFFB100,   90),
    (0xFFB600,   89),
    (0xFFBAFF,  125),
    (0xFFBB00,   88),
    (0xFFC000,   87),
    (0xFFC500,   86),
    (0xFFC8FF,  124),
    (0xFFCA00,   85),
    (0xFFCE00,   84),
    (0xFFD300,   83),
    (0xFFD6FF,  123),
    (0xFFD800,   82),
    (0xFFDD00,   81),
    (0xFFE200,   80),
    (0xFFE3FF,  122),
    (0xFFF1FF,  121),
    (0xFFFFFF,  120),
];

/// What the map is coloured with, in dBZ halves.
///
/// The National Weather Service's own steps, which is the convention the eye
/// already knows and the one the intensity scale on the radar screen is drawn
/// from - so for the first time the scale and the map are the same table rather
/// than two tables that agree by hand.
pub const BANDS: [(i16, [u8; 3]); 15] = [
    (10, [0x04, 0xE9, 0xE7]),
    (20, [0x01, 0x9F, 0xF4]),
    (30, [0x03, 0x00, 0xF4]),
    (40, [0x02, 0xFD, 0x02]),
    (50, [0x01, 0xC5, 0x01]),
    (60, [0x00, 0x8E, 0x00]),
    (70, [0xFD, 0xF8, 0x02]),
    (80, [0xE5, 0xBC, 0x00]),
    (90, [0xFD, 0x95, 0x00]),
    (100, [0xFD, 0x00, 0x00]),
    (110, [0xD4, 0x00, 0x00]),
    (120, [0xBC, 0x00, 0x00]),
    (130, [0xF8, 0x00, 0xFD]),
    (140, [0x98, 0x54, 0xC6]),
    (150, [0xFD, 0xFD, 0xFD]),
];

/// How many extra cells of the mosaic each tile is fetched with, on every side.
///
/// The smoothing in [`repaint`] is what makes a band boundary bend, and a blur
/// has no idea it is looking at a tile. Run it on the tile alone and the pixels
/// near the edge are averaged against nothing, so the field there is wrong, so
/// the contour lands in the wrong place — and the neighbouring tile gets it
/// wrong in the opposite direction. On screen that is a bright seam running the
/// full height and width of the view, exactly along the tile grid.
///
/// So each tile is fetched with a collar of its neighbours' cells, smoothed
/// across the whole thing, and trimmed back to its own extent. The collar only
/// has to outreach the blur: the kernel is half a cell and its tail is spent by
/// two, so four is comfortable and costs a few percent of a very small request.
pub const MARGIN: u32 = 4;

/// The most the field is enlarged before its contours are traced.
///
/// Not about sharpness, which a contour has by construction — it only decides
/// how closely the outline follows the data. Two is enough to round the
/// mosaic's square cells into curves; past that it is more vertices for a shape
/// the eye cannot tell apart.
pub const SMOOTHING: u32 = 2;

/// Enlarge the field and round its corners.
///
/// Both halves of this module start here, because both need the same thing: a
/// field that says where the reflectivity crosses each threshold, smoothly.
///
/// Catmull-Rom alone is not enough, and the reason is worth writing down: it is
/// a local interpolant, so it reproduces the grid it is given faithfully —
/// including the fact that the grid is square. Band the result and every
/// boundary follows cell corners, which on screen is a staircase however many
/// pixels it is drawn with. More resolution makes the steps smaller and never
/// makes them curves.
///
/// A blur on the field is what bends them. It moves the *position* of each
/// contour by a fraction of a cell, which is well inside what a kilometre of
/// averaged reflectivity claims to know, and it softens nothing: the edge is
/// created afterwards, by the banding, and a step function of a smooth field is
/// still a step. So the boundary curves and stays one pixel wide.
///
/// Sized to the cell rather than picked — half a source cell in the enlarged
/// picture — so it rounds cell corners and nothing larger.
fn enlarge(flat: &image::GrayImage, by: u32) -> image::GrayImage {
    if by <= 1 {
        return flat.clone();
    }
    let (w, h) = flat.dimensions();
    let grown = image::imageops::resize(
        flat,
        w * by,
        h * by,
        image::imageops::FilterType::CatmullRom,
    );
    image::imageops::blur(&grown, by as f32 * 0.5)
}

/// How the field is carried between reading it and drawing it.
///
/// One byte a cell, biased so that the whole useful range is positive: the
/// service quantises to half a dBZ and nothing below -32 or above 95 has ever
/// been seen. A byte is also what lets the interpolation be somebody else's
/// problem - see [`repaint`].
const BIAS: i16 = 64;

fn to_byte(halves: i16) -> u8 {
    (halves + BIAS).clamp(0, 255) as u8
}

fn from_byte(byte: u8) -> i16 {
    byte as i16 - BIAS
}

/// What one colour means, or `None` if it is not one of the service's.
pub fn dbz_of(rgb: u32) -> Option<i16> {
    RAMP.binary_search_by_key(&rgb, |(colour, _)| *colour)
        .ok()
        .map(|at| RAMP[at].1)
}

/// Read a rendered tile back into reflectivity.
///
/// Exact matches only, and it counts the misses. A pixel that is not one of the
/// service's colours is either an edge the renderer anti-aliased - a handful
/// per tile, and they are transparent enough to ignore - or a sign that the
/// style has changed under us, which is the failure this has to survive. So the
/// caller gets `None` when too much of the tile is unrecognised, and draws the
/// picture as it arrived instead. A restyle costs the sharpening, not the
/// radar.
pub fn to_dbz(image: &RgbaImage) -> Option<Vec<u8>> {
    /// How much of a tile may be unreadable before it is not our ramp at all.
    /// Anti-aliasing accounts for well under a percent; a restyle for nearly
    /// all of it.
    const GIVE_UP: usize = 5;

    let mut field = vec![to_byte(0); (image.width() * image.height()) as usize];
    let (mut painted, mut missed) = (0usize, 0usize);

    for (at, pixel) in image.pixels().enumerate() {
        // Only fully painted pixels sit on the ramp. The rest are the edge of
        // the echo, where the renderer has blended toward nothing.
        if pixel[3] < 250 {
            continue;
        }
        painted += 1;
        let rgb = ((pixel[0] as u32) << 16) | ((pixel[1] as u32) << 8) | pixel[2] as u32;
        match dbz_of(rgb) {
            Some(halves) => field[at] = to_byte(halves),
            None => missed += 1,
        }
    }

    if painted > 0 && missed * 100 > painted * GIVE_UP {
        log::warn!(
            "the mosaic's colours no longer match the table - {missed} of {painted} \
             pixels unrecognised; drawing it as sent"
        );
        return None;
    }
    Some(field)
}

// ------------------------------------------------------------- as shapes

/// One band's ground inside one tile, as triangles.
///
/// In the tile's own coordinates, 0 to 1 across and down, with 0 at the top —
/// so drawing is the tile's screen rectangle and nothing else. No projection,
/// no origin to carry, and the same numbers whatever level the tile came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Patch {
    /// Which of [`BANDS`] this is.
    pub band: usize,
    /// Three vertices per triangle, as sixteenths of a thousandth of the tile.
    ///
    /// Whole numbers rather than floats, and it halves what the loop holds. The
    /// coordinates only ever run from nothing to one tile, so the exponent a
    /// float spends most of its bits on is worth nothing here — sixteen bits
    /// spread over one tile is a quarter of a metre at the mosaic's own level
    /// and two metres at the widest, which is far below anything a screen can
    /// show. Undone by [`Patch::at`].
    pub triangles: Vec<[u16; 2]>,
}

impl Patch {
    /// One vertex, back in the tile's own zero-to-one coordinates.
    pub fn at(point: [u16; 2]) -> (f32, f32) {
        const FULL: f32 = u16::MAX as f32;
        (point[0] as f32 / FULL, point[1] as f32 / FULL)
    }
}

/// A tile's reflectivity, cut into shapes instead of pixels.
///
/// Least intense band first, so painting them in order leaves the heaviest rain
/// on top — the bands nest, and a lighter one drawn last would rub out the core
/// of the storm.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shapes {
    pub bands: Vec<Patch>,
    /// The tile's own ground as a coarse field, kept only so that consecutive
    /// sweeps can be compared and the weather's motion worked out — see
    /// [`motion`]. [`COARSE`] square, one byte a cell.
    pub coarse: Vec<u8>,
}

/// How coarse the field kept for motion estimation is, per side.
///
/// Chosen against what has to be measured rather than picked. A storm travels
/// at most about four kilometres in the two minutes between sweeps, and a tile
/// at the mosaic's own level spans 118 of them — so at 64 cells a side that is
/// 1.8 km a cell and the displacement to be found is half a cell to a little
/// over two. Enough to correlate, with the sub-cell fit in [`motion`] for the
/// rest.
///
/// Coarser levels have far more ground to a cell and the estimate there is
/// correspondingly vaguer. That is the right way round: a tile at 854 miles
/// across has four kilometres landing inside a single screen pixel, so the
/// motion nobody can see is the motion this measures worst.
pub const COARSE: usize = 64;

impl Shapes {
    /// How many triangles it came to. Only the tests ask — it is what the
    /// budgets in `ui::radar::sharpness_for` are set against, so it is how a
    /// change that made the shapes dearer would be noticed.
    #[cfg(test)]
    pub fn triangles(&self) -> usize {
        self.bands.iter().map(|patch| patch.triangles.len() / 3).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.bands.iter().all(|patch| patch.triangles.is_empty())
    }
}

/// Cut a rendered tile into the shapes its reflectivity encloses.
///
/// The other half of [`repaint`], and the same first three steps: read the
/// picture back into numbers, enlarge the field, round its corners. Where
/// `repaint` then colours pixels, this traces the outline of each band and
/// hands back the ground it encloses as triangles — so what is drawn is
/// geometry, and a band edge is exactly as sharp as the screen can draw however
/// far the map is zoomed in. A raster can only ever be as sharp as the texture
/// it was baked at.
///
/// It is used only where it pays. Zoomed out, the picture is already at screen
/// resolution and a tile is 44KB against a few hundred as shapes; zoomed in, the
/// picture would have to be baked at eight times the size — three megabytes a
/// tile — to keep up, and the shapes cost the same as they did. See
/// `ui::radar::sharpness_for`.
///
/// `smoothing` is how much the field is enlarged before it is traced, which
/// decides how finely the outline follows the data rather than how sharply it
/// is drawn — a contour is sharp at every zoom by construction. Two is enough
/// to round the mosaic's square cells into curves; more only adds vertices.
pub fn contour(image: &RgbaImage, smoothing: u32, margin: u32) -> Option<Shapes> {
    let field = to_dbz(image)?;
    let (w, h) = (image.width(), image.height());
    let flat = image::GrayImage::from_raw(w, h, field)?;
    let smoothing = smoothing.clamp(1, 4);
    let grown = enlarge(&flat, smoothing);

    let (gw, gh) = grown.dimensions();
    // The collar in the enlarged field, and the tile's own ground inside it.
    let trim = (margin * smoothing) as f64;
    let inside = [trim, trim, gw as f64 - trim, gh as f64 - trim];
    let (across, down) = (inside[2] - inside[0], inside[3] - inside[1]);
    if across <= 0.0 || down <= 0.0 {
        return None;
    }

    // The heaviest reading anywhere on the tile. Most tiles are light rain and
    // most bands are heavy, so without this a drizzle over Illinois is traced
    // fifteen times for the fourteen answers that are empty - and tracing walks
    // every cell of the field whether or not the level is anywhere near it.
    let peak = grown.as_raw().iter().copied().max().map(from_byte).unwrap_or(0);

    // Every threshold this tile actually reaches, traced once. Each contour is
    // wanted twice — as its own band's outline, and as the hole in the band
    // below it — so tracing them up front is what keeps that to one pass.
    let mut outlines: Vec<(usize, Vec<super::fill::Edge>)> = Vec::new();
    for (band, (edge, _)) in BANDS.iter().enumerate() {
        if *edge > peak {
            break;
        }
        let edges = trace(&grown, *edge);
        if edges.is_empty() {
            continue;
        }
        outlines.push((band, edges));
    }

    let mut out = Vec::new();
    let mut boundary: Vec<super::fill::Edge> = Vec::new();
    for at in 0..outlines.len() {
        let (band, edges) = &outlines[at];
        // A band is the ground between its own contour and the next one up,
        // rather than everything inside its own.
        //
        // They nest: a band is everywhere the reading reaches its threshold, so
        // it contains every heavier band whole. Drawn as nested shapes, lightest
        // first, each heavier one paints over the middle of the one beneath and
        // the picture comes out right - but only because every one of them is
        // opaque. Give a sweep any transparency at all and the lighter band
        // shows through the core it contains, and a core with three bands over
        // it comes out as three colours mixed rather than as the heaviest.
        //
        // Handing the contour above in as well makes each band the ring it
        // should always have been. `triangulate` fills by the even-odd rule, so
        // a contour inside another takes itself out for free - and contours of
        // one field at different levels cannot cross, so inside is the only
        // place it can be. It costs nothing per frame and saves the overdraw.
        boundary.clear();
        boundary.extend_from_slice(edges);
        if let Some((_, above)) = outlines.get(at + 1) {
            boundary.extend_from_slice(above);
        }
        // Clipped to the tile's own ground, so the collar does its job at the
        // edges and then contributes nothing. Contours are generated here and
        // cannot cross, so there is no need to go looking.
        let cut = super::fill::triangulate(&boundary, false, Some(inside));
        if cut.is_empty() {
            continue;
        }
        out.push(Patch {
            band: *band,
            triangles: cut
                .into_iter()
                .map(|[x, y]| {
                    let across = ((x - inside[0]) / across).clamp(0.0, 1.0);
                    let down = ((y - inside[1]) / down).clamp(0.0, 1.0);
                    [
                        (across * u16::MAX as f64).round() as u16,
                        (down * u16::MAX as f64).round() as u16,
                    ]
                })
                .collect(),
        });
    }
    Some(Shapes {
        bands: out,
        coarse: coarsen(&grown, inside),
    })
}

/// The tile's own ground, reduced to [`COARSE`] square by taking the strongest
/// reading in each block.
///
/// The strongest rather than the average, deliberately. Averaging a storm core
/// against the clear sky around it moves the core toward wherever there happens
/// to be more sky, which is a bias that varies from block to block — and the
/// whole point of this field is to find where a core *went*. A maximum keeps a
/// cell where the rain is.
fn coarsen(grown: &image::GrayImage, inside: [f64; 4]) -> Vec<u8> {
    let (across, down) = (inside[2] - inside[0], inside[3] - inside[1]);
    let mut out = vec![0u8; COARSE * COARSE];
    for row in 0..COARSE {
        for column in 0..COARSE {
            let x0 = inside[0] + across * column as f64 / COARSE as f64;
            let x1 = inside[0] + across * (column + 1) as f64 / COARSE as f64;
            let y0 = inside[1] + down * row as f64 / COARSE as f64;
            let y1 = inside[1] + down * (row + 1) as f64 / COARSE as f64;
            let mut peak = 0u8;
            let mut y = y0.floor().max(0.0) as u32;
            while (y as f64) < y1 && y < grown.height() {
                let mut x = x0.floor().max(0.0) as u32;
                while (x as f64) < x1 && x < grown.width() {
                    peak = peak.max(grown.get_pixel(x, y)[0]);
                    x += 1;
                }
                y += 1;
            }
            out[row * COARSE + column] = peak;
        }
    }
    out
}

/// The outline of `{ field >= level }`, as loose segments.
///
/// Marching squares. Each cell of the grid is looked at through its four
/// corners: which of them are above the level says how the boundary crosses it,
/// and where along each edge it crosses is one linear interpolation. Two of the
/// sixteen cases are ambiguous — opposite corners above, the other two below —
/// and the cell's own average settles which way the two arcs join.
///
/// Nothing chains them into rings, deliberately. The boundary of a region is a
/// closed *set* of segments whether or not anything has walked it, and even-odd
/// parity only needs the set — see [`super::fill`]. Chaining was written first
/// and got the winding wrong, which left half the rings open and streaked the
/// fill; the version with no chaining in it is both shorter and correct.
///
/// The field is padded with a value below the level, so a band running off the
/// side of the tile still closes rather than leaving the parity hanging.
fn trace(field: &image::GrayImage, level: i16) -> Vec<super::fill::Edge> {
    let (w, h) = (field.dimensions().0 as i32, field.dimensions().1 as i32);
    let at = |x: i32, y: i32| -> f64 {
        if x < 0 || y < 0 || x >= w || y >= h {
            return level as f64 - 1000.0;
        }
        from_byte(field.get_pixel(x as u32, y as u32)[0]) as f64
    };
    let level = level as f64;

    let mut edges = Vec::new();
    for y in -1..h {
        for x in -1..w {
            let (tl, tr, br, bl) = (at(x, y), at(x + 1, y), at(x + 1, y + 1), at(x, y + 1));
            let mut which = 0u8;
            for (bit, value) in [(8, tl), (4, tr), (2, br), (1, bl)] {
                if value >= level {
                    which |= bit;
                }
            }
            if which == 0 || which == 15 {
                continue;
            }
            // Where the level cuts each edge of the cell. Coordinates are the
            // grid's, with the cell's top-left corner at (x, y).
            let cut = |ax: f64, ay: f64, bx: f64, by: f64, va: f64, vb: f64| {
                let t = if (vb - va).abs() < f64::EPSILON {
                    0.5
                } else {
                    ((level - va) / (vb - va)).clamp(0.0, 1.0)
                };
                [ax + (bx - ax) * t, ay + (by - ay) * t]
            };
            let (fx, fy) = (x as f64, y as f64);
            let top = cut(fx, fy, fx + 1.0, fy, tl, tr);
            let right = cut(fx + 1.0, fy, fx + 1.0, fy + 1.0, tr, br);
            let bottom = cut(fx + 1.0, fy + 1.0, fx, fy + 1.0, br, bl);
            let left = cut(fx, fy + 1.0, fx, fy, bl, tl);

            match which {
                1 | 14 => edges.push((bottom, left)),
                2 | 13 => edges.push((right, bottom)),
                3 | 12 => edges.push((right, left)),
                4 | 11 => edges.push((top, right)),
                6 | 9 => edges.push((top, bottom)),
                7 | 8 => edges.push((top, left)),
                _ => {
                    // The saddle. The average of the four corners says whether
                    // the two arcs join around the high pair or the low one.
                    let middle = (tl + tr + br + bl) / 4.0;
                    if (which == 5) == (middle >= level) {
                        edges.push((top, right));
                        edges.push((bottom, left));
                    } else {
                        edges.push((top, left));
                        edges.push((right, bottom));
                    }
                }
            }
        }
    }
    edges
}


/// How far the weather moved between one sweep and the next, in tile fractions.
///
/// The loop is a slideshow: ten sweeps two minutes apart, shown 400ms apart. A
/// storm doing 50 km/h covers 1.7 km in those two minutes, which at the
/// thirteen-mile view is about ninety screen pixels — so the weather does not
/// travel across the map, it teleports ten times in four seconds. Knowing which
/// way it went is what turns that back into motion, because a shape displaced is
/// a change to where it is drawn and nothing else.
///
/// Found by sliding one coarse field over the other and taking the offset that
/// matches best. Two details matter and both were got wrong first:
///
/// The score is normalised by how much of the two fields actually overlap. Slide
/// far enough and only a corner is being compared, and an unnormalised sum of
/// differences is always happiest at the largest slide it is offered — every
/// tile came out moving hard toward one corner.
///
/// And the answer is refined below a whole cell. The displacement to be found is
/// often only half a cell across, so a whole-cell answer is either nothing or
/// twice what it should be, and the loop stutters between the two. A parabola
/// through the best score and its neighbours on each axis puts the minimum where
/// the data says it is rather than at the nearest cell.
///
/// `None` when there is not enough echo to match, which is most tiles most of the
/// time — the caller falls back to what the rest of the sky is doing.
pub fn motion(from: &Shapes, to: &Shapes) -> Option<[f32; 2]> {
    /// How far to look, in coarse cells. Four is about 7km at the mosaic's own
    /// level, comfortably past anything that travels.
    const REACH: i32 = 4;
    /// Below this there is nothing to match — an empty tile correlates equally
    /// well with itself at every offset, and the answer would be noise.
    const LEAST_ECHO: u32 = 40;

    let (a, b) = (&from.coarse, &to.coarse);
    if a.len() != COARSE * COARSE || b.len() != COARSE * COARSE {
        return None;
    }
    let floor = to_byte(BANDS[0].0);
    let echo = |field: &[u8]| field.iter().filter(|value| **value >= floor).count() as u32;
    if echo(a) < LEAST_ECHO || echo(b) < LEAST_ECHO {
        return None;
    }

    // The score at every offset in reach, kept so the best one's neighbours can
    // be looked at afterwards.
    let span = (REACH * 2 + 1) as usize;
    let mut scores = vec![f64::MAX; span * span];
    for dy in -REACH..=REACH {
        for dx in -REACH..=REACH {
            let mut sum = 0.0f64;
            let mut counted = 0u32;
            for y in 0..COARSE as i32 {
                let sy = y + dy;
                if sy < 0 || sy >= COARSE as i32 {
                    continue;
                }
                for x in 0..COARSE as i32 {
                    let sx = x + dx;
                    if sx < 0 || sx >= COARSE as i32 {
                        continue;
                    }
                    let one = a[(y * COARSE as i32 + x) as usize] as f64;
                    let other = b[(sy * COARSE as i32 + sx) as usize] as f64;
                    sum += (one - other).abs();
                    counted += 1;
                }
            }
            if counted == 0 {
                continue;
            }
            // Per cell compared, so a slide that only overlaps a corner is not
            // rewarded for having less to disagree about.
            scores[((dy + REACH) * (REACH * 2 + 1) + dx + REACH) as usize] =
                sum / counted as f64;
        }
    }

    let best = scores
        .iter()
        .enumerate()
        .filter(|(_, score)| score.is_finite())
        .min_by(|one, other| one.1.total_cmp(other.1))
        .map(|(at, _)| at)?;
    let (bx, by) = ((best % span) as i32 - REACH, (best / span) as i32 - REACH);

    // Sub-cell, by a parabola through the best and its neighbours on each axis.
    let score_at = |dx: i32, dy: i32| -> Option<f64> {
        if dx.abs() > REACH || dy.abs() > REACH {
            return None;
        }
        let value = scores[((dy + REACH) * (REACH * 2 + 1) + dx + REACH) as usize];
        value.is_finite().then_some(value)
    };
    let refine = |less: Option<f64>, here: f64, more: Option<f64>| -> f64 {
        match (less, more) {
            (Some(less), Some(more)) => {
                let bottom = less - 2.0 * here + more;
                if bottom.abs() < f64::EPSILON {
                    0.0
                } else {
                    ((less - more) / (2.0 * bottom)).clamp(-1.0, 1.0)
                }
            }
            _ => 0.0,
        }
    };
    let here = score_at(bx, by)?;
    let across = bx as f64 + refine(score_at(bx - 1, by), here, score_at(bx + 1, by));
    let down = by as f64 + refine(score_at(bx, by - 1), here, score_at(bx, by + 1));

    // Coarse cells to tile fractions, which is what the drawing works in.
    Some([
        (across / COARSE as f64) as f32,
        (down / COARSE as f64) as f32,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup is a binary search, so the table has to be sorted and free of
    /// duplicates. Generated code, but generated by hand once and pasted, which
    /// is exactly the sort of thing that rots quietly.
    #[test]
    fn the_ramp_is_ordered_and_unique() {
        for pair in RAMP.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "0x{:06X} does not sort before 0x{:06X}",
                pair[0].0,
                pair[1].0
            );
        }
        // And it spans the range the service actually draws.
        let lowest = RAMP.iter().map(|(_, v)| *v).min().unwrap();
        let highest = RAMP.iter().map(|(_, v)| *v).max().unwrap();
        assert!(lowest <= -10, "the ramp starts at {} halves", lowest);
        assert!(highest >= 150, "the ramp stops at {} halves", highest);
    }

    /// Spot checks against the service's own rendering, taken from a sweep
    /// cross-referenced with that sweep's GRIB2. If NCEP restyles, these are
    /// what will notice first.
    #[test]
    fn the_colours_everybody_sees_are_the_numbers_they_stand_for() {
        assert_eq!(dbz_of(0xFFDD00), Some(81), "40.5 dBZ, the yellow of a storm");
        assert_eq!(dbz_of(0xFF3500), Some(97), "48.5 dBZ, the orange-red core");
        assert_eq!(dbz_of(0xD80000), Some(105), "52.5 dBZ, deep red");
        assert_eq!(dbz_of(0x123456), None, "a colour it never draws");
    }

    fn tile_of(colours: &[u32], w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let c = colours[((y * w + x) as usize) % colours.len()];
            image::Rgba([(c >> 16) as u8, (c >> 8) as u8, c as u8, 255])
        })
    }

    /// A tile of the service's own colours comes back as exactly the numbers
    /// they were drawn from.
    #[test]
    fn a_rendered_tile_reads_back_as_the_field_it_was_drawn_from() {
        let picks: Vec<u32> = RAMP.iter().step_by(9).map(|(c, _)| *c).collect();
        let want: Vec<i16> = RAMP.iter().step_by(9).map(|(_, v)| *v).collect();
        let field = to_dbz(&tile_of(&picks, picks.len() as u32, 1)).expect("all ours");
        let got: Vec<i16> = field.iter().map(|b| from_byte(*b)).collect();
        assert_eq!(got, want);
    }

    /// A transparent pixel is the edge of the echo rather than a reading, and
    /// must come back as clear sky rather than as whatever colour was blended
    /// toward.
    #[test]
    fn the_unpainted_parts_read_as_clear_sky() {
        let mut tile = tile_of(&[0xFFDD00], 4, 1);
        tile.put_pixel(1, 0, image::Rgba([0xFF, 0xDD, 0x00, 0]));
        tile.put_pixel(2, 0, image::Rgba([0xFF, 0xDD, 0x00, 120]));
        let field = to_dbz(&tile).expect("all ours");
        assert_eq!(from_byte(field[0]), 81, "painted");
        assert_eq!(from_byte(field[1]), 0, "fully clear");
        assert_eq!(from_byte(field[2]), 0, "half painted is not a reading");
        assert!(from_byte(field[1]) < BANDS[0].0, "and is below anything worth drawing");
    }

    /// The guard. If NCEP restyles the mosaic the table stops meaning anything,
    /// and the honest response is to hand the picture back untouched rather
    /// than to colour the map from a lookup that no longer applies.
    #[test]
    fn a_tile_from_a_different_style_is_refused_rather_than_guessed() {
        let foreign = tile_of(&[0x123456, 0x654321, 0x0F0F0F], 8, 8);
        assert!(to_dbz(&foreign).is_none(), "nothing in it is ours");
        assert!(contour(&foreign, 2, 0).is_none(), "so there is nothing to trace");

        // A handful of anti-aliased pixels is not a restyle, though, and must
        // not throw a good tile away.
        let mut mostly = tile_of(&[0xFFDD00], 10, 10);
        mostly.put_pixel(0, 0, image::Rgba([0x12, 0x34, 0x56, 255]));
        assert!(to_dbz(&mostly).is_some(), "one stray pixel in a hundred is fine");
    }

    /// A tile with nothing in it encloses nothing, and must say so rather than
    /// hand back empty bands for the drawing to skip one at a time.
    #[test]
    fn a_clear_sky_traces_to_nothing() {
        let clear = RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 0, 0]));
        let shapes = contour(&clear, 2, MARGIN).expect("nothing in it is foreign");
        assert!(shapes.is_empty());
        assert_eq!(shapes.triangles(), 0);
    }

    /// The bands come back least intense first, which is the order the drawing
    /// walks them in. They no longer overlap — see
    /// [`a_band_is_the_ring_between_its_own_contour_and_the_next`] — but the
    /// order still has to be the order the thresholds are in.
    #[test]
    fn the_bands_come_back_weakest_first_because_they_nest() {
        // A bullseye: heavy rain in the middle, lighter around it.
        let tile = RgbaImage::from_fn(40, 40, |x, y| {
            let (dx, dy) = (x as f64 - 20.0, y as f64 - 20.0);
            let halves = (90.0 - (dx * dx + dy * dy).sqrt() * 4.0) as i16;
            match RAMP.iter().min_by_key(|(_, v)| (*v - halves).abs()) {
                Some((c, _)) if halves > 10 => {
                    image::Rgba([(c >> 16) as u8, (c >> 8) as u8, *c as u8, 255])
                }
                _ => image::Rgba([0, 0, 0, 0]),
            }
        });

        let shapes = contour(&tile, 2, MARGIN).expect("ours");
        assert!(shapes.bands.len() > 2, "a bullseye crosses several bands");
        for pair in shapes.bands.windows(2) {
            assert!(pair[0].band < pair[1].band, "the bands are out of order");
        }
        // Every band encloses something, and the strongest encloses least.
        assert!(shapes.bands.iter().all(|p| !p.triangles.is_empty()));
        let weakest = area_of(&shapes.bands[0]);
        let strongest = area_of(shapes.bands.last().expect("at least one"));
        assert!(
            strongest < weakest,
            "the heaviest band covers {strongest:.3} against the lightest {weakest:.3}"
        );
    }

    /// Whether a point is inside any of a band's triangles.
    fn covers(patch: &Patch, at: (f32, f32)) -> bool {
        patch.triangles.chunks_exact(3).any(|t| {
            let [a, b, c] = [Patch::at(t[0]), Patch::at(t[1]), Patch::at(t[2])];
            let side = |p: (f32, f32), q: (f32, f32)| {
                (q.0 - p.0) * (at.1 - p.1) - (q.1 - p.1) * (at.0 - p.0)
            };
            let (one, two, three) = (side(a, b), side(b, c), side(c, a));
            let negative = one < 0.0 || two < 0.0 || three < 0.0;
            let positive = one > 0.0 || two > 0.0 || three > 0.0;
            !(negative && positive)
        })
    }

    /// A band is the ground between its own contour and the next one up, not
    /// everything inside its own.
    ///
    /// They used to be nested shapes, each heavier one painted over the middle
    /// of the one beneath. That comes out right only while every band is
    /// opaque: give a sweep any transparency and the lighter band shows through
    /// the core it contains, so the core of a storm reads as several colours
    /// mixed rather than as the heaviest one. Rings composite honestly at any
    /// opacity, which is what lets one sweep fade into the next.
    #[test]
    fn a_band_is_the_ring_between_its_own_contour_and_the_next() {
        // The same bullseye: heavy rain in the middle, lighter around it.
        let tile = RgbaImage::from_fn(40, 40, |x, y| {
            let (dx, dy) = (x as f64 - 20.0, y as f64 - 20.0);
            let halves = (90.0 - (dx * dx + dy * dy).sqrt() * 4.0) as i16;
            match RAMP.iter().min_by_key(|(_, v)| (*v - halves).abs()) {
                Some((c, _)) if halves > 10 => {
                    image::Rgba([(c >> 16) as u8, (c >> 8) as u8, *c as u8, 255])
                }
                _ => image::Rgba([0, 0, 0, 0]),
            }
        });

        let shapes = contour(&tile, 2, MARGIN).expect("ours");
        assert!(shapes.bands.len() > 2, "a bullseye crosses several bands");

        // The centre is the core, and it belongs to the heaviest band alone.
        let middle = (0.5, 0.5);
        let over_the_core: Vec<usize> = shapes
            .bands
            .iter()
            .filter(|patch| covers(patch, middle))
            .map(|patch| patch.band)
            .collect();
        assert_eq!(
            over_the_core.len(),
            1,
            "the core is painted by bands {over_the_core:?} rather than by one"
        );
        assert_eq!(
            over_the_core[0],
            shapes.bands.last().expect("at least one").band,
            "and it is the heaviest of them"
        );

        // The lightest band has a hole in it now, which is the whole change.
        assert!(
            !covers(&shapes.bands[0], middle),
            "the lightest band still reaches over the core"
        );

        // Nowhere is painted twice, sampled across the tile rather than only in
        // the middle.
        for step_across in 0..13 {
            for step_down in 0..13 {
                let at = (step_across as f32 / 12.0, step_down as f32 / 12.0);
                let painting = shapes.bands.iter().filter(|p| covers(p, at)).count();
                assert!(painting <= 1, "{at:?} is painted by {painting} bands");
            }
        }
    }

    /// Making the bands rings must not lose any ground: what the echo covers
    /// altogether is the same, it is only divided up differently.
    #[test]
    fn the_rings_still_cover_the_whole_echo() {
        let tile = RgbaImage::from_fn(40, 40, |x, y| {
            let (dx, dy) = (x as f64 - 20.0, y as f64 - 20.0);
            let halves = (90.0 - (dx * dx + dy * dy).sqrt() * 4.0) as i16;
            match RAMP.iter().min_by_key(|(_, v)| (*v - halves).abs()) {
                Some((c, _)) if halves > 10 => {
                    image::Rgba([(c >> 16) as u8, (c >> 8) as u8, *c as u8, 255])
                }
                _ => image::Rgba([0, 0, 0, 0]),
            }
        });
        let shapes = contour(&tile, 2, MARGIN).expect("ours");

        // The rings tile the echo, so their areas sum to it. The echo's own
        // extent is measured off the sampling rather than off a band, so this
        // does not just restate the geometry it is checking.
        let total: f64 = shapes.bands.iter().map(area_of).sum();
        let mut covered = 0.0;
        const STEPS: usize = 60;
        for step_across in 0..STEPS {
            for step_down in 0..STEPS {
                let at = (
                    (step_across as f32 + 0.5) / STEPS as f32,
                    (step_down as f32 + 0.5) / STEPS as f32,
                );
                if shapes.bands.iter().any(|p| covers(p, at)) {
                    covered += 1.0;
                }
            }
        }
        let sampled = covered / (STEPS * STEPS) as f64;
        assert!(
            (total - sampled).abs() < 0.03,
            "the bands sum to {total:.3} of the tile against {sampled:.3} sampled"
        );
    }

    fn area_of(patch: &Patch) -> f64 {
        patch
            .triangles
            .chunks_exact(3)
            .map(|t| {
                let at = |p: [u16; 2]| {
                    let (x, y) = Patch::at(p);
                    (x as f64, y as f64)
                };
                let (a, b, c) = (at(t[0]), at(t[1]), at(t[2]));
                (((b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)) / 2.0).abs()
            })
            .sum()
    }

    /// The shapes are in the tile's own coordinates, which is what makes
    /// drawing them the tile's rectangle and nothing else. Anything outside
    /// them would be drawn over the neighbour.
    #[test]
    fn every_vertex_lands_inside_the_tile_it_came_from() {
        let tile = RgbaImage::from_fn(30, 30, |x, _| {
            // A ramp across, so bands run off both sides and the clip is
            // actually exercised.
            let halves = 20 + x as i16 * 4;
            let colour = RAMP
                .iter()
                .min_by_key(|(_, v)| (*v - halves).abs())
                .expect("a ramp entry")
                .0;
            image::Rgba([(colour >> 16) as u8, (colour >> 8) as u8, colour as u8, 255])
        });
        let shapes = contour(&tile, 2, MARGIN).expect("ours");
        assert!(!shapes.is_empty(), "a ramp across crosses several bands");

        // Whole numbers over one tile cannot leave it, which is half the point
        // of storing them that way. What is worth checking is that they use the
        // room: geometry crammed into a corner would mean the normalisation is
        // wrong even though every vertex is technically in range.
        let (mut widest, mut tallest) = (0u16, 0u16);
        for patch in &shapes.bands {
            for point in &patch.triangles {
                widest = widest.max(point[0]);
                tallest = tallest.max(point[1]);
            }
        }
        assert!(widest > u16::MAX / 2, "the shapes only reach {widest} across the tile");
        assert!(tallest > u16::MAX / 2, "the shapes only reach {tallest} down the tile");
    }

    /// The same property the recoloured path has to hold, for the same reason:
    /// a tile is smoothed with a collar of its neighbours' cells so its edge is
    /// right, and what it draws still stops exactly at its own boundary. Get it
    /// wrong and the tile grid shows as a seam down the whole view.
    #[test]
    fn a_traced_tile_covers_its_own_ground_and_no_more() {
        // Solid rain everywhere, so the bands are bounded only by the clip.
        let colour = RAMP.iter().find(|(_, v)| *v == 80).expect("40 dBZ").0;
        let tile = RgbaImage::from_pixel(
            24,
            24,
            image::Rgba([(colour >> 16) as u8, (colour >> 8) as u8, colour as u8, 255]),
        );
        let shapes = contour(&tile, 2, MARGIN).expect("ours");

        // Every band covers the whole tile, which in tile coordinates is 1.
        for patch in &shapes.bands {
            let covered = area_of(patch);
            assert!(
                (covered - 1.0).abs() < 0.02,
                "band {} covers {covered:.3} of its tile",
                patch.band
            );
        }
    }

    /// Which band covers a point, by the triangles alone — the last one that
    /// does, since they nest and are painted in order.
    fn band_at(shapes: &Shapes, at: [f32; 2]) -> Option<usize> {
        let mut found = None;
        for patch in &shapes.bands {
            let inside = patch.triangles.chunks_exact(3).any(|t| {
                let corner = |p: [u16; 2]| {
                    let (x, y) = Patch::at(p);
                    [x, y]
                };
                let (p0, p1, p2) = (corner(t[0]), corner(t[1]), corner(t[2]));
                let side = |p: [f32; 2], q: [f32; 2]| {
                    (q[0] - p[0]) * (at[1] - p[1]) - (q[1] - p[1]) * (at[0] - p[0])
                };
                let (ab, bc, ca) = (side(p0, p1), side(p1, p2), side(p2, p0));
                (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
            });
            if inside {
                found = Some(patch.band);
            }
        }
        found
    }

    /// The same property the recoloured path has to hold, and for the same
    /// reason: a tile is smoothed with a collar of its neighbours' cells so its
    /// contours are right at the edge, and what it draws still stops exactly at
    /// its own boundary. Get it wrong and the tile grid shows as a seam.
    ///
    /// Checked by asking both what covers the same ground rather than by
    /// comparing triangles, because two correct cuts of one shape need not use
    /// the same triangles.
    #[test]
    fn a_traced_tile_agrees_with_a_larger_fetch_of_the_same_ground() {
        // Something with structure in both directions, so contours cross the
        // seam at every angle rather than running parallel to it.
        let value = |x: u32, y: u32| -> i16 {
            let (fx, fy) = (x as f64 / 6.0, y as f64 / 5.0);
            (60.0 + 34.0 * (fx.sin() + fy.cos())) as i16
        };
        let paint = |halves: i16| {
            let colour = RAMP
                .iter()
                .min_by_key(|(_, v)| (*v - halves).abs())
                .expect("a ramp entry")
                .0;
            image::Rgba([(colour >> 16) as u8, (colour >> 8) as u8, colour as u8, 255])
        };

        // The neighbourhood, and one tile cut out of the middle of it. The
        // narrow one's own ground is columns 12..28 of the wide one — its
        // collar covers 8..32.
        let wide = RgbaImage::from_fn(40, 24, |x, y| paint(value(x, y)));
        let part = RgbaImage::from_fn(24, 24, |x, y| paint(value(x + 8, y)));

        let whole = contour(&wide, 2, MARGIN).expect("ours");
        let piece = contour(&part, 2, MARGIN).expect("ours");
        assert!(!piece.is_empty(), "the cut-out tile traced to nothing");

        // `whole` covers wide columns 4..36, `piece` covers 12..28. Walk the
        // narrow tile's ground and ask both.
        let (mut checked, mut differed) = (0, 0);
        for row in 1..20 {
            for column in 1..20 {
                let here = [column as f32 / 20.0, row as f32 / 20.0];
                // The same ground in the wide tile's coordinates.
                let there = [(8.0 + column as f32 * 16.0 / 20.0) / 32.0, here[1]];
                checked += 1;
                if band_at(&piece, here) != band_at(&whole, there) {
                    differed += 1;
                }
            }
        }
        // A handful may straddle a contour, where half a cell of rounding
        // decides it. A seam would put a whole column on the wrong side.
        assert!(
            differed * 20 <= checked,
            "{differed} of {checked} points disagree between the tile and its \
             neighbourhood - the collar is not doing its job"
        );
    }

    /// A tile the ramp no longer recognises has to refuse rather than trace
    /// nonsense, the same way the recoloured path does.
    #[test]
    fn a_tile_from_a_different_style_is_not_traced_either() {
        let foreign = RgbaImage::from_pixel(20, 20, image::Rgba([0x12, 0x34, 0x56, 255]));
        assert!(contour(&foreign, 2, MARGIN).is_none());
    }

    /// What tracing costs at its worst. This is the number the whole hybrid
    /// turns on: shapes are worth having precisely because they do not grow
    /// with the zoom, so if this ever climbs, the budget in
    /// `ui::radar::sharpness_for` is wrong. A real busy tile over a squall line
    /// comes to 368KB; this contrives a tile that is nothing but boundary.
    #[test]
    fn tracing_a_busy_tile_stays_within_its_budget() {
        // Diagonal bands across the whole tile: far more boundary than weather
        // ever draws, since real echo is blobs rather than stripes.
        let tile = RgbaImage::from_fn(107, 107, |x, y| {
            let halves = 20 + ((x + y) % 60) as i16 * 2;
            let colour = RAMP
                .iter()
                .min_by_key(|(_, v)| (*v - halves).abs())
                .expect("a ramp entry")
                .0;
            image::Rgba([(colour >> 16) as u8, (colour >> 8) as u8, colour as u8, 255])
        });
        let shapes = contour(&tile, 2, MARGIN).expect("ours");
        let bytes = shapes.triangles() * 3 * 4;
        assert!(
            bytes < 768 * 1024,
            "a worst-case tile traces to {} triangles, {:.0} KB - more than \
             `sharpness_for` budgets for",
            shapes.triangles(),
            bytes as f64 / 1024.0
        );
    }

    /// What tracing a real tile actually costs, against the live service.
    ///
    ///   cargo test -- --ignored --nocapture live_trace
    ///
    /// Worth having as a live test rather than a synthetic one because the cost
    /// is driven by how much *boundary* the weather has, and contrived weather
    /// has either far too much or far too little. Meaningful only in release.
    #[test]
    #[ignore]
    fn live_trace() {
        use super::super::radar;
        use super::super::tiles::{Layer, TileId};
        use std::time::Instant;

        let times = radar::times("conus", 1, 30);
        assert!(!times.is_empty(), "no sweeps published");
        let sources = radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times,
        };

        let (mut busiest, mut worst, mut total, mut seen) = (0usize, 0.0f64, 0.0f64, 0);
        for x in 55..70 {
            for y in 92..98 {
                let id = TileId { layer: Layer::Radar(0), level: radar::MOSAIC_LEVEL, x, y };
                let url = sources.url_for(&id).expect("conus serves the mosaic");
                let Ok(tile) = radar::fetch_tile_raw(&url) else { continue };
                let at = Instant::now();
                let Some(shapes) = contour(&tile, 2, MARGIN) else { continue };
                let took = at.elapsed().as_secs_f64() * 1000.0;
                if shapes.is_empty() {
                    continue;
                }
                seen += 1;
                total += took;
                busiest = busiest.max(shapes.triangles());
                worst = worst.max(took);
            }
        }

        println!("  {seen} tiles with weather in them");
        println!("  busiest traced to {busiest} triangles, {:.0} KB", busiest as f64 * 12.0 / 1024.0);
        println!("  {:.1} ms each on average, {:.1} ms at worst", total / seen.max(1) as f64, worst);
        assert!(seen > 0, "no weather anywhere in that band of tiles");
        // A zoom change retraces the visible loop - a few dozen tiles across
        // six workers - so a tile has to stay well inside a frame's worth.
        assert!(
            worst < 250.0,
            "the worst tile took {worst:.0} ms, which a zoom would be felt through"
        );
    }

    /// What tracing costs across the zoom range, not just at the mosaic's own
    /// level. A coarse tile covers far more ground, so it holds far more
    /// boundary, and the triangle count is what decides whether shapes can be
    /// used at every zoom or only some.
    ///
    ///   cargo test -- --ignored --nocapture live_trace_levels
    #[test]
    #[ignore]
    fn live_trace_levels() {
        use super::super::radar;
        use super::super::tiles::{Layer, TileId};
        use std::time::Instant;

        let times = radar::times("conus", 1, 30);
        assert!(!times.is_empty(), "no sweeps published");
        let sources = radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times,
        };

        // The same ground each time, over the Midwest, at every level the
        // sweeps are ever drawn from.
        for level in 4..=8 {
            let (x, y) = (60u32 >> (8 - level), 94u32 >> (8 - level));
            let id = TileId { layer: Layer::Radar(0), level, x, y };
            let Some(url) = sources.url_for(&id) else { continue };
            let Ok(tile) = radar::fetch_tile_raw(&url) else {
                println!("  level {level}: no tile");
                continue;
            };
            let at = Instant::now();
            let traced = contour(&tile, 2, MARGIN);
            let took = at.elapsed().as_secs_f64() * 1000.0;
            match traced {
                Some(shapes) => println!(
                    "  level {level}: field {}x{}  {:>7} triangles  {:>6.0} KB  {:>6.1} ms",
                    tile.width(),
                    tile.height(),
                    shapes.triangles(),
                    shapes.triangles() as f64 * 12.0 / 1024.0,
                    took
                ),
                None => println!("  level {level}: not ours"),
            }
        }
    }

    /// What a whole view costs to trace, which is the number that decides
    /// whether shapes can be used at every zoom.
    ///
    ///   cargo test -- --ignored --nocapture live_trace_views
    ///
    /// Per-tile figures mislead here: a coarse tile holds far more boundary but
    /// a coarse view holds far fewer tiles, and only the product says whether
    /// it fits. Run over ground that actually has weather on it.
    #[test]
    #[ignore]
    fn live_trace_views() {
        use super::super::radar;
        use super::super::tiles::{Layer, TileId, Viewport};
        use std::time::Instant;

        let times = radar::times("conus", 1, 30);
        assert!(!times.is_empty(), "no sweeps published");
        let sources = radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times,
        };

        // A 4K wall, which is the display this has to hold up on.
        let (w, h) = (3840.0f32, 1980.0f32);
        for (name, lat, lon, span) in [
            ("the country", 39.8, -98.6, 4500.0),
            ("a region", 41.2, -89.2, 1200.0),
            ("the radar", 41.2, -89.2, 360.0),
            ("a wall cell", 41.2, -89.2, 90.0),
        ] {
            let view = Viewport::new(lat, lon, Viewport::zoom_for_span(lat, span, h));
            let ground = view.level_within(288, w, h);
            // Shapes do not need the ground's level: they are resolution-free
            // when drawn, so what matters is holding the tile count down. Step
            // back until few enough tiles cover the view.
            let mut level = ground.min(radar::MOSAIC_LEVEL);
            while level > 2 && view.tile_count(level, w, h) > 16 {
                level -= 1;
            }
            let visible = view.visible_tiles_at(level, w, h);

            let at = Instant::now();
            let (mut triangles, mut bytes, mut fetched) = (0usize, 0usize, 0usize);
            for &(x, y) in visible.iter().take(64) {
                let id = TileId { layer: Layer::Radar(0), level, x, y };
                let Some(url) = sources.url_for(&id) else { continue };
                let Ok(tile) = radar::fetch_tile_raw(&url) else { continue };
                fetched += 1;
                if let Some(shapes) = contour(&tile, 2, MARGIN) {
                    triangles += shapes.triangles();
                    bytes += shapes.triangles() * 12;
                }
            }
            let took = at.elapsed().as_secs_f64();
            let scale = visible.len() as f64 / fetched.max(1) as f64;
            println!(
                "  {name:<13} level {level}  {:>4} tiles ({fetched} sampled)  \
                 {:>9.0} triangles  {:>7.1} MB  {:>5.1} s to trace",
                visible.len(),
                triangles as f64 * scale,
                bytes as f64 * scale / 1024.0 / 1024.0,
                took * scale
            );
        }
    }

    /// A field with a storm in it, shifted by a known amount.
    fn drifting(shift_x: f64, shift_y: f64) -> Shapes {
        let mut coarse = vec![0u8; COARSE * COARSE];
        for row in 0..COARSE {
            for column in 0..COARSE {
                // Two blobs, so the match is not ambiguous under a translation.
                let mut value: f64 = 0.0;
                for (cx, cy, size) in [(20.0, 24.0, 7.0), (40.0, 38.0, 5.0)] {
                    let dx = column as f64 - (cx + shift_x);
                    let dy = row as f64 - (cy + shift_y);
                    let near = (1.0 - (dx * dx + dy * dy).sqrt() / size).max(0.0);
                    value = value.max(near * 60.0);
                }
                coarse[row * COARSE + column] = to_byte((value * 2.0) as i16);
            }
        }
        Shapes { bands: Vec::new(), coarse }
    }

    /// The whole point: find which way the weather went, so the loop can move it
    /// there rather than jump it there.
    #[test]
    fn a_storm_that_moved_is_found_to_have_moved() {
        for (dx, dy) in [(2.0, 0.0), (0.0, -3.0), (1.0, 2.0), (-2.0, -1.0)] {
            let found = motion(&drifting(0.0, 0.0), &drifting(dx, dy)).expect("plenty of echo");
            let (fx, fy) = (found[0] as f64 * COARSE as f64, found[1] as f64 * COARSE as f64);
            assert!(
                (fx - dx).abs() < 0.5 && (fy - dy).abs() < 0.5,
                "moved by {dx},{dy} and it was read as {fx:.2},{fy:.2}"
            );
        }
    }

    /// Below a whole cell is the common case, not the exotic one: two minutes of
    /// an ordinary storm is half a coarse cell at the mosaic's own level. A
    /// whole-cell answer would round that to nothing or to double, and the loop
    /// would stutter between the two.
    #[test]
    fn half_a_cell_of_movement_is_not_rounded_away() {
        let found = motion(&drifting(0.0, 0.0), &drifting(0.5, 0.0)).expect("plenty of echo");
        let across = found[0] as f64 * COARSE as f64;
        assert!(
            (across - 0.5).abs() < 0.35,
            "moved half a cell and it was read as {across:.2}"
        );
        assert!(across > 0.15, "read as {across:.2}, which is nothing at all");
    }

    /// Standing still has to read as standing still, or the whole map creeps.
    #[test]
    fn weather_that_did_not_move_is_left_where_it_is() {
        let still = drifting(0.0, 0.0);
        let found = motion(&still, &still).expect("plenty of echo");
        let (across, down) = (found[0].abs() * COARSE as f32, found[1].abs() * COARSE as f32);
        assert!(
            across < 0.2 && down < 0.2,
            "standing still read as {across:.2},{down:.2} cells"
        );
    }

    /// Most tiles are empty sky most of the time, and an empty field correlates
    /// equally well with itself at every offset — so the answer would be noise.
    /// Saying so lets the caller use what the rest of the sky is doing.
    #[test]
    fn empty_sky_declines_to_guess() {
        let empty = Shapes { bands: Vec::new(), coarse: vec![to_byte(0); COARSE * COARSE] };
        assert!(motion(&empty, &empty).is_none());
        assert!(motion(&empty, &drifting(0.0, 0.0)).is_none());
        assert!(motion(&drifting(0.0, 0.0), &empty).is_none());
        // And a field that is not a field at all.
        let short = Shapes { bands: Vec::new(), coarse: vec![0; 4] };
        assert!(motion(&short, &drifting(0.0, 0.0)).is_none());
    }

    /// The score has to be per cell compared, not a bare sum. Slide far enough
    /// and only a corner of the two fields overlaps, and a sum of differences is
    /// always happiest where there is least to disagree about — which read as
    /// every tile bolting for one corner.
    #[test]
    fn a_slide_that_barely_overlaps_is_not_rewarded_for_it() {
        // A field that is busier on one side, which is what tempts the bias.
        let lopsided = |shift: f64| {
            let mut coarse = vec![to_byte(0); COARSE * COARSE];
            for row in 10..COARSE - 10 {
                for column in 6..20 {
                    let at = row * COARSE + (column as f64 + shift) as usize;
                    if at < coarse.len() {
                        coarse[at] = to_byte(80);
                    }
                }
            }
            Shapes { bands: Vec::new(), coarse }
        };
        let found = motion(&lopsided(0.0), &lopsided(2.0)).expect("plenty of echo");
        let across = found[0] as f64 * COARSE as f64;
        assert!(
            (across - 2.0).abs() < 0.6,
            "moved two cells and it was read as {across:.2} - the corner bias is back"
        );
    }

    /// What it makes of two real sweeps.
    ///
    ///   cargo test -- --ignored --nocapture live_motion
    ///
    /// Storms travel; they do not teleport and they do not all go different
    /// ways. Both are worth asserting against the real thing, because a plausible
    /// number here is the difference between the loop reading as motion and
    /// reading as a wobble.
    #[test]
    #[ignore]
    fn live_motion() {
        use super::super::radar;
        use super::super::tiles::{Layer, TileId};

        let times = radar::times("conus", 2, 30);
        assert!(times.len() >= 2, "need two consecutive sweeps");
        let sources = radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times: times.clone(),
        };

        let seconds = 120.0;
        let mut found: Vec<[f32; 2]> = Vec::new();
        for x in 55..70 {
            for y in 92..100 {
                let at = |frame: usize| TileId {
                    layer: Layer::Radar(frame),
                    level: radar::MOSAIC_LEVEL,
                    x,
                    y,
                };
                let Some(one) = sources.url_for(&at(0)) else { continue };
                let Some(other) = sources.url_for(&at(1)) else { continue };
                let (Ok(one), Ok(other)) =
                    (radar::fetch_tile_raw(&one), radar::fetch_tile_raw(&other))
                else {
                    continue;
                };
                let (Some(one), Some(other)) = (
                    contour(&one, SMOOTHING, MARGIN),
                    contour(&other, SMOOTHING, MARGIN),
                ) else {
                    continue;
                };
                if let Some(moved) = motion(&one, &other) {
                    found.push(moved);
                }
            }
        }

        println!("  {} tiles had enough echo to match", found.len());
        assert!(!found.is_empty(), "no weather anywhere in that band of tiles");

        // A level 8 tile spans about 118 km at these latitudes.
        let span_km = 118.0;
        let speeds: Vec<f64> = found
            .iter()
            .map(|moved| {
                let km = (moved[0].hypot(moved[1]) as f64) * span_km;
                km / seconds * 3600.0
            })
            .collect();
        let mut sorted = speeds.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        println!("  median {median:.0} km/h, fastest {:.0}", sorted[sorted.len() - 1]);
        for moved in found.iter().take(6) {
            println!("    {:+.4}, {:+.4} of a tile", moved[0], moved[1]);
        }

        assert!(
            median < 160.0,
            "the weather is apparently doing {median:.0} km/h, which it is not"
        );
    }

    /// Every colour the service draws is still one this table knows.
    ///
    /// The one that matters, and the one that cannot be checked offline:
    ///   cargo test -- --ignored --nocapture live_ramp
    ///
    /// The table was learned from the live service rather than agreed with it,
    /// so the only thing standing between it and a silent restyle is this test
    /// and the guard in [`to_dbz`].
    #[test]
    #[ignore]
    fn live_ramp() {
        use super::super::radar;
        use super::super::tiles::{Layer, TileId};

        let times = radar::times("conus", 1, 30);
        assert!(!times.is_empty(), "no sweeps published");
        let sources = radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times: times.clone(),
        };

        // A band of tiles across the middle of the country, so that whatever
        // weather there is today lands in some of them.
        let (mut painted, mut missed, mut seen) = (0usize, 0usize, 0usize);
        for x in 55..70 {
            for y in 92..98 {
                let id = TileId { layer: Layer::Radar(0), level: radar::MOSAIC_LEVEL, x, y };
                let url = sources.url_for(&id).expect("conus serves the mosaic");
                let Ok(tile) = radar::fetch_tile_raw(&url) else { continue };
                for pixel in tile.pixels() {
                    if pixel[3] < 250 {
                        continue;
                    }
                    painted += 1;
                    let rgb = ((pixel[0] as u32) << 16) | ((pixel[1] as u32) << 8) | pixel[2] as u32;
                    if dbz_of(rgb).is_none() {
                        missed += 1;
                    }
                }
                seen += 1;
            }
        }

        println!("  {seen} tiles, {painted} painted pixels, {missed} unrecognised");
        assert!(painted > 5_000, "only {painted} painted pixels - is there any weather?");
        assert!(
            missed * 1000 <= painted,
            "{missed} of {painted} pixels are colours the table does not know - \
             the mosaic has probably been restyled, and the table needs rebuilding"
        );
    }
}
