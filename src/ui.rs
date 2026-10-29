use crate::audio::{AudioDeviceInfo, AudioEngine, MODE_LOOPBACK_TEST};
use crate::config::{AppConfig, CallDirection, CallHistoryEntry, Contact, TransportProtocol};
use crate::sip::{CallPhase, RegistrationStatus, SipCommand, SipController};
use crate::theme::{
    self, action_button, call_control_button, card_frame, dial_key, elide, font, font_bold,
    font_mono, icon_tool_button, paint_icon, status_chip, tab_button, text_input, vu_meter, Icon,
    ACCENT_BLUE, BG_ACTIVE, BG_APP, BG_CARD, BG_INPUT, BG_PANEL, BORDER_FOCUS, BORDER_SUBTLE,
    FS_BODY, FS_CAPTION, FS_HEADING, FS_LABEL, FS_MICRO, FS_SUBHEAD, FS_TITLE, SP1, SP2, SP5,
    STATUS_AMBER, STATUS_GREEN, STATUS_RED, STATUS_RED_FILL, TEXT_MUTED, TEXT_PRIMARY,
    TEXT_SECONDARY,
};
use chrono::Local;
use eframe::egui::{self, CornerRadius, RichText, Sense, Stroke, Vec2};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Dialpad,
    Contacts,
    History,
    Settings,
    SipLogs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFilter {
    All,
    Outgoing,
    Incoming,
    Missed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFilter {
    All,
    Tx,
    Rx,
    Errors,
}

pub struct RustSipApp {
    config: AppConfig,
    audio: AudioEngine,
    sip: SipController,
    active_tab: ActiveTab,
    dial_input: String,
    show_password: bool,
    input_devices: Vec<AudioDeviceInfo>,
    output_devices: Vec<AudioDeviceInfo>,

    // Contatos
    contact_search: String,
    show_add_contact: bool,
    new_contact_name: String,
    new_contact_number: String,
    new_contact_notes: String,

    // Histórico
    history_filter: HistoryFilter,

    // Logs SIP
    log_filter: LogFilter,
    selected_log_index: Option<usize>,

    // Notificação / Toast
    status_toast: Option<(String, Instant)>,
}

impl RustSipApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::configure_style(&cc.egui_ctx);

        let mut config = AppConfig::load();
        let audio = AudioEngine::new();
        audio.state.set_mic_gain(config.audio.mic_gain);
        audio.state.set_speaker_volume(config.audio.speaker_volume);

        let sip = SipController::new(audio.clone());

        let input_devices = AudioEngine::list_input_devices();
        let output_devices = AudioEngine::list_output_devices();

        if let Some(ref saved_in) = config.audio.input_device {
            if !input_devices.iter().any(|d| &d.id == saved_in) {
                config.audio.input_device = None;
                config.save();
            }
        }
        if let Some(ref saved_out) = config.audio.output_device {
            if !output_devices.iter().any(|d| &d.id == saved_out) {
                config.audio.output_device = None;
                config.save();
            }
        }

        let active_tab = if config.account.is_configured() {
            ActiveTab::Dialpad
        } else {
            ActiveTab::Settings
        };

        if config.account.is_configured() && config.account.auto_connect {
            sip.send(SipCommand::Connect {
                account: config.account.clone(),
                input_dev: config.audio.input_device.clone(),
                output_dev: config.audio.output_device.clone(),
            });
        }

        Self {
            config,
            audio,
            sip,
            active_tab,
            dial_input: String::new(),
            show_password: false,
            input_devices,
            output_devices,
            contact_search: String::new(),
            show_add_contact: false,
            new_contact_name: String::new(),
            new_contact_number: String::new(),
            new_contact_notes: String::new(),
            history_filter: HistoryFilter::All,
            log_filter: LogFilter::All,
            selected_log_index: None,
            status_toast: None,
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.status_toast = Some((msg.into(), Instant::now()));
    }

    fn trigger_dial(&mut self, number: String) {
        let target = number.trim().to_string();
        if target.is_empty() {
            return;
        }
        if self.audio.state.mode.load(Ordering::Relaxed) == MODE_LOOPBACK_TEST {
            self.audio.stop_audio();
        }
        self.sip.send(SipCommand::Dial {
            target,
            input_dev: self.config.audio.input_device.clone(),
            output_dev: self.config.audio.output_device.clone(),
        });
        self.active_tab = ActiveTab::Dialpad;
    }

    fn press_keypad_digit(&mut self, digit: char, in_call: bool) {
        self.audio
            .play_keypad_dtmf(digit, self.config.audio.output_device.clone());
        if in_call {
            self.sip.send(SipCommand::SendDtmf(digit));
        } else {
            self.dial_input.push(digit);
        }
    }

    fn drain_completed_calls(&mut self) {
        let mut new_entries = Vec::new();
        if let Ok(mut q) = self.sip.state.completed_calls.lock() {
            while let Some(rec) = q.pop_front() {
                new_entries.push(rec);
            }
        }
        if !new_entries.is_empty() {
            for rec in new_entries {
                let contact_name = self
                    .config
                    .contacts
                    .iter()
                    .find(|c| c.number == rec.number)
                    .map(|c| c.name.clone())
                    .unwrap_or(rec.name);

                self.config.history.insert(
                    0,
                    CallHistoryEntry {
                        timestamp: Local::now().format("%d/%m/%Y %H:%M").to_string(),
                        number: rec.number,
                        name: contact_name,
                        direction: rec.direction,
                        duration_secs: rec.duration_secs,
                    },
                );
            }
            if self.config.history.len() > 100 {
                self.config.history.truncate(100);
            }
            self.config.save();
        }
    }
}

