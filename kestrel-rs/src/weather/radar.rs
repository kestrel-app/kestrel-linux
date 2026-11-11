//! The National Weather Service's Enhanced Radar, which is a map rather than a
//! picture.
//!
//! radar.weather.gov offers two things and they are not the same product. Its
//! "standard version" is a per-station GIF with the basemap, the legend and the
//! timestamp drawn into it — one request, nothing to compose. The enhanced
//! viewer is the other one: a seamless national mosaic served as a transparent
//! layer, over whatever map you care to put under it. That is what somebody
//! means when they say *the radar on weather.gov*, and it is worth the extra
//! work — it has no single-radar cone, it steps every two minutes rather than
//! every five, and it is not a white rectangle in the middle of a dark wall.
//!
//! So the picture is assembled from layers, and the assembly is nearly free:
//! they all come back for the same bounding box at the same aspect, so drawing
//! them into the same rectangle in order puts every pixel where it belongs with
//! no compositing code at all.
//!
//!   * ink        the app's own background
//!   * basemap    the ground, fetched once — see [`base_url`]
//!   * warnings   active watch and warning polygons — [`super::warnings`],
//!                which are geometry rather than pixels and are the one layer
//!                this file does not fetch
//!   * radar      MRMS base reflectivity, from NCEP
//!   * labels     place names, over the weather rather than under it
//!
//! Every one of those but the basemap is a government service. There are
//! prettier basemaps — the viewer's own is a commercial one — but a dark ground
//! suits the app better than a grey one, and which map goes underneath is a
//! setting because it is taste.
//!
//! Unlike the Roku channel, nothing here writes to a file. A television has to
//! hand a Poster a path; this has memory and a texture cache, so a sweep is
//! decoded once on the poller thread and handed over as pixels.

use crate::api::http;
use super::reflectivity;

/// How many sweeps make a loop. The service holds two hours at two-minute
/// steps; twenty minutes is as far back as is worth watching move.
pub const FRAMES: usize = 10;

/// Where the radar starts, and what "Reset view" returns it to.
///
/// A place and how much ground to show, which is how the setting is expressed.
/// The map works in zoom levels; [`super::tiles::Viewport::zoom_for_span`] is
/// where one becomes the other, and it needs to know the height it is being
/// drawn into — which is why this stays a span rather than a zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Home {
    pub lat: f64,
    pub lon: f64,
    pub span_km: f64,
}

pub fn geo_root() -> &'static str {
    "https://opengeo.ncep.noaa.gov/geoserver"
}

/// The basemap, and the only thing in the weather path that is not a government
/// service.
///
/// The National Weather Service's own reference map was the first instinct and
/// the wrong picture: county outlines on black, no water, no coast fill,
/// nothing past the border. The government publishes no dark cartographic
/// canvas, and the USGS alternatives are either light and busy or stop dead at
/// the national border, which puts a white hole in the frame for anyone living
/// near one. So this is Esri's, which is what radar.weather.gov draws under its
/// own mosaic.
pub fn canvas_root() -> &'static str {
    "https://services.arcgisonline.com/ArcGIS/rest/services"
}

/// Who to credit for the basemap, which their terms ask for and which is a
/// reasonable thing to be asked.
pub fn canvas_credit() -> &'static str {
    "Basemap: Esri, HERE, Garmin, © OpenStreetMap contributors"
}

/// Which mosaic covers a coordinate. The reflectivity is published per region
/// and the layer is named after the region, so this is the whole of the
/// dispatch.
///
/// Ordered so the wide tests come last: Guam is east of nothing and west of
/// everything, and the Caribbean and the Florida Keys are the same latitude.
pub fn region(lat: f64, lon: f64) -> &'static str {
    if lat >= 50.0 {
        return "alaska";
    }
    if lat < 25.0 && lon < -140.0 {
        return "hawaii";
    }
    if lon > 100.0 {
        return "guam";
    }
    if lat < 25.0 && lon > -70.0 {
        return "carib";
    }
    "conus"
}

