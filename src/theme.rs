//! ─────────────────────────────────────────────────────────────────────────
//! SISTEMA DE DESIGN DO RustSIP  (theme.rs)
//! ─────────────────────────────────────────────────────────────────────────
//! Estética sóbria e corporativa (linha Linear / Raycast / GNOME Adwaita Dark):
//! tipografia nítida, contraste preciso, superfícies limpas. Sem neon, sem
//! ornamento lúdico. Tudo o que a UI desenha deve sair deste arquivo — nada de
//! números soltos em ui.rs.
//!
//! TOKENS DO SISTEMA
//! -----------------
//! 1. COR — paleta por PAPEL semântico, não por matiz:
//!    - Superfícies: BG_APP < BG_PANEL < BG_CARD (profundidade crescente),
//!      BG_INPUT (campos), BG_HOVER / BG_ACTIVE (estados).
//!    - Texto: TEXT_PRIMARY > TEXT_SECONDARY > TEXT_MUTED (hierarquia).
//!    - Bordas: BORDER_SUBTLE (repouso), BORDER_FOCUS (foco/hover).
//!    - Acento: ACCENT_BLUE (ação primária / seleção).
//!    - Status: STATUS_GREEN (ok/ligar) · AMBER (atenção/mudo) · RED (erro).
//!      RED tem duas formas: STATUS_RED (#EF4444) p/ TEXTO/borda em superfície
//!      escura, STATUS_RED_FILL (#DC2626) p/ FILL com texto branco (AA).
//!
//! 2. TIPOGRAFIA — escala com PROPÓSITO (não tamanhos aleatórios). Cada degrau
//!    tem um uso. Pesos: regular (corpo) e semibold (títulos/ação/labels).
//!    TITLE 20 · HEADING 16 · SUBHEAD 14 · BODY 13 · LABEL 12 · CAPTION 11 ·
//!    MICRO 10 · MONO 12 (log/telemetria). Dígitos do teclado = DIAL 20.
//!
//! 3. ESPAÇAMENTO — grade de 4px. SP1=4 SP2=8 SP3=12 SP4=16 SP5=20 SP6=24.
//!    Nunca usar folga crua; usar o degrau.
//!
//! 4. RAIO — R_SM 4 (chips/botões densos) · R_MD 6 (cards/campos/teclas,
//!    raio padrão) · R_LG 8 (superfícies grandes). Bordas sempre 1px.
//!
//! 5. ESTADOS — todo elemento interativo tem repouso→hover→pressed→(ativo)
//!    por COR/opacidade, nunca por resize (evita layout shift). Foco de teclado
//!    é o anel BORDER_FOCUS. Clicável sempre com cursor de ponteiro.
//!
//! REGRA: zero emoji como ícone — todo glifo vem de `Icon::` + `paint_icon`.
//! ─────────────────────────────────────────────────────────────────────────

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, WidgetInfo,
    WidgetType,
};
use std::sync::Arc;

// ───────────────────────────── 1. Cor — tokens semânticos ─────────────────────────────

// Superfícies (profundidade crescente)
pub const BG_APP: Color32 = Color32::from_rgb(15, 17, 23); // #0F1117 fundo da janela
pub const BG_PANEL: Color32 = Color32::from_rgb(21, 24, 33); // #151821 cabeçalho / abas / rodapé
pub const BG_CARD: Color32 = Color32::from_rgb(26, 30, 42); // #1A1E2A cards e superfícies elevadas
pub const BG_INPUT: Color32 = Color32::from_rgb(18, 20, 28); // #12141C campos de entrada / poços
pub const BG_HOVER: Color32 = Color32::from_rgb(34, 40, 56); // #222838 estado hover
pub const BG_ACTIVE: Color32 = Color32::from_rgb(42, 50, 70); // #2A3246 selecionado / pressionado

// Bordas
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(42, 48, 66); // #2A3042 borda de repouso (visível no dark)
pub const BORDER_FOCUS: Color32 = Color32::from_rgb(96, 165, 250); // #60A5FA foco de teclado / hover (AA sobre superfícies)

