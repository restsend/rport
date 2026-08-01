use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use eframe::egui;
use rport::uuid::Uuid;

use crate::client_manager::{ClientStatus, ManagedClient};
use crate::config::{ClientConfig, ForwardRule, UiConfig};

const MAX_LOG_LINES: usize = 5000;

pub struct LogStore {
    pub entries: Arc<Mutex<VecDeque<String>>>,
}

impl LogStore {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

pub struct LogWriter {
    entries: Arc<Mutex<VecDeque<String>>>,
    line: Vec<u8>,
}

impl LogWriter {
    pub fn new(entries: Arc<Mutex<VecDeque<String>>>) -> Self {
        Self {
            entries,
            line: Vec::new(),
        }
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        while let Some(pos) = self.line.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.line.drain(..=pos).collect();
            if raw.len() > 1 {
                let s = String::from_utf8_lossy(&raw[..raw.len() - 1]).to_string();
                if !s.is_empty() {
                    let mut entries = self.entries.lock().unwrap();
                    entries.push_back(s);
                    while entries.len() > MAX_LOG_LINES {
                        entries.pop_front();
                    }
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum Action {
    StartClient(usize),
    StopClient(usize),
}

pub struct RportApp {
    config: UiConfig,
    clients: Vec<ManagedClient>,
    selected_idx: Option<usize>,

    show_add_dialog: bool,
    edit_idx: Option<usize>,
    edit_config: ClientConfig,

    show_add_forward: bool,
    edit_forward: ForwardRule,
    status_message: Option<String>,
    confirm_delete_forward: Option<(usize, usize)>,
    confirm_delete_client: Option<usize>,

    log_store: LogStore,
    show_logs: bool,
    log_filter: String,
}

impl RportApp {
    pub fn new(log_store: LogStore) -> Self {
        let config = UiConfig::load();
        let clients: Vec<ManagedClient> = config
            .clients
            .iter()
            .map(|c| ManagedClient::new(c.clone()))
            .collect();
        let has_clients = !clients.is_empty();
        Self {
            clients,
            selected_idx: if has_clients { Some(0) } else { None },
            config,
            show_add_dialog: false,
            edit_idx: None,
            edit_config: ClientConfig::default(),
            show_add_forward: false,
            edit_forward: ForwardRule::default(),
            status_message: None,
            confirm_delete_forward: None,
            confirm_delete_client: None,
            log_store,
            show_logs: true,
            log_filter: String::new(),
        }
    }
}

impl RportApp {
    fn save_config(&mut self) {
        self.config.clients = self.clients.iter().map(|c| c.config.clone()).collect();
        self.config.save();
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
    }

    fn add_client(&mut self) {
        let id = Uuid::new_v4().to_string();
        self.edit_config.id = id;
        let config = self.edit_config.clone();
        let idx = self.clients.len();
        self.clients.push(ManagedClient::new(config));
        self.selected_idx = Some(idx);
        self.set_status(format!("Added client '{}'", self.edit_config.name));
        self.save_config();
        self.show_add_dialog = false;
        self.edit_config = ClientConfig::default();
    }

    fn commit_edit(&mut self) {
        if let Some(idx) = self.edit_idx {
            let was_running = self.clients[idx].is_running();
            if was_running {
                self.clients[idx].stop();
            }
            self.clients[idx].config = self.edit_config.clone();
            self.set_status(format!("Updated client '{}'", self.edit_config.name));
            self.save_config();
        }
        self.edit_idx = None;
    }
}

impl eframe::App for RportApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for client in &self.clients {
            if client.status.has_changed().unwrap_or(false) {
                ctx.request_repaint();
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        // --- Top panel ---
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RPort Manager");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });

        // --- Log panel ---
        egui::TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .default_height(150.0)
            .min_height(60.0)
            .show(ctx, |ui| {
                egui::CollapsingHeader::new("Logs")
                    .default_open(self.show_logs)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Clear").clicked() {
                                self.log_store.clear();
                            }
                            ui.label("Filter:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.log_filter)
                                    .hint_text("filter...")
                                    .desired_width(120.0),
                            );
                            if ui.button("✕").clicked() {
                                self.show_logs = false;
                            }
                        });
                        let entries = self.log_store.entries.lock().unwrap();
                        let filter = self.log_filter.to_lowercase();
                        egui::ScrollArea::vertical()
                            .id_salt("log_scroll")
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for entry in entries.iter() {
                                    if !filter.is_empty()
                                        && !entry.to_lowercase().contains(&filter)
                                    {
                                        continue;
                                    }
                                    ui.label(entry);
                                }
                            });
                    });
            });

        // --- Status bar ---
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let running = self.clients.iter().filter(|c| c.is_running()).count();
                ui.label(format!(
                    "{} clients, {} running",
                    self.clients.len(),
                    running
                ));
                if let Some(ref msg) = self.status_message {
                    ui.separator();
                    ui.label(msg);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let log_label = if self.show_logs { "Hide Log" } else { "Log" };
                    if ui.button(log_label).clicked() {
                        self.show_logs = !self.show_logs;
                    }
                });
            });
        });

        // --- Collect actions from sidebar + main panel ---
        let mut action: Option<Action> = None;

        // --- Sidebar ---
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.label("Clients");
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .id_salt("client_list")
                        .show(ui, |ui| {
                            let mut i = 0;
                            while i < self.clients.len() {
                                let running = self.clients[i].is_running();
                                let status = self.clients[i].status.borrow().clone();
                                let name = self.clients[i].config.name.clone();
                                let is_selected = self.selected_idx == Some(i);

                                let status_icon = match status {
                                    ClientStatus::Connected => "●",
                                    ClientStatus::Connecting => "◌",
                                    ClientStatus::Disconnected => "○",
                                    ClientStatus::Failed(_) => "✕",
                                };

                                let color = match status {
                                    ClientStatus::Connected => egui::Color32::GREEN,
                                    ClientStatus::Connecting => egui::Color32::YELLOW,
                                    ClientStatus::Failed(_) => egui::Color32::RED,
                                    ClientStatus::Disconnected => egui::Color32::GRAY,
                                };

                                ui.horizontal(|ui| {
                                    ui.add(egui::Label::new(
                                        egui::RichText::new(status_icon).color(color).size(16.0),
                                    ));

                                    let label = if name.is_empty() {
                                        format!("<unnamed-{}>", i)
                                    } else {
                                        name.clone()
                                    };

                                    if ui
                                        .selectable_label(is_selected, &label)
                                        .clicked()
                                    {
                                        self.selected_idx = Some(i);
                                    }

                                    ui.menu_button("…", |ui| {
                                        if ui.button("Edit").clicked() {
                                            self.edit_idx = Some(i);
                                            self.edit_config = self.clients[i].config.clone();
                                            ui.close_menu();
                                        }
                                        if ui.button("Delete").clicked() {
                                            self.confirm_delete_client = Some(i);
                                            ui.close_menu();
                                        }
                                        if running {
                                            if ui.button("Stop").clicked() {
                                                action = Some(Action::StopClient(i));
                                                ui.close_menu();
                                            }
                                        } else {
                                            if ui.button("Start").clicked() {
                                                action = Some(Action::StartClient(i));
                                                ui.close_menu();
                                            }
                                        }
                                    });
                                });

                                i += 1;
                            }
                        });

                    ui.separator();
                    if ui.button("＋ Add Client").clicked() {
                        self.edit_config = ClientConfig::default();
                        self.edit_idx = None;
                        self.show_add_dialog = true;
                    }

                });
            });

        // --- Main panel ---
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(idx) = self.selected_idx {
                if idx < self.clients.len() {
                    let name = self.clients[idx].config.name.clone();
                    let server_addr = self.clients[idx].config.server_addr.clone();
                    let agent_id = self.clients[idx].config.agent_id.clone();
                    let token = self.clients[idx].config.token.clone();
                    let status = self.clients[idx].status.borrow().clone();
                    let sent = self.clients[idx]
                        .stats
                        .bytes_sent
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let recv = self.clients[idx]
                        .stats
                        .bytes_recv
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let is_running = self.clients[idx].is_running();

                    ui.heading(if name.is_empty() { "<unnamed>" } else { &name });

                    ui.separator();

                    let (status_text, status_color) = match &status {
                        ClientStatus::Connected => {
                            ("Connected".to_string(), egui::Color32::GREEN)
                        }
                        ClientStatus::Connecting => {
                            ("Connecting...".to_string(), egui::Color32::YELLOW)
                        }
                        ClientStatus::Disconnected => {
                            ("Disconnected".to_string(), egui::Color32::GRAY)
                        }
                        ClientStatus::Failed(reason) => {
                            (format!("Failed: {}", reason), egui::Color32::RED)
                        }
                    };
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.colored_label(status_color, status_text);
                    });

                    egui::Grid::new("connection_info")
                        .num_columns(2)
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label("Server:");
                            ui.label(&server_addr);
                            ui.end_row();
                            ui.label("Agent ID:");
                            ui.label(&agent_id);
                            ui.end_row();
                            ui.label("Token:");
                            ui.label(if token.len() > 8 {
                                format!("{}...", &token[..8])
                            } else {
                                token
                            });
                            ui.end_row();
                        });

                    ui.separator();

                    ui.heading("Traffic");
                    ui.horizontal(|ui| {
                        ui.label(format!("↑ {} sent", format_bytes(sent)));
                        ui.label(format!("↓ {} received", format_bytes(recv)));
                    });

                    ui.separator();

                    ui.heading("Port Forwards");
                    let fwd_count = self.clients[idx].config.forwards.len();
                    for fi in 0..fwd_count {
                        let old_l = self.clients[idx].config.forwards[fi].local_port;
                        let old_r = self.clients[idx].config.forwards[fi].remote_port;
                        let mut port_str = old_l.to_string();
                        let mut rport_str = old_r.to_string();
                        let mut host = self.clients[idx].config.forwards[fi].remote_host.clone();
                        let mut changed = false;
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("#{}", fi + 1));
                                ui.label("L");
                                let r1 = ui.add_sized(
                                    [50.0, 0.0],
                                    egui::TextEdit::singleline(&mut port_str),
                                );
                                if r1.changed() || r1.lost_focus() {
                                    let new_p = parse_port(&port_str, old_l);
                                    if new_p != old_l {
                                        self.clients[idx].config.forwards[fi].local_port = new_p;
                                        changed = true;
                                    }
                                }
                                ui.label("→");
                                ui.label("H");
                                if ui.text_edit_singleline(&mut host).changed() {
                                    self.clients[idx].config.forwards[fi].remote_host = host.clone();
                                    changed = true;
                                }
                                ui.label("P");
                                let r2 = ui.add_sized(
                                    [50.0, 0.0],
                                    egui::TextEdit::singleline(&mut rport_str),
                                );
                                if r2.changed() || r2.lost_focus() {
                                    let new_p = parse_port(&rport_str, old_r);
                                    if new_p != old_r {
                                        self.clients[idx].config.forwards[fi].remote_port = new_p;
                                        changed = true;
                                    }
                                }
                                if ui
                                    .checkbox(
                                        &mut self.clients[idx].config.forwards[fi].enabled,
                                        "",
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                                if ui.button("✕").clicked() {
                                    self.confirm_delete_forward = Some((idx, fi));
                                }
                            });
                        });
                        if changed {
                            self.save_config();
                        }
                    }
                    if ui.button("＋ Add Forward").clicked() {
                        self.edit_forward = ForwardRule::default();
                        self.show_add_forward = true;
                    }

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if is_running {
                            if ui
                                .button(egui::RichText::new("⏹ Stop").color(egui::Color32::RED))
                                .clicked()
                            {
                                action = Some(Action::StopClient(idx));
                            }
                        } else {
                            if ui
                                .button(egui::RichText::new("▶ Start").color(egui::Color32::GREEN))
                                .clicked()
                            {
                                action = Some(Action::StartClient(idx));
                            }
                        }

                        if ui.button("Edit").clicked() {
                            self.edit_idx = Some(idx);
                            self.edit_config = self.clients[idx].config.clone();
                        }

                        if ui.button("＋ Forward").clicked() {
                            self.edit_forward = ForwardRule::default();
                            self.show_add_forward = true;
                        }
                    });
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.heading("No client selected");
                    ui.label("Add a client to get started.");
                    if ui.button("＋ Add Client").clicked() {
                        self.edit_config = ClientConfig::default();
                        self.edit_idx = None;
                        self.show_add_dialog = true;
                    }
                });
            }

        });

        // --- Apply actions ---
        match action {
            Some(Action::StartClient(idx)) if idx < self.clients.len() => {
                self.clients[idx].start();
                self.set_status(format!(
                    "Starting client '{}'...",
                    self.clients[idx].config.name
                ));
            }
            Some(Action::StopClient(idx)) if idx < self.clients.len() => {
                let name = self.clients[idx].config.name.clone();
                self.clients[idx].stop();
                self.set_status(format!("Stopped client '{}'", name));
            }
            _ => {}
        }

        // --- Add/Edit dialog ---
        if self.show_add_dialog || self.edit_idx.is_some() {
            let title = if self.show_add_dialog {
                "Add Client"
            } else {
                "Edit Client"
            };
            let mut open = true;
            egui::Window::new(title)
                .open(&mut open)
                .resizable(true)
                .default_size([400.0, 320.0])
                .show(ctx, |ui| {
                    let cfg = &mut self.edit_config;

                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.text_edit_singleline(&mut cfg.name);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Server:");
                        ui.text_edit_singleline(&mut cfg.server_addr);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Token:");
                        ui.text_edit_singleline(&mut cfg.token);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Agent ID:");
                        ui.text_edit_singleline(&mut cfg.agent_id);
                    });

                    ui.separator();
                    ui.heading("Port Forwards");

                    let mut remove_fwd = None;
                    let mut i = 0;
                    while i < cfg.forwards.len() {
                        let old_l = cfg.forwards[i].local_port;
                        let old_r = cfg.forwards[i].remote_port;
                        let mut port_str = old_l.to_string();
                        let mut rport_str = old_r.to_string();
                        let mut host = cfg.forwards[i].remote_host.clone();
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("#{}", i + 1));
                                ui.label("L");
                                let r1 = ui.add_sized(
                                    [40.0, 0.0],
                                    egui::TextEdit::singleline(&mut port_str),
                                );
                                if r1.changed() {
                                    cfg.forwards[i].local_port = parse_port(&port_str, old_l);
                                }
                                ui.label("→");
                                ui.label("P");
                                let r2 = ui.add_sized(
                                    [40.0, 0.0],
                                    egui::TextEdit::singleline(&mut rport_str),
                                );
                                if r2.changed() {
                                    cfg.forwards[i].remote_port =
                                        parse_port(&rport_str, old_r);
                                }
                                ui.checkbox(&mut cfg.forwards[i].enabled, "");
                                if ui.button("✕").clicked() {
                                    remove_fwd = Some(i);
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.add_space(20.0);
                                ui.label("H");
                                if ui.text_edit_singleline(&mut host).changed() {
                                    cfg.forwards[i].remote_host = host;
                                }
                            });
                        });
                        i += 1;
                    }

                    if ui.button("＋ Add Forward").clicked() {
                        cfg.forwards.push(ForwardRule::default());
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.show_add_dialog = false;
                            self.edit_idx = None;
                        }
                        if ui.button("Save").clicked() {
                            if self.show_add_dialog {
                                self.add_client();
                            } else {
                                self.commit_edit();
                            }
                        }
                    });
                });

            if !open {
                self.show_add_dialog = false;
                self.edit_idx = None;
            }
        }

        // --- Confirm delete client ---
        if let Some(idx) = self.confirm_delete_client.take() {
            if idx < self.clients.len() {
                let name = self.clients[idx].config.name.clone();
                let mut confirmed = false;
                egui::Window::new("Confirm Delete Client")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(format!("Delete client '{}'?", name));
                        ui.label("This will stop the connection and remove all config.");
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                ui.close_menu();
                            }
                            if ui.button("Delete").clicked() {
                                confirmed = true;
                            }
                        });
                    });
                if confirmed {
                    if self.clients[idx].is_running() {
                        self.clients[idx].stop();
                    }
                    self.clients.remove(idx);
                    self.selected_idx = if self.clients.is_empty() {
                        None
                    } else {
                        Some(if idx >= self.clients.len() {
                            self.clients.len() - 1
                        } else {
                            idx
                        })
                    };
                    self.set_status(format!("Deleted client '{}'", name));
                    self.save_config();
                }
            }
        }

        // --- Confirm delete forward ---
        if let Some((cidx, fi)) = self.confirm_delete_forward.take() {
            let name = self.clients.get(cidx).map(|c| c.config.name.clone()).unwrap_or_default();
            let mut confirmed = false;
            egui::Window::new("Confirm Delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Delete forward rule #{} from '{}'?", fi + 1, name));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            ui.close_menu();
                        }
                        if ui.button("Delete").clicked() {
                            confirmed = true;
                        }
                    });
                });
            if confirmed {
                let was_running = self.clients[cidx].is_running();
                if was_running {
                    self.clients[cidx].stop();
                }
                self.clients[cidx].config.forwards.remove(fi);
                if was_running {
                    self.clients[cidx].start();
                }
                self.save_config();
            }
        }

        // --- Add Forward dialog ---
        if self.show_add_forward {
            let mut open = true;
            egui::Window::new("Add Forward Rule")
                .open(&mut open)
                .default_size([300.0, 150.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let old = self.edit_forward.local_port;
                        let mut port_str = old.to_string();
                        ui.label("Local");
                        if ui.text_edit_singleline(&mut port_str).changed() {
                            self.edit_forward.local_port = parse_port(&port_str, old);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Host");
                        ui.text_edit_singleline(&mut self.edit_forward.remote_host);
                    });
                    ui.horizontal(|ui| {
                        let old = self.edit_forward.remote_port;
                        let mut port_str = old.to_string();
                        ui.label("Remote");
                        if ui.text_edit_singleline(&mut port_str).changed() {
                            self.edit_forward.remote_port = parse_port(&port_str, old);
                        }
                    });
                    ui.checkbox(&mut self.edit_forward.enabled, "Enabled");

                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.show_add_forward = false;
                        }
                        if ui.button("Add").clicked() {
                            if let Some(idx) = self.selected_idx {
                                let was_running = self.clients[idx].is_running();
                                if was_running {
                                    self.clients[idx].stop();
                                }
                                self.clients[idx]
                                    .config
                                    .forwards
                                    .push(self.edit_forward.clone());
                                self.set_status(format!(
                                    "Added forward rule to '{}'",
                                    self.clients[idx].config.name
                                ));
                                self.save_config();
                                if was_running {
                                    self.clients[idx].start();
                                }
                            }
                            self.show_add_forward = false;
                        }
                    });
                });

            if !open {
                self.show_add_forward = false;
            }
        }
    }
}

fn parse_port(s: &str, old: u16) -> u16 {
    s.parse::<u16>().unwrap_or(old)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}
