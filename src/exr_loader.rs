use exr::prelude::*;
use std::path::Path;

pub struct ExrData {
    pub image: Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>,
}

impl ExrData {
    pub fn load(path: impl AsRef<Path>) -> std::result::Result<Self, exr::error::Error> {
        let image = read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .all_layers()
            .all_attributes()
            .from_file(path)?;
        Ok(Self { image })
    }
}
