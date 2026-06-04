use crate::exr_loader::ExrData;
use crate::viewer::ExrViewer;
use eframe::egui;
use rfd::FileDialog;
use std::path::PathBuf;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ExrApp {
    #[serde(skip)]
    loaded_file: Option<PathBuf>,
    #[serde(skip)]
    loaded_file_b: Option<PathBuf>,
    #[serde(skip)]
    exr_data: Option<ExrData>,
    #[serde(skip)]
    exr_data_b: Option<ExrData>,
    #[serde(skip)]
    error_msg: Option<String>,
    #[serde(skip)]
    viewer: ExrViewer,

    recent_files: Vec<PathBuf>,

    #[serde(skip)]
    show_help: bool,
    #[serde(skip)]
    show_settings: bool,

    #[serde(skip)]
    pub render_state: Option<eframe::egui_wgpu::RenderState>,

    ocio_path: String,
    lut_path: String,
    pub enable_lut: bool,
    #[serde(skip)]
    pub lut_bg: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    pub lut_error: Option<String>,

    #[serde(skip)]
    show_tools_window: bool,
    #[serde(skip)]
    tools_input_dir: String,
    #[serde(skip)]
    tools_output_dir: String,
    #[serde(skip)]
    conversion_progress: Option<(usize, usize)>,
    #[serde(skip)]
    conversion_status: String,
    #[serde(skip)]
    conversion_receiver: Option<std::sync::mpsc::Receiver<(usize, usize, String)>>,
    #[serde(skip)]
    conversion_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for ExrApp {
    fn default() -> Self {
        Self {
            loaded_file: None,
            loaded_file_b: None,
            exr_data: None,
            exr_data_b: None,
            error_msg: None,
            viewer: ExrViewer::default(),
            recent_files: Vec::new(),
            show_help: false,
            show_settings: false,
            render_state: None,
            ocio_path: String::new(),
            lut_path: String::new(),
            enable_lut: false,
            lut_bg: None,
            lut_error: None,
            show_tools_window: false,
            tools_input_dir: String::new(),
            tools_output_dir: String::new(),
            conversion_progress: None,
            conversion_status: String::new(),
            conversion_receiver: None,
            conversion_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl ExrApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Self::default()
        };

        app.render_state = cc.wgpu_render_state.clone();

        if let Some(rs) = &app.render_state {
            let gpu_state = crate::gpu::GpuState::new(&rs.device, rs.target_format);
            rs.renderer.write().callback_resources.insert(gpu_state);
        }

        app
    }

    fn open_file(&mut self, path: PathBuf, is_b: bool) {
        if !is_b {
            self.recent_files.retain(|p| p != &path);
            self.recent_files.insert(0, path.clone());
            self.recent_files.truncate(10);
            self.loaded_file = Some(path.clone());
        } else {
            self.loaded_file_b = Some(path.clone());
        }

        match ExrData::load(&path) {
            Ok(data) => {
                if is_b {
                    self.exr_data_b = Some(data);
                } else {
                    self.exr_data = Some(data);
                    self.exr_data_b = None; // Reset B when A changes
                    self.loaded_file_b = None;
                    self.viewer = ExrViewer::default(); // Reset viewer state
                }
                self.error_msg = None;
            }
            Err(e) => {
                if !is_b {
                    self.exr_data = None;
                }
                self.error_msg = Some(e.to_string());
            }
        }
    }
}

impl eframe::App for ExrApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.show_help {
            egui::Window::new("Help & Shortcuts")
                .open(&mut self.show_help)
                .show(ui.ctx(), |ui| {
                    ui.heading("Keyboard Shortcuts");
                    ui.label("R / G / B / A - Isolate specific channel");
                    ui.label("C - Return to full color composite");
                    ui.label("F - Frame image to fit the window");
                    ui.label("Shift + Click - Sample pixel color and save to swatches");

                    ui.add_space(10.0);
                    ui.heading("Features");
                    ui.label("• Dual Contact Sheets: Enable 'Contact Sheet' and use Compare Modes (A, B, A|B) to view side-by-side contact sheets.");
                    ui.label("• Metadata Explorer: When two images are loaded, EXR Info automatically displays metadata and layers for both Image A and Image B.");

                    ui.add_space(10.0);
                    ui.heading("About");
                    ui.label("EXR Analyzer - A professional tool for inspecting OpenEXR files.");
                    ui.add_space(5.0);
                    ui.hyperlink("https://github.com/byvfx/exr-analyzer");
                });
        }

        if self.show_tools_window {
            egui::Window::new("EXR Header Converter").open(&mut self.show_tools_window).show(ui.ctx(), |ui| {
                ui.heading("Batch Convert EXR Headers");
                ui.label("This tool processes all EXR files in a directory and renames their channels to standard RGBA format.");
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.label("Input Directory:");
                    if ui.button("Browse...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.tools_input_dir = path.to_string_lossy().to_string();
                            self.tools_output_dir = path.join("converted").to_string_lossy().to_string();
                        }
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.tools_input_dir).desired_width(f32::INFINITY));

                ui.add_space(5.0);
                
                ui.horizontal(|ui| {
                    ui.label("Output Directory:");
                    if ui.button("Browse...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.tools_output_dir = path.to_string_lossy().to_string();
                        }
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.tools_output_dir).desired_width(f32::INFINITY));

                ui.add_space(10.0);

                if self.conversion_receiver.is_none() {
                    if ui.button("Start Conversion").clicked() && !self.tools_input_dir.is_empty() && !self.tools_output_dir.is_empty() {
                        let (sender, receiver) = std::sync::mpsc::channel();
                        self.conversion_receiver = Some(receiver);
                        self.conversion_status = "Starting...".to_string();
                        self.conversion_progress = Some((0, 0));
                        
                        self.conversion_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
                        let cancel_flag = self.conversion_cancel.clone();

                        let in_dir = std::path::PathBuf::from(self.tools_input_dir.trim().trim_matches(|c| c == '"' || c == '\''));
                        let out_dir = std::path::PathBuf::from(self.tools_output_dir.trim().trim_matches(|c| c == '"' || c == '\''));
                        
                        std::thread::spawn(move || {
                            crate::tools::run_conversion_task(in_dir, out_dir, sender, cancel_flag);
                        });
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(false, |ui| {
                            let _ = ui.button("Start Conversion");
                        });
                        if ui.button("Cancel").clicked() {
                            self.conversion_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                            self.conversion_status = "Cancelling...".to_string();
                        }
                    });
                }

                if let Some(rx) = &self.conversion_receiver {
                    while let Ok((done, total, msg)) = rx.try_recv() {
                        self.conversion_status = msg;
                        self.conversion_progress = Some((done, total));
                    }
                    
                    if let Some((done, total)) = self.conversion_progress {
                        if total > 0 {
                            ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!("{}/{}", done, total)));
                        }
                        if done == total && total > 0 {
                            self.conversion_receiver = None; // Finished
                        }
                    }
                    ui.label(&self.conversion_status);
                } else if self.conversion_progress.map(|(d, t)| d == t && t > 0).unwrap_or(false) {
                     ui.add(egui::ProgressBar::new(1.0).text("Finished"));
                     ui.label("Conversion Complete!");
                }
            });
        }

        if self.show_settings {
            egui::Window::new("Color Management").open(&mut self.show_settings).show(ui.ctx(), |ui| {
                ui.heading("Settings");
                ui.add_space(5.0);
                
                ui.label("OCIO Environment / Config Path:");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.ocio_path);
                    if ui.button("Browse").clicked() {}
                });
                
                ui.add_space(10.0);
                
                ui.label("Custom LUT Path (.cube, .3dl):");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.lut_path);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("LUT", &["cube"])
                            .pick_file() 
                        {
                            self.lut_path = path.to_string_lossy().to_string();
                            match crate::color::cube::CubeLut::load(&path) {
                                Ok(lut) => {
                                    self.lut_error = None;
                                    if let Some(rs) = &self.render_state {
                                        let renderer = rs.renderer.read();
                                        if let Some(gpu_state) = renderer.callback_resources.get::<crate::gpu::GpuState>() {
                                            self.lut_bg = Some(gpu_state.create_lut_bind_group(&rs.device, &rs.queue, &lut));
                                            self.enable_lut = true; // Auto-enable on successful load
                                        } else {
                                            self.lut_error = Some("GPU state not found".to_string());
                                        }
                                    } else {
                                        self.lut_error = Some("Render state not found".to_string());
                                    }
                                }
                                Err(e) => {
                                    self.lut_error = Some(format!("Failed to load LUT: {}", e));
                                    self.lut_bg = None;
                                    self.enable_lut = false;
                                }
                            }
                        }
                    }
                });
                ui.checkbox(&mut self.enable_lut, "Enable Custom LUT");
                if let Some(err) = &self.lut_error {
                    ui.label(egui::RichText::new(err).color(egui::Color32::RED));
                }
                if self.lut_bg.is_some() {
                    ui.label(egui::RichText::new("LUT loaded and active!").color(egui::Color32::GREEN));
                }
                
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Note: OCIO is penciled in for future GPU rendering phases and is not currently active.").italics());
            });
        }

        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open EXR...").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("EXR Image", &["exr"])
                            .pick_file()
                        {
                            self.open_file(path, false);
                        }
                        ui.close();
                    }
                    if ui.button("Open Reference (Image B)...").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("EXR Image", &["exr"])
                            .pick_file()
                        {
                            self.open_file(path, true);
                        }
                        ui.close();
                    }
                    ui.menu_button("Open Recent", |ui| {
                        if self.recent_files.is_empty() {
                            ui.label("No recent files");
                        } else {
                            let mut clicked_path = None;
                            for path in &self.recent_files {
                                if ui
                                    .button(path.file_name().unwrap_or_default().to_string_lossy())
                                    .clicked()
                                {
                                    clicked_path = Some(path.clone());
                                }
                            }
                            if let Some(path) = clicked_path {
                                self.open_file(path, false);
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.viewer.show_contact_sheet, "Contact Sheet");
                });

                ui.menu_button("Settings", |ui| {
                    if ui.button("Color Management...").clicked() {
                        self.show_settings = true;
                        ui.close();
                    }
                });

                ui.menu_button("Tools", |ui| {
                    if ui.button("EXR Header Converter").clicked() {
                        self.show_tools_window = true;
                        ui.close();
                    }
                });

                ui.menu_button("Help", |ui| {
                    if ui.button("Keyboard Shortcuts").clicked() {
                        self.show_help = true;
                        ui.close();
                    }
                });
            });
        });

        egui::Panel::left("side_panel")
            .resizable(true)
            .min_size(200.0)
            .show_inside(ui, |ui| {
                ui.heading("EXR Info");
                ui.separator();
                if let Some(err) = &self.error_msg {
                    ui.colored_label(egui::Color32::RED, format!("Error: {}", err));
                    ui.separator();
                }

                let mut files_to_show = vec![];
                if let (Some(path), Some(data)) = (&self.loaded_file, &self.exr_data) {
                    files_to_show.push(("Image A", path, data));
                }
                if let (Some(path), Some(data)) = (&self.loaded_file_b, &self.exr_data_b) {
                    files_to_show.push(("Image B", path, data));
                }

                if !files_to_show.is_empty() {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (idx, (label, path, exr_data)) in files_to_show.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                                ui.add_space(10.0);
                            }
                            ui.heading(format!(
                                "{}: {}", label,
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ));
                            ui.add_space(5.0);

                            egui::CollapsingHeader::new("Image Metadata")
                                .id_salt(format!("image_metadata_header_{}", idx))
                                .default_open(false)
                                .show(ui, |ui| {
                                    let attrs = &exr_data.image.attributes;
                                    ui.label(format!(
                                        "Display Window: {}x{} at {},{}",
                                        attrs.display_window.size.x(),
                                        attrs.display_window.size.y(),
                                        attrs.display_window.position.x(),
                                        attrs.display_window.position.y()
                                    ));
                                    ui.label(format!("Pixel Aspect: {}", attrs.pixel_aspect));

                                    if !attrs.other.is_empty() {
                                        ui.add_space(5.0);
                                        egui::CollapsingHeader::new("Custom Attributes")
                                            .id_salt(format!("image_custom_attrs_header_{}", idx))
                                            .default_open(false)
                                            .show(ui, |ui| {
                                                for (name, val) in attrs.other.iter() {
                                                    ui.horizontal_wrapped(|ui| {
                                                        ui.strong(format!("{}: ", name));
                                                        ui.label(format!("{:?}", val));
                                                    });
                                                }
                                            });
                                    }
                                });

                            ui.separator();
                            ui.heading("Layers");

                            for (i, layer) in exr_data.image.layer_data.iter().enumerate() {
                                let is_selected = self.viewer.active_layer == i;
                                let layer_name = layer
                                    .attributes
                                    .layer_name
                                    .as_ref()
                                    .map(|n| n.to_string())
                                    .unwrap_or_else(|| "Unnamed Layer".to_string());

                                if ui.selectable_label(is_selected, &layer_name).clicked() {
                                    self.viewer.active_layer = i;
                                }

                                if is_selected {
                                    ui.indent("layer_details", |ui| {
                                        ui.label(format!(
                                            "Resolution: {}x{}",
                                            layer.size.0, layer.size.1
                                        ));
                                        ui.label(format!(
                                            "Channels: {}",
                                            layer.channel_data.list.len()
                                        ));

                                        if !layer.attributes.other.is_empty() {
                                            ui.add_space(5.0);
                                            egui::CollapsingHeader::new("Layer Attributes")
                                                .id_salt(format!("layer_attrs_header_{}_{}", idx, i))
                                                .default_open(false)
                                                .show(ui, |ui| {
                                                    for (name, val) in layer.attributes.other.iter()
                                                    {
                                                        ui.horizontal_wrapped(|ui| {
                                                            ui.strong(format!("{}: ", name));
                                                            ui.label(format!("{:?}", val));
                                                        });
                                                    }
                                                });
                                        }
                                    });
                                }
                            }
                        }
                    });
                }

                if let Some(_path) = &self.loaded_file {
                    if let Some(exr_data) = &self.exr_data {
                        ui.separator();
                        ui.heading("Color Sampler");

                        if !self.viewer.swatches.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(format!("{} saved", self.viewer.swatches.len()));
                                if ui.button("Clear All").clicked() {
                                    self.viewer.swatches.clear();
                                }
                            });
                            ui.add_space(5.0);

                            egui::ScrollArea::vertical()
                                .id_salt("swatches_scroll")
                                .show(ui, |ui| {
                                    let mut to_remove = None;
                                    for (i, swatch) in self.viewer.swatches.iter().enumerate() {
                                        ui.horizontal(|ui| {
                                            let [r, g, b, _a] = *swatch;

                                            // Preview color patch using current sRGB mode and exposure/gamma
                                            let mut disp_r = r * self.viewer.exposure.exp2();
                                            let mut disp_g = g * self.viewer.exposure.exp2();
                                            let mut disp_b = b * self.viewer.exposure.exp2();

                                            if self.viewer.gamma != 1.0 {
                                                let inv_gamma = 1.0 / self.viewer.gamma;
                                                disp_r = if disp_r > 0.0 {
                                                    disp_r.powf(inv_gamma)
                                                } else {
                                                    0.0
                                                };
                                                disp_g = if disp_g > 0.0 {
                                                    disp_g.powf(inv_gamma)
                                                } else {
                                                    0.0
                                                };
                                                disp_b = if disp_b > 0.0 {
                                                    disp_b.powf(inv_gamma)
                                                } else {
                                                    0.0
                                                };
                                            }

                                            if self.viewer.srgb {
                                                disp_r = self.viewer.linear_to_srgb(disp_r);
                                                disp_g = self.viewer.linear_to_srgb(disp_g);
                                                disp_b = self.viewer.linear_to_srgb(disp_b);
                                            }

                                            let r_u8 = (disp_r.clamp(0.0, 1.0) * 255.0) as u8;
                                            let g_u8 = (disp_g.clamp(0.0, 1.0) * 255.0) as u8;
                                            let b_u8 = (disp_b.clamp(0.0, 1.0) * 255.0) as u8;

                                            let color = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                                            let (rect, _resp) = ui.allocate_exact_size(
                                                egui::vec2(20.0, 20.0),
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().rect_filled(rect, 2.0, color);

                                            // Display values
                                            ui.vertical(|ui| {
                                                ui.label(format!(
                                                    "Float: {:.4}, {:.4}, {:.4}",
                                                    r, g, b
                                                ));
                                                ui.label(format!(
                                                    "8-bit: {}, {}, {}",
                                                    r_u8, g_u8, b_u8
                                                ));
                                                // HSV mapping
                                                let max = r.max(g).max(b);
                                                let min = r.min(g).min(b);
                                                let c = max - min;
                                                let h = if c == 0.0 {
                                                    0.0
                                                } else if max == r {
                                                    60.0 * (((g - b) / c) % 6.0)
                                                } else if max == g {
                                                    60.0 * (((b - r) / c) + 2.0)
                                                } else {
                                                    60.0 * (((r - g) / c) + 4.0)
                                                };
                                                let h = if h < 0.0 { h + 360.0 } else { h };
                                                let s = if max == 0.0 { 0.0 } else { c / max };
                                                let v = max;
                                                ui.label(format!(
                                                    "HSV: {:.1}°, {:.2}, {:.2}",
                                                    h, s, v
                                                ));
                                            });

                                            if ui.button("X").clicked() {
                                                to_remove = Some(i);
                                            }
                                        });
                                        ui.separator();
                                    }
                                    if let Some(i) = to_remove {
                                        self.viewer.swatches.remove(i);
                                    }
                                });
                        } else {
                            ui.label("Shift+Click on the image to save a swatch.");
                        }

                        ui.separator();
                        ui.heading("Histogram");
                        ui.horizontal(|ui| {
                            if ui
                                .checkbox(
                                    &mut self.viewer.log_histogram,
                                    "Log Scale (-10 to +10 EV)",
                                )
                                .changed()
                            {
                                self.viewer.histogram_layer = None;
                            }
                        });

                        self.viewer
                            .calculate_histogram(exr_data, self.exr_data_b.as_ref());

                        if let Some(bins) = &self.viewer.histogram {
                            let (rect, _resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 80.0),
                                egui::Sense::hover(),
                            );
                            let mut max_val = *bins.iter().max().unwrap_or(&1) as f32;
                            if let Some(bins_b) = &self.viewer.histogram_b {
                                max_val = max_val.max(*bins_b.iter().max().unwrap_or(&1) as f32);
                            }
                            let max_val = max_val.max(1.0);

                            let mut shapes = vec![];
                            let bar_width = rect.width() / 256.0;

                            for (i, &count) in bins.iter().enumerate() {
                                let h = (count as f32 / max_val).powf(0.5) * rect.height();
                                let x = rect.min.x + i as f32 * bar_width;
                                let y = rect.max.y - h;

                                shapes.push(egui::Shape::rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(x, y),
                                        egui::pos2(x + bar_width.max(1.0), rect.max.y),
                                    ),
                                    0.0,
                                    egui::Color32::from_white_alpha(150), // White for A
                                ));
                            }

                            if let Some(bins_b) = &self.viewer.histogram_b {
                                for (i, &count) in bins_b.iter().enumerate() {
                                    let h = (count as f32 / max_val).powf(0.5) * rect.height();
                                    let x = rect.min.x + i as f32 * bar_width;
                                    let y = rect.max.y - h;

                                    shapes.push(egui::Shape::rect_filled(
                                        egui::Rect::from_min_max(
                                            egui::pos2(x, y),
                                            egui::pos2(x + bar_width.max(1.0), rect.max.y),
                                        ),
                                        0.0,
                                        egui::Color32::from_rgba_unmultiplied(255, 50, 50, 150), // Red for B
                                    ));
                                }
                            }
                            ui.painter().extend(shapes);
                        }
                    }
                } else {
                    ui.label("No file loaded.");
                }
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.loaded_file.is_some() {
                if let Some(data) = &self.exr_data {
                    self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
                    self.viewer.ui(
                        ui,
                        data,
                        self.exr_data_b.as_ref(),
                        self.render_state.as_ref(),
                        self.lut_bg.clone(),
                    );
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open an EXR file to begin.");
                });
            }
        });
    }
}
