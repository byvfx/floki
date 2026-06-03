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

#[derive(PartialEq, Clone, Copy)]
pub enum CompareMode {
    SingleA,
    SingleB,
    Wipe,
    SideBySide,
    DiffMatte,
}

impl Default for ChannelMode {
    fn default() -> Self {
        Self::RGB
    }
}

pub struct ExrViewer {
    textures: Vec<Option<egui::TextureHandle>>,
    textures_b: Vec<Option<egui::TextureHandle>>,
    diff_texture: Option<egui::TextureHandle>,
    last_diff_params: (usize, f32), // (layer_index, diff_multiplier)
    pub blink_state: bool,
    // Add viewing options like exposure, gamma, srgb toggle
    pub exposure: f32,
    pub gamma: f32,
    pub srgb: bool,
    pub channel_mode: ChannelMode,
    pub compare_mode: CompareMode,
    pub wipe_position: f32,
    pub diff_multiplier: f32,
    pub active_layer: usize,
    pub show_contact_sheet: bool,
    pub swatches: Vec<[f32; 4]>,
    pub histogram: Option<[u32; 256]>,
    pub histogram_b: Option<[u32; 256]>,
    pub histogram_layer: Option<usize>,
    pub log_histogram: bool,

    // View transform
    pub scale: f32,
    pub translation: egui::Vec2,
    pub first_frame: bool,
}

impl Default for ExrViewer {
    fn default() -> Self {
        Self {
            textures: Vec::new(),
            textures_b: Vec::new(),
            diff_texture: None,
            last_diff_params: (0, 0.0),
            blink_state: false,
            exposure: 0.0,
            gamma: 1.0,
            srgb: true,
            channel_mode: ChannelMode::RGB,
            compare_mode: CompareMode::SingleA,
            wipe_position: 0.5,
            diff_multiplier: 1.0,
            active_layer: 0,
            show_contact_sheet: false,
            swatches: Vec::new(),
            histogram: None,
            histogram_b: None,
            histogram_layer: None,
            log_histogram: true,
            scale: 1.0,
            translation: egui::Vec2::ZERO,
            first_frame: true,
        }
    }
}

