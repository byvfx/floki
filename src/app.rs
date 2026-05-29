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

                    if let Some(data) = &self.exr_data {
                        ui.separator();
                        let num_layers = data.image.layer_data.len();
                        ui.label(format!("Layers: {}", num_layers));
                        for (i, layer) in data.image.layer_data.iter().enumerate() {
                            let name = layer
                                .attributes
                                .layer_name
                                .as_ref()
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "Unnamed".to_string());
                            let size = layer.size;
                            ui.label(format!("Layer {}: {} ({}x{})", i, name, size.0, size.1));
                            ui.label(format!("  Channels: {}", layer.channel_data.list.len()));
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
