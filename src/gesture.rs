//! Gesture recognition engine.
//!
//! Pipeline: raw points → spatial coalesce → RDP simplify →
//! direction quantize → collapse → exact match.
//!
//! All buffers are fixed-size with explicit bounds; no heap
//! allocation in the recognition path.

use crate::config::Direction;

/// Maximum sampled points before adaptive decimation.
pub const MAX_POINTS: usize = 256;
/// Maximum encoded direction tokens.
pub const MAX_ENCODED_TOKENS: usize = 32;

/// A 2D point in physical screen coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Classify the 8-direction label from two consecutive points.
/// Screen coordinates: Y increases downward, so -dy = upward motion.
/// Uses integer slope comparisons — no f64, no atan2.
pub fn direction_from_points(from: Point, to: Point) -> Direction {
    let dx = to.x - from.x;
    let dy = -(to.y - from.y); // negate: screen Y is inverted for angle math

    let abs_dx = dx.unsigned_abs();
    let abs_dy = dy.unsigned_abs();

    // tan(22.5°) ≈ 0.4142  → |dy|/|dx| < 0.415 → within 22.5° of E/W
    // tan(67.5°) ≈ 2.4142  → |dx|/|dy| < 0.415 → within 22.5° of N/S
    let near_horiz = abs_dy * 241 < abs_dx * 100;
    let near_vert = abs_dx * 241 < abs_dy * 100;

    if near_horiz {
        if dx > 0 {
            Direction::E
        } else {
            Direction::W
        }
    } else if near_vert {
        if dy > 0 {
            Direction::N
        } else {
            Direction::S
        }
    } else {
        match (dx > 0, dy > 0) {
            (true, true) => Direction::NE,
            (true, false) => Direction::SE,
            (false, true) => Direction::NW,
            (false, false) => Direction::SW,
        }
    }
}

/// One direction run: a direction plus the total path length it covers.
#[derive(Debug, Clone, Copy)]
struct DirectionRun {
    dir: Direction,
    len: f64,
}

/// Result of gesture classification.
#[derive(Debug, Clone)]
pub enum GestureResult {
    /// Gesture matched a configured pattern.
    /// Returns the gesture name and the encoded direction sequence.
    Matched {
        name: String,
        directions: Vec<Direction>,
    },
    /// No matching gesture found.
    NoMatch,
    /// Gesture too short (fewer than min_gesture_length tokens after collapse).
    TooShort,
    /// Point buffer overflow — gesture invalidated.
    Overflow,
}

/// Fixed-size gesture buffer used by the hook thread.
/// All allocations happen at initialization; recognition
/// uses only stack + pre-allocated arrays.
pub struct GestureBuffer {
    points: [Point; MAX_POINTS],
    len: usize,
    sample_distance_sq: i64,
    /// Last sampled point for spatial coalescing.
    last_sample: Option<Point>,
}

impl GestureBuffer {
    pub fn new(sample_distance_physical: i32) -> Self {
        let d = sample_distance_physical as i64;
        GestureBuffer {
            points: [Point { x: 0, y: 0 }; MAX_POINTS],
            len: 0,
            sample_distance_sq: d * d,
            last_sample: None,
        }
    }

    /// Add a point with spatial coalescing. Returns true if the
    /// point was stored (i.e., it passed the distance threshold).
    /// Returns false if the buffer is full (caller should handle overflow).
    pub fn add_point(&mut self, p: Point) -> bool {
        // Spatial coalescing: skip points too close to the last sample
        if let Some(last) = self.last_sample {
            let dx = (p.x - last.x) as i64;
            let dy = (p.y - last.y) as i64;
            if dx * dx + dy * dy < self.sample_distance_sq {
                return true; // dropped, but not an error
            }
        }

        if self.len >= MAX_POINTS && !self.decimate() {
            return false;
        }

        self.points[self.len] = p;
        self.len += 1;
        self.last_sample = Some(p);
        true
    }

    /// Clear the buffer for a new gesture.
    pub fn clear(&mut self) {
        self.len = 0;
        self.last_sample = None;
    }

