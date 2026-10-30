use crate::audio::{AudioDeviceInfo, AudioEngine, MODE_LOOPBACK_TEST};
use crate::config::{AppConfig, CallDirection, CallHistoryEntry, Contact, TransportProtocol};
use crate::ipc::ClientMsg;
use crate::sip::{CallPhase, RegistrationStatus, SipController};
use crate::theme::{
    self, action_button, call_control_button, card_frame, dial_key, elide, font, font_bold,
    font_mono, icon_tool_button, paint_icon, status_chip, tab_button, text_input, vu_meter, Icon,
    ACCENT_BLUE, BG_ACTIVE, BG_APP, BG_CARD, BG_INPUT, BG_PANEL, BORDER_FOCUS, BORDER_SUBTLE,
    FS_BODY, FS_CAPTION, FS_HEADING, FS_LABEL, FS_MICRO, FS_SUBHEAD, FS_TITLE, SP1, SP2, SP5,
    STATUS_AMBER, STATUS_GREEN, STATUS_RED, STATUS_RED_FILL, TEXT_MUTED, TEXT_PRIMARY,
    TEXT_SECONDARY,
};
use eframe::egui::{self, CornerRadius, RichText, Sense, Stroke, UiBuilder, Vec2};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// Animações sóbrias (app corporativo): transições curtas ease-out, sem bounce.
// ANIM_ENABLED=false desliga tudo (fallback de reduced-motion).
const ANIM_ENABLED: bool = true;
const ANIM_TAB_SECS: f32 = 0.22; // slide+fade da troca de aba (~220ms, notável)
const ANIM_TAB_DX: f32 = 28.0; // deslocamento horizontal do slide (px, sóbrio)
const ANIM_OPEN_SECS: f32 = 0.22; // fade-in global na abertura (~220ms)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActiveTab {
    Dialpad,
    Contacts,
    History,
    Settings,
    SipLogs,
}

impl ActiveTab {
    /// Índice estável na ordem da tab bar — usado p/ direção do slide de troca.
    fn index(self) -> i32 {
        match self {
            ActiveTab::Dialpad => 0,
            ActiveTab::Contacts => 1,
            ActiveTab::History => 2,
            ActiveTab::Settings => 3,
            ActiveTab::SipLogs => 4,
        }
    }
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

    // Animação: instante da primeira renderização (fade-in de abertura).
    started_at: Instant,

    // Animação de troca de aba (slide direcional + fade).
    last_tab: ActiveTab,
    tab_anim_start: Instant,
    tab_from_right: bool, // true = aba nova entra da direita (avançou)

    // F2: a janela é SEMPRE um CLIENTE do daemon. O config (conta/contatos/
    // histórico/áudio) vive no espelho `remote_config` que a thread de IPC
    // atualiza; o update() copia dele por frame. As AÇÕES da UI não tocam mais
    // SipController/AudioEngine/config locais — enviam um ClientMsg via `client`.
    remote_config: crate::remote::RemoteConfig,
    client: crate::remote::DaemonClient,

    // F3: sinais de controle do daemon (Raise/Shutdown/tray_available) + ctx
    // para a thread de IPC acordar a janela. A UI thread é a única que fala com
    // o viewport do egui, então ela POLLA estes flags por frame e age.
    control: std::sync::Arc<crate::remote::WindowControl>,
    // F3: true depois que já pedimos o fechamento do viewport (Shutdown ou
    // fallback sem tray), para não reenviar Close/Quit a cada frame enquanto o
    // egui processa o fechamento.
    closing: bool,

    // L6: a conta e o áudio são FORMULÁRIO (o usuário digita/arrasta em
    // `self.config.account`/`self.config.audio`). Copiar o espelho inteiro a
    // cada tick apagaria o que ele está digitando. Então só puxamos conta/áudio
    // do espelho quando o DAEMON os muda (frame Account/AudioCfg): guardamos o
    // último valor visto do espelho e, quando ele difere, copiamos uma vez.
    // Contatos/histórico (listas de exibição, não editadas in-place) são
    // copiados sempre.
    prev_mirror_account: crate::config::SipAccountConfig,
    prev_mirror_audio: crate::config::AudioConfig,
}

