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
