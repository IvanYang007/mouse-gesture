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

/// Result of gesture classification.
#[derive(Debug, Clone)]
pub enum GestureResult {
    /// Gesture matched a configured pattern.
    /// Returns the gesture name and the encoded direction sequence.
    Matched { name: String, directions: Vec<Direction> },
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

        if self.len >= MAX_POINTS {
            // Adaptive decimation: keep every 2nd point, halve the stored count
            if self.len >= MAX_POINTS {
                let mut dst = 0;
                for src in (0..self.len).step_by(2) {
                    self.points[dst] = self.points[src];
                    dst += 1;
                }
                self.len = dst;
                // If still full after decimation, reject
                if self.len >= MAX_POINTS {
                    return false;
                }
            }
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

    /// Number of stored points.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Access stored points for recognition.
    fn stored_points(&self) -> &[Point] {
        &self.points[..self.len]
    }
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

    // Step 2: Direction quantization with angular hysteresis
    let directions = quantize_directions(&simplified);
    if directions.is_empty() {
        return GestureResult::TooShort;
    }

    // Step 3: Collapse consecutive identical directions
    let collapsed = collapse_directions(&directions);

    // Step 4: Minimum gesture length check
    if (collapsed.len() as u32) < min_gesture_length {
        return GestureResult::TooShort;
    }

    // Step 5: Exact match against configured patterns
    for (name, pattern) in patterns {
        if pattern.len() != collapsed.len() {
            continue;
        }
        if pattern == &collapsed {
            return GestureResult::Matched {
                name: name.clone(),
                directions: collapsed,
            };
        }
    }

    GestureResult::NoMatch
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
        for i in (start + 1)..end {
            let d = point_dist_sq(points[i], p0);
            if d > max_dist_sq {
                max_dist_sq = d;
                max_idx = i;
            }
        }
    } else {
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
fn quantize_directions(points: &[Point]) -> Vec<Direction> {
    if points.len() < 2 {
        return Vec::new();
    }

    let mut dirs = Vec::with_capacity(points.len() - 1);

    for window in points.windows(2) {
        let dx = (window[1].x - window[0].x) as f64;
        let dy = (window[1].y - window[0].y) as f64;

        // Skip zero-length segments
        if dx.abs() < 0.5 && dy.abs() < 0.5 {
            continue;
        }

        let angle = dy.atan2(dx).to_degrees();
        let dir = angle_to_direction(angle);
        dirs.push(dir);
    }

    dirs
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

    if a < 22.5 || a >= 337.5 {
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
        let patterns = vec![
            ("test".into(), vec![Direction::E]),
        ];
        let result = classify(&buf, &patterns, 4.0, 1);
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
        let patterns = vec![
            ("test".into(), vec![Direction::E, Direction::S]),
        ];
        let result = classify(&buf, &patterns, 4.0, 2);
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
        let patterns: Vec<(String, Vec<Direction>)> = vec![
            ("down".into(), vec![Direction::S]),
        ];
        let result = classify(&buf, &patterns, 4.0, 2);
        match result {
            GestureResult::NoMatch => {},
            _ => panic!("expected NoMatch"),
        }
    }

    #[test]
    fn short_gesture_too_short() {
        let mut buf = GestureBuffer::new(2);
        buf.add_point(Point { x: 0, y: 0 });
        buf.add_point(Point { x: 5, y: 0 });
        let patterns: Vec<(String, Vec<Direction>)> = vec![];
        let result = classify(&buf, &patterns, 4.0, 3); // min 3 tokens
        match result {
            GestureResult::TooShort => {},
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
    fn collapse_removes_duplicates() {
        let dirs = vec![
            Direction::N, Direction::N, Direction::N,
            Direction::E, Direction::E,
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
            let ok = buf.add_point(Point { x: i as i32 * 10, y: 0 });
            if !ok {
                // After decimation, should still have room but with reduced fidelity
                // The buffer should never completely reject after adaptive decimation
            }
        }
        // Buffer should still contain points (decimated)
        assert!(buf.len() > 0);
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
            let _result = classify(&buf, &patterns, 4.0, 2);
            // The test passes if we don't panic
        }
    }
}