// Acento + status (ação)
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(37, 99, 235); // #2563EB ação primária / seleção
pub const STATUS_GREEN: Color32 = Color32::from_rgb(34, 197, 94); // #22C55E online / atender / ligar
pub const STATUS_AMBER: Color32 = Color32::from_rgb(234, 142, 20); // #EA8E14 conectando / mudo (mais legível que #D97706)
pub const STATUS_RED: Color32 = Color32::from_rgb(239, 68, 68); // #EF4444 erro/desligar como TEXTO ou borda (4.42:1 no card)
pub const STATUS_RED_FILL: Color32 = Color32::from_rgb(220, 38, 38); // #DC2626 FILL de botão vermelho + texto branco (AA 4.5:1+)

// Texto (hierarquia de leitura)
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(248, 250, 252); // #F8FAFC primário — salta
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(158, 172, 191); // #9EACBF secundário — recua (AA ~7:1 no card)
pub const TEXT_MUTED: Color32 = Color32::from_rgb(122, 136, 156); // #7A889C terciário — apenas metadados curtos (~4.6:1)

// ───────────────────────────── 2. Tipografia — escala ─────────────────────────────

pub const FAMILY_BOLD: &str = "rs-bold";
#[allow(dead_code)]
pub const FAMILY_LIGHT: &str = "rs-light";

// Degraus da escala tipográfica (pt). Usar SEMPRE estes nomes, não literais.
pub const FS_TITLE: f32 = 20.0; // marca / número no visor
pub const FS_DIAL: f32 = 20.0; // dígito principal da tecla
pub const FS_HEADING: f32 = 16.0; // nome na chamada ativa
pub const FS_SUBHEAD: f32 = 14.0; // título de seção / rótulo de ação
pub const FS_BODY: f32 = 13.0; // corpo padrão
pub const FS_LABEL: f32 = 12.0; // rótulos de campo / nomes de lista
pub const FS_CAPTION: f32 = 11.0; // subtítulos / metadados
pub const FS_MICRO: f32 = 10.0; // chips / badges / telemetria
pub const FS_MONO: f32 = 12.0; // log bruto e RTP

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}
pub fn font_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_BOLD.into()))
}
pub fn font_mono(size: f32) -> FontId {
    FontId::monospace(size)
}

// ───────────────────────────── 3. Espaçamento — grade 4px ─────────────────────────────

pub const SP1: f32 = 4.0;
pub const SP2: f32 = 8.0;
#[allow(dead_code)] // degrau da escala completa
pub const SP3: f32 = 12.0;
#[allow(dead_code)] // degrau da escala completa
pub const SP4: f32 = 16.0;
pub const SP5: f32 = 20.0;
#[allow(dead_code)] // degrau da escala completa; reservado para superfícies maiores
pub const SP6: f32 = 24.0;

// ───────────────────────────── 4. Raio ─────────────────────────────

pub const R_SM: u8 = 4; // chips, botões densos
pub const R_MD: u8 = 6; // raio padrão (cards, campos, teclas, botões)
#[allow(dead_code)] // degrau da escala completa; reservado para superfícies grandes
pub const R_LG: u8 = 8; // superfícies grandes

fn r(radius: u8) -> CornerRadius {
    CornerRadius::same(radius)
}

// ───────────────────────────── Carregamento de fontes ─────────────────────────────

