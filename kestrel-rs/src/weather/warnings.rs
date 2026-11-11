//! Watches, warnings and advisories as shapes rather than as pictures.
//!
//! These used to arrive the way the reflectivity still does: as tiles, rendered
//! by the service and stacked onto the map. That works, and it costs more than
//! it looks like it does. A picture of a polygon cannot say what it is — a red
//! shape is a red shape — so naming the hazards in view took a second request,
//! and answering "what is that" under the pointer took a third, to an identify
//! endpoint that had to be told how big the map was so it could guess how close
//! counted as *on*. The border had to be recovered by edge-detecting the
//! rendered fill, which is a per-pixel pass over every tile, and it came back
//! resampled at whatever level the zoom had settled on.
//!
//! The same MapServer that renders those tiles will hand back the geometry it
//! renders them from, and it is one request for both layers where the tiles
//! were dozens of requests each.
//!
//! What makes that affordable at every zoom is the simplification tolerance,
//! which is handed in as the size of one screen pixel. The whole country at
//! full fidelity is 29MB and 709,000 vertices; at a pixel's tolerance it is
//! 38,000. So the cost still rises as the view widens — there is more country
//! in it — but nothing like as fast as the ground does. Measured against the
//! live service on an ordinary afternoon, in vertices: a 60-mile wall cell
//! 1,077, a 226-mile radar view 13,502, the whole lower 48 on a 4K screen
//! 38,406. Two thousand times the ground for thirty-six times the geometry,
//! because the geometry is only ever as fine as the screen can draw.
//!
//! What that buys, beyond the bytes: the borders are strokes rather than
//! resampled pixels, so they stay crisp at any zoom; the key is read off the
//! features already in hand rather than asked for; and a click is a
//! point-in-polygon test against those same features, which is exact and
//! instant rather than a round trip and a tolerance.
//!
//! Everything here is in Web Mercator metres. The service will project on the
//! way out — `outSR=3857` with `f=geojson`, which it honours despite GeoJSON
//! nominally being a WGS84 format — and metres are what the map is drawn in, so
//! a vertex goes to the screen through an affine transform with no projection
//! maths per point and no trigonometry at all.

use crate::api::http;

/// The two layers, which are different products rather than a split of one.
///
/// Layer 0 carries the short-fuse, storm-based warnings a storm raises within
/// the hour — tornado, severe thunderstorm, flash flood, snow squall, special
/// marine. Five kinds, and on an ordinary day under one percent of the map.
/// Layer 1 is everything else: watches, advisories and statements that stand
/// for days, and on a summer afternoon of heat advisories a seventh of the
/// country in solid colour.
const SHORT_FUSE: u8 = 0;
const STANDING: u8 = 1;

/// How solid a polygon's interior is drawn, out of 255.
///
/// The service fills its polygons and a fill is opaque, so a summer afternoon
/// of heat advisories arrives as a seventh of the country in flat colour with
/// the map underneath it gone. Washing the inside out lets the ground and the
/// weather read straight through, and keeping the border solid is what stops
/// that costing the shape — an outline is how you see where a warning *is*, and
/// it is the one part of the polygon that carries no information behind it.
///
/// Two strengths, because the two layers do different jobs. A watch or an
/// advisory is context, an area that stands all day, and wants to be faint
/// enough to read a county name through. A short-fuse warning is the thing that
/// must not be missed.
///
/// These are the numbers the raster pass used, kept exactly: what changed is
/// where the wash is applied, not how the map looks. It used to be found by
/// comparing neighbouring pixels and dimming the ones that were not on an edge;
/// now the border is simply drawn as the stroke it is, and the wash is how much
/// of the hazard is left showing after the ground is drawn back over it. See
/// how the fill is drawn in `ui::radar`, which is where the two numbers are
/// used and where it matters that they are read differently.
pub const WATCH_WASH: u8 = 90;
pub const WARNING_WASH: u8 = 165;

/// One watch, warning or advisory, with the ground it covers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hazard {
    /// The service's own name for it: "Tornado Warning", "Flood Advisory".
    /// This is what [`super::hazards::colour_of`] is keyed on.
    pub event: String,
    /// Whether it came from the short-fuse layer, which decides how solidly it
    /// is drawn and which of the two goes on top.
    pub short_fuse: bool,
    /// When it runs out, in the service's own offset-bearing spelling.
    pub expires: String,
    /// The alert on api.weather.gov, which is where the wording lives.
    pub url: String,
    /// The office that issued it.
    pub office: String,
    /// The ground, in Web Mercator metres. One entry per polygon — a hazard
    /// issued over an archipelago is one feature with many.
    pub patches: Vec<Patch>,
}

/// One polygon: what to fill and what to stroke.
///
/// Both, because they are different shapes of the same thing. The fill is a
/// triangle list because a GPU cannot fill a concave outline, and the
/// county-aggregated advisories are wildly concave. The rings are kept
/// alongside because a border is the closed path it arrived as rather than the
/// edges of the triangles the ground was cut into — and because a click is
/// tested against them, so what is drawn and what is clickable come from the
/// same two readings of one thing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Patch {
    /// Three vertices per triangle, in Web Mercator metres.
    pub fill: Vec<[f64; 2]>,
    /// The outer ring first, then any holes. Not closed — the last vertex joins
    /// the first.
    pub rings: Vec<Vec<[f64; 2]>>,
    /// Left, bottom, right, top. Kept so a click can reject a polygon without
    /// walking it, which is most of them most of the time.
    pub bounds: (f64, f64, f64, f64),
}

impl Hazard {
    /// Whether a point in Mercator metres falls inside this hazard.
    pub fn covers(&self, x: f64, y: f64) -> bool {
        self.patches.iter().any(|patch| patch.covers(x, y))
    }
}

impl Patch {
    pub fn covers(&self, x: f64, y: f64) -> bool {
        let (left, bottom, right, top) = self.bounds;
        if x < left || x > right || y < bottom || y > top {
            return false;
        }
        // Even-odd against every ring at once. A hole is a ring like any other,
        // so a point inside the outer ring and inside a hole crosses both an
        // even number of times over and is correctly outside.
        self.rings
            .iter()
            .filter(|ring| in_ring(ring, [x, y]))
            .count()
            % 2
            == 1
    }
}

