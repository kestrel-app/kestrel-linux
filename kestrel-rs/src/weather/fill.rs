//! Turning the outline of a shape into the triangles that fill it.
//!
//! Two layers need this and neither can get it from egui, which fills a path as
//! a triangle fan from its first vertex — that is why the shape it offers is
//! called `convex_polygon` and means it. A county-aggregated advisory is not
//! convex and neither is a rain band, so fanning either paints the concavities
//! in.
//!
//! The method is a sweep in horizontal bands. Every vertex height is a band
//! boundary, and between two consecutive boundaries no edge starts, ends or
//! meets another — so every edge crossing that band crosses all of it as a
//! straight line. Walking those crossings left to right and taking them in
//! pairs gives the spans of the band that are inside the shape, by the even-odd
//! rule; each span is a trapezoid, and a trapezoid is two triangles.
//!
//! Nothing is approximated. Within a band the boundary really is a set of
//! straight lines, and where a shape crosses itself that crossing is made a
//! band boundary too.
//!
//! Ear clipping was the first answer and it is the textbook one. This data is
//! not textbook: holes have to be spliced into the outer ring before ears can
//! be clipped, and the splice is where it comes apart — a Small Craft Advisory
//! with 53 rocks in it becomes a ring of 275 vertices threaded with 53
//! zero-width channels, and the clipper stalled on it 43 triangles short with
//! one of the rocks left filled in. Bands have nothing to splice: a hole is a
//! ring like any other and the even-odd pairing takes it out for free. So does
//! a ring that crosses itself, and so does a boundary that arrived as loose
//! segments rather than as a ring at all — which is what the reflectivity's
//! contours are, and why they never need chaining.

/// One piece of a shape's boundary. Ends, not a ring: nothing here cares
/// whether they join up, only that the set as a whole is closed.
pub type Edge = ([f64; 2], [f64; 2]);

/// How many edges a shape may have before its self-crossings stop being looked
/// for. Quadratic, so bounded; see [`triangulate`].
const MOST_EDGES: usize = 1500;

/// The ground a boundary encloses, as triangles.
///
/// `crossing_bands` asks for every place two edges cross to be made a band
/// boundary as well. That is what stops a bowtie being filled straight across
/// its own waist, and it costs a comparison of every edge against every other —
/// worth paying where shapes are few and may be malformed, and not worth paying
/// for contours, which are generated here and cannot cross.
///
/// `clip` bounds the output to a rectangle, given as left, bottom, right, top.
/// A trapezoid clipped against a vertical line is still a trapezoid, so this is
/// exact rather than approximate — which matters, because it is what lets one
/// tile's shapes be computed from a little of its neighbours' ground and still
/// stop precisely at its own edge.
pub fn triangulate(edges: &[Edge], crossing_bands: bool, clip: Option<[f64; 4]>) -> Vec<[f64; 2]> {
    let mut levels: Vec<f64> = Vec::with_capacity(edges.len() * 2);
    for (a, b) in edges {
        levels.push(a[1]);
        levels.push(b[1]);
    }
    if crossing_bands && edges.len() <= MOST_EDGES {
        for (at, first) in edges.iter().enumerate() {
            for second in &edges[at + 1..] {
                if let Some(height) = crossing_height(*first, *second) {
                    levels.push(height);
                }
            }
        }
    }
    if let Some([_, bottom, _, top]) = clip {
        levels.push(bottom);
        levels.push(top);
    }

    levels.sort_by(f64::total_cmp);
    levels.dedup();
    if levels.len() < 2 {
        return Vec::new();
    }

    let (left_wall, right_wall) = match clip {
        Some([left, _, right, _]) => (left, right),
        None => (f64::NEG_INFINITY, f64::INFINITY),
    };
    let (floor, ceiling) = match clip {
        Some([_, bottom, _, top]) => (bottom, top),
        None => (f64::NEG_INFINITY, f64::INFINITY),
    };

    let mut out = Vec::new();
    let mut crossings: Vec<(f64, f64, f64)> = Vec::new();

    for band in levels.windows(2) {
        let (low, high) = (band[0], band[1]);
        if high <= low || high <= floor || low >= ceiling {
            continue;
        }
        let middle = (low + high) / 2.0;

        // Where each edge sits at the bottom of the band, at the top, and in
        // the middle. The middle is what they are sorted by: at a boundary two
        // edges can meet at a point and compare equal, and the order that
        // matters is the one they are in *across* the band.
        crossings.clear();
        for (a, b) in edges {
            // Every edge either spans the whole band or misses it — there is no
            // vertex inside a band for one to stop at.
            if a[1] == b[1] || a[1].min(b[1]) > low || a[1].max(b[1]) < high {
                continue;
            }
            let across = (b[0] - a[0]) / (b[1] - a[1]);
            let at_height = |y: f64| a[0] + (y - a[1]) * across;
            crossings.push((at_height(middle), at_height(low), at_height(high)));
        }
        crossings.sort_by(|left, right| left.0.total_cmp(&right.0));

        // In pairs: the ground between the first crossing and the second is
        // inside the shape, between the second and the third is outside, and so
        // on. A hole takes itself out of the fill by adding two crossings.
        for pair in crossings.chunks_exact(2) {
            let (left, right) = (pair[0], pair[1]);
            let bottom_left = left.1.clamp(left_wall, right_wall);
            let bottom_right = right.1.clamp(left_wall, right_wall);
            let top_left = left.2.clamp(left_wall, right_wall);
            let top_right = right.2.clamp(left_wall, right_wall);
            // A trapezoid is two triangles, and one that comes to a point at
            // one end is one. Both happen constantly: every vertex of the
            // boundary is where a span opens or closes.
            if bottom_right - bottom_left > 0.0 {
                out.extend_from_slice(&[[bottom_left, low], [bottom_right, low], [top_right, high]]);
            }
            if top_right - top_left > 0.0 {
                out.extend_from_slice(&[[bottom_left, low], [top_right, high], [top_left, high]]);
            }
        }
    }
    out
}

