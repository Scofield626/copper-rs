//! Occupancy grid backed by a PGM (P5) bitmap, plus Bresenham line-of-sight
//! collision checks against a world-frame line segment.
//!
//! Convention: cell values `>= occupied_threshold` are free (Nav2 "trinary"
//! default, threshold `128`). PGM row 0 is the top row; world y grows upward,
//! so world→cell flips the y axis. Cells outside the grid are treated as
//! occupied (a segment that leaves the grid is not free).

use std::fs;
use std::path::Path;

use cu29::prelude::*;

use crate::Point2D;

/// Nav2's "trinary" default: values `>=` are free, `<` are occupied.
pub const DEFAULT_OCCUPIED_THRESHOLD: u8 = 128;

/// PGM-backed occupancy grid in world-frame coordinates.
#[derive(Debug)]
pub struct OccupancyGrid {
    width: u32,
    height: u32,
    cells: Vec<u8>,
    resolution_m: f32,
    origin_x: f32,
    origin_y: f32,
    occupied_threshold: u8,
}

impl OccupancyGrid {
    /// Build a grid from raw cell bytes in row-major order (row 0 = top).
    ///
    /// `cells.len()` must equal `width * height`; `resolution_m` must be
    /// strictly positive.
    pub fn from_cells(
        width: u32,
        height: u32,
        cells: Vec<u8>,
        resolution_m: f32,
        origin_x: f32,
        origin_y: f32,
        occupied_threshold: u8,
    ) -> CuResult<Self> {
        let expected = width as usize * height as usize;
        if cells.len() != expected {
            return Err(CuError::from(format!(
                "OccupancyGrid: cell buffer length {} does not match {}x{} = {expected}",
                cells.len(),
                width,
                height,
            )));
        }
        if !(resolution_m.is_finite() && resolution_m > 0.0) {
            return Err(CuError::from(
                "OccupancyGrid: resolution_m must be a positive finite float",
            ));
        }
        Ok(Self {
            width,
            height,
            cells,
            resolution_m,
            origin_x,
            origin_y,
            occupied_threshold,
        })
    }