// ---------------------------------------------------------------- fetching

pub fn service_root() -> &'static str {
    "https://mapservices.weather.noaa.gov/eventdriven/rest/services/WWA/watch_warn_adv/MapServer"
}

/// Everything in force inside a bounding box, both layers, ready to draw.
///
/// `bbox` is left, bottom, right, top in Web Mercator metres — the view's own
/// extent. `tolerance` is how far a simplified vertex may stray from where it
/// belongs, in the same metres; pass the view's metres-per-pixel and the
/// simplification is by construction invisible.
///
/// Ordered so that drawing the list front to back puts the serious things on
/// top: the service maintains a priority order for exactly this reason, and a
/// tornado warning underneath a special weather statement is a map that has
/// buried its own point.
pub fn fetch(bbox: (f64, f64, f64, f64), tolerance: f64, timeout_seconds: u64) -> Vec<Hazard> {
    let agent = http::agent(timeout_seconds, false);
    let mut found = Vec::new();

    for layer in [STANDING, SHORT_FUSE] {
        let Ok(response) = http::request(
            &agent,
            "GET",
            &query_url(bbox, tolerance, layer),
            None,
            &super::nws::headers(),
        ) else {
            continue;
        };
        if !response.ok() {
            continue;
        }
        let Ok(doc) = response.json() else { continue };
        found.extend(parse(&doc, layer == SHORT_FUSE));
    }

    // Least serious first, so that painting them in order leaves the most
    // serious on top - a tornado warning under a heat advisory is a map that
    // has buried its own point. Stable within a rank, so a refetch does not
    // shuffle equals about.
    found.sort_by_key(|hazard| std::cmp::Reverse(rank(&hazard.event)));
    found
}

/// What the query looks like on the wire.
///
/// `maxAllowableOffset` is the whole economy of this: 1405 features at full
/// fidelity is 29MB and 709,000 vertices, and the same 1405 at a five-kilometre
/// tolerance is 832KB and 28,870. Nothing on a screen can show the difference,
/// because the tolerance is handed in as the size of a pixel.
///
/// `geometryPrecision=0` rounds the coordinates to the metre on the way out,
/// which costs nothing at any zoom a map is read at and takes the decimal tail
/// off every number in the document.
fn query_url(bbox: (f64, f64, f64, f64), tolerance: f64, layer: u8) -> String {
    let (left, bottom, right, top) = bbox;
    // A tolerance of zero asks for full fidelity, which for a wide view is the
    // 29MB document. There is no view where sub-metre geometry is wanted, so
    // the floor is a metre rather than nothing.
    let tolerance = tolerance.max(1.0);
    format!(
        "{}/{layer}/query?geometry={left},{bottom},{right},{top}\
         &geometryType=esriGeometryEnvelope&inSR=3857&outSR=3857\
         &spatialRel=esriSpatialRelIntersects&returnGeometry=true\
         &maxAllowableOffset={tolerance:.0}&geometryPrecision=0\
         &outFields=prod_type,expiration,url,wfo&where=1%3D1&f=geojson",
        service_root()
    )
}

/// Pull the hazards out of one layer's answer.
///
/// The properties are the service's own field names, which for the query
/// endpoint are the short database ones — `prod_type`, `wfo` — and not the
/// human-readable ones the identify endpoint used to answer with. Getting this
/// wrong is quiet rather than loud: `event` exists on these features and holds
/// a four-digit sequence number, so a hazard would be named "0334".
fn parse(doc: &serde_json::Value, short_fuse: bool) -> Vec<Hazard> {
    let Some(features) = doc.get("features").and_then(|f| f.as_array()) else {
        return Vec::new();
    };

    let mut found: Vec<Hazard> = Vec::new();
    for feature in features {
        let properties = feature.get("properties").cloned().unwrap_or_default();
        let event = super::pick(&properties, &["prod_type"]);
        if event.is_empty() {
            continue;
        }
        let patches = patches_of(feature.get("geometry"));
        if patches.is_empty() {
            continue;
        }
        found.push(Hazard {
            event,
            short_fuse,
            expires: super::pick(&properties, &["expiration"]),
            url: super::pick(&properties, &["url"]),
            office: super::pick(&properties, &["wfo"]),
            patches,
        });
    }
    found
}

/// A GeoJSON geometry as patches. Polygon and MultiPolygon, which are the only
/// two the service sends — 1260 and 145 of them respectively, nationally.
fn patches_of(geometry: Option<&serde_json::Value>) -> Vec<Patch> {
    let Some(geometry) = geometry else {
        return Vec::new();
    };
    let kind = super::pick(geometry, &["type"]);
    let Some(coordinates) = geometry.get("coordinates").and_then(|c| c.as_array()) else {
        return Vec::new();
    };

    match kind.as_str() {
        "Polygon" => patches_of_polygon(coordinates),
        "MultiPolygon" => coordinates
            .iter()
            .filter_map(|polygon| polygon.as_array())
            .flat_map(|rings| patches_of_polygon(rings))
            .collect(),
        _ => Vec::new(),
    }
}

/// One GeoJSON polygon — an outer ring followed by its holes — as one patch.
///
/// Nothing here has to decide which ring is which, and that is worth having:
/// GeoJSON says the first ring encloses and the rest are holes in it, and the
/// service's own data does not always agree. Esri distinguishes a hole from an
/// island by which way the ring is wound, and when the simplification collapses
/// a small island into a sliver it can come out wound the other way — so the
/// conversion to GeoJSON files it as a hole in whatever ring was listed first.
/// Live, that is a Rip Current Statement whose "hole" sits half a kilometre
/// north of its outer ring, sharing no ground with it at all, and a Small Craft
/// Advisory carrying 72 of them of which 19 are nowhere near the zone.
///
/// Both the fill and the click read the rings by the even-odd rule, which gives
/// the right answer either way: a ring inside another takes ground out of it,
/// and a ring beside it adds ground of its own. So a mislabelled island is
/// drawn as the island it is, without anything having had to work out that that
/// is what it was.
fn patches_of_polygon(rings: &[serde_json::Value]) -> Vec<Patch> {
    let rings: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .filter_map(|ring| ring.as_array().map(|points| ring_of(points)))
        .filter(|ring: &Vec<[f64; 2]>| ring.len() >= 3)
        .collect();
    if rings.is_empty() {
        return Vec::new();
    }

    let mut bounds = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for point in rings.iter().flatten() {
        bounds.0 = bounds.0.min(point[0]);
        bounds.1 = bounds.1.min(point[1]);
        bounds.2 = bounds.2.max(point[0]);
        bounds.3 = bounds.3.max(point[1]);
    }

    vec![Patch {
        fill: triangulate(&rings),
        rings,
        bounds,
    }]
}