/// The edges of a set of closed rings, ready for [`triangulate`].
pub fn edges_of(rings: &[Vec<[f64; 2]>]) -> Vec<Edge> {
    rings
        .iter()
        .flat_map(|ring| {
            (0..ring.len()).map(move |at| (ring[(at + ring.len() - 1) % ring.len()], ring[at]))
        })
        .filter(|(a, b)| a[1] != b[1])
        .collect()
}

/// Where two segments cross, if they properly do — each strictly straddling the
/// other. Touching at an endpoint is a vertex, and vertices are already band
/// boundaries.
pub fn crossing_height(first: Edge, second: Edge) -> Option<f64> {
    let (p, q) = first;
    let (a, b) = second;
    let (rx, ry) = (q[0] - p[0], q[1] - p[1]);
    let (sx, sy) = (b[0] - a[0], b[1] - a[1]);
    let denominator = rx * sy - ry * sx;
    if denominator == 0.0 {
        return None;
    }
    let (dx, dy) = (a[0] - p[0], a[1] - p[1]);
    let along = (dx * sy - dy * sx) / denominator;
    let across = (dx * ry - dy * rx) / denominator;
    let strictly = |t: f64| t > 0.0 && t < 1.0;
    (strictly(along) && strictly(across)).then(|| p[1] + along * ry)
}

