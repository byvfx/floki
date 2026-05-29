use crate::exr_loader::ExrData;
use eframe::egui;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    RGB,
    R,
    G,
    B,
    A,
}

impl Default for ChannelMode {
    fn default() -> Self {
        Self::RGB
    }
}

pub struct ExrViewer {
    textures: Vec<Option<egui::TextureHandle>>,
    // Add viewing options like exposure, gamma, srgb toggle
    pub exposure: f32,
    pub gamma: f32,
    pub srgb: bool,
    pub channel_mode: ChannelMode,
    pub active_layer: usize,
    pub show_contact_sheet: bool,
    pub swatches: Vec<[f32; 4]>,

    // View transform
    pub scale: f32,
    pub translation: egui::Vec2,
    pub first_frame: bool,
}

impl Default for ExrViewer {
    fn default() -> Self {
        Self {
            textures: Vec::new(),
            exposure: 0.0,
            gamma: 1.0,
            srgb: true,
            channel_mode: ChannelMode::RGB,
            active_layer: 0,
            show_contact_sheet: false,
            swatches: Vec::new(),
            scale: 1.0,
            translation: egui::Vec2::ZERO,
            first_frame: true,
        }
    }
}

impl ExrViewer {
    pub fn ui(&mut self, ui: &mut egui::Ui, exr_data: &ExrData) {
        // Ensure textures vector is correct size
        let layer_count = exr_data.image.layer_data.len();
        if self.textures.len() != layer_count {
            self.textures.clear();
            self.textures.resize(layer_count, None);
        }

        // Controls
        ui.horizontal(|ui| {
            ui.label("Exposure:");
            if ui
                .add(egui::Slider::new(&mut self.exposure, -5.0..=5.0))
                .changed()
            {
                self.textures.fill(None); // Force redraw all
            }
            ui.label("Gamma:");
            if ui.add(egui::Slider::new(&mut self.gamma, 0.1..=5.0)).changed() {
                self.textures.fill(None); // Force redraw all
            }
            if ui.checkbox(&mut self.srgb, "sRGB").changed() {
                self.textures.fill(None);
            }

            if layer_count > 1 {
                ui.toggle_value(&mut self.show_contact_sheet, "Contact Sheet");
            }
            
            ui.separator();
            ui.label("Channel:");
            let prev_mode = self.channel_mode;
            ui.selectable_value(&mut self.channel_mode, ChannelMode::RGB, "RGB (C)");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::R, "R");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::G, "G");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::B, "B");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::A, "A");
            
            if self.channel_mode != prev_mode {
                self.textures.fill(None);
            }

            if !self.show_contact_sheet {
                if ui.button("Frame (F)").clicked() {
                    self.first_frame = true;
                }

                // Layer selection
                if layer_count > 1 {
                    ui.label("Layer:");
                    egui::ComboBox::from_id_salt("layer_select")
                        .selected_text(format!("Layer {}", self.active_layer))
                        .show_ui(ui, |ui| {
                            for i in 0..layer_count {
                                let name = exr_data.image.layer_data[i]
                                    .attributes
                                    .layer_name
                                    .as_ref()
                                    .map(|t| t.to_string())
                                    .unwrap_or_else(|| "Unnamed".to_string());
                                if ui
                                    .selectable_value(
                                        &mut self.active_layer,
                                        i,
                                        format!("{} - {}", i, name),
                                    )
                                    .clicked()
                                {
                                    self.first_frame = true;
                                }
                            }
                        });
                }
            }
        });

        if self.show_contact_sheet {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0); // Add some spacing between thumbnails
                    for i in 0..layer_count {
                        if self.textures[i].is_none() {
                            self.textures[i] = self.generate_texture(ui.ctx(), exr_data, i);
                        }

                        if let Some(texture) = &self.textures[i] {
                            let thumb_width = 256.0;
                            let thumb_height =
                                thumb_width * (texture.size_vec2().y / texture.size_vec2().x);

                            // Allocate fixed width container for wrapping to work properly
                            ui.allocate_ui(egui::vec2(thumb_width, thumb_height + 30.0), |ui| {
                                ui.vertical(|ui| {
                                    let name = exr_data.image.layer_data[i]
                                        .attributes
                                        .layer_name
                                        .as_ref()
                                        .map(|t| t.to_string())
                                        .unwrap_or_else(|| "Unnamed".to_string());
                                    // Let text wrap if it's too long
                                    ui.label(
                                        egui::RichText::new(format!("Layer {}: {}", i, name))
                                            .strong(),
                                    );

                                    // Thumbnail
                                    let response =
                                        ui.add(egui::Image::new(texture).fit_to_exact_size(
                                            egui::vec2(thumb_width, thumb_height),
                                        ));

                                    if response.clicked() {
                                        self.active_layer = i;
                                        self.show_contact_sheet = false;
                                        self.first_frame = true;
                                    }
                                    if response.hovered() {
                                        response
                                            .clone()
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .on_hover_text("Click to view layer");
                                    }
                                });
                            });
                        }
                    }
                });
            });
        } else {
            // Handle Keyboard "F" to frame and Channel hotkeys
            ui.input(|i| {
                if i.key_pressed(egui::Key::F) { self.first_frame = true; }
                
                let prev_mode = self.channel_mode;
                if i.key_pressed(egui::Key::R) { self.channel_mode = ChannelMode::R; }
                if i.key_pressed(egui::Key::G) { self.channel_mode = ChannelMode::G; }
                if i.key_pressed(egui::Key::B) { self.channel_mode = ChannelMode::B; }
                if i.key_pressed(egui::Key::A) { self.channel_mode = ChannelMode::A; }
                if i.key_pressed(egui::Key::C) { self.channel_mode = ChannelMode::RGB; }
                if self.channel_mode != prev_mode {
                    self.textures.fill(None);
                }
            });

            // Ensure single active texture is generated
            if self.textures[self.active_layer].is_none() {
                self.textures[self.active_layer] =
                    self.generate_texture(ui.ctx(), exr_data, self.active_layer);
            }

            // Draw texture
            if let Some(texture) = &self.textures[self.active_layer] {
                let tex_size = texture.size_vec2();

                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
                
                if self.first_frame {
                    let scale_x = rect.width() / tex_size.x;
                    let scale_y = rect.height() / tex_size.y;
                    self.scale = scale_x.min(scale_y).min(1.0); // Fit but don't scale up past 1.0 initially
                    self.translation = egui::Vec2::ZERO;
                    self.first_frame = false;
                }

                // Handle Zoom
                if response.hovered() {
                    let zoom_delta = ui.input(|i| i.zoom_delta());
                    if zoom_delta != 1.0
                        && let Some(pos) = response.hover_pos() {
                            // Zoom around the cursor
                            let offset = pos - rect.center() - self.translation;
                            self.translation -= offset * (zoom_delta - 1.0);
                            self.scale *= zoom_delta;
                        }
                }

                // Handle Panning
                if response.dragged() {
                    self.translation += response.drag_delta();
                }

                // Render Image
                let image_size = tex_size * self.scale;
                let image_rect = egui::Rect::from_min_size(
                    rect.center() + self.translation - image_size / 2.0,
                    image_size,
                );

                // Clip to drawing area
                ui.painter().with_clip_rect(rect).image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                // Pixel Sampling & Swatches
                if let Some(pos) = response.hover_pos() {
                    let image_local_pos = pos - image_rect.min;
                    let x = (image_local_pos.x / self.scale) as usize;
                    let y = (image_local_pos.y / self.scale) as usize;

                    // Check if inside image
                    if x < tex_size.x as usize
                        && y < tex_size.y as usize
                        && image_local_pos.x >= 0.0
                        && image_local_pos.y >= 0.0
                    {
                        if let Some(val) = self.sample_pixel(exr_data, self.active_layer, x, y) {
                            egui::Window::new("Pixel Tooltip")
                                .fixed_pos(pos + egui::vec2(15.0, 15.0))
                                .title_bar(false)
                                .resizable(false)
                                .collapsible(false)
                                .show(ui.ctx(), |ui| {
                                    ui.label(format!("x: {}, y: {}\nVal: {:.4}, {:.4}, {:.4}, {:.4}", x, y, val[0], val[1], val[2], val[3]));
                                });
                            
                            // Shift+Click to add a persistent swatch
                            if ui.input(|i| i.modifiers.shift) && response.clicked() {
                                self.swatches.push(val);
                            }
                        }
                    }
                }
            }
        }
    }

    fn generate_texture(&self, ctx: &egui::Context, exr_data: &ExrData, layer_index: usize) -> Option<egui::TextureHandle> {
        let layer = exr_data.image.layer_data.get(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;
        
        let mut pixels = vec![egui::Color32::BLACK; width * height];
        
        // Find R, G, B, A channels with robust matching
        let (r_chan, g_chan, b_chan, a_chan) = Self::find_rgba_channels(layer);

        // Helper to get a pixel value from a channel
        let get_val = |chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
                       x: usize,
                       y: usize|
         -> f32 {
            if let Some(c) = chan {
                let index = y * width + x;
                match &c.sample_data {
                    exr::image::FlatSamples::F16(s) => s[index].to_f32(),
                    exr::image::FlatSamples::F32(s) => s[index],
                    exr::image::FlatSamples::U32(s) => s[index] as f32 / u32::MAX as f32,
                }
            } else {
                0.0
            }
        };

        let exp_mult = 2.0_f32.powf(self.exposure);

        for y in 0..height {
            for x in 0..width {
                let mut r = get_val(r_chan, x, y);
                let mut g = get_val(g_chan, x, y);
                let mut b = get_val(b_chan, x, y);
                let mut a = get_val(a_chan, x, y);
                
                if a_chan.is_none() {
                    a = 1.0;
                }

                match self.channel_mode {
                    ChannelMode::R => { g = r; b = r; a = 1.0; },
                    ChannelMode::G => { r = g; b = g; a = 1.0; },
                    ChannelMode::B => { r = b; g = b; a = 1.0; },
                    ChannelMode::A => { r = a; g = a; b = a; a = 1.0; },
                    ChannelMode::RGB => {},
                }

                let is_dark = ((x / 16) + (y / 16)) % 2 == 0;
                let bg_linear = if is_dark { 0.1 } else { 0.2 };

                // Apply exposure
                r *= exp_mult;
                g *= exp_mult;
                b *= exp_mult;

                // Composite over checkerboard (assuming EXR is pre-multiplied)
                let a_clamp = a.clamp(0.0, 1.0);
                r = r + bg_linear * (1.0 - a_clamp);
                g = g + bg_linear * (1.0 - a_clamp);
                b = b + bg_linear * (1.0 - a_clamp);

                if self.gamma != 1.0 {
                    let inv_gamma = 1.0 / self.gamma;
                    r = if r > 0.0 { r.powf(inv_gamma) } else { 0.0 };
                    g = if g > 0.0 { g.powf(inv_gamma) } else { 0.0 };
                    b = if b > 0.0 { b.powf(inv_gamma) } else { 0.0 };
                }

                if self.srgb {
                    r = self.linear_to_srgb(r);
                    g = self.linear_to_srgb(g);
                    b = self.linear_to_srgb(b);
                }

                let r_u8 = (r.clamp(0.0, 1.0) * 255.0) as u8;
                let g_u8 = (g.clamp(0.0, 1.0) * 255.0) as u8;
                let b_u8 = (b.clamp(0.0, 1.0) * 255.0) as u8;

                pixels[y * width + x] = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
            }
        }

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer", color_image, egui::TextureOptions::LINEAR))
    }

    fn sample_pixel(&self, exr_data: &ExrData, layer_index: usize, x: usize, y: usize) -> Option<[f32; 4]> {
        let layer = exr_data.image.layer_data.get(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;
        
        if x >= width || y >= height {
            return None;
        }

        let (r_chan, g_chan, b_chan, a_chan) = Self::find_rgba_channels(layer);

        let get_val = |chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>, x: usize, y: usize| -> f32 {
            if let Some(c) = chan {
                let index = y * width + x;
                match &c.sample_data {
                    exr::image::FlatSamples::F16(s) => s[index].to_f32(),
                    exr::image::FlatSamples::F32(s) => s[index],
                    exr::image::FlatSamples::U32(s) => s[index] as f32 / u32::MAX as f32,
                }
            } else {
                0.0
            }
        };

        let r = get_val(r_chan, x, y);
        let g = get_val(g_chan, x, y);
        let b = get_val(b_chan, x, y);
        let mut a = get_val(a_chan, x, y);

        if a_chan.is_none() {
            a = 1.0;
        }

        Some([r, g, b, a])
    }

    fn find_rgba_channels<'a>(layer: &'a exr::image::Layer<exr::image::AnyChannels<exr::image::FlatSamples>>) -> (
        Option<&'a exr::image::AnyChannel<exr::image::FlatSamples>>,
        Option<&'a exr::image::AnyChannel<exr::image::FlatSamples>>,
        Option<&'a exr::image::AnyChannel<exr::image::FlatSamples>>,
        Option<&'a exr::image::AnyChannel<exr::image::FlatSamples>>
    ) {
        let mut r_chan = None;
        let mut g_chan = None;
        let mut b_chan = None;
        let mut a_chan = None;

        for c in &layer.channel_data.list {
            let n = c.name.to_string();
            let upper = n.to_uppercase();
            if upper == "R" || upper == "RED" || upper.ends_with(".R") || upper.ends_with(".RED") {
                r_chan = Some(c);
            } else if upper == "G" || upper == "GREEN" || upper.ends_with(".G") || upper.ends_with(".GREEN") {
                g_chan = Some(c);
            } else if upper == "B" || upper == "BLUE" || upper.ends_with(".B") || upper.ends_with(".BLUE") {
                b_chan = Some(c);
            } else if upper == "A" || upper == "ALPHA" || upper.ends_with(".A") || upper.ends_with(".ALPHA") {
                a_chan = Some(c);
            }
        }

        // Fallback for single-channel or non-standard layers (e.g., Z-depth, Alpha, luminance)
        if r_chan.is_none() && g_chan.is_none() && b_chan.is_none() {
            if let Some(first) = layer.channel_data.list.first() {
                r_chan = Some(first);
                g_chan = Some(first);
                b_chan = Some(first);
            }
        }

        (r_chan, g_chan, b_chan, a_chan)
    }

    pub fn linear_to_srgb(&self, l: f32) -> f32 {
        if l <= 0.0031308 {
            l * 12.92
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        }
    }
}