/// Web Mercator, for the two services that are addressed in metres.
pub fn mercator_x(lon: f64) -> f64 {
    lon * 20_037_508.34 / 180.0
}

pub fn mercator_y(lat: f64) -> f64 {
    let radians = (90.0 + lat) * std::f64::consts::PI / 360.0;
    radians.tan().ln() / (std::f64::consts::PI / 180.0) * 20_037_508.34 / 180.0
}

// ---------------------------------------------------------------- the times

/// The last `count` sweeps the region has, oldest first.
///
/// Read out of the layer's own capabilities rather than counted backwards from
/// the clock. A mosaic is published when it is ready, which is about every two
/// minutes and not exactly, and GetMap is asked for a time that exists — a
/// timestamp invented by subtracting two minutes ten times would miss.
pub fn times(region: &str, count: usize, timeout_seconds: u64) -> Vec<String> {
    let layer = format!("{region}_bref_qcd");
    let url = format!(
        "{}/{region}/{layer}/ows?service=WMS&version=1.3.0&request=GetCapabilities",
        geo_root()
    );

    let agent = http::agent(timeout_seconds, false);
    let Ok(response) = http::request(&agent, "GET", &url, None, &super::nws::headers()) else {
        return Vec::new();
    };
    if !response.ok() {
        return Vec::new();
    }
    parse_times(&response.body, count)
}

/// Picked out of the XML by hand. This is one element in an 18KB document and
/// the only thing wanted from it, which is a poor trade for parsing the whole
/// tree — and a poor trade for carrying an XML dependency to do it.
fn parse_times(body: &str, count: usize) -> Vec<String> {
    let Some(at) = body.find("name=\"time\"") else {
        return Vec::new();
    };
    let rest = &body[at..];
    let Some(opens) = rest.find('>') else {
        return Vec::new();
    };
    let Some(closes) = rest[opens..].find('<') else {
        return Vec::new();
    };

    let stamps: Vec<String> = rest[opens + 1..opens + closes]
        .split(',')
        .map(|stamp| stamp.trim().to_string())
        .filter(|stamp| !stamp.is_empty())
        .collect();

    let first = stamps.len().saturating_sub(count);
    stamps[first..].to_vec()
}

/// What to call the view. The radar is addressed by coordinate rather than by
/// station, so there is no station name to borrow — this is the service's own
/// name for where the ZIP code landed, the same one the readings are titled
/// with.
pub fn place(lat: &str, lon: &str, timeout_seconds: u64) -> String {
    if lat.is_empty() || lon.is_empty() {
        return String::new();
    }
    let Ok(doc) = super::nws::get(
        &format!("{}/points/{lat},{lon}", super::nws::ROOT),
        timeout_seconds,
    ) else {
        return String::new();
    };
    doc.get("properties").map(super::nws::place).unwrap_or_default()
}

// ---------------------------------------------------------------- tiles
//
// One URL per service per tile. The two speak different dialects — Esri serves
// a plain XYZ pyramid and GeoServer wants a WMS GetMap with a bounding box —
// so this is where the tile grid meets what each of them will answer. The
// warnings MapServer was a third dialect here until its polygons stopped being
// pictures; [`super::warnings`] talks to it now.

use super::tiles::{self, Layer, TileId};

/// The basemap's tile pyramid.
///
/// Esri publishes these as `/tile/{z}/{y}/{x}` — row before column, which is
/// the one thing about this scheme that is easy to get backwards and produces
/// a map that looks plausible and is somewhere else entirely.
fn base_tile_url(service: &str, id: &TileId) -> String {
    format!(
        "{}/{service}/MapServer/tile/{}/{}/{}",
        canvas_root(),
        id.level,
        id.y,
        id.x
    )
}