impl RustSipApp {
    /// Construtor da janela CLIENTE (F2, modo default). A janela NÃO é dona de
    /// núcleo algum: recebe os estados ESPELHO (alimentados pela thread de IPC a
    /// partir das `ServerMsg` do daemon) e um `DaemonClient` para ENVIAR os
    /// comandos. O `SipController` é INERTE (sem supervisor/socket/REGISTER) e o
    /// `AudioEngine` é inerte (`from_state`, nunca abre PipeWire — fecha o A1).
    /// Reusa a MESMA struct/render da janela de antes: só muda a FONTE do estado
    /// (espelho) e o DESTINO das ações (IPC).
    pub fn new_remote(
        cc: &eframe::CreationContext<'_>,
        sip_state: std::sync::Arc<crate::sip::SipSharedState>,
        audio_state: std::sync::Arc<crate::audio::AudioSharedState>,
        remote_config: crate::remote::RemoteConfig,
        client: crate::remote::DaemonClient,
        control: std::sync::Arc<crate::remote::WindowControl>,
    ) -> Self {
        theme::configure_style(&cc.egui_ctx);

        // Snapshot inicial do config espelho (vazio até o Welcome chegar; o
        // update() o recopia a cada frame).
        let config = remote_config
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        // Base dos edge-triggers de conta/áudio (ver campos prev_mirror_*).
        let config_account_snapshot = config.account.clone();
        let config_audio_snapshot = config.audio.clone();

        let audio = AudioEngine::from_state(audio_state);
        let sip = SipController::new_inert(sip_state);

        // Enumera dispositivos localmente APENAS para listar nos combos (pactl
        // list é leitura pura — NÃO abre stream PipeWire, não viola o A1). A
        // TROCA de device envia ClientMsg::Set*Device; o daemon é quem aplica.
        let input_devices = AudioEngine::list_input_devices();
        let output_devices = AudioEngine::list_output_devices();

        let active_tab = if config.account.is_configured() {
            ActiveTab::Dialpad
        } else {
            ActiveTab::Settings
        };

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
            started_at: Instant::now(),
            last_tab: active_tab,
            tab_anim_start: Instant::now(),
            tab_from_right: true,
            remote_config,
            client,
            control,
            closing: false,
            prev_mirror_account: config_account_snapshot,
            prev_mirror_audio: config_audio_snapshot,
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
        // O daemon encerra o loopback antes de discar (apply_command::Dial);
        // aqui só envia o comando. A UI não toca mais o AudioEngine.
        self.client.send(ClientMsg::Dial { target });
        self.active_tab = ActiveTab::Dialpad;
    }

    fn press_keypad_digit(&mut self, digit: char, in_call: bool) {
        // O tom DTMF local e o DTMF SIP são feitos pelo DAEMON (ele é o dono do
        // áudio e da sessão). A UI só envia o dígito e, fora de chamada, compõe
        // o número no visor local.
        self.client.send(ClientMsg::KeypadDigit(digit));
        if !in_call {
            self.dial_input.push(digit);
        }
    }
}

