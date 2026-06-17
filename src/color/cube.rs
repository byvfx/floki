use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

/// A parsed Adobe `.cube` 3D LUT. `domain_min`/`domain_max` are consumed by
/// `ExrApp::reload_lut` and pushed to the GPU as uniform fields so the shader
/// can remap the lookup coordinate for non-unit-domain (HDR/film) LUTs.
#[derive(Debug)]
pub struct CubeLut {
    pub size: usize,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    pub data: Vec<[f32; 4]>, // Stored as RGBA to easily upload to GPU Rgba32Float format
}

impl CubeLut {
    /// Returns `(size as u32, &[u8])` — the 3D LUT extent and the raw RGBA
    /// bytes ready for `queue.write_texture`. Inverts the dependency so the
    /// GPU layer doesn't need to know the `CubeLut` struct layout.
    pub fn as_rgba_bytes(&self) -> (u32, &[u8]) {
        (self.size as u32, bytemuck::cast_slice(&self.data))
    }

    pub fn load<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path)?;
        let reader = io::BufReader::new(file);

        let mut size = 0;
        let mut domain_min = [0.0, 0.0, 0.0];
        let mut domain_max = [1.0, 1.0, 1.0];
        let mut data = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }

            match parts[0] {
                "TITLE" => continue, // Ignore title
                "LUT_3D_SIZE" => {
                    size = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                }
                "DOMAIN_MIN" => {
                    if parts.len() >= 4 {
                        domain_min[0] = parts[1].parse().unwrap_or(0.0);
                        domain_min[1] = parts[2].parse().unwrap_or(0.0);
                        domain_min[2] = parts[3].parse().unwrap_or(0.0);
                    }
                }
                "DOMAIN_MAX" => {
                    if parts.len() >= 4 {
                        domain_max[0] = parts[1].parse().unwrap_or(1.0);
                        domain_max[1] = parts[2].parse().unwrap_or(1.0);
                        domain_max[2] = parts[3].parse().unwrap_or(1.0);
                    }
                }
                _ => {
                    // A data row starts with a number. If the first token isn't a
                    // float, this is an unknown keyword we don't model (e.g.
                    // LUT_1D_SIZE, LUT_3D_INPUT_RANGE) — skip it leniently.
                    let Ok(r) = parts[0].parse::<f32>() else {
                        continue;
                    };
                    // It *is* a data row: now be strict. Silently dropping a
                    // malformed/garbage row would desync the count check or, worse,
                    // pass with wrong data; a non-finite value would upload NaN/inf
                    // into the LUT texture and render as garbage with no error.
                    if parts.len() < 3 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("LUT data row needs 3 components: {line}"),
                        ));
                    }
                    let (Ok(g), Ok(b)) = (parts[1].parse::<f32>(), parts[2].parse::<f32>()) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("Malformed LUT data row: {line}"),
                        ));
                    };
                    if !(r.is_finite() && g.is_finite() && b.is_finite()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("Non-finite value in LUT data row: {line}"),
                        ));
                    }
                    data.push([r, g, b, 1.0]); // Alpha is 1.0
                }
            }
        }

        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LUT_3D_SIZE not found or zero",
            ));
        }

        let expected_len = size * size * size;
        if data.len() != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Expected {} data points, found {}",
                    expected_len,
                    data.len()
                ),
            ));
        }

        Ok(Self {
            size,
            domain_min,
            domain_max,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Write `contents` to a temp `.cube` file and return the handle (kept alive
    /// by the caller so the file isn't deleted before `load` reads it).
    fn cube_file(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".cube")
            .tempfile()
            .expect("create temp cube");
        f.write_all(contents.as_bytes()).expect("write temp cube");
        f.flush().expect("flush temp cube");
        f
    }

    /// A full, valid 2x2x2 LUT: 8 rows. Distinct R/G/B per row so any reordering
    /// or off-by-one is detectable.
    const VALID_2X2X2: &str = "\
TITLE \"test\"
LUT_3D_SIZE 2
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";

    #[test]
    fn parses_valid_2x2x2_lut() {
        let f = cube_file(VALID_2X2X2);
        let lut = CubeLut::load(f.path()).expect("valid cube must parse");

        assert_eq!(lut.size, 2);
        assert_eq!(lut.data.len(), 8, "data must be size^3");
        // First and last rows land where expected, alpha is forced to 1.0.
        assert_eq!(lut.data[0], [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(lut.data[7], [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(lut.data[1], [1.0, 0.0, 0.0, 1.0]);
        assert!(
            lut.data.iter().all(|px| px[3] == 1.0),
            "alpha must always be 1.0"
        );
    }

    #[test]
    fn domain_defaults_to_unit_cube_when_absent() {
        let f = cube_file(VALID_2X2X2);
        let lut = CubeLut::load(f.path()).unwrap();
        assert_eq!(lut.domain_min, [0.0, 0.0, 0.0]);
        assert_eq!(lut.domain_max, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn parses_explicit_domain() {
        let contents = format!("DOMAIN_MIN -1.0 -2.0 -3.0\nDOMAIN_MAX 2.0 4.0 8.0\n{VALID_2X2X2}");
        let f = cube_file(&contents);
        let lut = CubeLut::load(f.path()).unwrap();
        assert_eq!(lut.domain_min, [-1.0, -2.0, -3.0]);
        assert_eq!(lut.domain_max, [2.0, 4.0, 8.0]);
    }

    #[test]
    fn hdr_domain_round_trips_into_app_uniform_fields() {
        // The GPU uniform carries the LUT domain as `[f32; 4]` (xyz + pad). This
        // proves the parsing → field-copy path that `ExrApp::reload_lut` uses:
        // a non-unit-domain LUT produces values that would remap the lookup
        // coordinate in the shader (the actual sampling can't be tested GPU-free,
        // but the wiring can).
        let contents = format!("DOMAIN_MIN -0.5 -0.5 -0.5\nDOMAIN_MAX 1.5 1.5 1.5\n{VALID_2X2X2}");
        let lut = CubeLut::load(cube_file(&contents).path()).unwrap();
        let uniform_min = [lut.domain_min[0], lut.domain_min[1], lut.domain_min[2], 0.0];
        let uniform_max = [lut.domain_max[0], lut.domain_max[1], lut.domain_max[2], 0.0];
        assert_eq!(uniform_min, [-0.5, -0.5, -0.5, 0.0]);
        assert_eq!(uniform_max, [1.5, 1.5, 1.5, 0.0]);
        // Identity-domain LUTs (the common case) produce a no-op remap.
        let identity = CubeLut::load(cube_file(VALID_2X2X2).path()).unwrap();
        assert_eq!(
            [
                identity.domain_min[0],
                identity.domain_min[1],
                identity.domain_min[2]
            ],
            [0.0, 0.0, 0.0]
        );
        assert_eq!(
            [
                identity.domain_max[0],
                identity.domain_max[1],
                identity.domain_max[2]
            ],
            [1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let contents = "\
# a leading comment
TITLE \"with junk\"

LUT_3D_SIZE 2

# interleaved comment
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0

0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
        let f = cube_file(contents);
        let lut = CubeLut::load(f.path()).expect("comments/blanks must be ignored");
        assert_eq!(lut.size, 2);
        assert_eq!(lut.data.len(), 8);
    }

    #[test]
    fn missing_size_is_invalid_data() {
        // No LUT_3D_SIZE line at all.
        let f = cube_file("0.0 0.0 0.0\n1.0 1.0 1.0\n");
        let err = CubeLut::load(f.path()).expect_err("must reject missing size");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn zero_size_is_invalid_data() {
        let f = cube_file("LUT_3D_SIZE 0\n");
        let err = CubeLut::load(f.path()).expect_err("must reject zero size");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn data_count_mismatch_is_invalid_data() {
        // Declares size 2 (needs 8 rows) but only provides 2.
        let f = cube_file("LUT_3D_SIZE 2\n0.0 0.0 0.0\n1.0 1.0 1.0\n");
        let err = CubeLut::load(f.path()).expect_err("must reject wrong data count");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_file_is_io_error() {
        let err =
            CubeLut::load("definitely/does/not/exist.cube").expect_err("missing file must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn non_finite_value_is_invalid_data() {
        // A NaN/inf in a data row would upload garbage into the LUT texture.
        let contents = "\
LUT_3D_SIZE 2
0.0 0.0 0.0
nan 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
inf 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
        let err =
            CubeLut::load(cube_file(contents).path()).expect_err("must reject non-finite values");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn malformed_data_row_is_invalid_data() {
        // A row that starts numeric but has a garbage component must not be
        // silently dropped (which would only surface as a confusing count error).
        let contents = "\
LUT_3D_SIZE 2
0.0 0.0 0.0
1.0 zzz 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
        let err =
            CubeLut::load(cube_file(contents).path()).expect_err("must reject malformed data rows");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_keyword_lines_are_skipped() {
        // Non-numeric keyword lines we don't model must be ignored, not treated
        // as data rows.
        let contents = format!("LUT_3D_INPUT_RANGE 0.0 1.0\n{VALID_2X2X2}");
        let lut =
            CubeLut::load(cube_file(&contents).path()).expect("unknown keywords must be skipped");
        assert_eq!(lut.data.len(), 8);
    }

    #[test]
    fn loads_committed_test_asset() {
        // The repo ships a small real .cube; prove the real-file path works.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/test.cube");
        let lut = CubeLut::load(&path).expect("assets/test.cube must parse");
        assert!(lut.size > 0);
        assert_eq!(lut.data.len(), lut.size * lut.size * lut.size);
        assert!(lut.data.iter().all(|px| px[3] == 1.0));
    }
}
