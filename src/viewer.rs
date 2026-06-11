use crate::exr_loader::ExrData;
use eframe::egui;
use rayon::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
#[allow(clippy::upper_case_acronyms)] // RGB matches the documented channel_mode mapping
pub enum ChannelMode {
    #[default]
    RGB,
    R,
    G,
    B,
    A,
}

impl ChannelMode {
    /// Integer encoding shared with the GPU. This is the **single source of
    /// truth** for the `channel_mode` mapping; the `switch` in
    /// `gpu/shader.wgsl` must use these same values (RGB=0, R=1, G=2, B=3, A=4).
    /// Changing a value here requires the matching change in the shader.
    pub fn as_u32(self) -> u32 {
        match self {
            ChannelMode::RGB => 0,
            ChannelMode::R => 1,
            ChannelMode::G => 2,
            ChannelMode::B => 3,
            ChannelMode::A => 4,
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum CompareMode {
    SingleA,
    SingleB,
    Wipe,
    SideBySide,
    DiffMatte,
    Composite,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum BlendMode {
    #[default]
    Over,
    Under,
    Add,
    Multiply,
    Screen,
}

impl BlendMode {
    /// Integer encoding shared with the GPU. This is the **single source of
    /// truth** for the `blend_mode` mapping; the `switch` in `gpu/shader.wgsl`
    /// must use these same values (Over=0, Under=1, Add=2, Multiply=3, Screen=4).
    /// Changing a value here requires the matching change in the shader.
    pub fn as_u32(self) -> u32 {
        match self {
            BlendMode::Over => 0,
            BlendMode::Under => 1,
            BlendMode::Add => 2,
            BlendMode::Multiply => 3,
            BlendMode::Screen => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Over => "Over",
            BlendMode::Under => "Under",
            BlendMode::Add => "Add",
            BlendMode::Multiply => "Multiply",
            BlendMode::Screen => "Screen",
        }
    }
}

pub struct ExrViewer {
    textures: Vec<Option<egui::TextureHandle>>,
    textures_b: Vec<Option<egui::TextureHandle>>,
    gpu_textures: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,
    gpu_textures_b: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,
    diff_texture: Option<egui::TextureHandle>,
    last_diff_params: (usize, f32), // (layer_index, diff_multiplier)
    composite_texture: Option<egui::TextureHandle>,
    last_composite_params: (usize, BlendMode), // (layer_index, blend_mode)
    pub blink_state: bool,
    pub blink_interval: f32,
    pub fullscreen: bool,
    // Add viewing options like exposure, gamma, srgb toggle
    pub exposure: f32,
    pub overscan_opacity: f32,
    pub gamma: f32,
    pub srgb: bool,
    pub enable_lut: bool,
    pub show_tooltip: bool,
    pub channel_mode: ChannelMode,
    pub compare_mode: CompareMode,
    pub blend_mode: BlendMode,
    pub sample_aperture: usize,
    pub wipe_position: f32,
    pub diff_multiplier: f32,
    pub active_layer: usize,
    pub show_contact_sheet: bool,
    pub normalize_side_by_side: bool,
    pub swatches: Vec<[f32; 4]>,
    pub histogram: Option<[u32; 256]>,
    pub histogram_b: Option<[u32; 256]>,
    pub histogram_layer: Option<usize>,
    pub log_histogram: bool,

    // View transform
    pub scale: f32,
    pub translation: egui::Vec2,
    pub first_frame: bool,
    pub last_hover_pos_img: Option<(usize, usize)>,
    pub last_sampled_val_a: Option<[f32; 4]>,
    pub last_sampled_val_b: Option<[f32; 4]>,
}

impl Default for ExrViewer {
    fn default() -> Self {
        Self {
            textures: Vec::new(),
            textures_b: Vec::new(),
            gpu_textures: Vec::new(),
            gpu_textures_b: Vec::new(),
            diff_texture: None,
            last_diff_params: (0, 0.0),
            composite_texture: None,
            last_composite_params: (0, BlendMode::Over),
            blink_state: false,
            blink_interval: 1.0,
            fullscreen: false,
            exposure: 0.0,
            overscan_opacity: 0.2,
            gamma: 1.0,
            srgb: true,
            enable_lut: false,
            show_tooltip: true,
            channel_mode: ChannelMode::RGB,
            compare_mode: CompareMode::SingleA,
            blend_mode: BlendMode::Over,
            sample_aperture: 1,
            wipe_position: 0.5,
            diff_multiplier: 1.0,
            active_layer: 0,
            show_contact_sheet: false,
            normalize_side_by_side: true,
            swatches: Vec::new(),
            histogram: None,
            histogram_b: None,
            histogram_layer: None,
            log_histogram: true,
            scale: 1.0,
            translation: egui::Vec2::ZERO,
            first_frame: true,
            last_hover_pos_img: None,
            last_sampled_val_a: None,
            last_sampled_val_b: None,
        }
    }
}

impl ExrViewer {
    /// Apply keyboard shortcuts that only mutate view state — compare-mode
    /// selection (1/2/Space) and channel isolation (R/G/B/A/C/F). Extracted
    /// from [`Self::ui`] as a rendering-free seam so the input handling can be
    /// driven headlessly in tests (no wgpu device required).
    ///
    /// `has_b` is whether a reference image (B) is loaded; the B-only shortcuts
    /// are inert without it. Channel shortcuts apply only in the single-view
    /// layout (not the contact sheet), matching the original inline behavior.
    pub fn handle_hotkeys(&mut self, ui: &egui::Ui, has_b: bool) {
        // Sending a viewport command requires `ui.ctx()`, which we cannot touch
        // while the input lock is held — defer it until after the closure.
        let mut fullscreen_changed = false;

        ui.input(|i| {
            if i.key_pressed(egui::Key::Num1) {
                self.compare_mode = CompareMode::SingleA;
                self.blink_state = false;
            }
            if has_b && i.key_pressed(egui::Key::Num2) {
                self.compare_mode = CompareMode::SingleB;
                self.blink_state = false;
            }
            if has_b && i.key_pressed(egui::Key::Space) {
                self.blink_state = !self.blink_state;
            }

            // Full-screen toggle (F11) and ESC-to-exit work in any mode.
            if i.key_pressed(egui::Key::F11) {
                self.fullscreen = !self.fullscreen;
                fullscreen_changed = true;
            }
            if self.fullscreen && i.key_pressed(egui::Key::Escape) {
                self.fullscreen = false;
                fullscreen_changed = true;
            }

            // Reset exposure (E) / gamma (Shift+G). Gamma deliberately uses
            // Shift+G because plain `G` isolates the green channel below.
            if i.key_pressed(egui::Key::E) {
                self.reset_exposure();
            }
            if i.modifiers.shift && i.key_pressed(egui::Key::G) {
                self.reset_gamma();
            }

            if !self.show_contact_sheet {
                if i.key_pressed(egui::Key::F) {
                    self.first_frame = true;
                }
                let prev_mode = self.channel_mode;
                if i.key_pressed(egui::Key::R) {
                    self.channel_mode = ChannelMode::R;
                }
                // Plain G only — Shift+G is the gamma reset handled above.
                if i.key_pressed(egui::Key::G) && !i.modifiers.shift {
                    self.channel_mode = ChannelMode::G;
                }
                if i.key_pressed(egui::Key::B) {
                    self.channel_mode = ChannelMode::B;
                }
                if i.key_pressed(egui::Key::A) {
                    self.channel_mode = ChannelMode::A;
                }
                if i.key_pressed(egui::Key::C) {
                    self.channel_mode = ChannelMode::RGB;
                }
                if self.channel_mode != prev_mode {
                    self.textures.fill(None);
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }
            }
        });

        if fullscreen_changed {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
    }

    /// Invalidate cached CPU display textures whose pixels depend on the
    /// exposure / gamma / sRGB tone pipeline, so they regenerate next frame.
    /// (The GPU path reads the live uniform each frame and needs no invalidation.)
    fn invalidate_tone(&mut self) {
        self.textures.fill(None);
        self.textures_b.fill(None);
        self.diff_texture = None;
        self.composite_texture = None;
    }

    fn reset_exposure(&mut self) {
        self.exposure = 0.0;
        self.invalidate_tone();
    }

    fn reset_gamma(&mut self) {
        self.gamma = 1.0;
        self.invalidate_tone();
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        exr_data: &ExrData,
        exr_data_b: Option<&ExrData>,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        self.handle_hotkeys(ui, exr_data_b.is_some());

        if self.blink_state && exr_data_b.is_some() {
            ui.ctx().request_repaint();
            let time = ui.input(|i| i.time);
            if ((time / self.blink_interval as f64) as usize).is_multiple_of(2) {
                self.compare_mode = CompareMode::SingleA;
            } else {
                self.compare_mode = CompareMode::SingleB;
            }
        }

        egui::Panel::top("viewer_controls").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if exr_data_b.is_some() {
                    ui.label("Compare:");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::SingleA, "A");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::SingleB, "B");

                    ui.add_enabled_ui(!self.show_contact_sheet, |ui| {
                        ui.selectable_value(&mut self.compare_mode, CompareMode::Wipe, "Wipe");
                    });
                    ui.selectable_value(
                        &mut self.compare_mode,
                        CompareMode::SideBySide,
                        "Side-by-Side",
                    );
                    ui.add_enabled_ui(!self.show_contact_sheet, |ui| {
                        ui.selectable_value(&mut self.compare_mode, CompareMode::DiffMatte, "Diff");
                        ui.selectable_value(&mut self.compare_mode, CompareMode::Composite, "Comp");
                    });
                    if ui
                        .toggle_value(&mut self.blink_state, "Blink (Spc)")
                        .clicked()
                        && !self.blink_state
                    {
                        self.compare_mode = CompareMode::SingleA;
                    }
                    if self.blink_state {
                        ui.add(egui::Slider::new(&mut self.blink_interval, 0.05..=5.0).suffix("s"));
                    }

                    if self.compare_mode == CompareMode::Wipe {
                        ui.add(egui::Slider::new(&mut self.wipe_position, 0.0..=1.0).text("Wipe"));
                    } else if self.compare_mode == CompareMode::DiffMatte {
                        ui.add(
                            egui::Slider::new(&mut self.diff_multiplier, 1.0..=100.0)
                                .text("Diff Multiplier"),
                        );
                    } else if self.compare_mode == CompareMode::SideBySide {
                        ui.checkbox(&mut self.normalize_side_by_side, "Normalize Size");
                    } else if self.compare_mode == CompareMode::Composite {
                        ui.label("Blend:");
                        egui::ComboBox::from_id_salt("blend_mode_select")
                            .selected_text(self.blend_mode.label())
                            .show_ui(ui, |ui| {
                                for mode in [
                                    BlendMode::Over,
                                    BlendMode::Under,
                                    BlendMode::Add,
                                    BlendMode::Multiply,
                                    BlendMode::Screen,
                                ] {
                                    ui.selectable_value(&mut self.blend_mode, mode, mode.label());
                                }
                            });
                    }
                    ui.separator();
                }

                // Right-clicking the label resets that control (Nuke-style).
                if ui
                    .add(egui::Label::new("Exposure:").sense(egui::Sense::click()))
                    .on_hover_text("Right-click to reset to 0.0 (key: E)")
                    .secondary_clicked()
                {
                    self.reset_exposure();
                }
                if ui
                    .add(egui::Slider::new(&mut self.exposure, -5.0..=5.0))
                    .changed()
                {
                    self.invalidate_tone();
                }

                ui.label("Overscan Opacity:");
                ui.add(egui::Slider::new(&mut self.overscan_opacity, 0.0..=1.0));
                if ui
                    .add(egui::Label::new("Gamma:").sense(egui::Sense::click()))
                    .on_hover_text("Right-click to reset to 1.0 (key: Shift+G)")
                    .secondary_clicked()
                {
                    self.reset_gamma();
                }
                if ui
                    .add(egui::Slider::new(&mut self.gamma, 0.1..=5.0))
                    .changed()
                {
                    self.invalidate_tone();
                }
                if ui
                    .button("⟲")
                    .on_hover_text("Reset exposure (0.0) & gamma (1.0)")
                    .clicked()
                {
                    self.reset_exposure();
                    self.reset_gamma();
                }
                if ui.checkbox(&mut self.srgb, "sRGB").changed() {
                    self.invalidate_tone();
                }

                ui.checkbox(&mut self.show_tooltip, "Show Pixel Tooltip");

                ui.separator();
                ui.label("Sample:");
                ui.selectable_value(&mut self.sample_aperture, 1, "1px");
                ui.selectable_value(&mut self.sample_aperture, 3, "3×3");
                ui.selectable_value(&mut self.sample_aperture, 9, "9×9");

                let layer_count = exr_data.logical_layers.len();
                if layer_count > 1
                    && ui
                        .toggle_value(&mut self.show_contact_sheet, "Contact Sheet")
                        .changed()
                    && self.show_contact_sheet
                    && (self.compare_mode == CompareMode::Wipe
                        || self.compare_mode == CompareMode::DiffMatte
                        || self.compare_mode == CompareMode::Composite)
                {
                    self.compare_mode = CompareMode::SideBySide;
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

                    // Layer (pass) selection
                    if layer_count > 1 {
                        ui.label("Layer:");
                        let selected_name = exr_data
                            .logical_layers
                            .get(self.active_layer)
                            .map(|l| l.name.as_str())
                            .unwrap_or("Unnamed");
                        egui::ComboBox::from_id_salt("layer_select")
                            .selected_text(selected_name)
                            .show_ui(ui, |ui| {
                                for (i, ll) in exr_data.logical_layers.iter().enumerate() {
                                    if ui
                                        .selectable_value(&mut self.active_layer, i, &ll.name)
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

        let layer_count = exr_data.logical_layers.len();
        if self.textures.len() != layer_count {
            self.textures.clear();
            self.textures.resize(layer_count, None);
            self.gpu_textures.clear();
            self.gpu_textures.resize(layer_count, None);
        }
        let layer_count_b = exr_data_b.map(|d| d.logical_layers.len()).unwrap_or(0);
        if self.textures_b.len() != layer_count_b {
            self.textures_b.clear();
            self.textures_b.resize(layer_count_b, None);
            self.gpu_textures_b.clear();
            self.gpu_textures_b.resize(layer_count_b, None);
        }

        if self.show_contact_sheet {
            let draw_sheet = |viewer: &mut ExrViewer,
                              ui: &mut egui::Ui,
                              data: &crate::exr_loader::ExrData,
                              is_a: bool| {
                let l_count = data.logical_layers.len();
                egui::ScrollArea::vertical()
                    .id_salt(if is_a { "sheet_a" } else { "sheet_b" })
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0);
                            for i in 0..l_count {
                                let tex_opt = if is_a {
                                    if viewer.textures[i].is_none() {
                                        viewer.textures[i] =
                                            viewer.generate_texture(ui.ctx(), data, i);
                                    }
                                    viewer.textures[i].as_ref()
                                } else {
                                    if viewer.textures_b[i].is_none() {
                                        viewer.textures_b[i] =
                                            viewer.generate_texture(ui.ctx(), data, i);
                                    }
                                    viewer.textures_b[i].as_ref()
                                };

                                if let Some(texture) = tex_opt {
                                    // Reserve an EXACTLY uniform cell, then position the image and
                                    // label by absolute geometry. Auto-layout (allocate_ui /
                                    // vertical_centered) let cell heights vary by a few px (inherited
                                    // item-spacing + variable label line count), and horizontal_wrapped
                                    // then center-aligned those unequal cells by different amounts —
                                    // producing the slight vertical "staircase". Fixed rects + paint_at
                                    // remove that degree of freedom entirely.
                                    let thumb_width = 256.0;
                                    let thumb_box = 256.0;
                                    let label_height = 30.0;
                                    let tex_size = texture.size_vec2();
                                    let aspect = if tex_size.y > 0.0 {
                                        tex_size.x / tex_size.y
                                    } else {
                                        1.0
                                    };
                                    let (fit_w, fit_h) = if aspect >= 1.0 {
                                        (thumb_box, thumb_box / aspect)
                                    } else {
                                        (thumb_box * aspect, thumb_box)
                                    };
                                    let name = data
                                        .logical_layers
                                        .get(i)
                                        .map(|l| l.name.as_str())
                                        .unwrap_or("Unnamed");

                                    let (cell_rect, response) = ui.allocate_exact_size(
                                        egui::vec2(thumb_width, thumb_box + label_height),
                                        egui::Sense::click(),
                                    );

                                    // Image: centered horizontally, centered within the top square box.
                                    let img_rect = egui::Rect::from_center_size(
                                        egui::pos2(
                                            cell_rect.center().x,
                                            cell_rect.top() + thumb_box * 0.5,
                                        ),
                                        egui::vec2(fit_w, fit_h),
                                    );
                                    egui::Image::new(texture).paint_at(ui, img_rect);

                                    // Label: centered in the strip beneath the box.
                                    ui.painter().text(
                                        egui::pos2(
                                            cell_rect.center().x,
                                            cell_rect.top() + thumb_box + label_height * 0.5,
                                        ),
                                        egui::Align2::CENTER_CENTER,
                                        format!("{}: {}", i, name),
                                        egui::FontId::proportional(14.0),
                                        ui.visuals().strong_text_color(),
                                    );

                                    if response.clicked() {
                                        viewer.active_layer = i;
                                        viewer.show_contact_sheet = false;
                                        viewer.first_frame = true;
                                        if !is_a {
                                            viewer.compare_mode = CompareMode::SingleB;
                                        } else if viewer.compare_mode == CompareMode::SingleB {
                                            viewer.compare_mode = CompareMode::SingleA;
                                        }
                                    }
                                    if response.hovered() {
                                        response
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .on_hover_text("Click to view layer");
                                    }
                                }
                            }
                        });
                    });
            };

            if let CompareMode::SideBySide | CompareMode::Wipe | CompareMode::DiffMatte =
                self.compare_mode
            {
                if let Some(exr_b) = exr_data_b {
                    ui.columns(2, |cols| {
                        cols[0].heading("Image A");
                        draw_sheet(self, &mut cols[0], exr_data, true);
                        cols[1].heading("Image B");
                        draw_sheet(self, &mut cols[1], exr_b, false);
                    });
                } else {
                    draw_sheet(self, ui, exr_data, true);
                }
            } else if self.compare_mode == CompareMode::SingleB {
                if let Some(exr_b) = exr_data_b {
                    draw_sheet(self, ui, exr_b, false);
                } else {
                    ui.label("Image B not loaded.");
                }
            } else {
                draw_sheet(self, ui, exr_data, true);
            }
        } else {
            // Channel/frame hotkeys are handled up-front in `handle_hotkeys`.
            let (tw, th) = exr_data.logical_size(self.active_layer).unwrap_or((1, 1));
            let tex_size = egui::vec2(tw as f32, th as f32);
            let mut tex_size_b = None;
            if let Some(data_b) = exr_data_b {
                let layer_b = self
                    .active_layer
                    .min(data_b.logical_layers.len().saturating_sub(1));
                if let Some((bw, bh)) = data_b.logical_size(layer_b) {
                    tex_size_b = Some(egui::vec2(bw as f32, bh as f32));
                }
            }

            if let Some(rs) = render_state {
                if self.gpu_textures[self.active_layer].is_none() {
                    self.gpu_textures[self.active_layer] =
                        self.generate_gpu_texture(rs, exr_data, self.active_layer);
                }
                if let Some(data_b) = exr_data_b {
                    let layer_b = self
                        .active_layer
                        .min(data_b.logical_layers.len().saturating_sub(1));
                    if self.gpu_textures_b[layer_b].is_none() {
                        self.gpu_textures_b[layer_b] =
                            self.generate_gpu_texture(rs, data_b, layer_b);
                    }
                }
            } else {
                if self.textures[self.active_layer].is_none() {
                    self.textures[self.active_layer] =
                        self.generate_texture(ui.ctx(), exr_data, self.active_layer);
                }
                if let Some(data_b) = exr_data_b {
                    let layer_b = self
                        .active_layer
                        .min(data_b.logical_layers.len().saturating_sub(1));
                    if self.textures_b[layer_b].is_none() {
                        self.textures_b[layer_b] = self.generate_texture(ui.ctx(), data_b, layer_b);
                    }
                }
                if let Some(exr_b) = exr_data_b
                    && self.compare_mode == CompareMode::DiffMatte
                    && (self.diff_texture.is_none()
                        || self.last_diff_params != (self.active_layer, self.diff_multiplier))
                {
                    let layer_b = self
                        .active_layer
                        .min(exr_b.logical_layers.len().saturating_sub(1));
                    self.diff_texture = self.generate_diff_texture(
                        ui.ctx(),
                        exr_data,
                        exr_b,
                        self.active_layer,
                        layer_b,
                    );
                    self.last_diff_params = (self.active_layer, self.diff_multiplier);
                }
                if let Some(exr_b) = exr_data_b
                    && self.compare_mode == CompareMode::Composite
                    && (self.composite_texture.is_none()
                        || self.last_composite_params != (self.active_layer, self.blend_mode))
                {
                    let layer_b = self
                        .active_layer
                        .min(exr_b.logical_layers.len().saturating_sub(1));
                    self.composite_texture = self.generate_composite_texture(
                        ui.ctx(),
                        exr_data,
                        exr_b,
                        self.active_layer,
                        layer_b,
                    );
                    self.last_composite_params = (self.active_layer, self.blend_mode);
                }
            }

            // Draw texture
            let has_texture = if render_state.is_some() {
                self.gpu_textures[self.active_layer].is_some()
            } else {
                self.textures[self.active_layer].is_some()
            };
            if has_texture {
                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());

                if self.first_frame {
                    let scale_x = rect.width() / tex_size.x;
                    let scale_y = rect.height() / tex_size.y;
                    self.scale = scale_x.min(scale_y).min(1.0); // Fit but don't scale up past 1.0 initially
                    self.translation = egui::Vec2::ZERO;
                    self.first_frame = false;
                }

                // Handle Zoom: pinch / ctrl+scroll via zoom_delta(), plus the plain
                // mouse wheel via smooth_scroll_delta (which zoom_delta() does NOT report).
                if response.hovered() {
                    let (zoom_delta, scroll_y) =
                        ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y));
                    let wheel_zoom = (scroll_y * 0.004).exp();
                    let total_zoom = zoom_delta * wheel_zoom;
                    if total_zoom != 1.0
                        && let Some(pos) = response.hover_pos()
                    {
                        // Zoom around the cursor
                        let offset = pos - rect.center() - self.translation;
                        self.translation -= offset * (total_zoom - 1.0);
                        self.scale = (self.scale * total_zoom).clamp(0.01, 100.0);
                    }
                }

                // Handle Panning
                if response.dragged() {
                    self.translation += response.drag_delta();
                }

                // Render Image
                let image_size = tex_size * self.scale;

                // Side-by-Side draws each image at its own offset position, so the
                // single centered overscan geometry below does not apply: skip the
                // overscan dimming pass and its annotations in that mode.
                let is_side_by_side = matches!(self.compare_mode, CompareMode::SideBySide);

                let disp_window = exr_data.image.attributes.display_window;
                let phys_idx = exr_data.logical_layers[self.active_layer].physical_index;
                let data_window_min = exr_data.image.layer_data[phys_idx]
                    .attributes
                    .layer_position;

                let disp_size =
                    egui::vec2(disp_window.size.x() as f32, disp_window.size.y() as f32)
                        * self.scale;
                let disp_rect = egui::Rect::from_min_size(
                    rect.center() + self.translation - disp_size / 2.0,
                    disp_size,
                );

                let data_offset = egui::vec2(
                    (data_window_min.0 - disp_window.position.x()) as f32,
                    (data_window_min.1 - disp_window.position.y()) as f32,
                ) * self.scale;

                let image_rect = egui::Rect::from_min_size(disp_rect.min + data_offset, image_size);

                let unclipped_painter = ui.painter().with_clip_rect(rect);
                let painter = ui.painter().with_clip_rect(rect.intersect(disp_rect));

                // Draw display window bounding box
                if !is_side_by_side {
                    draw_dashed_rect(
                        &unclipped_painter,
                        disp_rect,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 100),
                        5.0,
                        5.0,
                    );
                }

                // Labels for display window
                let is_overscanned = image_rect.min.x < disp_rect.min.x
                    || image_rect.min.y < disp_rect.min.y
                    || image_rect.max.x > disp_rect.max.x
                    || image_rect.max.y > disp_rect.max.y;
                let is_cropped = image_rect.min.x > disp_rect.min.x
                    || image_rect.min.y > disp_rect.min.y
                    || image_rect.max.x < disp_rect.max.x
                    || image_rect.max.y < disp_rect.max.y;

                if (is_overscanned || is_cropped) && !is_side_by_side {
                    unclipped_painter.text(
                        disp_rect.left_bottom() + egui::vec2(0.0, 5.0),
                        egui::Align2::LEFT_TOP,
                        format!("{},{}", disp_window.position.x(), disp_window.position.y()),
                        egui::FontId::proportional(12.0),
                        egui::Color32::GRAY,
                    );
                    let top_right_x = disp_window.position.x() + disp_window.size.x() as i32;
                    let top_right_y = disp_window.position.y() + disp_window.size.y() as i32;
                    unclipped_painter.text(
                        disp_rect.right_top() - egui::vec2(0.0, 5.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("{},{}", top_right_x, top_right_y),
                        egui::FontId::proportional(12.0),
                        egui::Color32::GRAY,
                    );
                }

                if let Some(_rs) = render_state {
                    // GPU RENDER PATH
                    use eframe::egui_wgpu::wgpu::util::DeviceExt;
                    let uniform_data = crate::gpu::Uniforms {
                        rect_min: [image_rect.min.x, image_rect.min.y],
                        rect_max: [image_rect.max.x, image_rect.max.y],
                        screen_size: [
                            ui.ctx().content_rect().width(),
                            ui.ctx().content_rect().height(),
                        ],
                        exposure: self.exposure,
                        gamma: self.gamma,
                        diff_multiplier: self.diff_multiplier,
                        channel_mode: self.channel_mode.as_u32(),
                        is_diff_mode: 0,
                        srgb: if self.srgb { 1 } else { 0 },
                        enable_lut: if self.enable_lut { 1 } else { 0 },
                        opacity: 1.0,
                        is_composite: 0,
                        blend_mode: self.blend_mode.as_u32(),
                    };

                    let default_lut = render_state
                        .as_ref()
                        .unwrap()
                        .renderer
                        .read()
                        .callback_resources
                        .get::<crate::gpu::GpuState>()
                        .unwrap()
                        .default_lut_bind_group
                        .clone();
                    let active_lut_bg = lut_bg_opt.clone().unwrap_or(default_lut);
                    let draw_gpu =
                        |painter: &egui::Painter,
                         bg_a: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
                         bg_b_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
                         clip_rect: egui::Rect,
                         target_rect: egui::Rect,
                         is_diff: bool,
                         is_composite: bool,
                         opacity: f32| {
                            let mut u = uniform_data;
                            u.rect_min = [target_rect.min.x, target_rect.min.y];
                            u.rect_max = [target_rect.max.x, target_rect.max.y];
                            u.is_diff_mode = if is_diff { 1 } else { 0 };
                            u.is_composite = if is_composite { 1 } else { 0 };
                            u.opacity = opacity;

                            let device = &render_state.as_ref().unwrap().device;
                            let renderer_guard = render_state.as_ref().unwrap().renderer.read();
                            let gpu_state = renderer_guard
                                .callback_resources
                                .get::<crate::gpu::GpuState>()
                                .unwrap();

                            let uniform_buffer = device.create_buffer_init(
                                &eframe::egui_wgpu::wgpu::util::BufferInitDescriptor {
                                    label: Some("Exr Uniform Buffer"),
                                    contents: bytemuck::bytes_of(&u),
                                    usage: eframe::egui_wgpu::wgpu::BufferUsages::UNIFORM
                                        | eframe::egui_wgpu::wgpu::BufferUsages::COPY_DST,
                                },
                            );

                            let uniform_bg = device.create_bind_group(
                                &eframe::egui_wgpu::wgpu::BindGroupDescriptor {
                                    label: Some("Exr Uniform Bind Group"),
                                    layout: &gpu_state.bind_group_layout_uniform,
                                    entries: &[eframe::egui_wgpu::wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: uniform_buffer.as_entire_binding(),
                                    }],
                                },
                            );

                            let bg_b = bg_b_opt
                                .unwrap_or_else(|| gpu_state.default_tex_bind_group.clone());

                            let callback = crate::gpu::ExrCallback {
                                bg_a,
                                bg_b,
                                uniform_bg,
                                lut_bg: active_lut_bg.clone(),
                            };

                            let final_clip_rect = painter.clip_rect().intersect(clip_rect);
                            painter.with_clip_rect(final_clip_rect).add(
                                eframe::egui_wgpu::Callback::new_paint_callback(
                                    final_clip_rect,
                                    callback,
                                ),
                            );
                        };

                    let bg_a_opt = self.gpu_textures[self.active_layer].clone();
                    if let Some(bg_a) = bg_a_opt {
                        let comp_mode = if self.blink_state {
                            if ((ui.input(|i| i.time) / self.blink_interval as f64) as usize)
                                .is_multiple_of(2)
                            {
                                CompareMode::SingleA
                            } else {
                                CompareMode::SingleB
                            }
                        } else {
                            self.compare_mode
                        };
                        let draw_all = |p: &egui::Painter, opac: f32| match comp_mode {
                            CompareMode::SingleA => {
                                draw_gpu(
                                    p,
                                    bg_a.clone(),
                                    None,
                                    rect,
                                    image_rect,
                                    false,
                                    false,
                                    opac,
                                );
                            }
                            CompareMode::SingleB => {
                                if let Some(bg_b) = exr_data_b.and_then(|d| {
                                    self.gpu_textures_b[self
                                        .active_layer
                                        .min(d.logical_layers.len().saturating_sub(1))]
                                    .clone()
                                }) {
                                    draw_gpu(
                                        p,
                                        bg_b.clone(),
                                        None,
                                        rect,
                                        image_rect,
                                        false,
                                        false,
                                        opac,
                                    );
                                }
                            }
                            CompareMode::Wipe => {
                                let wipe_x =
                                    image_rect.min.x + image_rect.width() * self.wipe_position;
                                let clamped_wipe_x = wipe_x.clamp(rect.min.x, rect.max.x);
                                let mut rect_a = rect;
                                rect_a.max.x = clamped_wipe_x;
                                let mut rect_b = rect;
                                rect_b.min.x = clamped_wipe_x;

                                draw_gpu(
                                    p,
                                    bg_a.clone(),
                                    None,
                                    rect_a,
                                    image_rect,
                                    false,
                                    false,
                                    opac,
                                );
                                if let Some(bg_b) = exr_data_b.and_then(|d| {
                                    self.gpu_textures_b[self
                                        .active_layer
                                        .min(d.logical_layers.len().saturating_sub(1))]
                                    .clone()
                                }) {
                                    draw_gpu(p, bg_b, None, rect_b, image_rect, false, false, opac);
                                }
                                painter.line_segment(
                                    [
                                        egui::pos2(wipe_x, rect.min.y),
                                        egui::pos2(wipe_x, rect.max.y),
                                    ],
                                    (2.0, egui::Color32::WHITE),
                                );
                            }
                            CompareMode::SideBySide => {
                                let bg_b_opt = exr_data_b.and_then(|d| {
                                    self.gpu_textures_b[self
                                        .active_layer
                                        .min(d.logical_layers.len().saturating_sub(1))]
                                    .clone()
                                });
                                if let Some(bg_b) = bg_b_opt {
                                    let mut image_size_b = tex_size_b.unwrap() * self.scale;
                                    if self.normalize_side_by_side {
                                        let scale_b =
                                            (tex_size.y * self.scale) / tex_size_b.unwrap().y;
                                        image_size_b = tex_size_b.unwrap() * scale_b;
                                    }
                                    let combined_width = image_size.x + image_size_b.x;
                                    let combined_height = image_size.y.max(image_size_b.y);
                                    let combined_rect = egui::Rect::from_center_size(
                                        rect.center() + self.translation,
                                        egui::vec2(combined_width, combined_height),
                                    );
                                    let mut image_rect_a =
                                        egui::Rect::from_min_size(combined_rect.min, image_size);
                                    image_rect_a.set_center(egui::pos2(
                                        image_rect_a.center().x,
                                        combined_rect.center().y,
                                    ));
                                    let mut image_rect_b = egui::Rect::from_min_size(
                                        egui::pos2(
                                            combined_rect.min.x + image_size.x,
                                            combined_rect.min.y,
                                        ),
                                        image_size_b,
                                    );
                                    image_rect_b.set_center(egui::pos2(
                                        image_rect_b.center().x,
                                        combined_rect.center().y,
                                    ));

                                    draw_gpu(
                                        p,
                                        bg_a.clone(),
                                        None,
                                        rect,
                                        image_rect_a,
                                        false,
                                        false,
                                        opac,
                                    );
                                    draw_gpu(
                                        p,
                                        bg_b.clone(),
                                        None,
                                        rect,
                                        image_rect_b,
                                        false,
                                        false,
                                        opac,
                                    );
                                    p.line_segment(
                                        [
                                            egui::pos2(image_rect_b.min.x, combined_rect.min.y),
                                            egui::pos2(image_rect_b.min.x, combined_rect.max.y),
                                        ],
                                        (2.0, egui::Color32::GRAY),
                                    );
                                } else {
                                    draw_gpu(
                                        p,
                                        bg_a.clone(),
                                        None,
                                        rect,
                                        image_rect,
                                        false,
                                        false,
                                        opac,
                                    );
                                }
                            }
                            CompareMode::DiffMatte => {
                                let bg_b_opt = exr_data_b.and_then(|d| {
                                    self.gpu_textures_b[self
                                        .active_layer
                                        .min(d.logical_layers.len().saturating_sub(1))]
                                    .clone()
                                });
                                if let Some(bg_b) = bg_b_opt {
                                    draw_gpu(
                                        p,
                                        bg_a.clone(),
                                        Some(bg_b.clone()),
                                        rect,
                                        image_rect,
                                        true,
                                        false,
                                        opac,
                                    );
                                }
                            }
                            CompareMode::Composite => {
                                let bg_b_opt = exr_data_b.and_then(|d| {
                                    self.gpu_textures_b[self
                                        .active_layer
                                        .min(d.logical_layers.len().saturating_sub(1))]
                                    .clone()
                                });
                                if let Some(bg_b) = bg_b_opt {
                                    draw_gpu(
                                        p,
                                        bg_a.clone(),
                                        Some(bg_b.clone()),
                                        rect,
                                        image_rect,
                                        false,
                                        true,
                                        opac,
                                    );
                                }
                            }
                        };

                        let is_sbs = matches!(comp_mode, CompareMode::SideBySide);
                        if self.overscan_opacity > 0.0 && !is_sbs {
                            draw_all(&unclipped_painter, self.overscan_opacity);
                        }
                        // Side-by-Side renders at full brightness with the full-canvas
                        // clip (no display-window clip), so overscan dimming is skipped.
                        draw_all(if is_sbs { &unclipped_painter } else { &painter }, 1.0);
                    }
                } else {
                    let texture = &self.textures[self.active_layer];
                    let draw_image = |painter: &egui::Painter,
                                      tex: &egui::TextureHandle,
                                      clip_rect: egui::Rect,
                                      target_rect: egui::Rect,
                                      opacity: f32| {
                        let alpha = opacity;
                        let final_clip_rect = painter.clip_rect().intersect(clip_rect);
                        painter.with_clip_rect(final_clip_rect).image(
                            tex.id(),
                            target_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::from_white_alpha((alpha * 255.0) as u8),
                        );
                    };

                    let draw_all_cpu = |p: &egui::Painter, opac: f32| match self.compare_mode {
                        CompareMode::SingleA => {
                            draw_image(p, texture.as_ref().unwrap(), rect, image_rect, opac);
                        }
                        CompareMode::SingleB => {
                            if let Some(tex_b) = exr_data_b.and_then(|d| {
                                self.textures_b[self
                                    .active_layer
                                    .min(d.logical_layers.len().saturating_sub(1))]
                                .as_ref()
                            }) {
                                draw_image(p, tex_b, rect, image_rect, opac);
                            }
                        }
                        CompareMode::Wipe => {
                            let wipe_x = image_rect.min.x + image_rect.width() * self.wipe_position;
                            let clamped_wipe_x = wipe_x.clamp(rect.min.x, rect.max.x);
                            let mut rect_a = rect;
                            rect_a.max.x = clamped_wipe_x;
                            let mut rect_b = rect;
                            rect_b.min.x = clamped_wipe_x;

                            draw_image(p, texture.as_ref().unwrap(), rect_a, image_rect, opac);
                            if let Some(tex_b) = exr_data_b.and_then(|d| {
                                self.textures_b[self
                                    .active_layer
                                    .min(d.logical_layers.len().saturating_sub(1))]
                                .as_ref()
                            }) {
                                draw_image(p, tex_b, rect_b, image_rect, opac);
                            }

                            painter.line_segment(
                                [
                                    egui::pos2(wipe_x, rect.min.y),
                                    egui::pos2(wipe_x, rect.max.y),
                                ],
                                (2.0, egui::Color32::WHITE),
                            );
                        }
                        CompareMode::SideBySide => {
                            let tex_b_opt = exr_data_b.and_then(|d| {
                                self.textures_b[self
                                    .active_layer
                                    .min(d.logical_layers.len().saturating_sub(1))]
                                .as_ref()
                            });
                            if let Some(tex_b) = tex_b_opt {
                                let mut image_size_b = tex_size_b.unwrap() * self.scale;
                                if self.normalize_side_by_side {
                                    let scale_b = (tex_size.y * self.scale) / tex_size_b.unwrap().y;
                                    image_size_b = tex_size_b.unwrap() * scale_b;
                                }
                                let combined_width = image_size.x + image_size_b.x;
                                let combined_height = image_size.y.max(image_size_b.y);

                                let combined_rect = egui::Rect::from_center_size(
                                    rect.center() + self.translation,
                                    egui::vec2(combined_width, combined_height),
                                );

                                let mut image_rect_a =
                                    egui::Rect::from_min_size(combined_rect.min, image_size);
                                image_rect_a.set_center(egui::pos2(
                                    image_rect_a.center().x,
                                    combined_rect.center().y,
                                ));

                                let mut image_rect_b = egui::Rect::from_min_size(
                                    egui::pos2(
                                        combined_rect.min.x + image_size.x,
                                        combined_rect.min.y,
                                    ),
                                    image_size_b,
                                );
                                image_rect_b.set_center(egui::pos2(
                                    image_rect_b.center().x,
                                    combined_rect.center().y,
                                ));

                                draw_image(p, texture.as_ref().unwrap(), rect, image_rect_a, opac);
                                draw_image(p, tex_b, rect, image_rect_b, opac);

                                p.line_segment(
                                    [
                                        egui::pos2(image_rect_b.min.x, combined_rect.min.y),
                                        egui::pos2(image_rect_b.min.x, combined_rect.max.y),
                                    ],
                                    (2.0, egui::Color32::GRAY),
                                );
                            } else {
                                draw_image(p, texture.as_ref().unwrap(), rect, image_rect, opac);
                            }
                        }
                        CompareMode::DiffMatte => {
                            if let Some(diff) = &self.diff_texture {
                                draw_image(p, diff, rect, image_rect, opac);
                            }
                        }
                        CompareMode::Composite => {
                            if let Some(comp) = &self.composite_texture {
                                draw_image(p, comp, rect, image_rect, opac);
                            }
                        }
                    };

                    if self.overscan_opacity > 0.0 && !is_side_by_side {
                        draw_all_cpu(&unclipped_painter, self.overscan_opacity);
                    }
                    // Side-by-Side renders at full brightness with the full-canvas clip.
                    draw_all_cpu(
                        if is_side_by_side {
                            &unclipped_painter
                        } else {
                            &painter
                        },
                        1.0,
                    );
                }

                // Draw data window bounding box over the image
                if (is_overscanned || is_cropped) && !is_side_by_side {
                    draw_dashed_rect(
                        &unclipped_painter,
                        image_rect,
                        egui::Color32::from_rgba_unmultiplied(255, 200, 100, 180),
                        4.0,
                        4.0,
                    );

                    unclipped_painter.text(
                        image_rect.right_bottom() + egui::vec2(5.0, 5.0),
                        egui::Align2::LEFT_TOP,
                        format!(
                            "Overscan: {}x{} (pos: {}, {})",
                            tex_size.x, tex_size.y, data_window_min.0, data_window_min.1
                        ),
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_rgb(255, 200, 100),
                    );
                }

                // Pixel Sampling & Swatches
                let mut hovered_pixel = None;
                if let Some(pos) = response.hover_pos() {
                    let mut hover_x = None;
                    let mut hover_y = None;
                    let mut hovered_b = false;

                    if self.compare_mode == CompareMode::SideBySide && exr_data_b.is_some() {
                        let tex_b_opt = exr_data_b.and_then(|d| {
                            self.textures_b[self
                                .active_layer
                                .min(d.logical_layers.len().saturating_sub(1))]
                            .as_ref()
                        });
                        if tex_b_opt.is_some() {
                            let mut image_size_b = tex_size_b.unwrap() * self.scale;
                            if self.normalize_side_by_side {
                                let scale_b = (tex_size.y * self.scale) / tex_size_b.unwrap().y;
                                image_size_b = tex_size_b.unwrap() * scale_b;
                            }
                            let combined_width = image_size.x + image_size_b.x;
                            let combined_height = image_size.y.max(image_size_b.y);

                            let combined_rect = egui::Rect::from_center_size(
                                rect.center() + self.translation,
                                egui::vec2(combined_width, combined_height),
                            );

                            let mut image_rect_a =
                                egui::Rect::from_min_size(combined_rect.min, image_size);
                            image_rect_a.set_center(egui::pos2(
                                image_rect_a.center().x,
                                combined_rect.center().y,
                            ));

                            let mut image_rect_b = egui::Rect::from_min_size(
                                egui::pos2(combined_rect.min.x + image_size.x, combined_rect.min.y),
                                image_size_b,
                            );
                            image_rect_b.set_center(egui::pos2(
                                image_rect_b.center().x,
                                combined_rect.center().y,
                            ));

                            if image_rect_a.contains(pos) {
                                let local = pos - image_rect_a.min;
                                hover_x = Some((local.x / self.scale) as usize);
                                hover_y = Some((local.y / self.scale) as usize);
                            } else if image_rect_b.contains(pos) {
                                let local = pos - image_rect_b.min;
                                let scale_b = if self.normalize_side_by_side {
                                    (tex_size.y * self.scale) / tex_size_b.unwrap().y
                                } else {
                                    self.scale
                                };
                                hover_x = Some((local.x / scale_b) as usize);
                                hover_y = Some((local.y / scale_b) as usize);
                                hovered_b = true;
                            }
                        }
                    } else {
                        let image_local_pos = pos - image_rect.min;
                        if image_local_pos.x >= 0.0 && image_local_pos.y >= 0.0 {
                            hover_x = Some((image_local_pos.x / self.scale) as usize);
                            hover_y = Some((image_local_pos.y / self.scale) as usize);
                        }
                    }
                    let mut val_a_opt = None;
                    let mut val_b_opt = None;
                    let mut x_final = None;
                    let mut y_final = None;

                    if let (Some(x), Some(y)) = (hover_x, hover_y) {
                        // Check if within bounds of the hovered image
                        let mut valid = false;
                        if hovered_b {
                            if let Some(s) = tex_size_b
                                && x < s.x as usize
                                && y < s.y as usize
                            {
                                valid = true;
                            }
                        } else if x < tex_size.x as usize && y < tex_size.y as usize {
                            valid = true;
                        }

                        if valid {
                            hovered_pixel = Some((x, y));
                            x_final = Some(x);
                            y_final = Some(y);
                            val_a_opt = self.sample_pixel(exr_data, self.active_layer, x, y);
                            val_b_opt = if let Some(exr_b) = exr_data_b {
                                let layer_b = self
                                    .active_layer
                                    .min(exr_b.logical_layers.len().saturating_sub(1));
                                self.sample_pixel(exr_b, layer_b, x, y)
                            } else {
                                None
                            };

                            self.last_hover_pos_img = Some((x, y));
                            self.last_sampled_val_a = val_a_opt;
                            self.last_sampled_val_b = val_b_opt;
                        }
                    }

                    if self.show_tooltip
                        && (val_a_opt.is_some() || val_b_opt.is_some())
                        && let (Some(x), Some(y)) = (x_final, y_final)
                    {
                        egui::Window::new("Pixel Tooltip")
                            .fixed_pos(pos + egui::vec2(15.0, 15.0))
                            .title_bar(false)
                            .resizable(false)
                            .collapsible(false)
                            .show(ui.ctx(), |ui| {
                                ui.label(format!("x={} y={}", x, y));

                                if let Some(val_a) = val_a_opt {
                                    ui.horizontal(|ui| {
                                        colored_rgba_label(
                                            ui,
                                            if val_b_opt.is_some() { "A:" } else { "" },
                                            val_a,
                                        );
                                        let (r, g, b) = (
                                            (val_a[0].clamp(0.0, 1.0) * 255.0) as u8,
                                            (val_a[1].clamp(0.0, 1.0) * 255.0) as u8,
                                            (val_a[2].clamp(0.0, 1.0) * 255.0) as u8,
                                        );
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(16.0, 16.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            0.0,
                                            egui::Color32::from_rgb(r, g, b),
                                        );
                                    });
                                    let (h, s, v, l) = rgb_to_hsvl(val_a[0], val_a[1], val_a[2]);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "H:{:.0} S:{:.2} V:{:.2} L:{:.5}",
                                            h, s, v, l
                                        ))
                                        .color(egui::Color32::LIGHT_GRAY),
                                    );
                                }

                                if let Some(val_b) = val_b_opt {
                                    ui.horizontal(|ui| {
                                        colored_rgba_label(ui, "B:", val_b);
                                        let (r, g, b) = (
                                            (val_b[0].clamp(0.0, 1.0) * 255.0) as u8,
                                            (val_b[1].clamp(0.0, 1.0) * 255.0) as u8,
                                            (val_b[2].clamp(0.0, 1.0) * 255.0) as u8,
                                        );
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(16.0, 16.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            0.0,
                                            egui::Color32::from_rgb(r, g, b),
                                        );
                                    });
                                    let (h, s, v, l) = rgb_to_hsvl(val_b[0], val_b[1], val_b[2]);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "H:{:.0} S:{:.2} V:{:.2} L:{:.5}",
                                            h, s, v, l
                                        ))
                                        .color(egui::Color32::LIGHT_GRAY),
                                    );
                                }