impl eframe::App for RustSipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(50));

        // F3 — sinais de controle do daemon (polling por frame; os flags são
        // setados pela thread de IPC, que também pede repaint para acordar aqui).
        //
        // (a) Shutdown (item 4): o daemon vai encerrar. Fecha a janela de
        //     verdade. `closing` evita reenviar Close a cada frame.
        if self.control.shutdown.load(std::sync::atomic::Ordering::Relaxed) && !self.closing {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // (b) Raise (item 2): tray "Mostrar" com janela viva, ou auto-show de
        //     chamada entrando. Traz à frente. No Wayland o Focus pode ser
        //     barrado pelo anti-foco do KWin (a janela só pisca na barra) — é
        //     limite de plataforma; RequestUserAttention é a mitigação. Visible
        //     é defensivo (o modelo não oculta a janela, mas não custa garantir).
        if self.control.raise.swap(false, std::sync::atomic::Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        // (c) Fallback sem tray (item 5): se NÃO há bandeja, fechar a janela
        //     deve encerrar o daemon também (senão o app fica inacessível em
        //     background). Detecta o pedido de fechar do WM/usuário e, sem tray,
        //     manda Quit ao daemon ANTES de deixar o viewport fechar. Com tray,
        //     fechar só encerra o processo-janela (o daemon segue vivo).
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            let has_tray = self
                .control
                .tray_available
                .load(std::sync::atomic::Ordering::Relaxed);
            if !has_tray {
                self.closing = true;
                self.client.send(ClientMsg::Quit);
            }
            // Em ambos os casos deixamos o fechamento do viewport prosseguir
            // (não cancelamos). Sem tray, o Quit derruba o daemon; com tray, o
            // daemon permanece e o tray reabre a janela depois.
        }

        // F2: a janela é cliente do daemon. Recopia do espelho (mantido pela
        // thread de IPC) a cada frame, com cuidado para não apagar o FORMULÁRIO:
        //   - contatos/histórico: listas de exibição → copia sempre;
        //   - conta/áudio: são formulário (o usuário digita/arrasta) → copia só
        //     quando o DAEMON os mudou (Account/AudioCfg mudam o espelho), via
        //     edge-trigger contra prev_mirror_*. O daemon é o dono do config; a
        //     janela nunca grava em disco.
        if let Ok(cfg) = self.remote_config.lock() {
            self.config.contacts = cfg.contacts.clone();
            self.config.history = cfg.history.clone();
            if cfg.account != self.prev_mirror_account {
                self.config.account = cfg.account.clone();
                self.prev_mirror_account = cfg.account.clone();
            }
            if cfg.audio != self.prev_mirror_audio {
                self.config.audio = cfg.audio.clone();
                self.prev_mirror_audio = cfg.audio.clone();
            }
            self.config.password_needs_reentry = cfg.password_needs_reentry;
        }

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
                // Fade-in global de abertura (~220ms, uma vez). set_opacity no root
                // do painel central escurece tudo suavemente no primeiro quarto de segundo.
                if ANIM_ENABLED {
                    let e = self.started_at.elapsed().as_secs_f32();
                    if e < ANIM_OPEN_SECS {
                        let a = (e / ANIM_OPEN_SECS).clamp(0.0, 1.0);
                        ui.set_opacity(a);
                        ctx.request_repaint(); // mantém o fade fluido até completar
                    }
                }
                // F3 (item 7): banner de serviço caído. O supervisor de IPC
                // (remote.rs) marca `broken` no EOF/erro do socket e, em
                // paralelo, tenta reconectar a cada 1s (respawnando o daemon uma
                // vez por tentativa via ensure_daemon). Aqui só REFLETIMOS o
                // estado — reusa o visual de card/toast do tema, sem componente
                // novo. Enquanto `broken`, pedimos repaint para o banner sumir
                // assim que a reconexão limpar o flag.
                if self.client.is_broken() {
                    ctx.request_repaint_after(Duration::from_millis(250));
                    egui::Frame::NONE
                        .fill(BG_CARD)
                        .stroke(Stroke::new(1.0_f32, STATUS_AMBER))
                        .corner_radius(CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                paint_icon(
                                    ui.painter(),
                                    Icon::Refresh,
                                    ui.cursor().min + Vec2::new(6.0, 7.0),
                                    12.0,
                                    STATUS_AMBER,
                                );
                                ui.add_space(14.0);
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("Serviço RustSIP parado — reconectando…")
                                            .font(font_bold(FS_LABEL))
                                            .color(TEXT_PRIMARY),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Se persistir, veja ~/.config/rustsip/daemon.log",
                                        )
                                        .font(font(FS_MICRO))
                                        .color(TEXT_MUTED),
                                    );
                                });
                            });
                        });
                    ui.add_space(6.0);
                }
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

                // Troca de aba: detecta a mudança, define a direção pelo índice e
                // reinicia o relógio da animação.
                if self.active_tab != self.last_tab {
                    self.tab_from_right = self.active_tab.index() > self.last_tab.index();
                    self.tab_anim_start = Instant::now();
                    self.last_tab = self.active_tab;
                }

                // Slide direcional + fade (~220ms, ease-out). O rect é TRANSLADADO em X
                // mas mantém o MESMO tamanho → available_width() inalterado, sem reintroduzir
                // corte horizontal. DX pequeno + fade disfarça a borda; CentralPanel clipa.
                let t = if ANIM_ENABLED {
                    (self.tab_anim_start.elapsed().as_secs_f32() / ANIM_TAB_SECS).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let call_phase_ref = &call_phase;
                if t < 1.0 {
                    let eased = 1.0 - (1.0 - t).powi(3); // ease-out cúbico
                    let dir = if self.tab_from_right { 1.0 } else { -1.0 };
                    let offset_x = dir * ANIM_TAB_DX * (1.0 - eased);
                    let alpha = eased;
                    ctx.request_repaint();

                    let content_rect = ui.max_rect().translate(Vec2::new(offset_x, 0.0));
                    ui.allocate_new_ui(UiBuilder::new().max_rect(content_rect), |ui| {
                        ui.set_opacity(alpha);
                        self.render_active_tab(ui, ctx, call_phase_ref);
                    });
                } else {
                    self.render_active_tab(ui, ctx, call_phase_ref);
                }
            });
    }
}