/// Which Esri service draws the ground, and which draws the lettering.
///
/// Only the dark canvas separates the two. The street and topographic maps
/// bake their names into the one picture, which is why their labels end up
/// under the weather — the price of those maps being one layer rather than two.
pub fn base_service(style: &str) -> &'static str {
    match style {
        "dark" => "Canvas/World_Dark_Gray_Base",
        "topo" => "World_Topo_Map",
        _ => "World_Street_Map",
    }
}

pub fn label_service(style: &str) -> Option<&'static str> {
    (style == "dark").then_some("Canvas/World_Dark_Gray_Reference")
}

/// One tile of reflectivity, from the region's mosaic, at one of the loop's
/// timestamps.
/// How many cells of the mosaic a tile covers, which is how many pixels it is
/// asked for — and, because that is fewer than the tile grid's 256, how much
/// the picture has to be enlarged before it is drawn.
///
/// The old rule was to ask for twice the tile size and average back down, on
/// the reasoning that GeoServer picks an overview at twice the pixels requested
/// and so hands back genuinely finer data. That is true below the mosaic's own
/// level and beside the point at it: MRMS is published on a hundredth of a
/// degree, and asking for more pixels than there are cells gets each cell
/// stamped out several times over. Those stamps are what the blur existed to
/// hide.
///
/// Now the picture is read back into numbers and resampled properly - see
/// [`super::reflectivity`] - so the right thing to ask for is exactly what the
/// mosaic holds, and not one pixel more. A cell is 0.01 degrees of latitude,
/// about 1113 metres; in Web Mercator that is 1113/cos(lat) projected metres,
/// which is why this takes the tile's own latitude rather than a constant.
///
/// It is also cheaper. At level 8 a tile has about 108 cells across at 40
/// degrees north against the 512 pixels the old rule asked for - a twentieth of
/// the pixels on the wire and a sixteenth of the texture, before any of it is
/// enlarged again for the screen.
pub fn cells_across(level: u32, top: f64, bottom: f64) -> u32 {
    const CELL_DEG: f64 = 0.01;
    const EDGE: f64 = 20_037_508.342_789_244;
    // The tile's middle latitude, from its own Mercator bounds.
    let middle = (top + bottom) / 2.0;
    let lat = (std::f64::consts::PI / 2.0
        - 2.0 * (-(middle / EDGE * std::f64::consts::PI)).exp().atan())
        .to_degrees();
    let span = EDGE * 2.0 / (1u64 << level) as f64;
    let cell = CELL_DEG / 360.0 * EDGE * 2.0 / lat.to_radians().cos().max(0.05);
    // Never more than the tile is drawn with. Below the mosaic's own level a
    // tile covers more ground than the grid has pixels for it — a level 5 tile
    // holds 849 cells and is drawn into 256 — and fetching all of them is
    // asking for detail the screen has nowhere to put. It also costs: the tile
    // would be a megabyte of texture against the grid's quarter.
    ((span / cell).round() as u32).clamp(32, tiles::TILE_PX)
}

/// The level at which the mosaic's own resolution runs out.
///
/// MRMS is published on a hundredth of a degree, about a kilometre a cell, and
/// the live service hands back 1.2km cells at level 8 and nothing finer at
/// level 9. So level 8 is the last one carrying data rather than arithmetic,
/// and a deep zoom asking for level 11 gets no more weather for sixteen times
/// the tiles.
///
/// It was 7 for a day, which was a level too mean. Having the data is not the
/// same as showing it: a tile fetched at level 7 has to be scaled up more than
/// five times to fill a 4K screen at a 220-mile view, and bilinear filtering
/// over that distance turns defined cells into mush. At 8 the same view is
/// scaled 2.7 times and an ordinary 1080p screen lands within a third of life
/// size - the picture tracks the zoom, which at 7 it visibly did not.
pub const MOSAIC_LEVEL: u32 = 8;