impl eframe::App for RustSipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(50));

        self.drain_completed_calls();

        let reg_status = self
            .sip
            .state
            .reg_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(RegistrationStatus::Disconnected);

        let call_phase = self
            .sip
            .state
            .call_phase
            .lock()
            .map(|p| p.clone())
            .unwrap_or(CallPhase::Idle);

        if matches!(call_phase, CallPhase::Incoming { .. })
            && self.active_tab != ActiveTab::SipLogs
        {
            self.active_tab = ActiveTab::Dialpad;
        }

        // Cabeçalho da aplicação (Status da Conta e Conexão)
        egui::TopBottomPanel::top("header_panel")
            .frame(
                egui::Frame::NONE
                    .fill(BG_PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                self.render_header(ui, &reg_status);
            });

        // Barra de Abas Corporativa
        egui::TopBottomPanel::top("tab_bar_panel")
            .frame(
                egui::Frame::NONE
                    .fill(BG_PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(10, 4)),
            )
            .show(ctx, |ui| {
                self.render_tab_bar(ui);
            });

        // Rodapé com Telemetria e Medidores de Áudio
        egui::TopBottomPanel::bottom("footer_panel")
            .frame(
                egui::Frame::NONE
                    .fill(BG_PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(14, 6)),
            )
            .show(ctx, |ui| {
                self.render_footer(ui);
            });

        // Painel Central
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BG_APP)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .show(ctx, |ui| {
                // Toast discreto de notificação
                if let Some((ref msg, ts)) = self.status_toast {
                    if ts.elapsed() < Duration::from_secs(3) {
                        egui::Frame::NONE
                            .fill(BG_CARD)
                            .stroke(Stroke::new(1.0_f32, BORDER_FOCUS))
                            .corner_radius(CornerRadius::same(5))
                            .inner_margin(egui::Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    paint_icon(ui.painter(), Icon::Check, ui.cursor().min + Vec2::new(6.0, 7.0), 12.0, STATUS_GREEN);
                                    ui.add_space(14.0);
                                    ui.label(RichText::new(msg).font(font_bold(FS_LABEL)).color(TEXT_PRIMARY));
                                });
                            });
                        ui.add_space(6.0);
                    }
                }

                // Card de Chamada Ativa (se houver)
                if !matches!(call_phase, CallPhase::Idle) {
                    self.render_call_card(ui, &call_phase);
                    ui.add_space(10.0);
                }

                match self.active_tab {
                    ActiveTab::Dialpad => self.render_dialpad_tab(ui, &call_phase),
                    ActiveTab::Contacts => self.render_contacts_tab(ui),
                    ActiveTab::History => self.render_history_tab(ui),
                    ActiveTab::Settings => self.render_settings_tab(ui),
                    ActiveTab::SipLogs => self.render_logs_tab(ui, ctx),
                }
            });
    }
}