fn load_first(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

fn fc_match(pattern: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lower = path.to_lowercase();
    if lower.ends_with(".ttf") || lower.ends_with(".otf") {
        std::fs::read(path).ok()
    } else {
        None
    }
}

pub fn configure_style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let default_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let default_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let regular = load_first(&[
        "/usr/share/fonts/inter/Inter-Regular.otf",
        "/usr/share/fonts/inter/Inter-Regular.ttf",
        "/usr/share/fonts/TTF/Inter-Regular.ttf",
        "/usr/share/fonts/OTF/Inter-Regular.otf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/cantarell/Cantarell-Regular.otf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ])
    .or_else(|| fc_match("Inter,Noto Sans,Cantarell:style=Regular"));

    let bold = load_first(&[
        "/usr/share/fonts/inter/Inter-SemiBold.otf",
        "/usr/share/fonts/inter/Inter-SemiBold.ttf",
        "/usr/share/fonts/TTF/Inter-SemiBold.ttf",
        "/usr/share/fonts/OTF/Inter-SemiBold.otf",
        "/usr/share/fonts/noto/NotoSans-SemiBold.ttf",
        "/usr/share/fonts/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/cantarell/Cantarell-Bold.otf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    ])
    .or_else(|| fc_match("Inter,Noto Sans,Cantarell:style=SemiBold"));

    let mono = load_first(&[
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ]);

    let mut add = |key: &str, data: Option<Vec<u8>>| -> Option<String> {
        data.map(|d| {
            fonts
                .font_data
                .insert(key.to_string(), Arc::new(FontData::from_owned(d)));
            key.to_string()
        })
    };
    let reg_key = add("rs-regular", regular);
    let bold_key = add("rs-bold-data", bold);
    let mono_key = add("rs-mono", mono);

    let mut prop = default_prop.clone();
    if let Some(k) = &reg_key {
        prop.insert(0, k.clone());
    }
    let mut bold_fam = prop.clone();
    if let Some(k) = &bold_key {
        bold_fam.insert(0, k.clone());
    }
    let mut mono_fam = default_mono;
    if let Some(k) = &mono_key {
        mono_fam.insert(0, k.clone());
    }

    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono_fam);
    fonts
        .families
        .insert(FontFamily::Name(FAMILY_BOLD.into()), bold_fam);
    ctx.set_fonts(fonts);

    ctx.style_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG_APP;
        v.window_fill = BG_PANEL;
        v.window_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
        v.window_corner_radius = r(R_MD);
        v.menu_corner_radius = r(R_MD);
        v.extreme_bg_color = BG_INPUT;
        v.faint_bg_color = BG_HOVER;
        v.selection.bg_fill = ACCENT_BLUE.gamma_multiply(0.4);
        v.selection.stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
        v.hyperlink_color = BORDER_FOCUS;
        v.text_cursor.stroke = Stroke::new(2.0_f32, BORDER_FOCUS);

        // Anel de foco de teclado perceptível e consistente (acessibilidade).
        v.widgets.noninteractive.bg_fill = BG_CARD;
        v.widgets.noninteractive.weak_bg_fill = BG_CARD;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_SECONDARY);
        v.widgets.noninteractive.corner_radius = r(R_MD);

        v.widgets.inactive.bg_fill = BG_CARD;
        v.widgets.inactive.weak_bg_fill = BG_CARD;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
        v.widgets.inactive.corner_radius = r(R_MD);

        v.widgets.hovered.bg_fill = BG_HOVER;
        v.widgets.hovered.weak_bg_fill = BG_HOVER;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BORDER_FOCUS);
        v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
        v.widgets.hovered.corner_radius = r(R_MD);

        v.widgets.active.bg_fill = BG_ACTIVE;
        v.widgets.active.weak_bg_fill = BG_ACTIVE;
        v.widgets.active.bg_stroke = Stroke::new(1.5_f32, BORDER_FOCUS);
        v.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
        v.widgets.active.corner_radius = r(R_MD);

        v.widgets.open = v.widgets.hovered;

        style.spacing.item_spacing = Vec2::new(SP2, SP2);
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style.text_styles.insert(egui::TextStyle::Body, font(FS_BODY));
        style.text_styles.insert(egui::TextStyle::Button, font(FS_BODY));
        style.text_styles.insert(egui::TextStyle::Small, font(FS_CAPTION));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, font_mono(FS_MONO));
    });
}

// ───────────────────────────── Ícones vetoriais ─────────────────────────────

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Phone,
    Hangup,
    Mic,
    MicOff,
    Pause,
    Play,
    Keypad,
    User,
    Users,
    Clock,
    Gear,
    Terminal,
    Backspace,
    Plus,
    Close,
    Trash,
    Search,
    ArrowOut,
    ArrowIn,
    ArrowMissed,
    Speaker,
    Check,
    Power,
    Copy,
    Refresh,
    Eye,
    EyeOff,
}

fn line(painter: &Painter, pts: &[Pos2], w: f32, color: Color32) {
    if pts.len() < 2 {
        return;
    }
    painter.add(Shape::line(pts.to_vec(), Stroke::new(w, color)));
}