use super::fill::in_ring;

/// One ring, with the closing repeat of the first vertex dropped.
///
/// GeoJSON closes its rings explicitly and everything downstream here treats
/// the last vertex as joining the first, so leaving the repeat in would put a
/// zero-length edge in the ring, and a zero-length edge is one that crosses a
/// band without ever being on either side of it.
fn ring_of(points: &[serde_json::Value]) -> Vec<[f64; 2]> {
    let mut ring: Vec<[f64; 2]> = points
        .iter()
        .filter_map(|point| {
            let pair = point.as_array()?;
            Some([pair.first()?.as_f64()?, pair.get(1)?.as_f64()?])
        })
        .collect();
    if ring.len() > 1 && ring.first() == ring.last() {
        ring.pop();
    }
    ring
}

// ----------------------------------------------------------- the fill

/// Cut a polygon into triangles, holes and all.
///
/// The work is [`super::fill`], which the reflectivity's contours use too. What
/// belongs here is only what is true of *these* shapes: they arrive as closed
/// rings, and they cannot be trusted not to cross themselves — 38 of some 5000
/// outer rings in a national fetch do, once the service has simplified them,
/// and every one is a coastal strip that would otherwise be filled straight
/// across a gap of open water. So the crossings are looked for.
///
/// Nothing is clipped: a warning polygon is fetched whole for the view rather
/// than per tile, so there is no neighbour's ground in it to trim off.
fn triangulate(rings: &[Vec<[f64; 2]>]) -> Vec<[f64; 2]> {
    super::fill::triangulate(&super::fill::edges_of(rings), true, None)
}

// ------------------------------------------------------------- reading it

/// How far up the key a hazard belongs. Lower sorts first.
pub fn rank(hazard: &str) -> u8 {
    if hazard.ends_with("Warning") {
        0
    } else if hazard.ends_with("Watch") {
        1
    } else if hazard.ends_with("Advisory") {
        2
    } else {
        3
    }
}

/// The distinct hazards in a set, most serious first — the key.
///
/// Read off what has already been fetched rather than asked for. This used to
/// be a request of its own, to the same MapServer with `returnGeometry=false`,
/// because the polygons arrived as pixels and the picture could not say what
/// its colours meant. The shapes know their own names.
pub fn kinds(hazards: &[Hazard]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for hazard in hazards {
        if !found.iter().any(|had| had == &hazard.event) {
            found.push(hazard.event.clone());
        }
    }
    found.sort_by_key(|hazard| (rank(hazard), hazard.clone()));
    found
}

/// One watch, warning or advisory the map was asked about, as a line to read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Warning {
    /// What it is: "Tropical Storm Warning", "Flood Advisory".
    pub event: String,
    /// What the service says about it — the headline where there is one, the
    /// area and the expiry otherwise.
    pub said: String,
    pub severe: bool,
}

/// One kind of hazard under a click, and every alert of that kind there.
///
/// The split between this and [`Warning`] is the split between what the map
/// already knows and what it would have to ask: this comes out of the geometry
/// the instant the click lands, and the wording is fetched afterwards for the
/// ones worth fetching.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub event: String,
    pub expires: String,
    pub office: String,
    /// Every alert of this kind covering the point. One product is issued
    /// separately for each zone it covers, so a point can sit under three
    /// alerts that are the same warning.
    pub urls: Vec<String>,
}

impl Found {
    /// The answer the geometry alone can give, which is available immediately.
    pub fn locally(&self) -> Warning {
        let said = match super::clock_from_offset(&self.expires) {
            Some(clock) => format!("until {}", super::clock_text(clock, false)),
            None => self.office.clone(),
        };
        Warning {
            event: self.event.clone(),
            said,
            // Refined by [`wording`] when the alert lands. Until then the
            // product's own name is the best available answer, and it is the
            // right one nearly always: the service reserves "Warning" for
            // conditions posing a threat to life or property.
            severe: rank(&self.event) == 0,
        }
    }
}

/// What covers a point, from the shapes already on screen.
///
/// This used to be the MapServer's identify endpoint: a round trip that had to
/// be handed the map's extent so it could scale a tolerance, and which was
/// asked speculatively because the tiles could only say whether *something* was
/// painted near the click. Now the click is tested against the geometry the map
/// was drawn from, so a click on empty ground costs nothing and a click on a
/// polygon is exact at the edge.
///
/// Gathered by what it *is*, not by what it says. One product issued over three
/// neighbouring zones is three features saying the same thing, and three
/// identical lines is not three facts.
pub fn at_point(hazards: &[Hazard], x: f64, y: f64) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    // Most serious first, which is the order they are worth reading in and the
    // reverse of the order they are drawn in.
    let mut under: Vec<&Hazard> = hazards.iter().filter(|h| h.covers(x, y)).collect();
    under.sort_by_key(|hazard| rank(&hazard.event));

    for hazard in under {
        match found.iter_mut().find(|had| had.event == hazard.event) {
            Some(had) => {
                if !hazard.url.is_empty() && !had.urls.contains(&hazard.url) {
                    had.urls.push(hazard.url.clone());
                }
            }
            None => {
                if found.len() >= 4 {
                    break;
                }
                found.push(Found {
                    event: hazard.event.clone(),
                    expires: hazard.expires.clone(),
                    office: hazard.office.clone(),
                    urls: if hazard.url.is_empty() {
                        Vec::new()
                    } else {
                        vec![hazard.url.clone()]
                    },
                });
            }
        }
    }
    found
}