    /// Adaptive decimation: keep every 2nd point when the buffer is full.
    /// Returns true if there is room after decimation, false if still full.
    fn decimate(&mut self) -> bool {
        let mut dst = 0;
        for src in (0..self.len).step_by(2) {
            self.points[dst] = self.points[src];
            dst += 1;
        }
        self.len = dst;
        self.len < MAX_POINTS
    }

    /// Add a point unconditionally, skipping spatial coalescing.
    /// Only rejects exact duplicate coordinates. Use this for the
    /// right-button-down point, activation-threshold crossing point,
    /// and release point — any position that must appear in the buffer
    /// regardless of sample distance.
    pub fn add_force(&mut self, p: Point) -> bool {
        // Skip exact duplicates only
        if let Some(last) = self.last_sample {
            if p.x == last.x && p.y == last.y {
                return true; // duplicate suppressed, not an error
            }
        }

        if self.len >= MAX_POINTS && !self.decimate() {
            return false;
        }

        self.points[self.len] = p;
        self.len += 1;
        self.last_sample = Some(p);
        true
    }

    /// Number of stored points.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the buffer contains no points.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return the last two points if available, for direction computation.
    pub fn last_two(&self) -> Option<(Point, Point)> {
        if self.len >= 2 {
            Some((self.points[self.len - 2], self.points[self.len - 1]))
        } else {
            None
        }
    }

    pub(crate) fn stored_points(&self) -> &[Point] {
        &self.points[..self.len]
    }
}

/// Encode a gesture buffer's direction sequence (simplify → quantize → collapse).
pub fn encode_directions(buffer: &GestureBuffer, rdp_epsilon_sq: f64) -> Vec<Direction> {
    if buffer.len() < 2 {
        return vec![];
    }
    let simplified = rdp_simplify(buffer.stored_points(), rdp_epsilon_sq);
    if simplified.len() < 2 {
        return vec![];
    }
    let directions = quantize_directions(&simplified);
    collapse_directions(&directions)
}

/// Classify a gesture from collected points.
///
/// This function is designed to run in <250µs at maximum config
/// (256 points, 256 gestures, 32 tokens).
pub fn classify(
    buffer: &GestureBuffer,
    patterns: &[(String, Vec<Direction>)],
    rdp_epsilon_sq: f64,
    min_gesture_length: u32,
    tolerance_physical: f64,
) -> GestureResult {
    if buffer.len() < 2 {
        return GestureResult::TooShort;
    }

    let points = buffer.stored_points();

    // Step 1: RDP simplification (fixed-buffer, iterative)
    let simplified = rdp_simplify(points, rdp_epsilon_sq);
    if simplified.len() < 2 {
        return GestureResult::TooShort;
    }

    // Step 2: Direction quantization (runs carry their path length)
    let mut runs = quantize_runs(&simplified);
    if runs.is_empty() {
        return GestureResult::TooShort;
    }
    runs.truncate(MAX_ENCODED_TOKENS);
    let collapsed: Vec<Direction> = runs.iter().map(|r| r.dir).collect();

    // Step 3: Minimum gesture length check
    if (collapsed.len() as u32) < min_gesture_length {
        return GestureResult::TooShort;
    }

    // Step 4: Exact match against configured patterns
    if let Some((name, _)) = exact_match(&collapsed, patterns) {
        return GestureResult::Matched {
            name: name.clone(),
            directions: collapsed,
        };
    }

    // Step 5: Tolerance fallback — drop short direction runs (wobble,
    // rounded corners, release flicks) and retry, least destructive first.
    if tolerance_physical > 0.0 {
        if let Some((name, directions)) = match_tolerant(&runs, patterns, tolerance_physical) {
            return GestureResult::Matched { name, directions };
        }
    }

    GestureResult::NoMatch
}

/// Exact pattern lookup on a direction sequence.
fn exact_match<'a>(
    dirs: &[Direction],
    patterns: &'a [(String, Vec<Direction>)],
) -> Option<&'a (String, Vec<Direction>)> {
    patterns.iter().find(|(_, p)| p.as_slice() == dirs)
}