pub fn paint_icon(painter: &Painter, icon: Icon, center: Pos2, size: f32, color: Color32) {
    let u = size / 2.0;
    let p = |x: f32, y: f32| -> Pos2 { center + Vec2::new(x * u, y * u) };
    let w = (u * 0.18).clamp(1.4, 2.4);

    match icon {
        Icon::Phone => {
            line(painter, &[p(-0.5, -0.6), p(-0.2, -0.7), p(-0.1, -0.4), p(-0.3, -0.2)], w, color);
            line(painter, &[p(-0.3, -0.2), p(0.2, 0.3)], w, color);
            line(painter, &[p(0.2, 0.3), p(0.4, 0.1), p(0.7, 0.2), p(0.6, 0.5)], w, color);
            line(painter, &[p(0.6, 0.5), p(0.2, 0.7), p(-0.5, 0.0), p(-0.6, -0.3), p(-0.5, -0.6)], w, color);
        }
        Icon::Hangup => {
            line(painter, &[p(-0.7, 0.2), p(-0.5, -0.3), p(0.5, -0.3), p(0.7, 0.2)], w * 1.2, color);
            line(painter, &[p(-0.8, 0.2), p(-0.5, 0.3), p(-0.3, 0.0)], w, color);
            line(painter, &[p(0.8, 0.2), p(0.5, 0.3), p(0.3, 0.0)], w, color);
        }
        Icon::Mic => {
            painter.rect_filled(Rect::from_min_max(p(-0.3, -0.8), p(0.3, 0.2)), r(R_SM), color);
            line(painter, &[p(-0.55, -0.1), p(-0.55, 0.2), p(0.0, 0.55), p(0.55, 0.2), p(0.55, -0.1)], w, color);
            line(painter, &[p(0.0, 0.55), p(0.0, 0.85)], w, color);
            line(painter, &[p(-0.35, 0.85), p(0.35, 0.85)], w, color);
        }
        Icon::MicOff => {
            painter.rect_filled(Rect::from_min_max(p(-0.3, -0.8), p(0.3, 0.2)), r(R_SM), color);
            line(painter, &[p(-0.55, -0.1), p(-0.55, 0.2), p(0.0, 0.55), p(0.55, 0.2), p(0.55, -0.1)], w, color);
            line(painter, &[p(0.0, 0.55), p(0.0, 0.85)], w, color);
            line(painter, &[p(-0.75, -0.75), p(0.75, 0.75)], w * 1.2, color);
        }
        Icon::Pause => {
            painter.rect_filled(Rect::from_min_max(p(-0.45, -0.6), p(-0.15, 0.6)), CornerRadius::same(2), color);
            painter.rect_filled(Rect::from_min_max(p(0.15, -0.6), p(0.45, 0.6)), CornerRadius::same(2), color);
        }
        Icon::Play => {
            painter.add(Shape::convex_polygon(
                vec![p(-0.35, -0.6), p(0.55, 0.0), p(-0.35, 0.6)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Keypad => {
            for row in 0..3 {
                for col in 0..3 {
                    let cx = -0.55 + col as f32 * 0.55;
                    let cy = -0.55 + row as f32 * 0.55;
                    painter.circle_filled(p(cx, cy), u * 0.14, color);
                }
            }
        }
        Icon::User => {
            painter.circle_filled(p(0.0, -0.35), u * 0.35, color);
            let body: Vec<Pos2> = vec![p(-0.6, 0.8), p(-0.5, 0.35), p(0.5, 0.35), p(0.6, 0.8)];
            line(painter, &body, w, color);
        }
        Icon::Users => {
            painter.circle_filled(p(-0.25, -0.3), u * 0.28, color);
            painter.circle_filled(p(0.35, -0.4), u * 0.22, color.gamma_multiply(0.6));
            line(painter, &[p(-0.7, 0.75), p(-0.55, 0.35), p(0.05, 0.35), p(0.2, 0.75)], w, color);
        }
        Icon::Clock => {
            painter.circle_stroke(center, u * 0.75, Stroke::new(w, color));
            line(painter, &[p(0.0, -0.4), p(0.0, 0.0), p(0.3, 0.2)], w, color);
        }
        Icon::Gear => {
            painter.circle_stroke(center, u * 0.45, Stroke::new(w * 1.5, color));
            for i in 0..6 {
                let ang = i as f32 * (std::f32::consts::PI / 3.0);
                let (s, c) = ang.sin_cos();
                line(painter, &[p(c * 0.5, s * 0.5), p(c * 0.8, s * 0.8)], w * 1.5, color);
            }
        }
        Icon::Terminal => {
            painter.rect_stroke(
                Rect::from_min_max(p(-0.8, -0.65), p(0.8, 0.65)),
                CornerRadius::same(3),
                Stroke::new(w, color),
                StrokeKind::Middle,
            );
            line(painter, &[p(-0.45, -0.25), p(-0.2, 0.0), p(-0.45, 0.25)], w, color);
            line(painter, &[p(0.0, 0.25), p(0.4, 0.25)], w, color);
        }
        Icon::Backspace => {
            let pts = vec![p(-0.8, 0.0), p(-0.35, -0.55), p(0.75, -0.55), p(0.75, 0.55), p(-0.35, 0.55)];
            painter.add(Shape::closed_line(pts, Stroke::new(w, color)));
            line(painter, &[p(-0.05, -0.2), p(0.35, 0.2)], w, color);
            line(painter, &[p(0.35, -0.2), p(-0.05, 0.2)], w, color);
        }
        Icon::Plus => {
            line(painter, &[p(0.0, -0.65), p(0.0, 0.65)], w * 1.1, color);
            line(painter, &[p(-0.65, 0.0), p(0.65, 0.0)], w * 1.1, color);
        }
        Icon::Close => {
            line(painter, &[p(-0.5, -0.5), p(0.5, 0.5)], w * 1.1, color);
            line(painter, &[p(0.5, -0.5), p(-0.5, 0.5)], w * 1.1, color);
        }
        Icon::Trash => {
            line(painter, &[p(-0.65, -0.5), p(0.65, -0.5)], w, color);
            line(painter, &[p(-0.25, -0.7), p(0.25, -0.7)], w, color);
            line(painter, &[p(-0.45, -0.5), p(-0.4, 0.75), p(0.4, 0.75), p(0.45, -0.5)], w, color);
            line(painter, &[p(-0.15, -0.2), p(-0.15, 0.5)], w * 0.8, color);
            line(painter, &[p(0.15, -0.2), p(0.15, 0.5)], w * 0.8, color);
        }
        Icon::Search => {
            painter.circle_stroke(p(-0.15, -0.15), u * 0.48, Stroke::new(w, color));
            line(painter, &[p(0.22, 0.22), p(0.7, 0.7)], w * 1.2, color);
        }
        Icon::ArrowOut => {
            line(painter, &[p(-0.5, 0.5), p(0.5, -0.5)], w, color);
            line(painter, &[p(0.0, -0.5), p(0.5, -0.5), p(0.5, 0.0)], w, color);
        }
        Icon::ArrowIn => {
            line(painter, &[p(0.5, -0.5), p(-0.5, 0.5)], w, color);
            line(painter, &[p(0.0, 0.5), p(-0.5, 0.5), p(-0.5, 0.0)], w, color);
        }
        Icon::ArrowMissed => {
            line(painter, &[p(-0.4, -0.4), p(0.4, 0.4)], w, color);
            line(painter, &[p(0.4, -0.4), p(-0.4, 0.4)], w, color);
        }
        Icon::Speaker => {
            painter.rect_filled(Rect::from_min_max(p(-0.7, -0.25), p(-0.35, 0.25)), CornerRadius::same(2), color);
            painter.add(Shape::convex_polygon(
                vec![p(-0.35, -0.25), p(0.1, -0.65), p(0.1, 0.65), p(-0.35, 0.25)],
                color,
                Stroke::NONE,
            ));
            line(painter, &[p(0.3, -0.35), p(0.5, 0.0), p(0.3, 0.35)], w, color);
        }
        Icon::Check => {
            line(painter, &[p(-0.6, 0.0), p(-0.15, 0.45), p(0.65, -0.45)], w * 1.2, color);
        }
        Icon::Power => {
            line(painter, &[p(0.0, -0.8), p(0.0, -0.1)], w * 1.2, color);
            painter.circle_stroke(center, u * 0.65, Stroke::new(w, color));
        }
        Icon::Copy => {
            painter.rect_stroke(
                Rect::from_min_max(p(-0.65, -0.65), p(0.25, 0.35)),
                CornerRadius::same(2),
                Stroke::new(w, color),
                StrokeKind::Middle,
            );
            painter.rect_stroke(
                Rect::from_min_max(p(-0.25, -0.25), p(0.65, 0.75)),
                CornerRadius::same(2),
                Stroke::new(w, color),
                StrokeKind::Middle,
            );
        }
        Icon::Refresh => {
            painter.circle_stroke(center, u * 0.65, Stroke::new(w, color));
            line(painter, &[p(0.3, -0.8), p(0.65, -0.5), p(0.3, -0.2)], w, color);
        }
        Icon::Eye => {
            let pts = vec![p(-0.8, 0.0), p(0.0, -0.45), p(0.8, 0.0), p(0.0, 0.45)];
            painter.add(Shape::closed_line(pts, Stroke::new(w, color)));
            painter.circle_filled(center, u * 0.2, color);
        }
        Icon::EyeOff => {
            let pts = vec![p(-0.8, 0.0), p(0.0, -0.45), p(0.8, 0.0), p(0.0, 0.45)];
            painter.add(Shape::closed_line(pts, Stroke::new(w, color)));
            line(painter, &[p(-0.7, -0.6), p(0.7, 0.6)], w * 1.1, color);
        }
    }
}

// ───────────────────────────── Componentes ─────────────────────────────

/// Card: superfície elevada, borda fina, raio padrão.
pub fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(r(R_MD))
        .inner_margin(egui::Margin::symmetric(14, 12))
}

/// Tecla do teclado numérico (tátil, com pressed visível por cor).
pub fn dial_key(
    ui: &mut Ui,
    main_label: &str,
    sub_label: &str,
    width: f32,
    height: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

    let is_down = resp.is_pointer_button_down_on();
    let is_hover = resp.hovered();

    let bg = if is_down {
        BG_ACTIVE
    } else if is_hover {
        BG_HOVER
    } else {
        BG_CARD
    };
    // Pressed intensifica a borda (toque tátil) sem mudar tamanho → sem layout shift.
    let border = if is_down {
        BORDER_FOCUS
    } else if is_hover {
        BORDER_FOCUS.gamma_multiply(0.7)
    } else {
        BORDER_SUBTLE
    };

    let p = ui.painter();
    p.rect_filled(rect, r(R_MD), bg);
    p.rect_stroke(rect, r(R_MD), Stroke::new(1.0_f32, border), StrokeKind::Inside);
    // Anel de foco de teclado (Tab percorre as 12 teclas) — paridade com os demais botões.
    if resp.has_focus() {
        p.rect_stroke(rect, r(R_MD), Stroke::new(2.0_f32, BORDER_FOCUS), StrokeKind::Outside);
    }

    let center = rect.center();
    if sub_label.is_empty() {
        // Dígito único centrado verticalmente.
        p.text(center, Align2::CENTER_CENTER, main_label, font_bold(FS_DIAL), TEXT_PRIMARY);
    } else {
        // Dígito + letras: baseline do dígito sobe, letras (ABC/DEF) alinhadas logo abaixo.
        p.text(center - Vec2::new(0.0, 7.0), Align2::CENTER_CENTER, main_label, font_bold(FS_DIAL - 1.0), TEXT_PRIMARY);
        p.text(center + Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, sub_label, font_bold(9.5), TEXT_MUTED);
    }

    resp
}

/// Mede a largura real (px) de um rótulo numa dada fonte — substitui a
/// heurística `len() * fs * 0.52` (que errava com acentos/UTF-8 e textos longos).
fn text_width(ui: &Ui, text: &str, font_id: FontId) -> f32 {
    ui.fonts(|f| f.layout_no_wrap(text.to_string(), font_id, Color32::WHITE).size().x)
}

/// Encurta `s` com reticências até caber em `max_w` px na fonte `font_id`.
/// Para texto fechado de ComboBox/label onde não há wrap nem truncate nativo.
/// Respeita limites de char (UTF-8), não de byte.
pub fn elide(ui: &Ui, s: &str, font_id: FontId, max_w: f32) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    if text_width(ui, s, font_id.clone()) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    // Busca o maior prefixo que, com "…", ainda cabe.
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if text_width(ui, &candidate, font_id.clone()) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        "…".to_string()
    } else {
        chars[..lo].iter().collect::<String>() + "…"
    }
}

/// Botão de ação primária preenchido (Ligar / Atender / Salvar...).
pub fn action_button(
    ui: &mut Ui,
    text: &str,
    icon: Option<Icon>,
    bg_color: Color32,
    width: f32,
    height: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    // Expõe o rótulo ao leitor de tela (AccessKit) — paridade com egui::Button.
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));

    let is_down = resp.is_pointer_button_down_on();
    let is_hover = resp.hovered();

    let fill = if is_down {
        bg_color.gamma_multiply(0.82)
    } else if is_hover {
        bg_color.gamma_multiply(1.12)
    } else {
        bg_color
    };

    let p = ui.painter();
    p.rect_filled(rect, r(R_MD), fill);
    // Anel de foco de teclado perceptível quando o botão recebe foco.
    if resp.has_focus() {
        p.rect_stroke(rect, r(R_MD), Stroke::new(2.0_f32, BORDER_FOCUS), StrokeKind::Outside);
    }

    let center = rect.center();
    let font_id = font_bold(FS_SUBHEAD);
    let label_w = text_width(ui, text, font_id.clone());
    let icon_sz = 16.0;
    let gap = 8.0;
    // Só mostra o ícone se o bloco (ícone+gap+texto) couber na largura útil do botão.
    let fits_icon = icon.is_some() && (icon_sz + gap + label_w) <= (width - 12.0);

    let p = ui.painter();
    if fits_icon {
        let block_w = icon_sz + gap + label_w;
        let icon_c = center - Vec2::new(block_w / 2.0 - icon_sz / 2.0, 0.0);
        paint_icon(p, icon.unwrap(), icon_c, icon_sz, Color32::WHITE);
        p.text(
            Pos2::new(icon_c.x + icon_sz / 2.0 + gap, center.y),
            Align2::LEFT_CENTER,
            text,
            font_id,
            Color32::WHITE,
        );
    } else {
        // Sem espaço para o ícone (ou sem ícone): centra só o texto.
        p.text(center, Align2::CENTER_CENTER, text, font_id, Color32::WHITE);
    }

    resp
}