impl RustSipApp {
    /// Cabeçalho Sério com Indicador Preciso de Status
    fn render_header(&mut self, ui: &mut egui::Ui, reg_status: &RegistrationStatus) {
        ui.horizontal(|ui| {
            let (status_dot, status_label, is_online) = match reg_status {
                RegistrationStatus::Registered => (STATUS_GREEN, "Registrado", true),
                RegistrationStatus::Connecting => (STATUS_AMBER, "Conectando...", false),
                RegistrationStatus::Disconnected => (TEXT_MUTED, "Desconectado", false),
                RegistrationStatus::Failed(err) => (STATUS_RED, err.as_str(), false),
            };

            // Botão PRIMEIRO, ancorado à direita (reserva seu espaço antes do texto —
            // senão o bloco de texto consome tudo e o botão é pintado por cima).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if is_online || matches!(reg_status, RegistrationStatus::Connecting) {
                    let btn = egui::Button::new(
                        RichText::new("Desconectar")
                            .font(font_bold(FS_CAPTION))
                            .color(STATUS_RED),
                    )
                    .fill(BG_CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .corner_radius(CornerRadius::same(theme::R_SM));

                    if ui.add(btn).clicked() {
                        self.sip.send(SipCommand::Disconnect);
                    }
                } else if self.config.account.is_configured() {
                    let btn = egui::Button::new(
                        RichText::new("Conectar")
                            .font(font_bold(FS_CAPTION))
                            .color(TEXT_PRIMARY),
                    )
                    .fill(ACCENT_BLUE)
                    .corner_radius(CornerRadius::same(theme::R_SM));

                    if ui.add(btn).clicked() {
                        self.sip.send(SipCommand::Connect {
                            account: self.config.account.clone(),
                            input_dev: self.config.audio.input_device.clone(),
                            output_dev: self.config.audio.output_device.clone(),
                        });
                    }
                }

                // Texto à esquerda, no espaço RESTANTE (left_to_right). O subtítulo
                // longo TRUNCA com reticências em vez de passar por baixo do botão.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // Ponto indicador de status
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.5, status_dot);

                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("RustSIP").font(font_bold(FS_SUBHEAD)).color(TEXT_PRIMARY));
                            status_chip(ui, self.config.account.transport.as_str(), ACCENT_BLUE);
                        });

                        let subtitle = if self.config.account.is_configured() {
                            format!("{}@{} • {}", self.config.account.username, self.config.account.effective_domain(), status_label)
                        } else {
                            "Configure sua conta SIP na aba Configurações".to_string()
                        };

                        ui.add(
                            egui::Label::new(RichText::new(subtitle).font(font(FS_CAPTION)).color(TEXT_SECONDARY))
                                .truncate(),
                        );
                    });
                });
            });
        });
    }

    /// Barra de Navegação Horizontal Limpa
    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        let tabs = [
            (ActiveTab::Dialpad, "Teclado", Icon::Keypad),
            (ActiveTab::Contacts, "Contatos", Icon::Users),
            (ActiveTab::History, "Histórico", Icon::Clock),
            (ActiveTab::Settings, "Conta", Icon::Gear),
            (ActiveTab::SipLogs, "Logs SIP", Icon::Terminal),
        ];

        let available_w = ui.available_width();
        // Desconta os (n-1) gaps de item_spacing.x que o ui.horizontal insere ENTRE
        // as abas — senão a soma (5 abas + 4 gaps) estoura e a 5ª ("Logs SIP") corta.
        let n = tabs.len() as f32;
        let gaps = ui.spacing().item_spacing.x * (n - 1.0);
        let tab_w = ((available_w - gaps) / n).max(0.0);

        ui.horizontal(|ui| {
            for (tab, label, icon) in tabs {
                if tab_button(ui, label, icon, self.active_tab == tab, tab_w).clicked() {
                    self.active_tab = tab;
                }
            }
        });
    }

    /// Rodapé com Níveis de Áudio e Métricas RTP
    fn render_footer(&mut self, ui: &mut egui::Ui) {
        let mic_lvl = self.audio.state.mic_level();
        let spk_lvl = self.audio.state.speaker_level();
        let tx_pkts = self.sip.state.rtp_stats.tx_packets.load(Ordering::Relaxed);
        let rx_pkts = self.sip.state.rtp_stats.rx_packets.load(Ordering::Relaxed);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Mic").font(font(FS_CAPTION)).color(TEXT_MUTED));
            vu_meter(ui, mic_lvl, 65.0, STATUS_GREEN);

            ui.add_space(SP2);
            ui.label(RichText::new("Fone").font(font(FS_CAPTION)).color(TEXT_MUTED));
            vu_meter(ui, spk_lvl, 65.0, ACCENT_BLUE);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("RTP: {} TX • {} RX", tx_pkts, rx_pkts))
                        .font(font_mono(FS_MICRO))
                        .color(TEXT_MUTED),
                );
            });
        });
    }

    /// Card Corporativo de Chamada Ativa
    fn render_call_card(&mut self, ui: &mut egui::Ui, call_phase: &CallPhase) {
        let border_color = match call_phase {
            CallPhase::Incoming { .. } => STATUS_GREEN,
            CallPhase::Dialing { .. } => ACCENT_BLUE,
            CallPhase::InCall { .. } => STATUS_GREEN,
            CallPhase::Idle => BORDER_SUBTLE,
        };

        egui::Frame::NONE
            .fill(BG_CARD)
            .stroke(Stroke::new(1.5_f32, border_color))
            .corner_radius(CornerRadius::same(theme::R_MD))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| match call_phase {
                CallPhase::Incoming {
                    remote_number,
                    remote_name,
                } => {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            status_chip(ui, "CHAMADA RECEBIDA", STATUS_GREEN);
                            ui.add_space(2.0);
                            ui.label(RichText::new(remote_name).font(font_bold(FS_HEADING)).color(TEXT_PRIMARY));
                            if remote_name != remote_number {
                                ui.label(RichText::new(remote_number).font(font(FS_LABEL)).color(TEXT_SECONDARY));
                            }
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if action_button(ui, "Recusar", Some(Icon::Hangup), STATUS_RED_FILL, 88.0, 32.0).clicked() {
                                self.sip.send(SipCommand::Hangup);
                            }
                            if action_button(ui, "Atender", Some(Icon::Phone), STATUS_GREEN, 88.0, 32.0).clicked() {
                                self.sip.send(SipCommand::Answer {
                                    input_dev: self.config.audio.input_device.clone(),
                                    output_dev: self.config.audio.output_device.clone(),
                                });
                            }
                        });
                    });
                }
                CallPhase::Dialing {
                    remote_number,
                    status_text,
                } => {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            status_chip(ui, status_text, ACCENT_BLUE);
                            ui.add_space(2.0);
                            ui.label(RichText::new(remote_number).font(font_bold(FS_HEADING)).color(TEXT_PRIMARY));
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if action_button(ui, "Cancelar", Some(Icon::Hangup), STATUS_RED_FILL, 92.0, 32.0).clicked() {
                                self.sip.send(SipCommand::Hangup);
                            }
                        });
                    });
                }
                CallPhase::InCall {
                    remote_number,
                    remote_name,
                    codec_name,
                    connected_at,
                } => {
                    let elapsed = connected_at.elapsed().as_secs();
                    let mins = elapsed / 60;
                    let secs = elapsed % 60;

                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                status_chip(ui, &format!("{:02}:{:02}", mins, secs), STATUS_GREEN);
                                status_chip(ui, codec_name, ACCENT_BLUE);
                            });
                            ui.add_space(2.0);
                            ui.label(RichText::new(remote_name).font(font_bold(FS_HEADING)).color(TEXT_PRIMARY));
                            if remote_name != remote_number {
                                ui.label(RichText::new(remote_number).font(font(FS_LABEL)).color(TEXT_SECONDARY));
                            }
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if action_button(ui, "Desligar", Some(Icon::Hangup), STATUS_RED_FILL, 92.0, 32.0).clicked() {
                                self.sip.send(SipCommand::Hangup);
                            }
                        });
                    });

                    ui.add_space(SP2);
                    ui.separator();
                    ui.add_space(SP1);

                    // Controles de áudio em chamada (Mudo e Espera) — ícones VETORIAIS.
                    let is_muted = self.audio.state.muted.load(Ordering::Relaxed);
                    let is_hold = self.audio.state.on_hold.load(Ordering::Relaxed);

                    ui.horizontal(|ui| {
                        // Dois botões + o item_spacing.x (= SP2) do layout horizontal entre eles
                        // = largura total. NÃO adicionar add_space extra (dobrava a folga → overflow).
                        let btn_w = (ui.available_width() - SP2) / 2.0;

                        // Mudo: Mic quando aberto, MicOff quando silenciado.
                        let mute_icon = if is_muted { Icon::MicOff } else { Icon::Mic };
                        let mute_label = if is_muted { "Mudo ativo" } else { "Silenciar" };
                        if call_control_button(ui, mute_icon, mute_label, is_muted, STATUS_AMBER, btn_w, 30.0)
                            .clicked()
                        {
                            self.audio.state.muted.store(!is_muted, Ordering::Relaxed);
                        }

                        // Espera: ação. Pause quando a chamada está ativa, Play (retomar) em espera.
                        let hold_icon = if is_hold { Icon::Play } else { Icon::Pause };
                        let hold_label = if is_hold { "Retomar" } else { "Pôr em espera" };
                        if call_control_button(ui, hold_icon, hold_label, is_hold, ACCENT_BLUE, btn_w, 30.0)
                            .clicked()
                        {
                            self.audio.state.on_hold.store(!is_hold, Ordering::Relaxed);
                        }
                    });
                }
                CallPhase::Idle => {}
            });
    }

    /// Aba 1: Teclado Numérico Profissional
    fn render_dialpad_tab(&mut self, ui: &mut egui::Ui, call_phase: &CallPhase) {
        let in_call = matches!(call_phase, CallPhase::InCall { .. });

        // Visor do Número
        egui::Frame::NONE
            .fill(BG_INPUT)
            .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Os botões backspace/limpar só aparecem com o campo preenchido.
                    // Reserva o espaço deles (2×28px + 2 gaps reais) SÓ nesse caso;
                    // vazio, o campo usa a largura cheia e o hint cabe inteiro.
                    let sp = ui.spacing().item_spacing.x;
                    let reserve = if self.dial_input.is_empty() {
                        0.0
                    } else {
                        28.0 * 2.0 + sp * 2.0
                    };
                    let resp = ui.add_sized(
                        Vec2::new((ui.available_width() - reserve).max(0.0), 32.0),
                        egui::TextEdit::singleline(&mut self.dial_input)
                            .font(font_bold(FS_TITLE))
                            .text_color(TEXT_PRIMARY)
                            .frame(false)
                            .hint_text(RichText::new("Número ou ramal").color(TEXT_MUTED)),
                    );

                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let target = self.dial_input.clone();
                        self.trigger_dial(target);
                    }

                    if !self.dial_input.is_empty() {
                        if icon_tool_button(ui, Icon::Backspace, 28.0, "Apagar dígito", false, BORDER_SUBTLE).clicked() {
                            self.dial_input.pop();
                        }
                        if icon_tool_button(ui, Icon::Close, 28.0, "Limpar", false, BORDER_SUBTLE).clicked() {
                            self.dial_input.clear();
                        }
                    }
                });
            });

        ui.add_space(10.0);

        // Grade 3x4 do Teclado Numérico
        let keys: [[(&str, &str, char); 3]; 4] = [
            [("1", "", '1'), ("2", "ABC", '2'), ("3", "DEF", '3')],
            [("4", "GHI", '4'), ("5", "JKL", '5'), ("6", "MNO", '6')],
            [("7", "PQRS", '7'), ("8", "TUV", '8'), ("9", "WXYZ", '9')],
            [("*", "", '*'), ("0", "+", '0'), ("#", "", '#')],
        ];

        let available_w = ui.available_width();
        let spacing = 6.0;
        let key_w = (available_w - (spacing * 2.0)) / 3.0;
        let key_h = 50.0;

        for row in keys {
            ui.horizontal(|ui| {
                // Zera o item_spacing do layout: o ÚNICO gap entre teclas é o nosso
                // add_space(spacing) controlado, então key_w = (avail - spacing*2)/3
                // fecha EXATO (senão item_spacing.x extra estoura a 3ª coluna).
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, (main_lbl, sub_lbl, ch)) in row.into_iter().enumerate() {
                    if i > 0 {
                        ui.add_space(spacing);
                    }
                    if dial_key(ui, main_lbl, sub_lbl, key_w, key_h).clicked() {
                        self.press_keypad_digit(ch, in_call);
                    }
                }
            });
            ui.add_space(spacing);
        }

        ui.add_space(SP1);

        // Botão de Discar
        if matches!(call_phase, CallPhase::Idle) {
            let call_resp = action_button(
                ui,
                "Realizar Chamada",
                Some(Icon::Phone),
                STATUS_GREEN,
                ui.available_width(),
                42.0,
            );

            if call_resp.clicked() {
                let target = self.dial_input.clone();
                self.trigger_dial(target);
            }
        }
    }

    /// Aba 2: Lista de Contatos
    fn render_contacts_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Reserva p/ o botão "novo contato" (28px) + o gap real de item_spacing.
            let sp = ui.spacing().item_spacing.x;
            let search_w = (ui.available_width() - (28.0 + sp)).max(0.0);
            egui::Frame::NONE
                .fill(BG_INPUT)
                .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        paint_icon(ui.painter(), Icon::Search, ui.cursor().min + Vec2::new(6.0, 7.0), 12.0, TEXT_MUTED);
                        ui.add_space(14.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.contact_search)
                                .frame(false)
                                .font(font(FS_LABEL))
                                .text_color(TEXT_PRIMARY)
                                .hint_text(RichText::new("Buscar contato...").color(TEXT_MUTED))
                                .desired_width(search_w - 30.0),
                        );
                    });
                });

            let add_btn = icon_tool_button(
                ui,
                if self.show_add_contact { Icon::Close } else { Icon::Plus },
                28.0,
                "Novo contato",
                self.show_add_contact,
                ACCENT_BLUE,
            );
            if add_btn.clicked() {
                self.show_add_contact = !self.show_add_contact;
            }
        });

        if self.show_add_contact {
            ui.add_space(SP2);
            card_frame().show(ui, |ui| {
                ui.label(RichText::new("Adicionar Contato").font(font_bold(FS_BODY)).color(TEXT_PRIMARY));
                ui.add_space(6.0);
                text_input(ui, "Nome:", &mut self.new_contact_name, "Ex: Suporte Técnico", false);
                ui.add_space(SP1);
                text_input(ui, "Número / Ramal:", &mut self.new_contact_number, "Ex: 2004 ou (31) 9...", false);
                ui.add_space(SP1);
                text_input(ui, "Observação (opcional):", &mut self.new_contact_notes, "Ex: Ramal interno", false);
                ui.add_space(SP2);

                if action_button(ui, "Salvar Contato", Some(Icon::Check), STATUS_GREEN, ui.available_width(), 32.0).clicked()
                    && !self.new_contact_name.trim().is_empty()
                    && !self.new_contact_number.trim().is_empty()
                {
                    self.config.contacts.push(Contact {
                        id: format!("{}", Local::now().timestamp_millis()),
                        name: self.new_contact_name.trim().to_string(),
                        number: self.new_contact_number.trim().to_string(),
                        notes: self.new_contact_notes.trim().to_string(),
                    });
                    self.config.save();
                    self.new_contact_name.clear();
                    self.new_contact_number.clear();
                    self.new_contact_notes.clear();
                    self.show_add_contact = false;
                    self.show_toast("Contato adicionado.");
                }
            });
        }

        ui.add_space(SP2);

        let query = self.contact_search.trim().to_lowercase();
        let filtered: Vec<(usize, &Contact)> = self
            .config
            .contacts
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                query.is_empty()
                    || c.name.to_lowercase().contains(&query)
                    || c.number.contains(&query)
            })
            .collect();

        let mut dial_target: Option<String> = None;
        let mut delete_idx: Option<usize> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            if filtered.is_empty() {
                ui.add_space(SP5);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Nenhum contato encontrado").font(font(FS_LABEL)).color(TEXT_MUTED));
                });
            }

            for (idx, contact) in filtered {
                card_frame()
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&contact.name).font(font_bold(FS_BODY)).color(TEXT_PRIMARY));
                                ui.label(RichText::new(&contact.number).font(font(FS_LABEL)).color(ACCENT_BLUE));
                                if !contact.notes.is_empty() {
                                    ui.label(RichText::new(&contact.notes).font(font(FS_CAPTION)).color(TEXT_MUTED));
                                }
                            });

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if icon_tool_button(ui, Icon::Trash, 26.0, "Excluir", false, BORDER_SUBTLE).clicked() {
                                    delete_idx = Some(idx);
                                }
                                if action_button(ui, "Ligar", Some(Icon::Phone), STATUS_GREEN, 72.0, 26.0).clicked() {
                                    dial_target = Some(contact.number.clone());
                                }
                            });
                        });
                    });
                ui.add_space(SP1);
            }
        });

        if let Some(idx) = delete_idx {
            self.config.contacts.remove(idx);
            self.config.save();
        }
        if let Some(num) = dial_target {
            self.dial_input = num.clone();
            self.trigger_dial(num);
        }
    }

    /// Aba 3: Histórico de Chamadas
    fn render_history_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let filters = [
                (HistoryFilter::All, "Todas"),
                (HistoryFilter::Outgoing, "Efetuadas"),
                (HistoryFilter::Incoming, "Recebidas"),
                (HistoryFilter::Missed, "Perdidas"),
            ];

            for (f, lbl) in filters {
                let is_sel = self.history_filter == f;
                let btn = egui::Button::new(
                    RichText::new(lbl)
                        .font(font_bold(FS_CAPTION))
                        .color(if is_sel { TEXT_PRIMARY } else { TEXT_SECONDARY }),
                )
                .fill(if is_sel { BG_ACTIVE } else { BG_CARD })
                .stroke(Stroke::new(1.0_f32, if is_sel { BORDER_FOCUS } else { BORDER_SUBTLE }))
                .corner_radius(CornerRadius::same(4));

                if ui.add(btn).clicked() {
                    self.history_filter = f;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.config.history.is_empty()
                    && icon_tool_button(ui, Icon::Trash, 26.0, "Limpar histórico", false, BORDER_SUBTLE).clicked()
                {
                    self.config.history.clear();
                    self.config.save();
                    self.show_toast("Histórico limpo.");
                }
            });
        });

        ui.add_space(SP2);

        let filtered: Vec<&CallHistoryEntry> = self
            .config
            .history
            .iter()
            .filter(|e| match self.history_filter {
                HistoryFilter::All => true,
                HistoryFilter::Outgoing => matches!(e.direction, CallDirection::Outgoing),
                HistoryFilter::Incoming => matches!(e.direction, CallDirection::Incoming),
                HistoryFilter::Missed => matches!(e.direction, CallDirection::Missed),
            })
            .collect();

        let mut redial_target: Option<String> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            if filtered.is_empty() {
                ui.add_space(SP5);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Nenhuma chamada no histórico").font(font(FS_LABEL)).color(TEXT_MUTED));
                });
            }

            for entry in filtered {
                let (dir_icon, dir_color, dir_label) = match entry.direction {
                    CallDirection::Outgoing => (Icon::ArrowOut, ACCENT_BLUE, "Efetuada"),
                    CallDirection::Incoming => (Icon::ArrowIn, STATUS_GREEN, "Recebida"),
                    CallDirection::Missed => (Icon::ArrowMissed, STATUS_RED, "Perdida"),
                };
                let mins = entry.duration_secs / 60;
                let secs = entry.duration_secs % 60;

                card_frame()
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            paint_icon(ui.painter(), dir_icon, ui.cursor().min + Vec2::new(8.0, 10.0), 14.0, dir_color);
                            ui.add_space(18.0);

                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&entry.name).font(font_bold(FS_BODY)).color(TEXT_PRIMARY));
                                    status_chip(ui, dir_label, dir_color);
                                });
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&entry.number).font(font(FS_CAPTION)).color(TEXT_SECONDARY));
                                    ui.label(RichText::new("•").font(font(FS_MICRO)).color(TEXT_MUTED));
                                    ui.label(RichText::new(&entry.timestamp).font(font(FS_MICRO)).color(TEXT_MUTED));
                                    if entry.duration_secs > 0 {
                                        ui.label(RichText::new("•").font(font(FS_MICRO)).color(TEXT_MUTED));
                                        ui.label(RichText::new(format!("{:02}:{:02}", mins, secs)).font(font(FS_MICRO)).color(ACCENT_BLUE));
                                    }
                                });
                            });

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if action_button(ui, "Rediscar", Some(Icon::Phone), STATUS_GREEN, 82.0, 26.0).clicked() {
                                    redial_target = Some(entry.number.clone());
                                }
                            });
                        });
                    });
                ui.add_space(SP1);
            }
        });

        if let Some(target) = redial_target {
            self.dial_input = target.clone();
            self.trigger_dial(target);
        }
    }

    /// Aba 4: Configurações da Conta e Áudio PipeWire
    fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // Seção 1: Servidor SIP (FreeSWITCH)
            card_frame().show(ui, |ui| {
                ui.label(RichText::new("Servidor SIP (FreeSWITCH)").font(font_bold(FS_BODY)).color(TEXT_PRIMARY));
                ui.add_space(SP2);

                text_input(ui, "Endereço do Servidor:", &mut self.config.account.server, "Ex: voipotimizar.otimizartecnologia.com.br", false);
                ui.add_space(SP1);
                text_input(ui, "Domínio / Realm:", &mut self.config.account.domain, "Opcional (padrão = host do servidor)", false);
                ui.add_space(SP1);
                text_input(ui, "Usuário / Ramal:", &mut self.config.account.username, "Ex: 2003", false);
                ui.add_space(SP1);
                text_input(ui, "ID de Autenticação (Auth ID):", &mut self.config.account.auth_id, "Opcional (padrão = ramal)", false);
                ui.add_space(SP1);

                // Campo de Senha com alternador
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Senha SIP:").font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let eye_icon = if self.show_password { Icon::EyeOff } else { Icon::Eye };
                        if icon_tool_button(ui, eye_icon, 20.0, "Mostrar senha", false, BORDER_SUBTLE).clicked() {
                            self.show_password = !self.show_password;
                        }
                    });
                });
                ui.add_space(-2.0);
                egui::Frame::NONE
                    .fill(BG_INPUT)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 7))
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.account.password)
                                .frame(false)
                                .password(!self.show_password)
                                .font(font(FS_BODY))
                                .text_color(TEXT_PRIMARY)
                                .desired_width(f32::INFINITY),
                        );
                    });

                ui.add_space(SP1);
                text_input(ui, "Nome de Exibição:", &mut self.config.account.display_name, "Ex: João Rocha", false);

                ui.add_space(SP2);
                ui.label(RichText::new("Protocolo de Transporte:").font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
                ui.horizontal(|ui| {
                    let udp_sel = self.config.account.transport == TransportProtocol::Udp;
                    if ui.selectable_label(udp_sel, "UDP (Padrão)").clicked() {
                        self.config.account.transport = TransportProtocol::Udp;
                    }
                    let tcp_sel = self.config.account.transport == TransportProtocol::Tcp;
                    if ui.selectable_label(tcp_sel, "TCP").clicked() {
                        self.config.account.transport = TransportProtocol::Tcp;
                    }
                });

                ui.add_space(SP2);
                ui.checkbox(&mut self.config.account.use_stun, "Descobrir IP público RTP via STUN");
                if self.config.account.use_stun {
                    ui.add_space(SP1);
                    text_input(ui, "Servidor STUN:", &mut self.config.account.stun_server, "stun.l.google.com:19302", false);
                }

                ui.add_space(10.0);
                if action_button(ui, "Salvar e Reconectar", Some(Icon::Power), ACCENT_BLUE, ui.available_width(), 34.0).clicked() {
                    self.config.save();
                    if self.config.account.is_configured() {
                        self.sip.send(SipCommand::Connect {
                            account: self.config.account.clone(),
                            input_dev: self.config.audio.input_device.clone(),
                            output_dev: self.config.audio.output_device.clone(),
                        });
                        self.show_toast("Conectando ao servidor...");
                    }
                }
            });

            ui.add_space(10.0);

            // Seção 2: Áudio PipeWire
            card_frame().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Dispositivos de Áudio (PipeWire Linux)").font(font_bold(FS_BODY)).color(TEXT_PRIMARY));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_tool_button(ui, Icon::Refresh, 24.0, "Atualizar lista", false, BORDER_SUBTLE).clicked() {
                            self.input_devices = AudioEngine::list_input_devices();
                            self.output_devices = AudioEngine::list_output_devices();
                            self.show_toast("Lista de áudio atualizada.");
                        }
                    });
                });
                ui.add_space(SP2);

                // Microfone
                ui.label(RichText::new("Entrada de Microfone:").font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
                let def_in = self
                    .input_devices
                    .iter()
                    .find(|d| d.is_default)
                    .map(|d| format!("Padrão do Sistema ({})", d.description))
                    .unwrap_or_else(|| "Padrão do Sistema".to_string());

                let cur_in = match &self.config.audio.input_device {
                    Some(id) => self
                        .input_devices
                        .iter()
                        .find(|d| &d.id == id)
                        .map(|d| d.display_label())
                        .unwrap_or_else(|| id.clone()),
                    None => def_in.clone(),
                };

                // Largura com folga da borda do card; o texto fechado ELIDE (a seta
                // do combo consome ~24px, por isso o -28 na largura de texto).
                let combo_w = (ui.available_width() - SP1).max(0.0);
                let cur_in = elide(ui, &cur_in, font(FS_BODY), combo_w - 28.0);
                egui::ComboBox::from_id_salt("combo_in_dev")
                    .selected_text(RichText::new(cur_in).color(TEXT_PRIMARY))
                    .width(combo_w)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(self.config.audio.input_device.is_none(), &def_in).clicked() {
                            self.config.audio.input_device = None;
                            self.config.save();
                            self.audio.reload_devices_if_active(None, self.config.audio.output_device.clone());
                        }
                        for dev in &self.input_devices {
                            let sel = self.config.audio.input_device.as_deref() == Some(&dev.id);
                            if ui.selectable_label(sel, dev.display_label()).clicked() {
                                self.config.audio.input_device = Some(dev.id.clone());
                                self.config.save();
                                self.audio.reload_devices_if_active(Some(dev.id.clone()), self.config.audio.output_device.clone());
                            }
                        }
                    });

                ui.add_space(SP2);

                // Fone / Saída
                ui.label(RichText::new("Saída de Áudio:").font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
                let def_out = self
                    .output_devices
                    .iter()
                    .find(|d| d.is_default)
                    .map(|d| format!("Padrão do Sistema ({})", d.description))
                    .unwrap_or_else(|| "Padrão do Sistema".to_string());

                let cur_out = match &self.config.audio.output_device {
                    Some(id) => self
                        .output_devices
                        .iter()
                        .find(|d| &d.id == id)
                        .map(|d| d.display_label())
                        .unwrap_or_else(|| id.clone()),
                    None => def_out.clone(),
                };

                let combo_w = (ui.available_width() - SP1).max(0.0);
                let cur_out = elide(ui, &cur_out, font(FS_BODY), combo_w - 28.0);
                egui::ComboBox::from_id_salt("combo_out_dev")
                    .selected_text(RichText::new(cur_out).color(TEXT_PRIMARY))
                    .width(combo_w)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(self.config.audio.output_device.is_none(), &def_out).clicked() {
                            self.config.audio.output_device = None;
                            self.config.save();
                            self.audio.reload_devices_if_active(self.config.audio.input_device.clone(), None);
                        }
                        for dev in &self.output_devices {
                            let sel = self.config.audio.output_device.as_deref() == Some(&dev.id);
                            if ui.selectable_label(sel, dev.display_label()).clicked() {
                                self.config.audio.output_device = Some(dev.id.clone());
                                self.config.save();
                                self.audio.reload_devices_if_active(self.config.audio.input_device.clone(), Some(dev.id.clone()));
                            }
                        }
                    });

                ui.add_space(10.0);

                // Sliders de ganho e volume
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Ganho Mic:").font(font(FS_LABEL)).color(TEXT_SECONDARY));
                    if ui.add(egui::Slider::new(&mut self.config.audio.mic_gain, 0.2..=3.0).text("x")).changed() {
                        self.audio.state.set_mic_gain(self.config.audio.mic_gain);
                        self.config.save();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Volume:").font(font(FS_LABEL)).color(TEXT_SECONDARY));
                    if ui.add(egui::Slider::new(&mut self.config.audio.speaker_volume, 0.1..=2.5).text("x")).changed() {
                        self.audio.state.set_speaker_volume(self.config.audio.speaker_volume);
                        self.config.save();
                    }
                });

                ui.add_space(10.0);

                // Teste de Loopback
                let is_loopback = self.audio.state.mode.load(Ordering::Relaxed) == MODE_LOOPBACK_TEST;
                let (test_txt, test_col) = if is_loopback {
                    ("Parar teste de áudio", STATUS_RED_FILL)
                } else {
                    ("Testar áudio (loopback)", ACCENT_BLUE)
                };

                if action_button(ui, test_txt, Some(Icon::Speaker), test_col, ui.available_width(), 32.0).clicked() {
                    if is_loopback {
                        self.audio.stop_audio();
                    } else {
                        self.audio.start_loopback_test(
                            self.config.audio.input_device.clone(),
                            self.config.audio.output_device.clone(),
                        );
                    }
                }
            });
        });
    }

    /// Aba 5: Inspetor Técnico de Logs SIP
    fn render_logs_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let logs_snapshot: Vec<_> = self
            .sip
            .state
            .logs
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();

        let pub_addr = self
            .sip
            .state
            .detected_public_addr
            .lock()
            .ok()
            .and_then(|g| g.clone());

        ui.horizontal(|ui| {
            let filters = [
                (LogFilter::All, "Todos"),
                (LogFilter::Tx, "TX"),
                (LogFilter::Rx, "RX"),
                (LogFilter::Errors, "Erros"),
            ];

            // Ícones copiar/limpar PRIMEIRO, ancorados à direita (reservam seu espaço);
            // os filtros fluem no restante à esquerda e não colidem com eles.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_tool_button(ui, Icon::Trash, 26.0, "Limpar logs", false, BORDER_SUBTLE).clicked() {
                    if let Ok(mut q) = self.sip.state.logs.lock() {
                        q.clear();
                    }
                    self.selected_log_index = None;
                }
                if icon_tool_button(ui, Icon::Copy, 26.0, "Copiar tudo", false, BORDER_SUBTLE).clicked() {
                    let mut full = String::new();
                    for l in &logs_snapshot {
                        full.push_str(&format!("[{}] {} {}\n{}\n---\n", l.timestamp, l.direction, l.summary, l.raw));
                    }
                    ctx.copy_text(full);
                    self.show_toast("Logs copiados.");
                }

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    for (f, lbl) in filters {
                        let is_sel = self.log_filter == f;
                        let btn = egui::Button::new(
                            RichText::new(lbl)
                                .font(font_bold(FS_CAPTION))
                                .color(if is_sel { TEXT_PRIMARY } else { TEXT_SECONDARY }),
                        )
                        .fill(if is_sel { BG_ACTIVE } else { BG_CARD })
                        .stroke(Stroke::new(1.0_f32, if is_sel { BORDER_FOCUS } else { BORDER_SUBTLE }))
                        .corner_radius(CornerRadius::same(4));

                        if ui.add(btn).clicked() {
                            self.log_filter = f;
                        }
                    }
                });
            });
        });

        if let Some(addr) = pub_addr {
            ui.add_space(2.0);
            ui.label(RichText::new(format!("NAT Público Detectado: {}", addr)).font(font_mono(FS_MICRO)).color(ACCENT_BLUE));
        }

        ui.add_space(6.0);

        let filtered_logs: Vec<(usize, &crate::sip::SipLogEntry)> = logs_snapshot
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| match self.log_filter {
                LogFilter::All => true,
                LogFilter::Tx => entry.direction == "TX",
                LogFilter::Rx => entry.direction == "RX",
                LogFilter::Errors => entry.direction == "ERR",
            })
            .collect();

        card_frame().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("sip_logs_list_scroll")
                .max_height(200.0)
                .show(ui, |ui| {
                    if filtered_logs.is_empty() {
                        ui.label(RichText::new("Nenhum registro para o filtro atual.").font(font(FS_LABEL)).color(TEXT_MUTED));
                    }

                    for (idx, entry) in filtered_logs {
                        let badge_col = match entry.direction {
                            "TX" => ACCENT_BLUE,
                            "RX" => STATUS_GREEN,
                            "ERR" => STATUS_RED,
                            _ => STATUS_AMBER,
                        };
                        let is_sel = self.selected_log_index == Some(idx);

                        let row_resp = ui.horizontal(|ui| {
                            status_chip(ui, entry.direction, badge_col);
                            ui.label(RichText::new(&entry.timestamp).font(font_mono(FS_MICRO)).color(TEXT_MUTED));
                            // Summary longo TRUNCA com reticências — não empurra a largura da linha.
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&entry.summary)
                                        .font(font(FS_CAPTION))
                                        .color(if is_sel { TEXT_PRIMARY } else { TEXT_SECONDARY }),
                                )
                                .truncate(),
                            );
                        }).response;

                        if row_resp.interact(Sense::click()).clicked() {
                            self.selected_log_index = Some(idx);
                        }
                    }
                });
        });

        ui.add_space(6.0);

        // Painel de Detalhes do Pacote Selecionado
        let raw_content = self
            .selected_log_index
            .and_then(|i| logs_snapshot.get(i))
            .or_else(|| logs_snapshot.last())
            .map(|e| e.raw.as_str())
            .unwrap_or("Selecione um evento para exibir o pacote SIP bruto.");

        card_frame().show(ui, |ui| {
            ui.label(RichText::new("Mensagem SIP Bruta:").font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
            ui.add_space(SP1);
            egui::ScrollArea::vertical()
                .id_salt("sip_raw_scroll")
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut raw_content.to_string())
                            .font(font_mono(FS_MICRO))
                            .text_color(TEXT_PRIMARY)
                            .desired_width(ui.available_width())
                            .desired_rows(8)
                            .frame(false),
                    );
                });
        });
    }
}