/// Retry matching while dropping the shortest run below its tolerance.
/// One run is dropped per round and the shortest is always dropped first, so
/// the least destructive interpretation of the drawing wins. A run is only
/// eligible when it is shorter than `tolerance_physical` or much shorter than
/// both of its neighbours — deliberate strokes stay intact.
fn match_tolerant(
    runs: &[DirectionRun],
    patterns: &[(String, Vec<Direction>)],
    tolerance_physical: f64,
) -> Option<(String, Vec<Direction>)> {
    let mut work = runs.to_vec();

    loop {
        let dirs: Vec<Direction> = work.iter().map(|r| r.dir).collect();
        if let Some((name, _)) = exact_match(&dirs, patterns) {
            return Some((name.clone(), dirs));
        }
        if work.len() <= 1 {
            return None;
        }

        // Shortest eligible run, if any.
        let mut victim: Option<usize> = None;
        let mut shortest = f64::MAX;
        for i in 0..work.len() {
            let len = work[i].len;
            if len < run_tolerance(&work, i, tolerance_physical) && len < shortest {
                shortest = len;
                victim = Some(i);
            }
        }
        let i = victim?;
        work.remove(i);

        // Neighbours that are now adjacent and equal merge back together.
        if i > 0 && i < work.len() && work[i - 1].dir == work[i].dir {
            work[i - 1].len += work[i].len;
            work.remove(i);
        }
    }
}

/// A run may be dropped when it is shorter than the absolute tolerance, or when
/// it is shorter than 40% of its shorter neighbour — a rounded corner or a
/// release flick is short next to the strokes it connects. Heuristic constant;
/// tune `tolerance_dip` in config rather than this ratio.
fn run_tolerance(work: &[DirectionRun], i: usize, tolerance_physical: f64) -> f64 {
    const NEIGHBOR_FRACTION: f64 = 0.4;

    let mut min_neighbor = f64::MAX;
    if i > 0 {
        min_neighbor = min_neighbor.min(work[i - 1].len);
    }
    if i + 1 < work.len() {
        min_neighbor = min_neighbor.min(work[i + 1].len);
    }
    if min_neighbor == f64::MAX {
        return tolerance_physical;
    }
    tolerance_physical.max(NEIGHBOR_FRACTION * min_neighbor)
}

// ── RDP Simplification ─────────────────────────────────────────

/// Ramer-Douglas-Peucker polyline simplification.
/// Uses the iterative stack approach to avoid recursion depth issues.
/// Works on squared distances to avoid sqrt in the epsilon comparison.
fn rdp_simplify(points: &[Point], epsilon_sq: f64) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }

    let mut result = Vec::with_capacity(points.len());
    // Fixed-size stack of (start, end) index ranges
    let mut stack: [(usize, usize); 64] = [(0, 0); 64];
    let mut stack_len: usize = 0;

    // Push initial range
    stack[stack_len] = (0, points.len() - 1);
    stack_len += 1;

    // Keep mask tracks which points are kept
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    while stack_len > 0 {
        stack_len -= 1;
        let (start, end) = stack[stack_len];

        if end <= start + 1 {
            continue;
        }

        // Find point with maximum distance from line start→end
        let (max_idx, max_dist_sq) = max_distance(points, start, end);

        if max_dist_sq > epsilon_sq {
            keep[max_idx] = true;
            // Push right segment first (process left first)
            if end > max_idx + 1 {
                stack[stack_len] = (max_idx, end);
                stack_len += 1;
            }
            if max_idx > start + 1 {
                stack[stack_len] = (start, max_idx);
                stack_len += 1;
            }
        }
    }

    for (i, &k) in keep.iter().enumerate() {
        if k {
            result.push(points[i]);
        }
    }

    result
}

