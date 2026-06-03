
mod color {
    pub mod cube {
        use std::fs::File;
        use std::io::{self, BufRead};
        use std::path::Path;

        pub struct CubeLut {
            pub size: usize,
            pub domain_min: [f32; 3],
            pub domain_max: [f32; 3],
            pub data: Vec<[f32; 4]>,
        }
        impl CubeLut {
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
                            if parts.len() >= 3 {
                                if let (Ok(r), Ok(g), Ok(b)) = (
                                    parts[0].parse::<f32>(),
                                    parts[1].parse::<f32>(),
                                    parts[2].parse::<f32>(),
                                ) {
                                    data.push([r, g, b, 1.0]);
                                }
                            }
                        }
                    }
                }
                if size == 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "LUT_3D_SIZE not found or zero"));
                }
                let expected_len = size * size * size;
                if data.len() != expected_len {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Expected {} data points, found {}", expected_len, data.len())));
                }
                Ok(Self { size, domain_min, domain_max, data })
            }
        }
    }
}
fn main() {
    let lut = color::cube::CubeLut::load("test.cube").unwrap();
    println!("Loaded LUT of size: {}, data points: {}", lut.size, lut.data.len());
}