    /// Load a P5 (binary) PGM from disk.
    pub fn from_pgm_path(
        path: impl AsRef<Path>,
        resolution_m: f32,
        origin_x: f32,
        origin_y: f32,
        occupied_threshold: u8,
    ) -> CuResult<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .map_err(|e| CuError::new_with_cause("OccupancyGrid: PGM read failed", e))?;
        Self::from_pgm_bytes(&bytes, resolution_m, origin_x, origin_y, occupied_threshold)
    }

    /// Parse a P5 (binary) PGM from a byte buffer.
    pub fn from_pgm_bytes(
        bytes: &[u8],
        resolution_m: f32,
        origin_x: f32,
        origin_y: f32,
        occupied_threshold: u8,
    ) -> CuResult<Self> {
        let mut cursor = 0usize;
        let magic = pgm_read_token(bytes, &mut cursor)?;
        if magic != b"P5" {
            return Err(CuError::from(format!(
                "OccupancyGrid: expected PGM magic 'P5', got '{}'",
                String::from_utf8_lossy(magic),
            )));
        }
        let width: u32 = pgm_parse_u32(bytes, &mut cursor, "width")?;
        let height: u32 = pgm_parse_u32(bytes, &mut cursor, "height")?;
        let maxval: u32 = pgm_parse_u32(bytes, &mut cursor, "maxval")?;
        if maxval == 0 || maxval > 255 {
            return Err(CuError::from(format!(
                "OccupancyGrid: only 8-bit PGM supported (maxval={maxval})",
            )));
        }
        // Exactly one whitespace byte separates the header from the raster.
        if cursor >= bytes.len() || !bytes[cursor].is_ascii_whitespace() {
            return Err(CuError::from(
                "OccupancyGrid: missing whitespace after PGM header",
            ));
        }
        cursor += 1;
        let expected = width as usize * height as usize;
        if bytes.len() - cursor < expected {
            return Err(CuError::from(format!(
                "OccupancyGrid: PGM raster short: need {expected} bytes, have {}",
                bytes.len() - cursor,
            )));
        }
        let cells = bytes[cursor..cursor + expected].to_vec();
        Self::from_cells(
            width,
            height,
            cells,
            resolution_m,
            origin_x,
            origin_y,
            occupied_threshold,
        )
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn resolution_m(&self) -> f32 {
        self.resolution_m
    }

    pub fn origin(&self) -> (f32, f32) {
        (self.origin_x, self.origin_y)
    }

    /// True when the point falls inside a cell whose value is `>= threshold`.
    /// Off-grid points count as occupied.
    pub fn is_point_free(&self, p: Point2D) -> bool {
        let (col, row) = self.world_to_cell(p);
        self.cell_free(col, row)
    }

    /// True when every cell touched by the Bresenham line from `a` to `b`
    /// (inclusive of both endpoints) is free.
    pub fn is_segment_free(&self, a: Point2D, b: Point2D) -> bool {
        let (x0, y0) = self.world_to_cell(a);
        let (x1, y1) = self.world_to_cell(b);
        // Standard integer Bresenham; visits every cell on the discretised
        // line including both endpoints.
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut x = x0;
        let mut y = y0;
        loop {
            if !self.cell_free(x, y) {
                return false;
            }
            if x == x1 && y == y1 {
                return true;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    fn world_to_cell(&self, p: Point2D) -> (i32, i32) {
        let col = ((p.x - self.origin_x) / self.resolution_m).floor() as i32;
        let row_from_bottom = ((p.y - self.origin_y) / self.resolution_m).floor() as i32;
        let row = self.height as i32 - 1 - row_from_bottom;
        (col, row)
    }

    fn cell_free(&self, col: i32, row: i32) -> bool {
        if col < 0 || row < 0 || col >= self.width as i32 || row >= self.height as i32 {
            return false;
        }
        let idx = row as usize * self.width as usize + col as usize;
        self.cells[idx] >= self.occupied_threshold
    }
}

fn pgm_read_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> CuResult<&'a [u8]> {
    loop {
        if *cursor >= bytes.len() {
            return Err(CuError::from("OccupancyGrid: unexpected EOF in PGM header"));
        }
        let b = bytes[*cursor];
        if b.is_ascii_whitespace() {
            *cursor += 1;
            continue;
        }
        if b == b'#' {
            while *cursor < bytes.len() && bytes[*cursor] != b'\n' {
                *cursor += 1;
            }
            continue;
        }
        break;
    }
    let start = *cursor;
    while *cursor < bytes.len() {
        let b = bytes[*cursor];
        if b.is_ascii_whitespace() || b == b'#' {
            break;
        }
        *cursor += 1;
    }
    Ok(&bytes[start..*cursor])
}

fn pgm_parse_u32(bytes: &[u8], cursor: &mut usize, field: &str) -> CuResult<u32> {
    let tok = pgm_read_token(bytes, cursor)?;
    let s = core::str::from_utf8(tok).map_err(|_| {
        CuError::from(format!(
            "OccupancyGrid: PGM {field} not valid ASCII: {tok:?}"
        ))
    })?;
    s.parse::<u32>()
        .map_err(|_| CuError::from(format!("OccupancyGrid: PGM {field} not a u32: '{s}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 8x8 grid, resolution 1.0 m, origin at world (0, 0). Column 4 is a full
    // vertical wall (all 0s); every other cell is free (255). Row 0 in `cells`
    // is the *top* of the image, i.e. world y ∈ [7, 8).
    fn wall_grid() -> OccupancyGrid {
        let mut cells = vec![255u8; 8 * 8];
        for row in 0..8 {
            cells[row * 8 + 4] = 0;
        }
        OccupancyGrid::from_cells(8, 8, cells, 1.0, 0.0, 0.0, DEFAULT_OCCUPIED_THRESHOLD).unwrap()
    }

    #[test]
    fn point_free_and_wall_cells() {
        let g = wall_grid();
        assert!(g.is_point_free(Point2D { x: 0.5, y: 0.5 }));
        assert!(!g.is_point_free(Point2D { x: 4.5, y: 3.5 }));
    }

    #[test]
    fn off_grid_points_are_occupied() {
        let g = wall_grid();
        assert!(!g.is_point_free(Point2D { x: -0.5, y: 0.5 }));
        assert!(!g.is_point_free(Point2D { x: 8.5, y: 0.5 }));
        assert!(!g.is_point_free(Point2D { x: 0.5, y: -0.5 }));
        assert!(!g.is_point_free(Point2D { x: 0.5, y: 8.5 }));
    }

    #[test]
    fn horizontal_segment_hits_wall() {
        let g = wall_grid();
        let a = Point2D { x: 0.5, y: 0.5 };
        let b = Point2D { x: 7.5, y: 0.5 };
        assert!(!g.is_segment_free(a, b));
    }

    #[test]
    fn parallel_wall_segment_stays_free() {
        let g = wall_grid();
        // Column 3, all 8 rows — never crosses column 4.
        let a = Point2D { x: 3.5, y: 0.5 };
        let b = Point2D { x: 3.5, y: 7.5 };
        assert!(g.is_segment_free(a, b));
    }

    #[test]
    fn diagonal_segment_hits_wall() {
        let g = wall_grid();
        let a = Point2D { x: 0.5, y: 0.5 };
        let b = Point2D { x: 7.5, y: 7.5 };
        assert!(!g.is_segment_free(a, b));
    }

    #[test]
    fn segment_leaving_grid_is_occupied() {
        let g = wall_grid();
        let a = Point2D { x: 1.5, y: 1.5 };
        let b = Point2D { x: 10.0, y: 1.5 };
        assert!(!g.is_segment_free(a, b));
    }

    #[test]
    fn pgm_round_trip_matches_from_cells() {
        // Small 3x2 grid with a mix of values.
        let raw_cells: Vec<u8> = vec![255, 0, 200, 100, 50, 128];
        let mut pgm: Vec<u8> = Vec::new();
        pgm.extend_from_slice(b"P5\n# hand-rolled test image\n3 2\n255\n");
        pgm.extend_from_slice(&raw_cells);
        let g = OccupancyGrid::from_pgm_bytes(&pgm, 1.0, 0.0, 0.0, 128).unwrap();
        assert_eq!(g.width(), 3);
        assert_eq!(g.height(), 2);
        // Cell (col=0, row=0) is `255` → free at world y ∈ [1, 2), x ∈ [0, 1).
        assert!(g.is_point_free(Point2D { x: 0.5, y: 1.5 }));
        // Cell (col=1, row=0) is `0` → occupied.
        assert!(!g.is_point_free(Point2D { x: 1.5, y: 1.5 }));
        // Cell (col=2, row=1) is `128` → free (`>=` threshold).
        assert!(g.is_point_free(Point2D { x: 2.5, y: 0.5 }));
        // Cell (col=1, row=1) is `100` → occupied.
        assert!(!g.is_point_free(Point2D { x: 1.5, y: 0.5 }));
    }

    #[test]
    fn pgm_rejects_wrong_magic() {
        let err =
            OccupancyGrid::from_pgm_bytes(b"P4\n1 1\n255\n\0", 1.0, 0.0, 0.0, 128).unwrap_err();
        assert!(format!("{err:?}").contains("P5"));
    }

    #[test]
    fn pgm_rejects_short_raster() {
        let err = OccupancyGrid::from_pgm_bytes(b"P5\n2 2\n255\n\x00\x01", 1.0, 0.0, 0.0, 128)
            .unwrap_err();
        assert!(format!("{err:?}").contains("short"));
    }

    #[test]
    fn origin_and_resolution_shift_the_lookup() {
        // Same wall grid, but shifted so the wall now lives around world x=6.
        let mut cells = vec![255u8; 8 * 8];
        for row in 0..8 {
            cells[row * 8 + 4] = 0;
        }
        let g = OccupancyGrid::from_cells(8, 8, cells, 0.5, 4.0, -1.0, 128).unwrap();
        // Wall column 4 spans world x ∈ [6.0, 6.5). A point at 6.25 must hit.
        assert!(!g.is_point_free(Point2D { x: 6.25, y: 0.0 }));
        // Just outside — column 3 — must be free.
        assert!(g.is_point_free(Point2D { x: 5.75, y: 0.0 }));
    }
}