                                if let (Some(val_a), Some(val_b)) = (val_a_opt, val_b_opt) {
                                    let diff = [
                                        (val_b[0] - val_a[0]).abs(),
                                        (val_b[1] - val_a[1]).abs(),
                                        (val_b[2] - val_a[2]).abs(),
                                        (val_b[3] - val_a[3]).abs(),
                                    ];
                                    colored_rgba_label(ui, "Diff:", diff);
                                }
                            });

                        // Shift+Click to add a persistent swatch
                        if ui.input(|i| i.modifiers.shift)
                            && response.clicked()
                            && let Some(v) = val_a_opt.or(val_b_opt)
                        {
                            self.swatches.push(v);
                        }
                    }
                }

                if hovered_pixel.is_none() {
                    self.last_hover_pos_img = None;
                    self.last_sampled_val_a = None;
                    self.last_sampled_val_b = None;
                }
            }
        }
    }

    fn generate_gpu_texture(
        &self,
        render_state: &eframe::egui_wgpu::RenderState,
        exr_data: &ExrData,
        layer_index: usize,
    ) -> Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        // Pack into Rgba32Float
        let mut pixels = vec![0.0f32; width * height * 4];

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

        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                pixels[i] = get_val(r_chan, x, y);
                pixels[i + 1] = get_val(g_chan, x, y);
                pixels[i + 2] = get_val(b_chan, x, y);
                pixels[i + 3] = if a_chan.is_some() {
                    get_val(a_chan, x, y)
                } else {
                    1.0
                };
            }
        }

        let device = &render_state.device;
        let queue = &render_state.queue;

        let texture = device.create_texture(&eframe::egui_wgpu::wgpu::TextureDescriptor {
            label: Some("Exr GPU Texture"),
            size: eframe::egui_wgpu::wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: eframe::egui_wgpu::wgpu::TextureDimension::D2,
            format: eframe::egui_wgpu::wgpu::TextureFormat::Rgba32Float,
            usage: eframe::egui_wgpu::wgpu::TextureUsages::TEXTURE_BINDING
                | eframe::egui_wgpu::wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            eframe::egui_wgpu::wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: eframe::egui_wgpu::wgpu::Origin3d::ZERO,
                aspect: eframe::egui_wgpu::wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&pixels),
            eframe::egui_wgpu::wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((width * 4 * 4) as u32),
                rows_per_image: Some(height as u32),
            },
            eframe::egui_wgpu::wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&eframe::egui_wgpu::wgpu::TextureViewDescriptor::default());

        let renderer_read = render_state.renderer.read();
        let gpu_state = renderer_read
            .callback_resources
            .get::<crate::gpu::GpuState>()
            .unwrap();

        let bind_group = device.create_bind_group(&eframe::egui_wgpu::wgpu::BindGroupDescriptor {
            label: Some("Exr Texture Bind Group"),
            layout: &gpu_state.bind_group_layout_tex,
            entries: &[
                eframe::egui_wgpu::wgpu::BindGroupEntry {
                    binding: 0,
                    resource: eframe::egui_wgpu::wgpu::BindingResource::TextureView(&view),
                },
                eframe::egui_wgpu::wgpu::BindGroupEntry {
                    binding: 1,
                    resource: eframe::egui_wgpu::wgpu::BindingResource::Sampler(&gpu_state.sampler),
                },
            ],
        });

        Some(std::sync::Arc::new(bind_group))
    }

    fn generate_texture(
        &self,
        ctx: &egui::Context,
        exr_data: &ExrData,
        layer_index: usize,
    ) -> Option<egui::TextureHandle> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        let mut pixels = vec![egui::Color32::BLACK; width * height];

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

        // Hoist all loop-invariant scalars out of the per-pixel work.
        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let channel_mode = self.channel_mode;

        // Process rows in parallel; each row is an independent, contiguous slice.
        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let mut r = get_val(r_chan, x, y);
                    let mut g = get_val(g_chan, x, y);
                    let mut b = get_val(b_chan, x, y);
                    let mut a = get_val(a_chan, x, y);

                    if a_chan.is_none() {
                        a = 1.0;
                    }

                    match channel_mode {
                        ChannelMode::R => {
                            g = r;
                            b = r;
                            a = 1.0;
                        }
                        ChannelMode::G => {
                            r = g;
                            b = g;
                            a = 1.0;
                        }
                        ChannelMode::B => {
                            r = b;
                            g = b;
                            a = 1.0;
                        }
                        ChannelMode::A => {
                            r = a;
                            g = a;
                            b = a;
                            a = 1.0;
                        }
                        ChannelMode::RGB => {}
                    }

                    let is_dark = ((x / 16) + (y / 16)) % 2 == 0;
                    let bg_linear = if is_dark { 0.1 } else { 0.2 };

                    // Apply exposure
                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    // Composite over checkerboard (assuming EXR is pre-multiplied)
                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg_linear * (1.0 - a_clamp);
                    g += bg_linear * (1.0 - a_clamp);
                    b += bg_linear * (1.0 - a_clamp);

                    if apply_gamma {
                        r = crate::render_math::apply_gamma(r, gamma);
                        g = crate::render_math::apply_gamma(g, gamma);
                        b = crate::render_math::apply_gamma(b, gamma);
                    }

                    if apply_srgb {
                        r = Self::linear_to_srgb(r);
                        g = Self::linear_to_srgb(g);
                        b = Self::linear_to_srgb(b);
                    }

                    let r_u8 = (r.clamp(0.0, 1.0) * 255.0) as u8;
                    let g_u8 = (g.clamp(0.0, 1.0) * 255.0) as u8;
                    let b_u8 = (b.clamp(0.0, 1.0) * 255.0) as u8;

                    *px = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer", color_image, egui::TextureOptions::LINEAR))
    }

    fn generate_diff_texture(
        &self,
        ctx: &egui::Context,
        data_a: &ExrData,
        data_b: &ExrData,
        layer_a_idx: usize,
        layer_b_idx: usize,
    ) -> Option<egui::TextureHandle> {
        let (layer_a, r_chan_a, g_chan_a, b_chan_a, _) = data_a.logical_channels(layer_a_idx)?;
        let (layer_b, r_chan_b, g_chan_b, b_chan_b, _) = data_b.logical_channels(layer_b_idx)?;

        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);

        let mut pixels = vec![egui::Color32::BLACK; width * height];

        let get_val = |chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
                       x: usize,
                       y: usize,
                       w: usize,
                       h: usize|
         -> f32 {
            if x >= w || y >= h {
                return 0.0;
            }
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

        // Hoist all loop-invariant scalars out of the per-pixel work.
        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let diff_multiplier = self.diff_multiplier;
        let (aw, ah) = (layer_a.size.0, layer_a.size.1);
        let (bw, bh) = (layer_b.size.0, layer_b.size.1);

        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let r_a = get_val(r_chan_a, x, y, aw, ah);
                    let g_a = get_val(g_chan_a, x, y, aw, ah);
                    let b_a = get_val(b_chan_a, x, y, aw, ah);

                    let r_b = get_val(r_chan_b, x, y, bw, bh);
                    let g_b = get_val(g_chan_b, x, y, bw, bh);
                    let b_b = get_val(b_chan_b, x, y, bw, bh);

                    // Difference calculation
                    let mut diff_r = (r_a - r_b).abs() * diff_multiplier;
                    let mut diff_g = (g_a - g_b).abs() * diff_multiplier;
                    let mut diff_b = (b_a - b_b).abs() * diff_multiplier;

                    // Tone mapping logic for the diff to be visible
                    diff_r *= exp_mult;
                    diff_g *= exp_mult;
                    diff_b *= exp_mult;

                    if apply_gamma {
                        diff_r = crate::render_math::apply_gamma(diff_r, gamma);
                        diff_g = crate::render_math::apply_gamma(diff_g, gamma);
                        diff_b = crate::render_math::apply_gamma(diff_b, gamma);
                    }

                    if apply_srgb {
                        diff_r = Self::linear_to_srgb(diff_r);
                        diff_g = Self::linear_to_srgb(diff_g);
                        diff_b = Self::linear_to_srgb(diff_b);
                    }

                    let r_u8 = (diff_r.clamp(0.0, 1.0) * 255.0) as u8;
                    let g_u8 = (diff_g.clamp(0.0, 1.0) * 255.0) as u8;
                    let b_u8 = (diff_b.clamp(0.0, 1.0) * 255.0) as u8;

                    *px = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer_diff", color_image, egui::TextureOptions::LINEAR))
    }

    /// CPU-fallback parity for [`CompareMode::Composite`]. Blends A and B in
    /// linear space (premultiplied-alpha aware) per [`BlendMode`], then runs the
    /// same exposure → checkerboard → gamma → sRGB tone pipeline as
    /// [`Self::generate_texture`]. Like the CPU diff path it ignores per-channel
    /// isolation — the GPU path (the default) applies that after the blend.
    fn generate_composite_texture(
        &self,
        ctx: &egui::Context,
        data_a: &ExrData,
        data_b: &ExrData,
        layer_a_idx: usize,
        layer_b_idx: usize,
    ) -> Option<egui::TextureHandle> {
        let (layer_a, r_chan_a, g_chan_a, b_chan_a, a_chan_a) =
            data_a.logical_channels(layer_a_idx)?;
        let (layer_b, r_chan_b, g_chan_b, b_chan_b, a_chan_b) =
            data_b.logical_channels(layer_b_idx)?;

        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);

        let mut pixels = vec![egui::Color32::BLACK; width * height];

        let get_val = |chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
                       x: usize,
                       y: usize,
                       w: usize,
                       h: usize|
         -> f32 {
            if x >= w || y >= h {
                return 0.0;
            }
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

        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let blend_mode = self.blend_mode;
        let (aw, ah) = (layer_a.size.0, layer_a.size.1);
        let (bw, bh) = (layer_b.size.0, layer_b.size.1);

        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let ar = get_val(r_chan_a, x, y, aw, ah);
                    let ag = get_val(g_chan_a, x, y, aw, ah);
                    let ab = get_val(b_chan_a, x, y, aw, ah);
                    let aa = if a_chan_a.is_some() {
                        get_val(a_chan_a, x, y, aw, ah)
                    } else {
                        1.0
                    };

                    let br = get_val(r_chan_b, x, y, bw, bh);
                    let bg = get_val(g_chan_b, x, y, bw, bh);
                    let bb = get_val(b_chan_b, x, y, bw, bh);
                    let ba = if a_chan_b.is_some() {
                        get_val(a_chan_b, x, y, bw, bh)
                    } else {
                        1.0
                    };

                    // Premultiplied-alpha blends; keep in lockstep with the
                    // `blend_mode` switch in gpu/shader.wgsl.
                    let (mut r, mut g, mut b, a) = match blend_mode {
                        BlendMode::Over => (
                            ar + br * (1.0 - aa),
                            ag + bg * (1.0 - aa),
                            ab + bb * (1.0 - aa),
                            aa + ba * (1.0 - aa),
                        ),
                        BlendMode::Under => (
                            br + ar * (1.0 - ba),
                            bg + ag * (1.0 - ba),
                            bb + ab * (1.0 - ba),
                            ba + aa * (1.0 - ba),
                        ),
                        BlendMode::Add => (ar + br, ag + bg, ab + bb, (aa + ba).min(1.0)),
                        BlendMode::Multiply => (ar * br, ag * bg, ab * bb, aa),
                        BlendMode::Screen => (
                            ar + br - ar * br,
                            ag + bg - ag * bg,
                            ab + bb - ab * bb,
                            aa + ba - aa * ba,
                        ),
                    };

                    let is_dark = ((x / 16) + (y / 16)) % 2 == 0;
                    let bg_linear = if is_dark { 0.1 } else { 0.2 };

                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg_linear * (1.0 - a_clamp);
                    g += bg_linear * (1.0 - a_clamp);
                    b += bg_linear * (1.0 - a_clamp);

                    if apply_gamma {
                        r = crate::render_math::apply_gamma(r, gamma);
                        g = crate::render_math::apply_gamma(g, gamma);
                        b = crate::render_math::apply_gamma(b, gamma);
                    }

                    if apply_srgb {
                        r = Self::linear_to_srgb(r);
                        g = Self::linear_to_srgb(g);
                        b = Self::linear_to_srgb(b);
                    }

                    let r_u8 = (r.clamp(0.0, 1.0) * 255.0) as u8;
                    let g_u8 = (g.clamp(0.0, 1.0) * 255.0) as u8;
                    let b_u8 = (b.clamp(0.0, 1.0) * 255.0) as u8;

                    *px = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture(
            "exr_viewer_composite",
            color_image,
            egui::TextureOptions::LINEAR,
        ))
    }

    fn sample_pixel(
        &self,
        exr_data: &ExrData,
        layer_index: usize,
        x: usize,
        y: usize,
    ) -> Option<[f32; 4]> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        if x >= width || y >= height {
            return None;
        }

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

        // Aperture averaging: 1 (single pixel), 3 (3×3) or 9 (9×9). The window is
        // centered on (x, y) with edge-clamped coordinates so it stays valid at
        // the image border (replicate edge), and the average is over every sample
        // in the window.
        let radius = (self.sample_aperture / 2) as isize;
        let mut sum = [0.0f32; 4];
        let mut count = 0.0f32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let sx = (x as isize + dx).clamp(0, width as isize - 1) as usize;
                let sy = (y as isize + dy).clamp(0, height as isize - 1) as usize;
                sum[0] += get_val(r_chan, sx, sy);
                sum[1] += get_val(g_chan, sx, sy);
                sum[2] += get_val(b_chan, sx, sy);
                sum[3] += if a_chan.is_some() {
                    get_val(a_chan, sx, sy)
                } else {
                    1.0
                };
                count += 1.0;
            }
        }

        Some([
            sum[0] / count,
            sum[1] / count,
            sum[2] / count,
            sum[3] / count,
        ])
    }

    /// Thin re-export of [`crate::render_math::linear_to_srgb`] so existing
    /// `ExrViewer::linear_to_srgb(..)` call sites (here and in `app.rs`) keep
    /// working while the math lives in one tested place.
    pub fn linear_to_srgb(l: f32) -> f32 {
        crate::render_math::linear_to_srgb(l)
    }

    pub fn calculate_histogram(&mut self, exr_data: &ExrData, exr_data_b: Option<&ExrData>) {
        if self.histogram_layer == Some(self.active_layer) {
            return;
        }

        let calc_bins = |data: &ExrData, layer_idx: usize| -> Option<[u32; 256]> {
            let mut bins = [0u32; 256];
            let (layer, r_chan, g_chan, b_chan, _) = data.logical_channels(layer_idx)?;
            let width = layer.size.0;
            let height = layer.size.1;

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

            for y in 0..height {
                for x in 0..width {
                    let r = get_val(r_chan, x, y);
                    let g = get_val(g_chan, x, y);
                    let b = get_val(b_chan, x, y);

                    // Luminance
                    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;

                    let bin = if self.log_histogram {
                        let ev = if lum <= 0.0 {
                            -10.0
                        } else {
                            lum.log2().clamp(-10.0, 10.0)
                        };
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
        self.histogram_b = exr_data_b.and_then(|d| {
            calc_bins(
                d,
                self.active_layer
                    .min(d.logical_layers.len().saturating_sub(1)),
            )
        });
        self.histogram_layer = Some(self.active_layer);
    }
}

fn rgb_to_hsvl(r: f32, g: f32, b: f32) -> (f32, f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let mut h = 0.0;
    if delta > 0.0 {
        if max == r {
            h = 60.0 * (((g - b) / delta) % 6.0);
        } else if max == g {
            h = 60.0 * (((b - r) / delta) + 2.0);
        } else if max == b {
            h = 60.0 * (((r - g) / delta) + 4.0);
        }
    }
    if h < 0.0 {
        h += 360.0;
    }

    let s = if max > 0.0 { delta / max } else { 0.0 };
    let v = max;
    let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;

    (h, s, v, l)
}

fn colored_rgba_label(ui: &mut egui::Ui, prefix: &str, val: [f32; 4]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if !prefix.is_empty() {
            ui.label(prefix);
        }
        ui.label(
            egui::RichText::new(format!("{:.5}", val[0]))
                .color(egui::Color32::from_rgb(255, 80, 80)),
        );
        ui.label(
            egui::RichText::new(format!("{:.5}", val[1]))
                .color(egui::Color32::from_rgb(80, 255, 80)),
        );
        ui.label(
            egui::RichText::new(format!("{:.5}", val[2]))
                .color(egui::Color32::from_rgb(100, 150, 255)),
        );
        ui.label(egui::RichText::new(format!("{:.5}", val[3])).color(egui::Color32::LIGHT_GRAY));
    });
}

fn draw_dashed_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    dash_length: f32,
    gap_length: f32,
) {
    let draw_line = |start: egui::Pos2, end: egui::Pos2| {
        let dir = end - start;
        let len = dir.length();
        let dir_norm = dir / len;
        let mut t = 0.0;
        while t < len {
            let t_end = (t + dash_length).min(len);
            painter.line_segment(
                [start + dir_norm * t, start + dir_norm * t_end],
                (1.0, color),
            );
            t += dash_length + gap_length;
        }
    };

    draw_line(rect.left_top(), rect.right_top());
    draw_line(rect.right_top(), rect.right_bottom());
    draw_line(rect.right_bottom(), rect.left_bottom());
    draw_line(rect.left_bottom(), rect.left_top());
}