impl RustSipApp {
    /// Despacha a renderização da aba ativa (extraído p/ reuso no caminho animado e estável).
    fn render_active_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, call_phase: &CallPhase) {
        match self.active_tab {
            ActiveTab::Dialpad => self.render_dialpad_tab(ui, call_phase),
            ActiveTab::Contacts => self.render_contacts_tab(ui),
            ActiveTab::History => self.render_history_tab(ui),
            ActiveTab::Settings => self.render_settings_tab(ui),
            ActiveTab::SipLogs => self.render_logs_tab(ui, ctx),
        }
    }

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
                        self.client.send(ClientMsg::Disconnect);
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
                        self.client.send(ClientMsg::Connect);
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
                                self.client.send(ClientMsg::Hangup);
                            }
                            if action_button(ui, "Atender", Some(Icon::Phone), STATUS_GREEN, 88.0, 32.0).clicked() {
                                self.client.send(ClientMsg::Answer);
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
                                self.client.send(ClientMsg::Hangup);
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
                                self.client.send(ClientMsg::Hangup);
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
                            self.client.send(ClientMsg::SetMuted(!is_muted));
                        }

                        // Espera: ação. Pause quando a chamada está ativa, Play (retomar) em espera.
                        let hold_icon = if is_hold { Icon::Play } else { Icon::Pause };
                        let hold_label = if is_hold { "Retomar" } else { "Pôr em espera" };
                        if call_control_button(ui, hold_icon, hold_label, is_hold, ACCENT_BLUE, btn_w, 30.0)
                            .clicked()
                        {
                            self.client.send(ClientMsg::SetHold(!is_hold));
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
                    // O daemon gera o id, grava e emite Contacts (a lista atualiza
                    // no espelho). A UI só manda o comando e limpa o formulário.
                    self.client.send(ClientMsg::AddContact {
                        name: self.new_contact_name.trim().to_string(),
                        number: self.new_contact_number.trim().to_string(),
                        notes: self.new_contact_notes.trim().to_string(),
                    });
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
        let mut delete_id: Option<String> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            if filtered.is_empty() {
                ui.add_space(SP5);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Nenhum contato encontrado").font(font(FS_LABEL)).color(TEXT_MUTED));
                });
            }

            for (_idx, contact) in filtered {
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
                                    delete_id = Some(contact.id.clone());
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

        if let Some(id) = delete_id {
            // O daemon remove por id, grava e emite Contacts (a lista atualiza
            // no espelho).
            self.client.send(ClientMsg::DeleteContact(id));
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
                    // O daemon limpa, grava e emite History (o espelho atualiza).
                    self.client.send(ClientMsg::ClearHistory);
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
                    // Um único comando: o daemon grava a conta (único writer),
                    // reconecta se configurada, e emite Account de volta (o
                    // formulário é reaplicado só quando esse frame chega).
                    let configured = self.config.account.is_configured();
                    self.client.send(ClientMsg::SaveAccount {
                        account: self.config.account.clone(),
                        reconnect: true,
                    });
                    if configured {
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
                            self.prev_mirror_audio.input_device = None;
                            self.client.send(ClientMsg::SetInputDevice(None));
                        }
                        for dev in &self.input_devices {
                            let sel = self.config.audio.input_device.as_deref() == Some(&dev.id);
                            if ui.selectable_label(sel, dev.display_label()).clicked() {
                                self.config.audio.input_device = Some(dev.id.clone());
                                self.prev_mirror_audio.input_device = Some(dev.id.clone());
                                self.client.send(ClientMsg::SetInputDevice(Some(dev.id.clone())));
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
                            self.prev_mirror_audio.output_device = None;
                            self.client.send(ClientMsg::SetOutputDevice(None));
                        }
                        for dev in &self.output_devices {
                            let sel = self.config.audio.output_device.as_deref() == Some(&dev.id);
                            if ui.selectable_label(sel, dev.display_label()).clicked() {
                                self.config.audio.output_device = Some(dev.id.clone());
                                self.prev_mirror_audio.output_device = Some(dev.id.clone());
                                self.client.send(ClientMsg::SetOutputDevice(Some(dev.id.clone())));
                            }
                        }
                    });

                ui.add_space(10.0);

                // Sliders de ganho e volume.
                // A trilha usa widgets.inactive.bg_fill, que globalmente = BG_CARD (cor do
                // próprio card) → trilha invisível. Escopo local: poço BG_INPUT (mais escuro)
                // + trailing fill ACCENT_BLUE mostrando o nível.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Ganho Mic:").font(font(FS_LABEL)).color(TEXT_SECONDARY));
                    ui.scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.bg_fill = BG_INPUT;
                        v.widgets.hovered.bg_fill = BG_INPUT;
                        v.widgets.active.bg_fill = BG_INPUT;
                        v.selection.bg_fill = ACCENT_BLUE;
                        if ui
                            .add(egui::Slider::new(&mut self.config.audio.mic_gain, 0.2..=3.0).text("x").trailing_fill(true))
                            .changed()
                        {
                            self.prev_mirror_audio.mic_gain = self.config.audio.mic_gain;
                            self.client.send(ClientMsg::SetMicGain(self.config.audio.mic_gain));
                        }
                    });
                });

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Volume:").font(font(FS_LABEL)).color(TEXT_SECONDARY));
                    ui.scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.bg_fill = BG_INPUT;
                        v.widgets.hovered.bg_fill = BG_INPUT;
                        v.widgets.active.bg_fill = BG_INPUT;
                        v.selection.bg_fill = ACCENT_BLUE;
                        if ui
                            .add(egui::Slider::new(&mut self.config.audio.speaker_volume, 0.1..=2.5).text("x").trailing_fill(true))
                            .changed()
                        {
                            self.prev_mirror_audio.speaker_volume = self.config.audio.speaker_volume;
                            self.client.send(ClientMsg::SetSpeakerVolume(self.config.audio.speaker_volume));
                        }
                    });
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
                        self.client.send(ClientMsg::StopLoopbackTest);
                    } else {
                        self.client.send(ClientMsg::StartLoopbackTest);
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