/// The same hazards, with the wording the alerts themselves carry.
///
/// The geometry says what a hazard is and when it runs out, which is enough to
/// answer a click the moment it lands. It does not carry the headline, which
/// lives on the alert — so this is still a request, and still worth making, but
/// it is now an enrichment of an answer already on screen rather than the thing
/// being waited for. Nothing here blocks the picture, and a failure leaves the
/// local answer standing.
///
/// Bounded, because a point inside a dozen overlapping advisories should not
/// become a dozen round trips.
pub fn wording(found: &[Found]) -> Vec<Warning> {
    const FETCHES: usize = 3;
    let mut spent = 0;
    let mut out = Vec::with_capacity(found.len());

    for hazard in found {
        let mut line = hazard.locally();
        let mut headline = String::new();
        let mut areas: Vec<String> = Vec::new();

        for url in &hazard.urls {
            if spent >= FETCHES || !url.starts_with("https://") {
                break;
            }
            spent += 1;
            let Some((said, area, severe)) = describe(url) else {
                continue;
            };
            if headline.is_empty() {
                headline = said;
            }
            if !area.is_empty() && !areas.contains(&area) {
                areas.push(area);
            }
            line.severe |= severe;
        }

        // The panel names the hazard on the line above this one.
        let headline = super::without_event(&headline, &hazard.event);
        let area = areas.join(", ");
        let said = match (headline.is_empty(), area.is_empty()) {
            (false, false) => format!("{headline}  ·  {area}"),
            (false, true) => headline,
            (true, false) => area,
            (true, true) => line.said.clone(),
        };
        line.said = said;
        out.push(line);
    }
    out
}