/// Botão secundário quadrado com ícone e borda (ferramentas / toggles).
pub fn icon_tool_button(
    ui: &mut Ui,
    icon: Icon,
    size: f32,
    tooltip: &str,
    active: bool,
    active_color: Color32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip);

    let is_hover = resp.hovered();
    let is_down = resp.is_pointer_button_down_on();
    let bg = if active {
        active_color.gamma_multiply(0.2)
    } else if is_down {
        BG_ACTIVE
    } else if is_hover {
        BG_HOVER
    } else {
        BG_CARD
    };

    let border = if active {
        active_color
    } else if is_hover {
        BORDER_FOCUS
    } else {
        BORDER_SUBTLE
    };

    let fg = if active {
        active_color
    } else if is_hover {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };

    let p = ui.painter();
    p.rect_filled(rect, r(R_MD), bg);
    p.rect_stroke(rect, r(R_MD), Stroke::new(1.0_f32, border), StrokeKind::Inside);
    if resp.has_focus() {
        p.rect_stroke(rect, r(R_MD), Stroke::new(2.0_f32, BORDER_FOCUS), StrokeKind::Outside);
    }
    paint_icon(p, icon, rect.center(), size * 0.5, fg);

    resp
}

/// Botão de controle em chamada (Mudo / Espera): ícone VETORIAL + rótulo,
/// estado ativo evidente. Substitui o anti-padrão de emoji.
pub fn call_control_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    active: bool,
    active_color: Color32,
    width: f32,
    height: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    // Expõe o rótulo ao leitor de tela (AccessKit) — paridade com egui::Button.
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));

    let is_hover = resp.hovered();
    let is_down = resp.is_pointer_button_down_on();

    let bg = if active {
        active_color.gamma_multiply(0.18)
    } else if is_down {
        BG_ACTIVE
    } else if is_hover {
        BG_HOVER
    } else {
        BG_CARD
    };
    let border = if active {
        active_color
    } else if is_hover {
        BORDER_FOCUS
    } else {
        BORDER_SUBTLE
    };
    let fg = if active { active_color } else { TEXT_SECONDARY };

    let p = ui.painter();
    p.rect_filled(rect, r(R_MD), bg);
    p.rect_stroke(rect, r(R_MD), Stroke::new(1.0_f32, border), StrokeKind::Inside);
    if resp.has_focus() {
        p.rect_stroke(rect, r(R_MD), Stroke::new(2.0_f32, BORDER_FOCUS), StrokeKind::Outside);
    }

    // Ícone + rótulo agrupados e centrados como bloco.
    let icon_sz = 15.0;
    let gap = 8.0;
    let font_id = font_bold(FS_LABEL);
    let label_w = text_width(ui, label, font_id.clone());
    let fits_icon = (icon_sz + gap + label_w) <= (width - 12.0);
    let c = rect.center();

    let p = ui.painter();
    if fits_icon {
        let block_w = icon_sz + gap + label_w;
        let icon_c = c - Vec2::new(block_w / 2.0 - icon_sz / 2.0, 0.0);
        paint_icon(p, icon, icon_c, icon_sz, fg);
        p.text(
            Pos2::new(icon_c.x + icon_sz / 2.0 + gap, c.y),
            Align2::LEFT_CENTER,
            label,
            font_id,
            fg,
        );
    } else {
        p.text(c, Align2::CENTER_CENTER, label, font_id, fg);
    }

    resp
}

