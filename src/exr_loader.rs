use exr::prelude::*;
use std::path::Path;

pub struct ExrData {
    pub image: Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>,
}

impl ExrData {
    pub fn load(path: impl AsRef<Path>) -> std::result::Result<Self, String> {
        let path_ref = path.as_ref();
        match read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .all_layers()
            .all_attributes()
            .from_file(path_ref) 
        {
            Ok(image) => Ok(Self { image }),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("file identifier missing") {
                    // Try to read the first 4 bytes to help the user
                    if let Ok(mut f) = std::fs::File::open(path_ref) {
                        use std::io::Read;
                        let mut buf = [0u8; 4];
                        if f.read_exact(&mut buf).is_ok() {
                            let hex_str = format!("{:02X} {:02X} {:02X} {:02X}", buf[0], buf[1], buf[2], buf[3]);
                            let ascii_str: String = buf.iter().map(|&b| if b >= 32 && b <= 126 { b as char } else { '.' }).collect();
                            return Err(format!(
                                "Not a valid EXR file (magic number missing).\nFirst 4 bytes: [{}] ('{}')\nMake sure this is actually an OpenEXR file and not a renamed PNG, JPG, or corrupted file.",
                                hex_str, ascii_str
                            ));
                        }
                    }
                    Err("Not a valid EXR file (magic number missing). The file might be corrupted or in another format.".to_string())
                } else {
                    Err(err_str)
                }
            }
        }
    }
}