fn radar_tile_url(region: &str, id: &TileId, time: Option<&str>) -> String {
    let layer = format!("{region}_bref_qcd");
    let (left, bottom, right, top) = tiles::tile_bbox(id.level, id.x, id.y);
    // Exactly as many pixels as the mosaic has cells here, so every pixel that
    // comes back is one the service actually holds a number for. Asking for
    // more would only stamp each cell out several times, and stamped-out cells
    // cannot be told from real ones once they are colours.
    let cells = cells_across(id.level, top, bottom);
    // Fetched with a collar of the neighbours' cells, so the smoothing that
    // bends a band boundary has real data to work with at the tile's own edge
    // rather than averaging against nothing. Trimmed off again in
    // [`super::reflectivity::repaint`]; without it the tile grid shows as a
    // seam across the whole view. See [`super::reflectivity::MARGIN`].
    let margin = super::reflectivity::MARGIN;
    let per_cell = (right - left) / cells as f64;
    let collar = per_cell * margin as f64;
    let (left, bottom, right, top) =
        (left - collar, bottom - collar, right + collar, top + collar);
    let size = cells + margin * 2;
    // Nearest, deliberately, which is also the default. An interpolated pixel
    // is a blend of two of the ramp's colours and matches neither, so a blended
    // tile cannot be read back into reflectivity at all - blending has to
    // happen *after* the numbers are recovered, not before.
    let mut url = format!(
        "{}/{region}/{layer}/ows?service=WMS&version=1.3.0&request=GetMap\
         &layers={layer}&crs=EPSG:3857&bbox={left},{bottom},{right},{top}\
         &width={size}&height={size}&format=image/png&transparent=true",
        geo_root()
    );
    if let Some(time) = time {
        url.push_str("&time=");
        url.push_str(time);
    }
    url
}

/// Everything the tile pool needs to turn a tile into a request.
#[derive(Clone, Debug, PartialEq)]
pub struct Sources {
    pub region: String,
    pub style: String,
    /// The loop's timestamps, oldest first. A [`Layer::Radar`] indexes this.
    pub times: Vec<String>,
}

impl Sources {
    pub fn url_for(&self, id: &TileId) -> Option<String> {
        let url = match id.layer {
            Layer::Base => base_tile_url(base_service(&self.style), id),
            Layer::Labels => base_tile_url(label_service(&self.style)?, id),
            Layer::Radar(frame) => {
                // An index past the end is a frame the service has rolled past
                // since the list was fetched; asking without a time gets the
                // newest, which is the honest answer to "that one is gone".
                radar_tile_url(&self.region, id, self.times.get(frame).map(String::as_str))
            }
        };
        Some(url)
    }
}

/// One tile, fetched and decoded.
///
/// The `id` is filled in by the caller — this only knows how to turn a URL into
/// pixels, which is what keeps it usable for every layer.
///
/// `detail` is how much larger than the fetch the reflectivity's texture should
/// be, and is ignored by the layers that are pictures rather than fields.
pub fn fetch_tile(url: &str, id: &TileId) -> Result<tiles::Decoded, TileError> {
    let decoded = fetch_tile_raw(url)?;
    // The reflectivity is read back into the numbers it was drawn from and
    // recoloured at the size the screen will draw it - see
    // [`super::reflectivity::repaint`]. Everything else is a picture and stays
    // one. A tile the ramp no longer recognises comes back untouched, which is
    // the old behaviour and the right thing to fall back to.
    // The reflectivity is read back into the numbers it was drawn from and
    // traced into the bands it encloses — at every zoom, because a shape is
    // maths and a zoom is a transform, and there is nothing about being far out
    // or close in that a transform cannot express. Everything else here is a
    // picture and stays one.
    //
    // A tile the ramp no longer recognises is drawn exactly as it arrived,
    // which is what the app did before any of this and the right thing to fall
    // back to.
    if matches!(id.layer, Layer::Radar(_)) {
        if let Some(shapes) =
            reflectivity::contour(&decoded, reflectivity::SMOOTHING, reflectivity::MARGIN)
        {
            let opaque = !shapes.is_empty();
            return Ok(tiles::Decoded {
                id: TileId { layer: Layer::Base, level: 0, x: 0, y: 0 },
                width: 0,
                height: 0,
                rgba: Vec::new(),
                shapes: Some(shapes),
                opaque,
            });
        }
    }
    let (width, height) = decoded.dimensions();
    let rgba = decoded.into_raw();
    let opaque = rgba.chunks_exact(4).any(|pixel| pixel[3] > 0);

    Ok(tiles::Decoded {
        // Replaced by the caller, which is the one that knows.
        id: TileId {
            layer: Layer::Base,
            level: 0,
            x: 0,
            y: 0,
        },
        width,
        height,
        rgba,
        shapes: None,
        opaque,
    })
}