/// Campo de texto rotulado.
pub fn text_input(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    password: bool,
) -> Response {
    ui.label(egui::RichText::new(label).font(font_bold(FS_CAPTION)).color(TEXT_SECONDARY));
    ui.add_space(-2.0);

    egui::Frame::NONE
        .fill(BG_INPUT)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(r(R_MD))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(value)
                    .frame(false)
                    .password(password)
                    .font(font(FS_BODY))
                    .text_color(TEXT_PRIMARY)
                    .hint_text(egui::RichText::new(hint).color(TEXT_MUTED))
                    .desired_width(f32::INFINITY),
            )
        })
        .inner
}

/// Aba de navegação.
pub fn tab_button(ui: &mut Ui, label: &str, icon: Icon, selected: bool, width: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

    let is_hover = resp.hovered();
    let bg = if selected {
        BG_ACTIVE
    } else if is_hover {
        BG_HOVER
    } else {
        Color32::TRANSPARENT
    };

    let fg = if selected || is_hover {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };

    let p = ui.painter();
    p.rect_filled(rect, r(R_SM), bg);
    if selected {
        // Marcador inferior de aba ativa.
        let line_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, rect.bottom() - 2.0),
            Pos2::new(rect.right() - 6.0, rect.bottom()),
        );
        p.rect_filled(line_rect, CornerRadius::ZERO, ACCENT_BLUE);
    }
    if resp.has_focus() {
        p.rect_stroke(rect, r(R_SM), Stroke::new(2.0_f32, BORDER_FOCUS), StrokeKind::Outside);
    }

    // Ícone + rótulo centrados como bloco. Em aba estreita (janela a 360px) o
    // bloco não cabe → degrada para SÓ TEXTO em vez de cortar (mesma lógica de
    // action_button / call_control_button). Medição real, não heurística.
    let icon_sz = 14.0;
    let gap = 7.0;
    let font_id = font_bold(FS_LABEL);
    let label_w = text_width(ui, label, font_id.clone());
    let fits_icon = (icon_sz + gap + label_w) <= (width - 8.0);
    let c = rect.center();

    let p = ui.painter();
    if fits_icon {
        let block_w = icon_sz + gap + label_w;
        let icon_c = c - Vec2::new(block_w / 2.0 - icon_sz / 2.0, 0.0);
        paint_icon(p, icon, icon_c, icon_sz, fg);
        p.text(
            Pos2::new(icon_c.x + icon_sz / 2.0 + gap, c.y),
            Align2::LEFT_CENTER,
            label,
            font_id,
            fg,
        );
    } else {
        p.text(c, Align2::CENTER_CENTER, label, font_id, fg);
    }

    resp
}

/// Chip de status discreto.
pub fn status_chip(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.45)))
        .corner_radius(r(R_SM))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).font(font_bold(FS_MICRO)).color(color));
        });
}

/// Medidor de áudio linear.
pub fn vu_meter(ui: &mut Ui, level: f32, width: f32, active_color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let p = ui.painter();

    p.rect_filled(rect, CornerRadius::same(3), BG_INPUT);
    p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.0_f32, BORDER_SUBTLE), StrokeKind::Inside);

    let lvl = level.clamp(0.0, 1.0);
    if lvl > 0.02 {
        let fill_w = (rect.width() * lvl).max(3.0);
        let fill_rect = Rect::from_min_size(rect.min, Vec2::new(fill_w, rect.height()));
        // Pico alto vira âmbar (feedback de clipping) sem sair da paleta.
        let col = if lvl > 0.9 { STATUS_AMBER } else { active_color };
        p.rect_filled(fill_rect, CornerRadius::same(3), col);
    }
}