#[cfg(test)]
mod gui_tests {
    //! Headless GUI interaction tests. They drive the rendering-free
    //! [`ExrViewer::handle_hotkeys`] seam through `egui_kittest`, so they run
    //! anywhere — no wgpu device, no loaded image — yet exercise the real egui
    //! input pipeline (events → `key_pressed` → state mutation).
    use super::{ChannelMode, CompareMode, ExrViewer};
    use eframe::egui;
    use egui_kittest::Harness;

    struct State {
        viewer: ExrViewer,
        has_b: bool,
    }

    fn harness(has_b: bool) -> Harness<'static, State> {
        Harness::new_ui_state(
            |ui, s: &mut State| {
                let has_b = s.has_b;
                s.viewer.handle_hotkeys(ui, has_b);
            },
            State {
                viewer: ExrViewer::default(),
                has_b,
            },
        )
    }

    #[test]
    fn channel_keys_isolate_and_reset() {
        let mut h = harness(false);

        for (key, expected) in [
            (egui::Key::R, ChannelMode::R),
            (egui::Key::G, ChannelMode::G),
            (egui::Key::B, ChannelMode::B),
            (egui::Key::A, ChannelMode::A),
            (egui::Key::C, ChannelMode::RGB), // C returns to full RGB
        ] {
            h.key_press(key);
            h.run();
            assert_eq!(h.state().viewer.channel_mode, expected, "key {key:?}");
        }
    }

    #[test]
    fn reset_keys_zero_exposure_and_gamma() {
        let mut h = harness(false);
        h.state_mut().viewer.exposure = 2.0;
        h.state_mut().viewer.gamma = 2.2;

        h.key_press(egui::Key::E);
        h.run();
        assert_eq!(h.state().viewer.exposure, 0.0, "E should reset exposure");
        // Gamma untouched by the exposure reset.
        assert_eq!(h.state().viewer.gamma, 2.2);

        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::G);
        h.run();
        assert_eq!(h.state().viewer.gamma, 1.0, "Shift+G should reset gamma");
    }

    #[test]
    fn plain_g_still_isolates_green_not_gamma_reset() {
        let mut h = harness(false);
        h.state_mut().viewer.gamma = 2.2;

        h.key_press(egui::Key::G);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            ChannelMode::G,
            "plain G must isolate the green channel"
        );
        assert_eq!(
            h.state().viewer.gamma,
            2.2,
            "plain G must NOT reset gamma (that's Shift+G)"
        );
    }

    #[test]
    fn channel_keys_are_inert_in_contact_sheet() {
        let mut h = harness(false);
        h.state_mut().viewer.show_contact_sheet = true;
        let before = h.state().viewer.channel_mode;

        h.key_press(egui::Key::R);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            before,
            "channel hotkeys must not fire in contact-sheet mode"
        );
    }

    #[test]
    fn compare_keys_switch_mode_when_reference_loaded() {
        let mut h = harness(true);

        h.key_press(egui::Key::Num2);
        h.run();
        assert_eq!(h.state().viewer.compare_mode, CompareMode::SingleB);

        h.key_press(egui::Key::Num1);
        h.run();
        assert_eq!(h.state().viewer.compare_mode, CompareMode::SingleA);
    }

    #[test]
    fn reference_only_shortcuts_are_inert_without_b() {
        let mut h = harness(false);
        let before = h.state().viewer.compare_mode;

        h.key_press(egui::Key::Num2);
        h.run();
        assert_eq!(
            h.state().viewer.compare_mode,
            before,
            "Num2 must do nothing without a reference image"
        );
    }

    #[test]
    fn space_toggles_blink_only_with_reference() {
        // With a reference image, Space toggles the blink (A/B flip) state.
        let mut h = harness(true);
        assert!(!h.state().viewer.blink_state);
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            h.state().viewer.blink_state,
            "Space should enable blink with B"
        );
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            !h.state().viewer.blink_state,
            "Space should toggle blink back off"
        );

        // Without a reference image, Space is inert.
        let mut h = harness(false);
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            !h.state().viewer.blink_state,
            "Space must be inert without B"
        );
    }

    #[test]
    fn test_blink_interval_math() {
        let blink_interval = 1.0;
        let is_even_phase = |time: f64| ((time / blink_interval) as usize).is_multiple_of(2);

        assert!(is_even_phase(0.0));
        assert!(is_even_phase(0.5));
        assert!(!is_even_phase(1.0));
        assert!(!is_even_phase(1.5));
        assert!(is_even_phase(2.0));

        let blink_interval = 0.5;
        let is_even_phase = |time: f64| ((time / blink_interval) as usize).is_multiple_of(2);
        assert!(is_even_phase(0.0));
        assert!(is_even_phase(0.25));
        assert!(!is_even_phase(0.5));
        assert!(!is_even_phase(0.75));
        assert!(is_even_phase(1.0));
    }
}