/// The picture as the service sent it, before anything is made of it.
///
/// Split out so that the reflectivity's colour table can be checked against the
/// live service without the recolouring it feeds getting in the way — see
/// `super::reflectivity::live_ramp`.
pub fn fetch_tile_raw(url: &str) -> Result<image::RgbaImage, TileError> {
    let (status, body) = http::request_bytes(
        tile_agent(),
        url,
        &super::nws::image_headers(),
        TILE_LIMIT,
    )
    .map_err(|err| TileError::Blocked(err.to_string()))?;

    if !(200..300).contains(&status) {
        let said = format!("the map service answered {status}");
        // A service shedding load, or asking to be asked more slowly. Opening
        // the map fires several hundred requests at one host, so this is an
        // ordinary answer rather than a verdict on the tile.
        return Err(if status == 408 || status == 429 || status >= 500 {
            TileError::Blocked(said)
        } else {
            TileError::Refused(said)
        });
    }
    if !is_image(&body) {
        // GeoServer answers a request it does not like with HTTP 200 and a
        // ServiceExceptionReport — an XML document served as a success. Without
        // this the tile is simply absent with nothing saying why, which is
        // exactly how a wrong layer name would present.
        let why = why_not_image(&body);
        return Err(TileError::Refused(if why.is_empty() {
            "not an image".to_string()
        } else {
            why
        }));
    }

    image::load_from_memory(&body)
        .map_err(|err| TileError::Refused(format!("undecodable: {err}")))
        .map(|image| image.to_rgba8())
}

/// Why a tile did not arrive, and whether asking again could ever help.
///
/// The distinction is the whole of it. A tile outside the mosaic's region or
/// past the level the service publishes will never come, and asking each time
/// it scrolls into view is a request thrown away. A connection reset under a
/// burst of three hundred will come on the next attempt — and treating that as
/// final is what left holes in the basemap that nothing but changing the style
/// would fill in.
#[derive(Debug)]
pub enum TileError {
    /// The service will not serve this.
    Refused(String),
    /// Something was in the way.
    Blocked(String),
}

impl std::fmt::Display for TileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileError::Refused(why) | TileError::Blocked(why) => f.write_str(why),
        }
    }
}

/// One agent for every tile, rather than one per tile.
///
/// Kept because the connections are: these are hundreds of small requests to a
/// handful of hosts, and a fresh agent each time meant a fresh TCP and TLS
/// handshake each time. Sized to the pool so all six workers can hold a
/// connection at once instead of five of them re-opening one every tile.
fn tile_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| http::pooled_agent(TILE_TIMEOUT, tiles::WORKERS))
}

/// A tile is small and there are a lot of them, so neither budget here is the
/// one a whole-screen picture wanted.
const TILE_TIMEOUT: u64 = 15;
const TILE_LIMIT: usize = 400_000;

