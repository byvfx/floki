use exr::prelude::*;

#[allow(clippy::type_complexity)] // scratch tool: the explicit type is the point
fn main() {
    let path = "test.exr";
    let image: std::result::Result<
        Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>,
        exr::error::Error,
    > = read()
        .no_deep_data()
        .largest_resolution_level()
        .all_channels()
        .all_layers()
        .all_attributes()
        .from_file(path);

    let disp = image.unwrap().attributes.display_window;
    let _disp_pos = disp.position;
    let _disp_size = disp.size;
}