impl ExrViewer {
    pub fn ui(&mut self, ui: &mut egui::Ui, exr_data: &ExrData, exr_data_b: Option<&ExrData>) {
        if ui.input(|i| i.key_pressed(egui::Key::Num1)) {
            self.compare_mode = CompareMode::SingleA;
            self.blink_state = false;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Num2)) && exr_data_b.is_some() {
            self.compare_mode = CompareMode::SingleB;
            self.blink_state = false;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Space)) && exr_data_b.is_some() {
            self.blink_state = !self.blink_state;
        }

        if self.blink_state && exr_data_b.is_some() {
            ui.ctx().request_repaint();
            let time = ui.input(|i| i.time);
            if (time * 5.0) as usize % 2 == 0 {
                self.compare_mode = CompareMode::SingleA;
            } else {
                self.compare_mode = CompareMode::SingleB;
            }
        }
        
        egui::TopBottomPanel::top("viewer_controls").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if exr_data_b.is_some() {
                    ui.label("Compare:");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::SingleA, "A");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::SingleB, "B");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::Wipe, "Wipe");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::SideBySide, "Side-by-Side");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::DiffMatte, "Diff");
                    if ui.toggle_value(&mut self.blink_state, "Blink (Spc)").clicked() {
                        if !self.blink_state { self.compare_mode = CompareMode::SingleA; }
                    }
                    
                    if self.compare_mode == CompareMode::Wipe {
                        ui.add(egui::Slider::new(&mut self.wipe_position, 0.0..=1.0).text("Wipe"));
                    } else if self.compare_mode == CompareMode::DiffMatte {
                        ui.add(egui::Slider::new(&mut self.diff_multiplier, 1.0..=100.0).text("Diff Multiplier"));
                    }
                    ui.separator();
                }

                ui.label("Exposure:");
                if ui
                    .add(egui::Slider::new(&mut self.exposure, -5.0..=5.0))
                    .changed()
                {
                    self.textures.fill(None);
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }
                ui.label("Gamma:");
                if ui.add(egui::Slider::new(&mut self.gamma, 0.1..=5.0)).changed() {
                    self.textures.fill(None);
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }
                if ui.checkbox(&mut self.srgb, "sRGB").changed() {
                    self.textures.fill(None);
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }

                let layer_count = exr_data.image.layer_data.len();
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
        });

        let layer_count = exr_data.image.layer_data.len();
        if self.textures.len() != layer_count {
            self.textures.clear();
            self.textures.resize(layer_count, None);
        }
        let layer_count_b = exr_data_b.map(|d| d.image.layer_data.len()).unwrap_or(0);
        if self.textures_b.len() != layer_count_b {
            self.textures_b.clear();
            self.textures_b.resize(layer_count_b, None);
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
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }
            });

            // Ensure textures are generated based on compare mode
            if self.textures[self.active_layer].is_none() {
                self.textures[self.active_layer] = self.generate_texture(ui.ctx(), exr_data, self.active_layer);
            }
            if let Some(data_b) = exr_data_b {
                let layer_b = self.active_layer.min(data_b.image.layer_data.len().saturating_sub(1));
                if self.textures_b[layer_b].is_none() {
                    self.textures_b[layer_b] = self.generate_texture(ui.ctx(), data_b, layer_b);
                }
            }
            if self.compare_mode == CompareMode::DiffMatte && exr_data_b.is_some() {
                if self.diff_texture.is_none() || self.last_diff_params != (self.active_layer, self.diff_multiplier) {
                    let layer_b = self.active_layer.min(exr_data_b.unwrap().image.layer_data.len().saturating_sub(1));
                    self.diff_texture = self.generate_diff_texture(ui.ctx(), exr_data, exr_data_b.unwrap(), self.active_layer, layer_b);
                    self.last_diff_params = (self.active_layer, self.diff_multiplier);
                }
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

                let painter = ui.painter().with_clip_rect(rect);

                let draw_image = |painter: &egui::Painter, tex: &egui::TextureHandle, clip_rect: egui::Rect, target_rect: egui::Rect| {
                    let alpha = if self.blink_state && (ui.input(|i| i.time) % 1.0 > 0.5) { 0.0 } else { 1.0 };
                    painter.with_clip_rect(clip_rect).image(
                        tex.id(),
                        target_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::from_white_alpha((alpha * 255.0) as u8),
                    );
                };

                match self.compare_mode {
                    CompareMode::SingleA => {
                        draw_image(&painter, texture, rect, image_rect);
                    }
                    CompareMode::SingleB => {
                        if let Some(tex_b) = exr_data_b.and_then(|d| self.textures_b[self.active_layer.min(d.image.layer_data.len().saturating_sub(1))].as_ref()) {
                            draw_image(&painter, tex_b, rect, image_rect);
                        }
                    }
                    CompareMode::Wipe => {
                        let wipe_x = image_rect.min.x + image_rect.width() * self.wipe_position;
                        let rect_a = egui::Rect::from_min_max(rect.min, egui::pos2(wipe_x, rect.max.y));
                        let rect_b = egui::Rect::from_min_max(egui::pos2(wipe_x, rect.min.y), rect.max);
                        
                        draw_image(&painter, texture, rect_a, image_rect);
                        if let Some(tex_b) = exr_data_b.and_then(|d| self.textures_b[self.active_layer.min(d.image.layer_data.len().saturating_sub(1))].as_ref()) {
                            draw_image(&painter, tex_b, rect_b, image_rect);
                        }
                        
                        // Draw Wipe Line
                        painter.line_segment([egui::pos2(wipe_x, rect.min.y), egui::pos2(wipe_x, rect.max.y)], (2.0, egui::Color32::WHITE));
                    }
                    CompareMode::SideBySide => {
                        let tex_b_opt = exr_data_b.and_then(|d| self.textures_b[self.active_layer.min(d.image.layer_data.len().saturating_sub(1))].as_ref());
                        if let Some(tex_b) = tex_b_opt {
                            let tex_size_b = tex_b.size_vec2();
                            let image_size_b = tex_size_b * self.scale;
                            let combined_width = image_size.x + image_size_b.x;
                            let combined_height = image_size.y.max(image_size_b.y);
                            
                            let combined_rect = egui::Rect::from_center_size(
                                rect.center() + self.translation,
                                egui::vec2(combined_width, combined_height)
                            );
                            
                            let mut image_rect_a = egui::Rect::from_min_size(combined_rect.min, image_size);
                            image_rect_a.set_center(egui::pos2(image_rect_a.center().x, combined_rect.center().y));
                            
                            let mut image_rect_b = egui::Rect::from_min_size(egui::pos2(combined_rect.min.x + image_size.x, combined_rect.min.y), image_size_b);
                            image_rect_b.set_center(egui::pos2(image_rect_b.center().x, combined_rect.center().y));
                            
                            draw_image(&painter, texture, rect, image_rect_a);
                            draw_image(&painter, tex_b, rect, image_rect_b);
                            
                            painter.line_segment([egui::pos2(image_rect_b.min.x, combined_rect.min.y), egui::pos2(image_rect_b.min.x, combined_rect.max.y)], (2.0, egui::Color32::GRAY));
                        } else {
                            draw_image(&painter, texture, rect, image_rect);
                        }
                    }
                    CompareMode::DiffMatte => {
                        if let Some(diff) = &self.diff_texture {
                            draw_image(&painter, diff, rect, image_rect);
                        }
                    }
                }

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

    fn generate_diff_texture(&self, ctx: &egui::Context, data_a: &ExrData, data_b: &ExrData, layer_a_idx: usize, layer_b_idx: usize) -> Option<egui::TextureHandle> {
        let layer_a = data_a.image.layer_data.get(layer_a_idx)?;
        let layer_b = data_b.image.layer_data.get(layer_b_idx)?;
        
        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);
        
        let mut pixels = vec![egui::Color32::BLACK; width * height];
        
        let (r_chan_a, g_chan_a, b_chan_a, _) = Self::find_rgba_channels(layer_a);
        let (r_chan_b, g_chan_b, b_chan_b, _) = Self::find_rgba_channels(layer_b);

        let get_val = |chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>, x: usize, y: usize, w: usize, h: usize| -> f32 {
            if x >= w || y >= h { return 0.0; }
            if let Some(c) = chan {
                let index = y * w + x;
                match &c.sample_data {
                    exr::image::FlatSamples::F16(s) => s[index].to_f32(),
                    exr::image::FlatSamples::F32(s) => s[index],
                    exr::image::FlatSamples::U32(s) => s[index] as f32 / u32::MAX as f32,
                }
            } else {
                0.0
            }
        };

        for y in 0..height {
            for x in 0..width {
                let r_a = get_val(r_chan_a, x, y, layer_a.size.0, layer_a.size.1);
                let g_a = get_val(g_chan_a, x, y, layer_a.size.0, layer_a.size.1);
                let b_a = get_val(b_chan_a, x, y, layer_a.size.0, layer_a.size.1);

                let r_b = get_val(r_chan_b, x, y, layer_b.size.0, layer_b.size.1);
                let g_b = get_val(g_chan_b, x, y, layer_b.size.0, layer_b.size.1);
                let b_b = get_val(b_chan_b, x, y, layer_b.size.0, layer_b.size.1);

                // Difference calculation
                let mut diff_r = (r_a - r_b).abs() * self.diff_multiplier;
                let mut diff_g = (g_a - g_b).abs() * self.diff_multiplier;
                let mut diff_b = (b_a - b_b).abs() * self.diff_multiplier;

                // Tone mapping logic for the diff to be visible
                let exp_mult = 2.0_f32.powf(self.exposure);
                diff_r *= exp_mult;
                diff_g *= exp_mult;
                diff_b *= exp_mult;

                if self.gamma != 1.0 {
                    let inv_gamma = 1.0 / self.gamma;
                    diff_r = if diff_r > 0.0 { diff_r.powf(inv_gamma) } else { 0.0 };
                    diff_g = if diff_g > 0.0 { diff_g.powf(inv_gamma) } else { 0.0 };
                    diff_b = if diff_b > 0.0 { diff_b.powf(inv_gamma) } else { 0.0 };
                }

                if self.srgb {
                    diff_r = self.linear_to_srgb(diff_r);
                    diff_g = self.linear_to_srgb(diff_g);
                    diff_b = self.linear_to_srgb(diff_b);
                }

                let r_u8 = (diff_r.clamp(0.0, 1.0) * 255.0) as u8;
                let g_u8 = (diff_g.clamp(0.0, 1.0) * 255.0) as u8;
                let b_u8 = (diff_b.clamp(0.0, 1.0) * 255.0) as u8;

                pixels[y * width + x] = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
            }
        }

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer_diff", color_image, egui::TextureOptions::LINEAR))
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

    pub fn calculate_histogram(&mut self, exr_data: &ExrData, exr_data_b: Option<&ExrData>) {
        if self.histogram_layer == Some(self.active_layer) {
            return;
        }

        let calc_bins = |data: &ExrData, layer_idx: usize| -> Option<[u32; 256]> {
            let mut bins = [0u32; 256];
            let layer = data.image.layer_data.get(layer_idx)?;
            let width = layer.size.0;
            let height = layer.size.1;

            let (r_chan, g_chan, b_chan, _) = Self::find_rgba_channels(layer);

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

            for y in 0..height {
                for x in 0..width {
                    let r = get_val(r_chan, x, y);
                    let g = get_val(g_chan, x, y);
                    let b = get_val(b_chan, x, y);

                    // Luminance
                    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;

                    let bin = if self.log_histogram {
                        let ev = if lum <= 0.0 { -10.0 } else { lum.log2().clamp(-10.0, 10.0) };
                        ((ev + 10.0) / 20.0 * 255.0) as usize
                    } else {
                        (lum.clamp(0.0, 1.0) * 255.0) as usize
                    };

                    if bin < 256 {
                        bins[bin] += 1;
                    }
                }
            }
            Some(bins)
        };

        self.histogram = calc_bins(exr_data, self.active_layer);
        self.histogram_b = exr_data_b.and_then(|d| calc_bins(d, self.active_layer.min(d.image.layer_data.len().saturating_sub(1))));
        self.histogram_layer = Some(self.active_layer);
    }
}