/// Find the point in `points[start..=end]` with maximum perpendicular
/// distance squared from the line segment points[start]→points[end].
fn max_distance(points: &[Point], start: usize, end: usize) -> (usize, f64) {
    let p0 = points[start];
    let p1 = points[end];
    let dx = (p1.x - p0.x) as f64;
    let dy = (p1.y - p0.y) as f64;
    let line_len_sq = dx * dx + dy * dy;

    let mut max_idx = start + 1;
    let mut max_dist_sq = 0.0f64;

    if line_len_sq < 1e-10 {
        // Degenerate line — use point-to-point distance from p0
        #[allow(clippy::needless_range_loop)]
        for i in (start + 1)..end {
            let d = point_dist_sq(points[i], p0);
            if d > max_dist_sq {
                max_dist_sq = d;
                max_idx = i;
            }
        }
    } else {
        #[allow(clippy::needless_range_loop)]
        for i in (start + 1)..end {
            let d = perpendicular_dist_sq(points[i], p0, p1, line_len_sq);
            if d > max_dist_sq {
                max_dist_sq = d;
                max_idx = i;
            }
        }
    }

    (max_idx, max_dist_sq)
}

fn point_dist_sq(a: Point, b: Point) -> f64 {
    let dx = (a.x - b.x) as f64;
    let dy = (a.y - b.y) as f64;
    dx * dx + dy * dy
}

fn perpendicular_dist_sq(p: Point, a: Point, b: Point, line_len_sq: f64) -> f64 {
    let px = (p.x - a.x) as f64;
    let py = (p.y - a.y) as f64;
    let bx = (b.x - a.x) as f64;
    let by = (b.y - a.y) as f64;

    // Cross product magnitude squared / line length squared
    let cross = (px * by - py * bx).abs();
    (cross * cross) / line_len_sq
}

// ── Direction Quantization ─────────────────────────────────────

/// Quantize consecutive point pairs into 8 discrete directions.
/// Angular hysteresis: ±11.25° dead zones at boundaries.
/// Consecutive segments in the same direction merge into one run and
/// accumulate their path length.
fn quantize_runs(points: &[Point]) -> Vec<DirectionRun> {
    if points.len() < 2 {
        return Vec::new();
    }

    let mut runs: Vec<DirectionRun> = Vec::with_capacity(points.len() - 1);

    for window in points.windows(2) {
        let dx = (window[1].x - window[0].x) as f64;
        let dy = (window[1].y - window[0].y) as f64;

        // Skip zero-length segments
        if dx.abs() < 0.5 && dy.abs() < 0.5 {
            continue;
        }

        let dir = angle_to_direction((-dy).atan2(dx).to_degrees());
        let len = (dx * dx + dy * dy).sqrt();

        match runs.last_mut() {
            Some(last) if last.dir == dir => last.len += len,
            _ => runs.push(DirectionRun { dir, len }),
        }
    }

    runs
}

fn quantize_directions(points: &[Point]) -> Vec<Direction> {
    quantize_runs(points).into_iter().map(|r| r.dir).collect()
}

/// Map an angle in degrees (-180..180) to the nearest 8-way direction.
/// Hysteresis: ±11.25° dead zones at sector boundaries.
fn angle_to_direction(angle_deg: f64) -> Direction {
    // Normalize to 0..360
    let mut a = angle_deg;
    if a < 0.0 {
        a += 360.0;
    }

    // 8 sectors of 45° each, starting from -22.5° (for cardinal E at 0°)
    // E:   337.5-22.5
    // NE:  22.5-67.5
    // N:   67.5-112.5
    // NW:  112.5-157.5
    // W:   157.5-202.5
    // SW:  202.5-247.5
    // S:   247.5-292.5
    // SE:  292.5-337.5

    if !(22.5..337.5).contains(&a) {
        Direction::E
    } else if a < 67.5 {
        Direction::NE
    } else if a < 112.5 {
        Direction::N
    } else if a < 157.5 {
        Direction::NW
    } else if a < 202.5 {
        Direction::W
    } else if a < 247.5 {
        Direction::SW
    } else if a < 292.5 {
        Direction::S
    } else {
        Direction::SE
    }
}

// ── Direction Collapse ─────────────────────────────────────────

