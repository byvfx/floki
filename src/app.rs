use crate::exr_loader::ExrData;
use crate::viewer::ExrViewer;
use eframe::egui;
use rfd::FileDialog;
use std::path::PathBuf;

pub struct ExrApp {
    loaded_file: Option<PathBuf>,
    exr_data: Option<ExrData>,
    error_msg: Option<String>,
    viewer: ExrViewer,
}

impl ExrApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            loaded_file: None,
            exr_data: None,
            error_msg: None,
            viewer: ExrViewer::default(),
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        self.loaded_file = Some(path.clone());
        match ExrData::load(&path) {
            Ok(data) => {
                self.exr_data = Some(data);
                self.error_msg = None;
                self.viewer = ExrViewer::default(); // Reset viewer state
            }
            Err(e) => {
                self.exr_data = None;
                self.error_msg = Some(e.to_string());
            }
        }
    }
}

impl eframe::App for ExrApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open EXR...").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("EXR Image", &["exr"])
                            .pick_file()
                        {
                            self.open_file(path);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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

                if let Some(path) = &self.loaded_file {
                    ui.label(format!(
                        "File: {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));

                    if let Some(exr_data) = &self.exr_data {
                        ui.separator();
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.heading("Image Metadata");
                            let attrs = &exr_data.image.attributes;
                            ui.label(format!("Display Window: {}x{} at {},{}", 
                                attrs.display_window.size.x(), attrs.display_window.size.y(),
                                attrs.display_window.position.x(), attrs.display_window.position.y()
                            ));
                            ui.label(format!("Pixel Aspect: {}", attrs.pixel_aspect));
                            
                            if !attrs.other.is_empty() {
                                ui.add_space(5.0);
                                ui.label("Custom Attributes:");
                                for (name, val) in attrs.other.iter() {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.strong(format!("{}: ", name));
                                        ui.label(format!("{:?}", val));
                                    });
                                }
                            }

                            ui.separator();
                            ui.heading("Layers");
                            
                            for (i, layer) in exr_data.image.layer_data.iter().enumerate() {
                                let is_selected = self.viewer.active_layer == i;
                                let layer_name = layer.attributes.layer_name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "Unnamed Layer".to_string());
                                
                                if ui.selectable_label(is_selected, &layer_name).clicked() {
                                    self.viewer.active_layer = i;
                                }
                                
                                if is_selected {
                                    ui.indent("layer_details", |ui| {
                                        ui.label(format!("Resolution: {}x{}", layer.size.0, layer.size.1));
                                        ui.label(format!("Channels: {}", layer.channel_data.list.len()));
                                        
                                        if !layer.attributes.other.is_empty() {
                                            ui.add_space(5.0);
                                            ui.label("Layer Attributes:");
                                            for (name, val) in layer.attributes.other.iter() {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.strong(format!("{}: ", name));
                                                    ui.label(format!("{:?}", val));
                                                });
                                            }
                                        }
                                    });
                                }
                            }
                        });
                        
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
                            
                            egui::ScrollArea::vertical().id_salt("swatches_scroll").show(ui, |ui| {
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
                                            disp_r = if disp_r > 0.0 { disp_r.powf(inv_gamma) } else { 0.0 };
                                            disp_g = if disp_g > 0.0 { disp_g.powf(inv_gamma) } else { 0.0 };
                                            disp_b = if disp_b > 0.0 { disp_b.powf(inv_gamma) } else { 0.0 };
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
                                        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                                        ui.painter().rect_filled(rect, 2.0, color);
                                        
                                        // Display values
                                        ui.vertical(|ui| {
                                            ui.label(format!("Float: {:.4}, {:.4}, {:.4}", r, g, b));
                                            ui.label(format!("8-bit: {}, {}, {}", r_u8, g_u8, b_u8));
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
                                            ui.label(format!("HSV: {:.1}°, {:.2}, {:.2}", h, s, v));
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

                    }
                } else {
                    ui.label("No file loaded.");
                }
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.loaded_file.is_some() {
                if let Some(data) = &self.exr_data {
                    self.viewer.ui(ui, data);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open an EXR file to begin.");
                });
            }
        });
    }
}