/// Whether what came back is a picture at all.
///
/// Two formats, because the services disagree and one of them changed its mind
/// depending on which endpoint is asked. The old whole-image path requested
/// `format=png32` from ArcGIS's *export* endpoint and got PNG; the *tile*
/// endpoint serves whatever is in its cache, and for the canvas basemaps that
/// is JPEG. Checking only for PNG rejected every basemap tile — with the
/// service's own JFIF header quoted back as the reason, which is how it was
/// found.
fn is_image(body: &[u8]) -> bool {
    body.starts_with(&[0x89, b'P', b'N', b'G']) || body.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Whatever the service said instead of sending a picture, as one short line.
///
/// GeoServer answers with a ServiceExceptionReport and ArcGIS with its own
/// error document, both with a human-readable sentence in the middle naming the
/// actual problem. Stripping the markup turns "it did not work" into something
/// that can be fixed.
fn why_not_image(body: &[u8]) -> String {
    let head = &body[..body.len().min(600)];
    if head.is_empty() {
        return "it sent nothing at all".into();
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return "it sent compressed data".into();
    }

    let text = String::from_utf8_lossy(head);
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ => {
                let ch = if ch.is_whitespace() { ' ' } else { ch };
                if !(ch == ' ' && out.ends_with(' ')) {
                    out.push(ch);
                }
            }
        }
    }
    let out = out.trim();
    if out.chars().count() > 150 {
        format!("{}…", out.chars().take(150).collect::<String>())
    } else {
        out.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guam is east of nothing and west of everything, and the Caribbean and
    /// the Florida Keys sit at the same latitude — so the order of these tests
    /// is the whole of the dispatch.
    #[test]
    fn each_territory_lands_on_its_own_mosaic() {
        assert_eq!(region(42.06, -72.63), "conus", "Massachusetts");
        assert_eq!(region(25.76, -80.19), "conus", "Miami");
        assert_eq!(region(61.22, -149.90), "alaska", "Anchorage");
        assert_eq!(region(21.31, -157.86), "hawaii", "Honolulu");
        assert_eq!(region(13.44, 144.79), "guam", "Guam");
        assert_eq!(region(18.47, -66.11), "carib", "San Juan");
    }





    /// A timed request and an untimed one are different requests — the loop
    /// depends on it, and so does the fallback when a stamp has rolled past.
    #[test]
    fn a_frame_is_only_timed_when_there_is_a_time() {
        use super::tiles::{Layer, TileId};
        let sources = Sources {
            region: "conus".into(),
            style: "dark".into(),
            times: vec!["2026-08-16T18:52:00Z".into()],
        };
        let at = |layer| TileId { layer, level: 8, x: 76, y: 94 };

        let timed = sources.url_for(&at(Layer::Radar(0))).unwrap();
        assert!(timed.contains("&time=2026-08-16T18:52:00Z"));
        assert!(timed.contains("conus_bref_qcd"));
        assert!(timed.contains("EPSG:3857"));

        // An index past the end is a sweep the service has rolled past; asking
        // without a time gets the newest, which is the honest answer.
        let rolled = sources.url_for(&at(Layer::Radar(9))).unwrap();
        assert!(!rolled.contains("&time="));
    }

    /// The mosaic is a kilometre a cell, and asking GeoServer for more pixels
    /// than it has cells does not get finer weather - it gets each cell stamped
    /// out several times over. That is what the reflectivity was: a 4.3x
    /// upsample of blocks, blurred afterwards to hide them.
    #[test]
    fn the_mosaic_is_asked_for_exactly_the_cells_it_holds() {
        use std::f64::consts::PI;
        // Level 7 is the last one at or coarser than a kilometre a cell,
        // across every latitude the lower 48 covers.
        let metres_per_px = |lat: f64, level: u32| {
            156_543.033_928 * (lat * PI / 180.0).cos() / 2f64.powi(level as i32)
        };
        for lat in [25.0f64, 41.0, 49.0] {
            assert!(
                metres_per_px(lat, MOSAIC_LEVEL) >= 400.0,
                "level {MOSAIC_LEVEL} at {lat}N is {:.0}m a pixel, finer than the data",
                metres_per_px(lat, MOSAIC_LEVEL)
            );
        }

        // A level 8 tile over the lower 48 holds about a hundred cells, and
        // that is what is asked for - not the 512 the old oversampling rule
        // wanted, and not the 256 of the tile grid either.
        for (y, name) in [(94u32, "40N"), (86, "48N"), (105, "26N")] {
            let (_, bottom, _, top) = tiles::tile_bbox(MOSAIC_LEVEL, 60, y);
            let cells = cells_across(MOSAIC_LEVEL, top, bottom);
            assert!(
                (80..=160).contains(&cells),
                "a level {MOSAIC_LEVEL} tile at {name} asked for {cells} pixels"
            );
        }

        let url = Sources {
            region: "conus".into(),
            style: "dark".into(),
            times: vec!["2026-08-18T18:52:00Z".into()],
        }
        .url_for(&TileId { layer: Layer::Radar(0), level: MOSAIC_LEVEL, x: 60, y: 94 })
        .unwrap();
        // Nearest, which is the default and is load bearing: an interpolated
        // pixel is a blend of two of the ramp's colours and reads back as
        // neither. See `super::reflectivity`.
        assert!(!url.contains("interpolations"), "{url}");
        assert!(!url.contains(&format!("width={}", tiles::TILE_PX)), "{url}");
    }

    /// Only the dark canvas serves its lettering separately; the other two bake
    /// it into the ground, which is why their labels end up under the weather.
    #[test]
    fn only_the_dark_canvas_has_a_second_layer() {
        use super::tiles::{Layer, TileId};
        let at = |style: &str| Sources {
            region: "conus".into(),
            style: style.into(),
            times: Vec::new(),
        }
        .url_for(&TileId { layer: Layer::Labels, level: 8, x: 1, y: 1 });

        assert!(at("dark").is_some());
        assert!(at("street").is_none());
        assert!(at("topo").is_none());
    }

    /// Esri serves the pyramid as /tile/{z}/{y}/{x} — row before column, the
    /// one thing here that is easy to get backwards and produces a map that
    /// looks plausible and is somewhere else entirely.
    #[test]
    fn the_basemap_pyramid_is_addressed_row_before_column() {
        use super::tiles::{Layer, TileId};
        let url = Sources {
            region: "conus".into(),
            style: "street".into(),
            times: Vec::new(),
        }
        .url_for(&TileId { layer: Layer::Base, level: 8, x: 76, y: 94 })
        .unwrap();
        assert!(url.ends_with("/tile/8/94/76"), "{url}");
        assert!(url.contains("World_Street_Map"));
    }



    #[test]
    fn the_capabilities_document_gives_up_its_last_few_sweeps() {
        let body = r#"<Dimension name="time" units="ISO8601" default="current">
            2026-08-16T18:30:00Z,2026-08-16T18:32:00Z,2026-08-16T18:34:00Z,
            2026-08-16T18:36:00Z</Dimension>"#;

        let all = parse_times(body, 10);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], "2026-08-16T18:30:00Z");

        // Oldest first, and the newest is always kept.
        let last_two = parse_times(body, 2);
        assert_eq!(last_two, vec!["2026-08-16T18:34:00Z", "2026-08-16T18:36:00Z"]);
    }

    /// A document that does not parse must come back empty so the untimed
    /// fallback runs, rather than half a list of nonsense.
    #[test]
    fn an_unparseable_capabilities_document_yields_nothing() {
        assert!(parse_times("", 10).is_empty());
        assert!(parse_times("<Capabilities/>", 10).is_empty());
        assert!(parse_times("name=\"time\"", 10).is_empty());
    }

    /// GeoServer answers a request it does not like with HTTP 200 and XML.
    #[test]
    fn an_error_document_is_not_mistaken_for_a_picture() {
        let xml = br#"<?xml version="1.0"?><ServiceExceptionReport>
            <ServiceException code="LayerNotDefined">Could not find layer
            conus_bref_qcx</ServiceException></ServiceExceptionReport>"#;
        assert!(!is_image(xml));

        let why = why_not_image(xml);
        assert!(why.contains("Could not find layer"), "got: {why}");
        assert!(!why.contains('<'), "markup survived: {why}");
    }

    /// Both formats, because the services disagree: GeoServer answers PNG and
    /// Esri's tile cache answers JPEG for the canvas basemaps.
    #[test]
    fn both_formats_the_services_serve_are_recognised() {
        assert!(is_image(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]), "png");
        assert!(is_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F']), "jpeg");
        assert!(!is_image(b"GIF89a"));
        assert!(!is_image(b"<?xml version=\"1.0\"?><ServiceExceptionReport/>"));
        assert!(!is_image(&[]));
    }

    #[test]
    fn the_three_ways_a_layer_can_arrive_wrong_read_differently() {
        assert_eq!(why_not_image(&[]), "it sent nothing at all");
        assert_eq!(why_not_image(&[0x1f, 0x8b, 0x08]), "it sent compressed data");
        assert!(why_not_image(b"{\"error\":{\"message\":\"Invalid bbox\"}}").contains("Invalid bbox"));
    }

    /// Every layer against the real services, as tiles. Ignored by default:
    ///   cargo test -- --ignored --nocapture live_radar
    ///
    /// This is the one worth running by hand after touching a URL. The three
    /// layers come from two organisations in two dialects, each of which
    /// answers a malformed request with a 200 and a document rather than an
    /// error, and none of it is covered by anything checkable offline. The
    /// warning polygons are checked the same way in [`super::warnings`].
    #[test]
    #[ignore]
    fn live_radar() {
        use super::tiles::{self, Layer, TileId, Viewport};

        let zip = std::env::var("KESTREL_TEST_ZIP").unwrap_or_else(|_| "01001".into());
        let found = crate::weather::zip::lookup(&zip).expect("a ZIP in the gazetteer");
        let (lat, lon) = (
            found.lat.parse::<f64>().unwrap(),
            found.lon.parse::<f64>().unwrap(),
        );

        let region = region(lat, lon);
        println!("  region  {region}");
        println!("  place   {}", place(&found.lat, &found.lon, 20));

        let times = times(region, FRAMES, 20);
        println!("  sweeps  {} available", times.len());
        assert!(!times.is_empty(), "the capabilities document gave no times");

        // The tile under the coordinate, at a level worth looking at.
        let view = Viewport::new(lat, lon, 8.0);
        let level = view.level();
        let (cx, cy) = view.centre_px();
        let (x, y) = (
            (cx / tiles::TILE_PX as f64) as u32,
            (cy / tiles::TILE_PX as f64) as u32,
        );
        println!("  tile    {level}/{x}/{y}");

        let sources = Sources {
            region: region.to_string(),
            style: "dark".into(),
            times: times.clone(),
        };

        for layer in [Layer::Base, Layer::Labels, Layer::Radar(times.len() - 1)] {
            let id = TileId { layer, level, x, y };
            let url = sources.url_for(&id).expect("the dark canvas serves all three");
            match fetch_tile(&url, &id) {
                Ok(tile) => println!(
                    "  {:<16} {}x{}  {}",
                    format!("{layer:?}"),
                    tile.width,
                    tile.height,
                    if tile.opaque { "painted" } else { "empty" }
                ),
                Err(err) => panic!("{layer:?} failed: {err}\n  {url}"),
            }
        }
    }

    /// A very long complaint is trimmed rather than shown whole under the
    /// picture.
    #[test]
    fn a_long_complaint_is_cut_short() {
        let long = vec![b'x'; 500];
        let why = why_not_image(&long);
        assert!(why.chars().count() <= 151, "got {} chars", why.chars().count());
        assert!(why.ends_with('…'));
    }
}