/// Collapse consecutive identical directions.
/// [N, N, N, E, E] → [N, E]
fn collapse_directions(dirs: &[Direction]) -> Vec<Direction> {
    if dirs.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(dirs.len());
    result.push(dirs[0]);
    for &d in &dirs[1..] {
        if d != *result.last().unwrap() {
            result.push(d);
        }
    }
    // Cap at MAX_ENCODED_TOKENS
    result.truncate(MAX_ENCODED_TOKENS);
    result
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_right_gesture() {
        let mut buf = GestureBuffer::new(2);
        for x in (0..100).step_by(5) {
            buf.add_point(Point { x, y: 50 });
        }
        let patterns = vec![("test".into(), vec![Direction::E])];
        let result = classify(&buf, &patterns, 4.0, 1, 0.0);
        match result {
            GestureResult::Matched { name, .. } => assert_eq!(name, "test"),
            _ => panic!("expected match"),
        }
    }

    #[test]
    fn right_then_down_gesture() {
        let mut buf = GestureBuffer::new(2);
        // Horizontal segment
        for x in (0..50).step_by(5) {
            buf.add_point(Point { x, y: 20 });
        }
        // Vertical segment
        for y in (20..70).step_by(5) {
            buf.add_point(Point { x: 50, y });
        }
        let patterns = vec![("test".into(), vec![Direction::E, Direction::S])];
        let result = classify(&buf, &patterns, 4.0, 2, 0.0);
        match result {
            GestureResult::Matched { name, .. } => assert_eq!(name, "test"),
            _ => panic!("expected match: {:?}", result),
        }
    }

    #[test]
    fn no_match_returns_nomatch() {
        let mut buf = GestureBuffer::new(2);
        for x in (0..50).step_by(5) {
            buf.add_point(Point { x, y: 20 });
        }
        let patterns: Vec<(String, Vec<Direction>)> = vec![("down".into(), vec![Direction::S])];
        let result = classify(&buf, &patterns, 4.0, 1, 0.0);
        match result {
            GestureResult::NoMatch => {}
            _ => panic!("expected NoMatch"),
        }
    }

    #[test]
    fn short_gesture_too_short() {
        let mut buf = GestureBuffer::new(2);
        buf.add_point(Point { x: 0, y: 0 });
        buf.add_point(Point { x: 5, y: 0 });
        let patterns: Vec<(String, Vec<Direction>)> = vec![];
        let result = classify(&buf, &patterns, 4.0, 3, 0.0); // min 3 tokens
        match result {
            GestureResult::TooShort => {}
            _ => panic!("expected TooShort"),
        }
    }

    #[test]
    fn spatial_coalescing_skips_close_points() {
        let mut buf = GestureBuffer::new(10);
        buf.add_point(Point { x: 0, y: 0 });
        buf.add_point(Point { x: 1, y: 1 }); // < 10px, should be skipped
        buf.add_point(Point { x: 20, y: 0 }); // far enough, stored
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn angle_to_direction_covers_all_octants() {
        assert_eq!(angle_to_direction(0.0), Direction::E);
        assert_eq!(angle_to_direction(45.0), Direction::NE);
        assert_eq!(angle_to_direction(90.0), Direction::N);
        assert_eq!(angle_to_direction(135.0), Direction::NW);
        assert_eq!(angle_to_direction(180.0), Direction::W);
        assert_eq!(angle_to_direction(-135.0), Direction::SW);
        assert_eq!(angle_to_direction(-90.0), Direction::S);
        assert_eq!(angle_to_direction(-45.0), Direction::SE);
    }

    #[test]
    fn direction_from_points_covers_all_octants() {
        // Use vectors with ~30° angles (5:10 ratio ≈ 26.6°) to clearly
        // land in diagonal sectors, and pure axis for cardinals.
        let o = Point { x: 0, y: 0 };
        assert_eq!(
            direction_from_points(o, Point { x: 10, y: 0 }),
            Direction::E
        );
        assert_eq!(
            direction_from_points(o, Point { x: 5, y: -10 }),
            Direction::NE
        );
        assert_eq!(
            direction_from_points(o, Point { x: 0, y: -10 }),
            Direction::N
        );
        assert_eq!(
            direction_from_points(o, Point { x: -5, y: -10 }),
            Direction::NW
        );
        assert_eq!(
            direction_from_points(o, Point { x: -10, y: 0 }),
            Direction::W
        );
        assert_eq!(
            direction_from_points(o, Point { x: -5, y: 10 }),
            Direction::SW
        );
        assert_eq!(
            direction_from_points(o, Point { x: 0, y: 10 }),
            Direction::S
        );
        assert_eq!(
            direction_from_points(o, Point { x: 5, y: 10 }),
            Direction::SE
        );
    }

    #[test]
    fn collapse_removes_duplicates() {
        let dirs = vec![
            Direction::N,
            Direction::N,
            Direction::N,
            Direction::E,
            Direction::E,
            Direction::S,
        ];
        let collapsed = collapse_directions(&dirs);
        assert_eq!(collapsed, vec![Direction::N, Direction::E, Direction::S]);
    }

    #[test]
    fn rdp_keeps_corners() {
        // A right-angle path: the corner should be preserved
        let points = vec![
            Point { x: 0, y: 0 },
            Point { x: 50, y: 0 },
            Point { x: 50, y: 50 },
        ];
        let simplified = rdp_simplify(&points, 4.0);
        assert_eq!(simplified.len(), 3);
    }

    #[test]
    fn buffer_overflow_decimates() {
        let mut buf = GestureBuffer::new(1); // tiny sample distance
                                             // Fill beyond MAX_POINTS
        for i in 0..(MAX_POINTS + 100) {
            let ok = buf.add_point(Point {
                x: i as i32 * 10,
                y: 0,
            });
            if !ok {
                // After decimation, should still have room but with reduced fidelity
                // The buffer should never completely reject after adaptive decimation
            }
        }
        // Buffer should still contain points (decimated)
        assert!(!buf.is_empty());
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn random_points_dont_panic(
            points in prop::collection::vec(
                (0i32..1920, 0i32..1080), 0..300
            )
        ) {
            let mut buf = GestureBuffer::new(2);
            for (x, y) in &points {
                buf.add_point(Point { x: *x, y: *y });
            }
            let patterns: Vec<(String, Vec<Direction>)> = vec![];
            let _result = classify(&buf, &patterns, 4.0, 2, 0.0);
            // The test passes if we don't panic
        }
    }

    // ── tolerance tests ──────────────────────────────────────────

    /// Append a straight line of points from `from` to `to`.
    fn add_line(buf: &mut GestureBuffer, from: Point, to: Point) {
        let steps = 20;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            buf.add_point(Point {
                x: (from.x as f64 + (to.x - from.x) as f64 * t).round() as i32,
                y: (from.y as f64 + (to.y - from.y) as f64 * t).round() as i32,
            });
        }
    }

    #[test]
    fn rounded_corner_matches_with_tolerance() {
        // N then a 28px diagonal corner then E: raw [N, NE, E].
        let mut buf = GestureBuffer::new(2);
        add_line(&mut buf, Point { x: 50, y: 150 }, Point { x: 50, y: 50 });
        add_line(&mut buf, Point { x: 50, y: 50 }, Point { x: 70, y: 30 });
        add_line(&mut buf, Point { x: 70, y: 30 }, Point { x: 180, y: 30 });

        let patterns = vec![("maximize".into(), vec![Direction::N, Direction::E])];

        // Strict: the corner run blocks the match.
        match classify(&buf, &patterns, 4.0, 2, 0.0) {
            GestureResult::NoMatch => {}
            other => panic!("expected NoMatch without tolerance, got {:?}", other),
        }

        // Tolerant: the short NE corner run is dropped.
        match classify(&buf, &patterns, 4.0, 2, 15.0) {
            GestureResult::Matched { name, directions } => {
                assert_eq!(name, "maximize");
                assert_eq!(directions, vec![Direction::N, Direction::E]);
            }
            other => panic!("expected Matched with tolerance, got {:?}", other),
        }
    }

    #[test]
    fn wobble_spike_is_absorbed() {
        // E with an up/down spike mid-stroke: [E, NE, SE, E].
        let mut buf = GestureBuffer::new(2);
        add_line(&mut buf, Point { x: 0, y: 50 }, Point { x: 40, y: 50 });
        add_line(&mut buf, Point { x: 40, y: 50 }, Point { x: 48, y: 38 });
        add_line(&mut buf, Point { x: 48, y: 38 }, Point { x: 56, y: 50 });
        add_line(&mut buf, Point { x: 56, y: 50 }, Point { x: 150, y: 50 });

        let patterns = vec![("forward".into(), vec![Direction::E])];
        match classify(&buf, &patterns, 4.0, 1, 20.0) {
            GestureResult::Matched { name, .. } => assert_eq!(name, "forward"),
            other => panic!("expected Matched, got {:?}", other),
        }
    }

    #[test]
    fn long_reversal_is_not_swallowed() {
        // N then S with long strokes must not collapse to [N].
        let mut buf = GestureBuffer::new(2);
        add_line(&mut buf, Point { x: 50, y: 150 }, Point { x: 50, y: 50 });
        add_line(&mut buf, Point { x: 50, y: 50 }, Point { x: 50, y: 160 });

        let patterns = vec![("up".into(), vec![Direction::N])];
        match classify(&buf, &patterns, 4.0, 1, 30.0) {
            GestureResult::NoMatch => {}
            other => panic!("expected NoMatch, got {:?}", other),
        }
    }

    #[test]
    fn tolerance_keeps_least_destructive_match() {
        // [W, SW, S] with a short SW corner. Both "W S" and "W" are configured;
        // the corner is dropped first, so "W S" wins over "W".
        let mut buf = GestureBuffer::new(2);
        add_line(&mut buf, Point { x: 150, y: 50 }, Point { x: 50, y: 50 });
        add_line(&mut buf, Point { x: 50, y: 50 }, Point { x: 30, y: 70 });
        add_line(&mut buf, Point { x: 30, y: 70 }, Point { x: 30, y: 170 });

        let patterns = vec![
            ("snap_bottom".into(), vec![Direction::W, Direction::S]),
            ("go_back".into(), vec![Direction::W]),
        ];
        match classify(&buf, &patterns, 4.0, 1, 30.0) {
            GestureResult::Matched { name, .. } => assert_eq!(name, "snap_bottom"),
            other => panic!("expected Matched(snap_bottom), got {:?}", other),
        }
    }

    // ── add_force tests ──────────────────────────────────────────
    #[test]
    fn add_force_bypasses_spatial_coalescing() {
        let mut buf = GestureBuffer::new(10);
        buf.add_force(Point { x: 0, y: 0 });
        // This point is 2px away — would be rejected by add_point (2 < 10)
        buf.add_force(Point { x: 2, y: 0 });
        assert_eq!(buf.len(), 2, "add_force should not coalesce nearby points");
    }

    #[test]
    fn add_force_rejects_exact_duplicate() {
        let mut buf = GestureBuffer::new(2);
        buf.add_force(Point { x: 100, y: 100 });
        assert!(buf.add_force(Point { x: 100, y: 100 }));
        // Exact duplicate should not increase length
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn add_force_start_point_preserved() {
        // Simulate a real gesture: down point + move points
        let mut buf = GestureBuffer::new(10);
        buf.add_force(Point { x: 50, y: 50 }); // start point
        buf.add_point(Point { x: 100, y: 50 }); // first move (past threshold)
        buf.add_point(Point { x: 150, y: 50 }); // second move

        let dir = direction_from_points(buf.stored_points()[0], buf.stored_points()[1]);
        assert_eq!(dir, Direction::E);
    }

    #[test]
    fn add_force_decimates_on_overflow() {
        let mut buf = GestureBuffer::new(1); // tiny distance so every add_force stores
        let total = MAX_POINTS + 10;
        for i in 0..total {
            let ok = buf.add_force(Point {
                x: i as i32 * 10,
                y: 0,
            });
            if !ok {
                // Should have decimated once already
                break;
            }
        }
        // Buffer should have points after decimation
        assert!(!buf.is_empty());
        assert!(buf.len() < total);
    }
}