/// The headline of one alert, where it applies, and whether it is a severe one.
fn describe(url: &str) -> Option<(String, String, bool)> {
    let agent = http::agent(10, false);
    let response = http::request(&agent, "GET", url, None, &super::nws::headers()).ok()?;
    if !response.ok() {
        return None;
    }
    let properties = response.json().ok()?.get("properties")?.clone();

    let headline = super::pick(&properties, &["headline"]);
    let area = super::pick(&properties, &["areaDesc"]);
    let severity = super::pick(&properties, &["severity"]).to_ascii_lowercase();
    let severe = severity == "extreme" || severity == "severe";

    if headline.is_empty() && area.is_empty() {
        return None;
    }
    Some((headline, area, severe))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twice the area of a triangle list, which for a correct cut equals twice
    /// the area of what was cut.
    fn covered(triangles: &[[f64; 2]]) -> f64 {
        triangles
            .chunks_exact(3)
            .map(|t| {
                ((t[1][0] - t[0][0]) * (t[2][1] - t[0][1])
                    - (t[1][1] - t[0][1]) * (t[2][0] - t[0][0]))
                    .abs()
            })
            .sum()
    }

    fn in_triangles(triangles: &[[f64; 2]], point: [f64; 2]) -> bool {
        triangles.chunks_exact(3).any(|t| {
            let side = |p: [f64; 2], q: [f64; 2]| {
                (q[0] - p[0]) * (point[1] - p[1]) - (q[1] - p[1]) * (point[0] - p[0])
            };
            let (ab, bc, ca) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
            (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
        })
    }

    /// A patch built straight from rings, the way the parser builds one.
    fn patch(rings: Vec<Vec<[f64; 2]>>) -> Patch {
        let mut bounds = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for point in rings.iter().flatten() {
            bounds.0 = bounds.0.min(point[0]);
            bounds.1 = bounds.1.min(point[1]);
            bounds.2 = bounds.2.max(point[0]);
            bounds.3 = bounds.3.max(point[1]);
        }
        Patch {
            fill: triangulate(&rings),
            rings,
            bounds,
        }
    }

    /// One polygon's worth of ground, cut into triangles.
    fn cut(rings: Vec<Vec<[f64; 2]>>) -> Vec<[f64; 2]> {
        triangulate(&rings)
    }

    /// Whether a ring crosses itself, which the service's own simplification
    /// leaves behind on a coastal strip often enough to matter. O(n²), which
    /// for rings of a few dozen vertices is nothing.
    fn self_crossing(ring: &[[f64; 2]]) -> bool {
        let n = ring.len();
        let side = |a: [f64; 2], b: [f64; 2], c: [f64; 2]| {
            let turn = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            turn.partial_cmp(&0.0).unwrap()
        };
        (0..n).any(|i| {
            ((i + 2)..n).any(|j| {
                // Neighbouring edges share a vertex and always "touch".
                if (j + 1) % n == i {
                    return false;
                }
                let (p, q) = (ring[i], ring[(i + 1) % n]);
                let (a, b) = (ring[j], ring[(j + 1) % n]);
                side(p, q, a) != side(p, q, b) && side(a, b, p) != side(a, b, q)
            })
        })
    }

    fn square(left: f64, bottom: f64, side: f64) -> Vec<[f64; 2]> {
        vec![
            [left, bottom],
            [left + side, bottom],
            [left + side, bottom + side],
            [left, bottom + side],
        ]
    }

    #[test]
    fn a_square_is_two_triangles_and_all_of_its_area() {
        let out = cut(vec![square(0.0, 0.0, 10.0)]);
        assert!((covered(&out) - 200.0).abs() < 1e-9, "twice 10x10");
        assert!(in_triangles(&out, [5.0, 5.0]));
    }

    /// The whole reason this exists. egui fills a path as a fan from its first
    /// vertex, which paints a concavity solid - and the county-aggregated
    /// advisories are nothing but concavities. A fan over this shape would
    /// cover the notch; a correct cut leaves it empty.
    #[test]
    fn a_notch_is_not_filled_in() {
        // An L: a 10x10 square with the top-right quarter taken out.
        let ell = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 5.0],
            [5.0, 5.0],
            [5.0, 10.0],
            [0.0, 10.0],
        ];
        let out = cut(vec![ell]);

        // 100 minus the 25 that was cut away.
        assert!((covered(&out) - 150.0).abs() < 1e-9, "twice 75, got {}", covered(&out) / 2.0);
        assert!(in_triangles(&out, [2.0, 2.0]), "the body of the shape is filled");
        assert!(
            !in_triangles(&out, [7.5, 7.5]),
            "the notch is outside the shape and must stay empty"
        );
    }

    /// A ring that arrives clockwise is the same ground as one that arrives
    /// counter-clockwise. The service sends both — and its own converter flips
    /// one now and again — so nothing here may depend on which. The even-odd
    /// rule does not.
    #[test]
    fn winding_does_not_change_the_ground() {
        let mut backwards = square(0.0, 0.0, 10.0);
        backwards.reverse();
        assert!((covered(&cut(vec![backwards])) - 200.0).abs() < 1e-9);
    }

    /// A national fetch carries hundreds of holes — 617 across 88 features the
    /// day this was written.
    /// A hole that is not honoured is a warning drawn over ground it does not
    /// cover.
    #[test]
    fn a_hole_is_left_out_of_the_fill() {
        let out = cut(vec![square(0.0, 0.0, 20.0), square(8.0, 8.0, 4.0)]);

        // 400 minus the 16 of the hole.
        assert!(
            (covered(&out) - 768.0).abs() < 1e-6,
            "twice 384, got {}",
            covered(&out) / 2.0
        );
        assert!(!in_triangles(&out, [10.0, 10.0]), "the hole is empty");
        assert!(in_triangles(&out, [2.0, 2.0]), "the ring around it is not");
        assert!(in_triangles(&out, [18.0, 10.0]), "including beside the hole");
    }

    /// Two holes, so the second has to bridge into a ring the first has already
    /// been spliced into.
    #[test]
    fn two_holes_are_both_left_out() {
        let out = cut(vec![
            square(0.0, 0.0, 30.0),
            square(4.0, 4.0, 4.0),
            square(20.0, 20.0, 4.0),
        ]);

        assert!(
            (covered(&out) - (900.0 - 32.0) * 2.0).abs() < 1e-6,
            "twice 868, got {}",
            covered(&out) / 2.0
        );
        assert!(!in_triangles(&out, [6.0, 6.0]), "the first hole is empty");
        assert!(!in_triangles(&out, [22.0, 22.0]), "and so is the second");
        assert!(in_triangles(&out, [15.0, 15.0]), "the ground between them is not");
    }

    /// The fill and the click have to agree. A shape drawn with a hole in it
    /// and a click that reports a warning in the middle of that hole is worse
    /// than either fault alone.
    #[test]
    fn the_fill_and_the_click_agree_about_a_hole() {
        let patch = patch(vec![square(0.0, 0.0, 20.0), square(8.0, 8.0, 4.0)]);

        assert!(patch.covers(2.0, 2.0), "inside the ring");
        assert!(!patch.covers(10.0, 10.0), "inside the hole is outside the hazard");
        assert!(!patch.covers(25.0, 10.0), "and outside is outside");
        // The bounding box is the cheap rejection and must not reject anything
        // real - a warning that cannot be clicked is a warning that is not
        // there.
        for (x, y) in [(0.5, 0.5), (19.5, 19.5), (0.5, 19.5), (19.5, 0.5)] {
            assert!(patch.covers(x, y), "the corner at {x},{y} is inside");
        }
    }

    /// Real geometry is not always a polygon at all. A ring can be a straight
    /// line or a single point repeated, and either has to come back as nothing
    /// to draw rather than as an error or a hang — a wall that stops repainting
    /// is the worst outcome available here.
    #[test]
    fn a_degenerate_ring_encloses_nothing() {
        // Every vertex collinear: no band has any height, so there is no ground
        // between the crossings.
        let flat: Vec<[f64; 2]> = (0..8).map(|i| [i as f64, 0.0]).collect();
        assert!(covered(&cut(vec![flat])) < 1e-9, "a line encloses nothing");

        // The same vertex over and over.
        assert!(covered(&cut(vec![vec![[1.0, 1.0]; 6]])) < 1e-9);

        // And nothing at all.
        assert!(triangulate(&[]).is_empty());
        assert!(cut(vec![vec![[0.0, 0.0], [1.0, 1.0]]]).is_empty());
    }

    /// Esri distinguishes a hole from an island by which way the ring is wound,
    /// and the simplification can flip a collapsed sliver - so the conversion to
    /// GeoJSON files an island as a hole in whatever ring was listed first.
    /// Live, that is a Rip Current Statement whose "hole" sits half a kilometre
    /// north of its outer ring.
    ///
    /// Nothing has to work out which is which. A ring beside another adds its
    /// own ground and a ring inside one takes ground away, and the even-odd
    /// rule says so without being told.
    #[test]
    fn a_ring_beside_another_is_ground_and_a_ring_inside_it_is_a_hole() {
        let rings = vec![square(0.0, 0.0, 10.0), square(100.0, 100.0, 10.0)];
        let apart = patch(rings.clone());
        let fill = cut(rings);
        assert!(apart.covers(5.0, 5.0), "the first piece is solid");
        assert!(apart.covers(105.0, 105.0), "and so is the second");
        assert!(!apart.covers(50.0, 50.0), "and the gap between them is not");
        assert!(in_triangles(&fill, [5.0, 5.0]));
        assert!(in_triangles(&fill, [105.0, 105.0]));
        assert!(!in_triangles(&fill, [50.0, 50.0]));
        assert!((covered(&fill) - 400.0).abs() < 1e-6, "two squares, no more");

        let rings = vec![square(0.0, 0.0, 20.0), square(8.0, 8.0, 4.0)];
        let nested = patch(rings.clone());
        let fill = cut(rings);
        assert!(!nested.covers(10.0, 10.0), "the hole is not covered");
        assert!(!in_triangles(&fill, [10.0, 10.0]), "and not filled");
        assert!((covered(&fill) - 768.0).abs() < 1e-6);
    }

    /// A ring that crosses itself has no inside anybody agrees on, and the
    /// service's simplification leaves 35 of them in a national fetch. What
    /// matters is that the fill and the click still say the same thing, and
    /// that neither runs away with the shape.
    #[test]
    fn a_ring_that_crosses_itself_still_fills_where_it_says_it_does() {
        // A bowtie: two lobes meeting at a crossing in the middle.
        let rings = vec![vec![[0.0, 0.0], [10.0, 10.0], [0.0, 10.0], [10.0, 0.0]]];
        let bowtie = patch(rings.clone());
        let fill = cut(rings);
        // The fill and the click are the same rule read twice, so they must
        // never disagree - a polygon drawn over ground a click says is clear is
        // the worst of both.
        let both = |x: f64, y: f64| (bowtie.covers(x, y), in_triangles(&fill, [x, y]));

        for (x, y) in [(5.0, 2.0), (5.0, 8.0)] {
            assert_eq!(both(x, y), (true, true), "the lobe at {x},{y} is filled and clickable");
        }
        for (x, y) in [(1.0, 5.0), (9.0, 5.0), (20.0, 5.0)] {
            assert_eq!(both(x, y), (false, false), "{x},{y} is outside by both readings");
        }
    }

    /// The fill is drawn opaque and washed afterwards - see `ui::radar` - so
    /// which hazard ends up on any shared ground is decided entirely by the
    /// order they come back in. Least serious first, so the most serious is
    /// painted last and survives.
    #[test]
    fn the_order_they_arrive_in_is_the_order_they_are_painted_in() {
        let named = |event: &str| Hazard {
            event: event.into(),
            ..Default::default()
        };
        let mut over_one_place = vec![
            named("Tornado Warning"),
            named("Special Weather Statement"),
            named("Heat Advisory"),
            named("Severe Thunderstorm Watch"),
        ];
        // The same sort `fetch` applies before handing them back.
        over_one_place.sort_by_key(|hazard| std::cmp::Reverse(rank(&hazard.event)));

        let order: Vec<&str> = over_one_place.iter().map(|h| h.event.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "Special Weather Statement",
                "Heat Advisory",
                "Severe Thunderstorm Watch",
                "Tornado Warning",
            ],
            "the tornado warning is painted over the heat advisory, not under it"
        );
    }

    /// GeoJSON closes its rings and everything here treats the last vertex as
    /// joining the first, so the repeat has to come off - it is a zero-length
    /// edge, and a band bounded by two copies of the same height is no band.
    #[test]
    fn the_closing_repeat_is_dropped() {
        let closed: Vec<serde_json::Value> =
            [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]
                .iter()
                .map(|p| serde_json::json!(p))
                .collect();
        let ring = ring_of(&closed);
        assert_eq!(ring.len(), 4, "five points, four corners");
        assert_eq!(ring[0], [0.0, 0.0]);
    }

    /// The field names are the service's own. Getting one wrong is quiet rather
    /// than loud: `event` exists on these features and holds a four-digit
    /// sequence number, so a hazard would be named "0334".
    #[test]
    fn a_feature_becomes_a_hazard_with_its_name_on_it() {
        let doc = serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "properties": {
                    "prod_type": "Tornado Warning",
                    "event": "0334",
                    "expiration": "2026-08-21T16:45:00-04:00",
                    "url": "https://api.weather.gov/alerts/urn:oid:2.49.0.1.840",
                    "wfo": "KTAE"
                },
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]]]
                }
            }]
        });

        let found = parse(&doc, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].event, "Tornado Warning", "not the sequence number");
        assert_eq!(found[0].office, "KTAE");
        assert!(found[0].short_fuse);
        assert!(found[0].covers(50.0, 50.0));
        assert!(!found[0].covers(150.0, 50.0));
        // And it is drawable, which is the other half of having parsed it.
        assert_eq!(found[0].patches.len(), 1);
        assert!(!found[0].patches[0].fill.is_empty(), "and it has ground to fill");
    }

    /// One hazard over an archipelago is one feature with many polygons. 145 of
    /// 1405 features in a national fetch are these.
    #[test]
    fn a_multipolygon_keeps_all_of_its_ground() {
        let doc = serde_json::json!({
            "features": [{
                "properties": { "prod_type": "Tropical Storm Warning" },
                "geometry": {
                    "type": "MultiPolygon",
                    "coordinates": [
                        [[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]],
                        [[[50.0, 50.0], [60.0, 50.0], [60.0, 60.0], [50.0, 60.0]]]
                    ]
                }
            }]
        });

        let found = parse(&doc, false);
        assert_eq!(found[0].patches.len(), 2, "both islands");
        assert!(found[0].covers(5.0, 5.0));
        assert!(found[0].covers(55.0, 55.0));
        assert!(!found[0].covers(30.0, 30.0), "and not the water between them");
    }

    /// A feature with no geometry, or a ring too short to enclose anything, is
    /// nothing to draw. It must not become a hazard with no shape, which would
    /// show up in the key naming something the map cannot point at.
    #[test]
    fn a_feature_with_nothing_to_draw_is_not_a_hazard() {
        let doc = serde_json::json!({
            "features": [
                { "properties": { "prod_type": "Flood Advisory" }, "geometry": null },
                { "properties": { "prod_type": "Flood Advisory" },
                  "geometry": { "type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 1.0]]] } },
                { "properties": { "wfo": "KTAE" },
                  "geometry": { "type": "Polygon",
                                "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]] } }
            ]
        });
        assert!(parse(&doc, false).is_empty());
    }

    /// The key is read top-down, so what it puts at the top has to be what is
    /// worth knowing. A tornado warning under an air stagnation advisory is a
    /// key nobody reads twice.
    #[test]
    fn the_key_leads_with_what_matters() {
        let named = |event: &str| Hazard {
            event: event.into(),
            ..Default::default()
        };
        let hazards = vec![
            named("Special Weather Statement"),
            named("Heat Advisory"),
            named("Tornado Watch"),
            named("Tornado Warning"),
            named("Flood Warning"),
            // The same hazard twice - one product issued over two zones - is
            // one line in the key.
            named("Heat Advisory"),
        ];
        assert_eq!(
            kinds(&hazards),
            vec![
                "Flood Warning",
                "Tornado Warning",
                "Tornado Watch",
                "Heat Advisory",
                "Special Weather Statement",
            ]
        );
    }

    /// The drawing order is the reading order backwards: the serious things are
    /// drawn last so they are on top, and listed first so they are read first.
    #[test]
    fn the_serious_things_are_drawn_last_and_read_first() {
        let ground = serde_json::json!({
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]]
        });
        let at = |event: &str| Hazard {
            event: event.into(),
            patches: patches_of(Some(&ground)),
            ..Default::default()
        };

        let mut over_one_place = vec![
            at("Tornado Warning"),
            at("Special Weather Statement"),
            at("Heat Advisory"),
        ];
        over_one_place.sort_by_key(|hazard| std::cmp::Reverse(rank(&hazard.event)));
        assert_eq!(
            over_one_place.last().map(|h| h.event.as_str()),
            Some("Tornado Warning"),
            "the tornado warning is painted over the heat advisory, not under it"
        );

        let found = at_point(&over_one_place, 5.0, 5.0);
        assert_eq!(found[0].event, "Tornado Warning", "and it is named first");
        assert_eq!(found.len(), 3, "all three cover the point");
        assert!(found[0].locally().severe, "a warning is treated as severe");
        assert!(!found[2].locally().severe);
    }

    /// One product issued over three neighbouring zones is three features
    /// saying the same thing, and three identical lines is not three facts.
    #[test]
    fn one_hazard_issued_three_times_reads_as_one() {
        let ground = serde_json::json!({
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]]
        });
        let zone = |url: &str| Hazard {
            event: "Tropical Storm Warning".into(),
            url: url.into(),
            patches: patches_of(Some(&ground)),
            ..Default::default()
        };

        let found = at_point(&[zone("https://a"), zone("https://b"), zone("https://c")], 5.0, 5.0);
        assert_eq!(found.len(), 1, "one warning, however many alerts carry it");
        assert_eq!(found[0].urls.len(), 3, "and all three are kept to ask about");
    }

    /// A click on empty ground costs nothing and says nothing. This used to be
    /// a round trip ending in "no watch or warning here", guarded by a guess
    /// about whether the tile under the click had arrived empty.
    #[test]
    fn a_click_on_nothing_finds_nothing() {
        let doc = serde_json::json!({
            "features": [{
                "properties": { "prod_type": "Heat Advisory" },
                "geometry": { "type": "Polygon",
                              "coordinates": [[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]] }
            }]
        });
        let hazards = parse(&doc, false);
        assert!(at_point(&hazards, 500.0, 500.0).is_empty());
        assert!(at_point(&[], 5.0, 5.0).is_empty());
    }

    /// The tolerance is the whole economy of this. Measured against the live
    /// service: the same 1405 features are 29MB at full fidelity and 832KB at
    /// five kilometres, and the tolerance handed in is the size of a pixel.
    #[test]
    fn the_query_asks_for_geometry_no_finer_than_the_screen() {
        let url = query_url((-9_800_000.0, 3_200_000.0, -9_000_000.0, 4_000_000.0), 1234.6, 1);
        assert!(url.contains("/1/query?"), "{url}");
        assert!(url.contains("f=geojson"), "{url}");
        assert!(url.contains("outSR=3857"), "metres out, not degrees: {url}");
        assert!(url.contains("maxAllowableOffset=1235"), "{url}");
        assert!(url.contains("returnGeometry=true"), "{url}");
        assert!(url.contains("prod_type"), "the name has to come with it: {url}");

        // A tolerance of zero would ask for full fidelity, which for a wide
        // view is the 29MB document.
        assert!(query_url((0.0, 0.0, 1.0, 1.0), 0.0, 0).contains("maxAllowableOffset=1"));
    }

    /// Every layer against the real service, as geometry. Ignored by default:
    ///   cargo test -- --ignored --nocapture live_warnings
    ///
    /// The one worth running by hand after touching the query. ArcGIS answers a
    /// request it does not like with a 200 and an error document, so a broken
    /// field name or a rejected projection presents as an empty map rather than
    /// as a failure.
    #[test]
    #[ignore]
    fn live_warnings() {
        // The whole of the lower 48, in Mercator metres, at a tolerance that
        // would suit a wide view on a large screen.
        let bbox = (
            super::super::radar::mercator_x(-125.0),
            super::super::radar::mercator_y(24.0),
            super::super::radar::mercator_x(-66.0),
            super::super::radar::mercator_y(50.0),
        );
        let found = fetch(bbox, 5000.0, 30);
        println!("  {} hazards over the lower 48", found.len());
        for hazard in kinds(&found) {
            let count = found.iter().filter(|h| h.event == hazard).count();
            println!(
                "    {:<34} x{count}{}",
                hazard,
                if super::super::hazards::colour_of(&hazard).is_none() {
                    "   (no colour in the legend)"
                } else {
                    ""
                }
            );
        }

        assert!(!found.is_empty(), "the service reported nothing at all");

        // The invariant that matters, checked on every triangle in the view:
        // what is filled is what a click would find, and it belongs to the
        // hazard the click would name. The two are the same even-odd reading of
        // the same rings, so nothing in the live data is allowed to make them
        // disagree - ground drawn in a colour a click says is not there is the
        // worst of both answers.
        //
        // Area was the first thing tried and it is the wrong question. A ring
        // that crosses itself does not enclose a well-defined area, and 38 of
        // some 5000 do; the shoelace counts a reversed lobe negatively and any
        // correct cut disagrees with it. Containment holds whatever the ring
        // does.
        let mut vertices = 0;
        let mut simple = 0;
        let mut crossing = 0;
        for hazard in &found {
            assert!(
                hazard.event.chars().any(|c| c.is_alphabetic()),
                "the hazard came back as {:?}, which is a code rather than a name",
                hazard.event
            );
            assert!(!hazard.patches.is_empty(), "{} has no ground", hazard.event);
            for patch in &hazard.patches {
                vertices += patch.rings.iter().map(Vec::len).sum::<usize>();
                assert!(
                    !patch.fill.is_empty(),
                    "{} has a polygon that cut into no triangles",
                    hazard.event
                );
                // Counted, not exempted. The bands can be held to account on a
                // ring that crosses itself, because a crossing is a band
                // boundary like a vertex is; the count is kept so that a change
                // which starts producing them shows up as a number moving.
                if patch.rings.iter().any(|ring| self_crossing(ring)) {
                    crossing += 1;
                } else {
                    simple += 1;
                }
            }
        }

        // The invariant that matters, checked on every triangle of every
        // polygon: what is filled is what a click would find. The two are the
        // same even-odd reading of the same rings, so nothing in the live data
        // is allowed to make them disagree - ground drawn as a hazard that a
        // click says is clear is the worst of both answers.
        //
        // Area was the first thing tried and it is the wrong question. A ring
        // that crosses itself does not enclose a well-defined area, and 38 of
        // some 5000 do; the shoelace counts a reversed lobe negatively and any
        // correct cut disagrees with it. Containment holds whatever the ring
        // does.
        let mut slivers = 0;
        let mut triangles = 0;
        for hazard in &found {
            for patch in &hazard.patches {
                for corner in patch.fill.chunks_exact(3) {
                    let twice = ((corner[1][0] - corner[0][0]) * (corner[2][1] - corner[0][1])
                        - (corner[1][1] - corner[0][1]) * (corner[2][0] - corner[0][0]))
                        .abs();
                    // A sliver of under a square metre is beneath the geometry's
                    // own precision, so which side of an edge its centroid falls
                    // on is arithmetic rather than fact.
                    if twice < 2.0 {
                        slivers += 1;
                        continue;
                    }
                    triangles += 1;
                    let middle = [
                        (corner[0][0] + corner[1][0] + corner[2][0]) / 3.0,
                        (corner[0][1] + corner[1][1] + corner[2][1]) / 3.0,
                    ];
                    assert!(
                        patch.covers(middle[0], middle[1]),
                        "{} was cut a triangle at {middle:?}, which is outside the \
                         shape it came from - bounds {:?}",
                        hazard.event,
                        patch.bounds
                    );
                }
            }
        }

        println!("  {vertices} vertices cut into {triangles} triangles ({slivers} slivers)");
        println!("  {simple} simple polygons, {crossing} that cross themselves");

        // Every colour the key will need. A hazard the legend does not know is
        // drawn as nothing at all, which is the one failure that looks like
        // clear weather.
        let unknown: Vec<String> = kinds(&found)
            .into_iter()
            .filter(|hazard| super::super::hazards::colour_of(hazard).is_none())
            .collect();
        assert!(unknown.is_empty(), "no colour for {unknown:?}");
    }

    /// What the fill actually costs at the views the map is drawn at.
    ///
    /// Not a correctness test - a size one. The geometry is cut once per fetch
    /// and drawn every frame, so the triangle count is the number that decides
    /// whether a 4K wall showing the whole country stays smooth.
    ///
    ///   cargo test -- --ignored --nocapture live_cost
    #[test]
    #[ignore]
    fn live_cost() {
        // (name, half-width in metres, screen pixels across)
        let views = [
            ("a wall cell, 60mi", 48_000.0, 620.0),
            ("the radar, 226mi", 182_000.0, 1170.0),
            ("a region, 600mi", 483_000.0, 1170.0),
            ("4K, the country", 2_300_000.0, 3840.0),
        ];
        for (name, half, across) in views {
            // Tallahassee-ish, where there is weather today.
            let (x, y) = (
                super::super::radar::mercator_x(-90.1),
                super::super::radar::mercator_y(31.45),
            );
            let tolerance = half * 2.0 / across;
            let at = std::time::Instant::now();
            let found = fetch((x - half, y - half, x + half, y + half), tolerance, 30);
            let took = at.elapsed();
            let vertices: usize = found
                .iter()
                .flat_map(|hazard| &hazard.patches)
                .flat_map(|patch| &patch.rings)
                .map(Vec::len)
                .sum();
            let triangles: usize = found
                .iter()
                .flat_map(|hazard| &hazard.patches)
                .map(|patch| patch.fill.len() / 3)
                .sum();
            println!(
                "  {name:<22} {:>4} hazards  {vertices:>6} vertices  \
                 {triangles:>7} triangles  {:>5}ms",
                found.len(),
                took.as_millis()
            );
            // What is drawn every frame. A view of the country on a 4K screen is
            // the most this ever asks for, and it has to stay a mesh a GPU
            // shrugs at - it was 2.2 million while the overlaps were being
            // resolved geometrically.
            assert!(
                triangles < 200_000,
                "{name} would draw {triangles} triangles every frame"
            );
        }
    }

    /// What a click on a warning polygon actually reports, end to end.
    ///
    /// Needs somewhere with active weather, so it takes a coordinate:
    ///   KESTREL_TEST_LATLON=30.44,-84.28 \
    ///     cargo test -- --ignored --nocapture live_click
    #[test]
    #[ignore]
    fn live_click() {
        let Ok(point) = std::env::var("KESTREL_TEST_LATLON") else {
            eprintln!("KESTREL_TEST_LATLON not set (lat,lon of somewhere with weather)");
            return;
        };
        let (lat, lon) = point.split_once(',').expect("lat,lon");
        let (lat, lon): (f64, f64) = (
            lat.trim().parse().expect("a latitude"),
            lon.trim().parse().expect("a longitude"),
        );
        let (x, y) = (
            super::super::radar::mercator_x(lon),
            super::super::radar::mercator_y(lat),
        );

        // A view about 200km across, which is what the map would have fetched.
        let half = 100_000.0;
        let found = fetch((x - half, y - half, x + half, y + half), 200.0, 30);
        println!("  {} hazards in the view", found.len());

        let here = at_point(&found, x, y);
        println!("  {} of them cover the point", here.len());
        assert!(!here.is_empty(), "nothing was reported at that point");

        for warning in wording(&here) {
            println!(
                "    {}{}\n      {}",
                warning.event,
                if warning.severe { "  (severe)" } else { "" },
                warning.said
            );
            assert!(!warning.said.is_empty(), "{} said nothing", warning.event);
        }
    }
}