/// Whether a point is inside a ring, by the even-odd rule.
pub fn in_ring(ring: &[[f64; 2]], point: [f64; 2]) -> bool {
    let (x, y) = (point[0], point[1]);
    let mut inside = false;
    let mut j = ring.len().saturating_sub(1);
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > y) != (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn square(left: f64, bottom: f64, side: f64) -> Vec<[f64; 2]> {
        vec![
            [left, bottom],
            [left + side, bottom],
            [left + side, bottom + side],
            [left, bottom + side],
        ]
    }

    #[test]
    fn a_square_is_all_of_its_own_area() {
        let out = triangulate(&edges_of(&[square(0.0, 0.0, 10.0)]), true, None);
        assert!((covered(&out) - 200.0).abs() < 1e-9, "twice 10x10");
    }

    /// The whole reason this exists rather than a triangle fan.
    #[test]
    fn a_notch_is_not_filled_in() {
        let ell = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 5.0],
            [5.0, 5.0],
            [5.0, 10.0],
            [0.0, 10.0],
        ];
        let out = triangulate(&edges_of(&[ell]), true, None);
        assert!((covered(&out) - 150.0).abs() < 1e-9, "twice 75");
        assert!(in_triangles(&out, [2.0, 2.0]), "the body is filled");
        assert!(!in_triangles(&out, [7.5, 7.5]), "the notch stays empty");
    }

    #[test]
    fn a_hole_is_left_out() {
        let out = triangulate(
            &edges_of(&[square(0.0, 0.0, 20.0), square(8.0, 8.0, 4.0)]),
            true,
            None,
        );
        assert!((covered(&out) - 768.0).abs() < 1e-6, "400 less the hole's 16");
        assert!(!in_triangles(&out, [10.0, 10.0]), "the hole is empty");
        assert!(in_triangles(&out, [2.0, 2.0]), "the ring around it is not");
    }

    /// Loose segments, never chained into rings. The reflectivity's contours
    /// arrive this way and the even-odd rule does not care.
    #[test]
    fn a_boundary_that_never_became_a_ring_fills_just_the_same() {
        let ring = square(0.0, 0.0, 10.0);
        let mut loose = edges_of(&[ring]);
        // Shuffled and turned around, so nothing about the order can matter.
        loose.reverse();
        for edge in loose.iter_mut().step_by(2) {
            *edge = (edge.1, edge.0);
        }
        let out = triangulate(&loose, false, None);
        assert!((covered(&out) - 200.0).abs() < 1e-9);
        assert!(in_triangles(&out, [5.0, 5.0]));
    }

    /// The clip is what lets a tile borrow a little of its neighbour's ground
    /// to get its edges right and still stop exactly at its own boundary. A
    /// trapezoid cut by a vertical line is still a trapezoid, so it is exact.
    #[test]
    fn the_clip_cuts_exactly_and_tiles_abut() {
        let wide = edges_of(&[square(0.0, 0.0, 20.0)]);
        let left = triangulate(&wide, false, Some([0.0, 0.0, 10.0, 20.0]));
        let right = triangulate(&wide, false, Some([10.0, 0.0, 20.0, 20.0]));

        assert!((covered(&left) - 400.0).abs() < 1e-9, "half of 400 is 200, twice it is 400");
        assert!((covered(&right) - 400.0).abs() < 1e-9);
        // Together they are the whole square and no more: no gap at the seam
        // and nothing drawn twice.
        assert!(((covered(&left) + covered(&right)) - 800.0).abs() < 1e-9);

        assert!(in_triangles(&left, [5.0, 10.0]));
        assert!(!in_triangles(&left, [15.0, 10.0]), "the clip really cuts");
        assert!(in_triangles(&right, [15.0, 10.0]));

        // A shape wholly outside the clip contributes nothing.
        let away = edges_of(&[square(50.0, 50.0, 5.0)]);
        assert!(triangulate(&away, false, Some([0.0, 0.0, 10.0, 10.0])).is_empty());
    }

    /// A shape that crosses itself has no inside anybody agrees on, but the
    /// waist of a bowtie must not be filled straight across.
    #[test]
    fn a_bowtie_is_not_filled_across_its_waist() {
        let bowtie = vec![vec![[0.0, 0.0], [10.0, 10.0], [0.0, 10.0], [10.0, 0.0]]];
        let out = triangulate(&edges_of(&bowtie), true, None);
        assert!(in_triangles(&out, [5.0, 2.0]), "the lower lobe is filled");
        assert!(in_triangles(&out, [5.0, 8.0]), "and the upper");
        assert!(!in_triangles(&out, [1.0, 5.0]), "the waist is not");
        assert!(!in_triangles(&out, [9.0, 5.0]));
    }

    #[test]
    fn nothing_at_all_encloses_nothing() {
        assert!(triangulate(&[], true, None).is_empty());
        assert!(triangulate(&edges_of(&[vec![[0.0, 0.0], [1.0, 1.0]]]), true, None).is_empty());
        // A line has no area whichever way it is asked about.
        let flat: Vec<[f64; 2]> = (0..8).map(|i| [i as f64, 0.0]).collect();
        assert!(covered(&triangulate(&edges_of(&[flat]), true, None)) < 1e-9);
    }
}
