//! Drawing: title bar, panes, minimap, and popups.

use alacritty_terminal::vte::ansi::CursorShape;
use lucide_icons::Icon;

use crate::render::{Align, Font, Frame, Quad, Row, Span};
use crate::theme::mix;
use crate::*;

const WHITE: Rgb = [0xff, 0xff, 0xff];

/// Draws rectangles and text above the terminal, and records the parts that react to the mouse.
struct Ui<'a> {
    frame: &'a mut Frame,
    gpu: &'a mut Gpu,
    hits: &'a mut Vec<(Rect, Hit)>,
    anim: &'a mut Anim,
    /// The parts go to the popup layer, above all other parts.
    popup: bool,
    /// Transparency and vertical offset of the popup that opens now.
    fade: u8,
    dy: f32,
    t: Theme,
    s: f32,
    mouse: (f32, f32),
    w: f32,
    h: f32,
    clip: [f32; 4],
}

impl Ui<'_> {
    fn rect(&mut self, r: Rect, color: Rgb, alpha: u8, radius: f32) {
        let alpha = (alpha as u16 * (255 - self.fade) as u16 / 255) as u8;
        let layer = match self.frame.popups.last_mut() {
            Some(popup) if self.popup => &mut popup.0,
            _ => &mut self.frame.overlay,
        };
        layer.push(Quad { x: r.x, y: r.y + self.dy, w: r.w, h: r.h, color: [color[0], color[1], color[2], alpha], radius });
    }

    /// Starts a popup on a new layer above all earlier parts.
    fn layer(&mut self) {
        self.frame.popups.push(Default::default());
        self.popup = true;
    }

    /// Starts a popup. While it opens, its parts fade in and move down.
    fn pop(&mut self, key: u32) {
        self.layer();
        let progress = self.anim.to(key, 1.0, 0.02);
        self.fade = ((1.0 - progress) * 255.0) as u8;
        self.dy = ((progress - 1.0) * 8.0 * self.s).round();
    }

    fn unpop(&mut self) {
        self.popup = false;
        self.fade = 0;
        self.dy = 0.0;
    }

    /// A Lucide icon with its center at (`x`, `mid`).
    fn icon(&mut self, x: f32, mid: f32, icon: Icon, color: Rgb, size: f32) {
        self.text(x, mid, &icon.unicode().to_string(), color, Font::Icon, size, Align::Center);
    }

    /// A popup background: a border and a fill.
    fn panel(&mut self, r: Rect, fill: Rgb, radius: f32) {
        let border = mix(self.t.border, self.t.fg, 0.12);
        self.rect(r, border, 255, radius);
        self.rect(Rect { x: r.x + 1.0, y: r.y + 1.0, w: r.w - 2.0, h: r.h - 2.0 }, fill, 255, (radius - 1.0).max(0.0));
    }

    /// Text with its vertical center at `mid`. `size` is in logical pixels.
    fn spans(&mut self, x: f32, mid: f32, spans: Vec<Span>, font: Font, size: f32, align: Align) {
        let size = (size * self.s).round();
        let y = mid - (size * 1.4).ceil() / 2.0 + self.dy;
        let layer = match self.frame.popups.last_mut() {
            Some(popup) if self.popup => &mut popup.1,
            _ => &mut self.frame.top_rows,
        };
        layer.push(Row { x, y, clip: self.clip, spans, font, size, align, fade: self.fade, image: None });
    }

    fn text(&mut self, x: f32, mid: f32, text: &str, color: Rgb, font: Font, size: f32, align: Align) {
        self.spans(x, mid, vec![Span { text: text.to_string(), color, bold: false, italic: false }], font, size, align);
    }

    fn width(&mut self, text: &str, font: Font, size: f32) -> f32 {
        self.gpu.text_width(text, font, (size * self.s).round())
    }

    fn hot(&self, r: Rect) -> bool {
        r.contains(self.mouse.0, self.mouse.1 - self.dy)
    }

    fn hit(&mut self, r: Rect, hit: Hit) {
        self.hits.push((Rect { y: r.y + self.dy, ..r }, hit));
    }

    /// An image with its left edge at `x` and its vertical center at `mid`. `size` is in logical pixels.
    /// Image `id` is the version for the dark theme, and `id + 10` is the version for the light theme.
    fn image(&mut self, x: f32, mid: f32, id: u16, size: f32) {
        let id = if self.t.dark { id } else { id + 10 };
        let px = self.gpu.image_size(id, size * self.s);
        let row = Row { x: x.round(), y: (mid - px as f32 / 2.0).round() + self.dy, clip: self.clip, image: Some((id, px)), ..Default::default() };
        let layer = match self.frame.popups.last_mut() {
            Some(popup) if self.popup => &mut popup.1,
            _ => &mut self.frame.top_rows,
        };
        layer.push(row);
    }

    /// The icon of a shell profile, 16 pixels wide.
    fn badge(&mut self, x: f32, mid: f32, profile: Profile) {
        self.image(x, mid, profile.image(), 16.0);
    }
}

impl App {
    pub(crate) fn draw(&mut self) {
        let m = self.metrics();
        let Some(mut gpu) = self.gpu.take() else { return };
        let t = self.theme;
        let (w, h) = gpu.size();
        let mut frame = Frame::default();
        let mut hits = std::mem::take(&mut self.hits);
        hits.clear();
        let focus = self.focus();
        let multi = self.pane_rects.len() > 1;

        let mut anim = std::mem::take(&mut self.anim);
        let now = Instant::now();
        // After a pause, the first frame of a movement uses the time of one frame.
        anim.dt = if anim.active { (now - self.last_frame).as_secs_f32().min(0.05) } else { 1.0 / 60.0 };
        anim.active = false;
        anim.on = self.settings.animations;
        anim.caret_live = self.last_input.elapsed() < Duration::from_millis(400);
        self.last_frame = now;
        // A closed popup opens from the start the next time.
        let popups = [
            (A_PALETTE, self.palette.is_some()),
            (A_MENU, self.menu),
            (A_COLON, self.colon.is_some()),
            (A_FIND, self.find.is_some()),
            (A_FONTS, self.font_menu.is_some()),
            (A_CONFIRM, self.confirm.is_some()),
        ];
        for (key, open) in popups {
            if !open {
                anim.set(key, 0.0);
            }
        }

        let open = !self.in_settings();
        let panes = self.pane_rects.clone();
        for (id, rect) in panes.iter().copied() {
            let Some(pane) = self.panes.get(&id) else { continue };
            let focused = id == focus;
            let find = self.find.as_ref().filter(|_| focused);
            let colon = self.colon.as_ref().filter(|_| focused);
            let link = self.hover_link.filter(|link| link.0 == id).map(|link| (link.1, link.2, link.3));
            let lane_hot = self.drag_minimap == Some(id) || self.hover == Some(Hit::Minimap(id));
            let view = View { rect, focused, multi, bottom: self.bottom, find, colon, link, lane_hot };
            let (top, cursor) = draw_pane(&mut frame, &mut anim, id, pane, &view, &m, &t, &self.settings);
            hits.push((Rect { x: rect.x + rect.w - m.lane, y: rect.y, w: m.lane, h: rect.h }, Hit::Minimap(id)));
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.view_top = top;
                pane.cursor_px = cursor;
            }
        }
        for d in &self.dividers {
            frame.overlay.push(Quad { x: d.rect.x, y: d.rect.y, w: d.rect.w, h: d.rect.h, color: rgba(t.border), radius: 0.0 });
        }

        let mut ui = Ui {
            frame: &mut frame,
            gpu: &mut gpu,
            hits: &mut hits,
            anim: &mut anim,
            popup: false,
            fade: 0,
            dy: 0.0,
            t,
            s: m.s,
            mouse: self.mouse,
            w,
            h,
            clip: [0.0, 0.0, w, h],
        };
        let menu_x = self.draw_titlebar(&mut ui, &m);
        for (id, rect) in &panes {
            if let Some(pane) = self.panes.get(id) {
                draw_badge(&mut ui, &m, *rect, pane);
            }
        }
        if let (Some(rect), Some(pane)) = (self.pane_rect(focus), self.panes.get(&focus)) {
            if let Some(find) = &self.find {
                ui.pop(A_FIND);
                draw_find(&mut ui, &m, rect, find);
            }
            if let (Some(colon), Some(cursor)) = (&self.colon, pane.cursor_px) {
                ui.pop(A_COLON);
                draw_colon(&mut ui, &m, rect, cursor, colon, self.settings.popup);
            }
            ui.unpop();
        }
        if let Some((id, clean)) = self.confirm.filter(|_| open)
            && let (Some(rect), Some(pane)) = (self.pane_rect(id), self.panes.get(&id))
        {
            ui.pop(A_CONFIRM);
            draw_confirm(&mut ui, &m, rect, pane, clean);
            ui.unpop();
        }
        if !open {
            self.draw_settings(&mut ui, &m);
        }
        if self.menu {
            ui.pop(A_MENU);
            self.draw_menu(&mut ui, &m, menu_x);
            ui.unpop();
        }
        if self.palette.is_some() {
            ui.pop(A_PALETTE);
            self.draw_palette(&mut ui, &m);
            ui.unpop();
        }

        gpu.render(&frame, t.bg);
        self.gpu = Some(gpu);
        self.hits = hits;
        if anim.active {
            self.redraw();
        }
        self.anim = anim;
    }

    /// Draws the title bar. Returns the x position for the profile menu.
    fn draw_titlebar(&self, ui: &mut Ui, m: &Metrics) -> f32 {
        let (t, s) = (ui.t, m.s);
        let bar = Rect { x: 0.0, y: 0.0, w: ui.w, h: m.bar };
        let mid = (m.bar / 2.0).round();
        ui.rect(bar, t.bar, 255, 0.0);
        ui.rect(Rect { y: m.bar - 1.0, h: 1.0, ..bar }, mix(t.bar, t.border, 0.5), 255, 0.0);
        ui.hit(bar, Hit::TitleDrag);

        let mut x = 10.0 * s;
        ui.image(x - s, mid, 0, 16.0);
        x += 21.0 * s;
        ui.text(x, mid, "bebo", t.fg, Font::UiBold, 12.0, Align::Left);
        x += ui.width("bebo", Font::UiBold, 12.0) + 10.0 * s;
        if self.admin {
            ui.icon(x + 6.0 * s, mid, Icon::ShieldCheck, t.yellow, 12.0);
            ui.text(x + 16.0 * s, mid, "Administrator", t.yellow, Font::Ui, 11.0, Align::Left);
            x += 28.0 * s + ui.width("Administrator", Font::Ui, 11.0);
        }

        let buttons = 3.0 * 46.0 * s;
        let dim = mix(t.fg, t.dim, 0.4);
        let show_tabs = match self.settings.tab_bar {
            1 => true,
            2 => false,
            _ => self.tabs.len() > 1,
        };
        if !show_tabs {
            // One tab: the title bar shows the name of the tab.
            ui.text(x, mid, "/", t.dim, Font::Ui, 12.0, Align::Left);
            x += 14.0 * s;
            let title = match self.focus_pane() {
                Some(pane) => {
                    ui.badge(x, mid, pane.profile);
                    self.tab_title(self.active, pane)
                },
                None => {
                    ui.icon(x + 7.0 * s, mid, Icon::Settings, dim, 12.0);
                    "Settings".to_string()
                },
            };
            let limit = ui.w - buttons - 70.0 * s;
            ui.clip = [0.0, 0.0, limit, m.bar];
            ui.text(x + 21.0 * s, mid, &title, dim, Font::Ui, 12.0, Align::Left);
            ui.clip = [0.0, 0.0, ui.w, ui.h];
            x = (x + 27.0 * s + ui.width(&title, Font::Ui, 12.0)).min(limit);
        } else {
            // The tabs share the width. A narrow tab shows only its icon.
            // ponytail: below 24 px for each tab, the tabs go behind the window buttons. Add a tab list popup for that number of tabs.
            let available = ui.w - buttons - x - 70.0 * s;
            let tab_w = (available / self.tabs.len() as f32).clamp(24.0 * s, 200.0 * s);
            let compact = tab_w < 80.0 * s;
            // The tab below the mouse that does not show its full name: (center, name).
            let mut tip: Option<(f32, String)> = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                let r = Rect { x, y: 4.0 * s, w: tab_w - 3.0 * s, h: m.bar - 8.0 * s };
                let (active, hot) = (i == self.active, ui.hot(r));
                if active {
                    ui.panel(r, t.bg, 5.0 * s);
                } else if hot {
                    ui.rect(r, t.panel_hi, 255, 5.0 * s);
                }
                ui.hit(r, Hit::Tab(i));
                let color = if active { t.fg } else { t.dim };
                let pane = self.panes.get(&tab.focus);
                let title = pane.map_or("Settings".to_string(), |pane| self.tab_title(i, pane));
                // A narrow tab has the icon in its middle. On the active tab, the close button replaces the icon below the mouse.
                // A tab that is not active keeps its icon, so that a click selects the tab and does not close it.
                let icon_x = if compact { r.x + (r.w - 16.0 * s) / 2.0 } else { r.x + 7.0 * s };
                let close = if compact {
                    Rect { x: icon_x, y: mid - 8.0 * s, w: 16.0 * s, h: 16.0 * s }
                } else {
                    Rect { x: r.x + r.w - 21.0 * s, y: mid - 8.0 * s, w: 16.0 * s, h: 16.0 * s }
                };
                let show_close = if compact { hot && active } else { active || hot };
                if !(compact && show_close) {
                    match pane {
                        Some(pane) => ui.badge(icon_x, mid, pane.profile),
                        None => ui.icon(icon_x + 8.0 * s, mid, Icon::Settings, color, 12.0),
                    }
                }
                let room = r.w - 28.0 * s - 24.0 * s;
                if !compact {
                    ui.clip = [r.x, 0.0, r.x + 28.0 * s + room, m.bar];
                    ui.text(r.x + 28.0 * s, mid, &title, color, Font::Ui, 12.0, Align::Left);
                    ui.clip = [0.0, 0.0, ui.w, ui.h];
                }
                if show_close {
                    let close_hot = ui.hot(close);
                    if close_hot {
                        ui.rect(close, mix(t.panel_hi, t.fg, 0.12), 255, 4.0 * s);
                    }
                    ui.icon(close.x + close.w / 2.0, mid, Icon::X, if close_hot { t.fg } else { t.dim }, 11.0);
                    ui.hit(close, Hit::TabClose(i));
                }
                if hot && (compact || ui.width(&title, Font::Ui, 12.0) > room) {
                    tip = Some((r.x + r.w / 2.0, title));
                }
                x += tab_w;
            }
            // The full name shows below a tab that has no room for it.
            if let Some((center, title)) = tip.filter(|_| !self.menu && self.palette.is_none()) {
                let width = ui.width(&title, Font::Ui, 11.5) + 18.0 * s;
                let r = Rect { x: (center - width / 2.0).clamp(4.0 * s, (ui.w - width - 4.0 * s).max(4.0 * s)), y: m.bar + 3.0 * s, w: width, h: 24.0 * s };
                ui.layer();
                ui.panel(r, t.panel, 5.0 * s);
                ui.text(r.x + r.w / 2.0, r.y + r.h / 2.0, &title, t.fg, Font::Ui, 11.5, Align::Center);
                ui.popup = false;
            }
        }

        // New tab, and the menu of shell profiles.
        let mut menu_x = x;
        for (icon, hit) in [(Icon::Plus, Hit::NewTab), (Icon::ChevronDown, Hit::Menu)] {
            let r = Rect { x: x + 2.0 * s, y: 4.0 * s, w: 24.0 * s, h: m.bar - 8.0 * s };
            let hot = ui.hot(r) || (hit == Hit::Menu && self.menu);
            if hot {
                ui.rect(r, t.panel_hi, 255, 5.0 * s);
            }
            ui.icon(r.x + r.w / 2.0, mid, icon, if hot { t.fg } else { dim }, 13.0);
            ui.hit(r, hit);
            if hit == Hit::Menu {
                menu_x = r.x - 130.0 * s;
            }
            x += 26.0 * s;
        }

        let maximized = self.window.as_ref().is_some_and(|w| w.is_maximized());
        let icons = [Icon::Minus, if maximized { Icon::Copy } else { Icon::Square }, Icon::X];
        for (i, hit) in [Hit::WinMin, Hit::WinMax, Hit::WinClose].into_iter().enumerate() {
            let r = Rect { x: ui.w - (3 - i) as f32 * 46.0 * s, y: 0.0, w: 46.0 * s, h: m.bar - 1.0 };
            let hot = ui.hot(r);
            if hot {
                ui.rect(r, if hit == Hit::WinClose { [0xc4, 0x2b, 0x1c] } else { t.panel_hi }, 255, 0.0);
            }
            let color = if hot && hit == Hit::WinClose { WHITE } else { t.fg };
            ui.icon(r.x + r.w / 2.0, mid, icons[i], color, if i == 1 { 10.5 } else { 13.0 });
            ui.hit(r, hit);
        }
        menu_x.max(4.0 * s)
    }

    /// The list of shell profiles below the tab bar. The shield button opens the shell as administrator.
    fn draw_menu(&self, ui: &mut Ui, m: &Metrics, x: f32) {
        let (t, s) = (ui.t, m.s);
        let row_h = 28.0 * s;
        let width = 300.0 * s;
        let r = Rect { x: x.min(ui.w - width - 4.0 * s), y: m.bar + 2.0 * s, w: width, h: (self.profiles.len() + 1) as f32 * row_h + 13.0 * s };
        ui.panel(r, t.panel, 6.0 * s);
        ui.hit(r, Hit::Block);
        for (i, profile) in self.profiles.iter().enumerate() {
            let row = Rect { x: r.x + 4.0 * s, y: r.y + 4.0 * s + i as f32 * row_h, w: r.w - 8.0 * s, h: row_h };
            let mid = row.y + row_h / 2.0;
            let shield = Rect { x: row.x + row.w - 28.0 * s, y: mid - 11.0 * s, w: 24.0 * s, h: 22.0 * s };
            let shield_hot = ui.hot(shield);
            if ui.hot(row) {
                ui.rect(row, t.panel_hi, 255, 4.0 * s);
            }
            ui.badge(row.x + 7.0 * s, mid, *profile);
            ui.text(row.x + 30.0 * s, mid, profile.name(), t.fg, Font::Ui, 12.0, Align::Left);
            ui.hit(row, Hit::MenuItem(i));
            if shield_hot {
                ui.rect(shield, mix(t.panel_hi, t.yellow, 0.25), 255, 4.0 * s);
                ui.text(shield.x - 8.0 * s, mid, "As administrator", t.yellow, Font::Ui, 10.5, Align::Right);
            } else {
                ui.text(shield.x - 8.0 * s, mid, &format!("Ctrl+Shift+{}", i + 1), t.dim, Font::Ui, 10.5, Align::Right);
            }
            ui.icon(shield.x + shield.w / 2.0, mid, Icon::Shield, if shield_hot { t.yellow } else { t.dim }, 12.5);
            ui.hit(shield, Hit::MenuAdmin(i));
        }
        let row = Rect { x: r.x + 4.0 * s, y: r.y + r.h - row_h - 4.0 * s, w: r.w - 8.0 * s, h: row_h };
        ui.rect(Rect { x: r.x + 1.0, y: row.y - 3.0 * s, w: r.w - 2.0, h: 1.0 }, mix(t.border, t.panel, 0.3), 255, 0.0);
        if ui.hot(row) {
            ui.rect(row, t.panel_hi, 255, 4.0 * s);
        }
        let mid = row.y + row_h / 2.0;
        ui.icon(row.x + 14.0 * s, mid, Icon::Settings, t.dim, 12.5);
        ui.text(row.x + 30.0 * s, mid, "Settings", t.fg, Font::Ui, 12.0, Align::Left);
        ui.text(row.x + row.w - 8.0 * s, mid, "Ctrl+,", t.dim, Font::Ui, 10.5, Align::Right);
        ui.hit(row, Hit::MenuItem(usize::MAX));
    }

    /// The settings page. It is the content of the settings tab.
    fn draw_settings(&self, ui: &mut Ui, m: &Metrics) {
        enum Control {
            /// Row id, labels, selected label.
            Choice(u8, Vec<&'static str>, usize),
            Step(u8, String),
            Toggle(u8, bool),
            /// A box that opens a list.
            Select(u8, String),
            Label(String),
        }
        use Control::*;

        let (t, s) = (ui.t, m.s);
        let area = Rect { x: 0.0, y: m.bar, w: ui.w, h: ui.h - m.bar };
        ui.rect(area, t.bg, 255, 0.0);
        ui.hit(area, Hit::Block);
        let line = mix(t.bg, t.border, 0.55);

        let side = (185.0 * s).min(ui.w * 0.3);
        ui.rect(Rect { x: side, y: m.bar, w: 1.0, h: area.h }, line, 255, 0.0);
        let pages = [("Appearance", Icon::Palette), ("Startup", Icon::Rocket), ("Shortcuts", Icon::Keyboard)];
        let page = self.settings_page.min(pages.len() - 1);
        for (i, (name, icon)) in pages.into_iter().enumerate() {
            let r = Rect { x: 7.0 * s, y: m.bar + 12.0 * s + i as f32 * 29.0 * s, w: side - 14.0 * s, h: 27.0 * s };
            if i == page {
                ui.rect(r, t.panel_hi, 255, 5.0 * s);
            } else if ui.hot(r) {
                ui.rect(r, mix(t.bg, t.panel_hi, 0.5), 255, 5.0 * s);
            }
            let color = if i == page { t.fg } else { mix(t.fg, t.dim, 0.4) };
            ui.icon(r.x + 16.0 * s, r.y + r.h / 2.0, icon, color, 13.0);
            ui.text(r.x + 32.0 * s, r.y + r.h / 2.0, name, color, Font::Ui, 12.5, Align::Left);
            ui.hit(r, Hit::SettingsPage(i));
        }

        // The page uses the width of the window. The rows and the preview are in the middle.
        let room = ui.w - side;
        let content = (room - 64.0 * s).clamp(260.0 * s, 1100.0 * s);
        let left = (side + ((room - content) / 2.0).max(24.0 * s)).round();
        let preview = page == 0 && content > 720.0 * s;
        let right = (left + if preview { content * 0.6 } else { content.min(760.0 * s) }).round();
        ui.text(left, m.bar + 34.0 * s, pages[page].0, t.fg, Font::UiBold, 17.0, Align::Left);

        let st = &self.settings;
        let row = |title: &str, description: &'static str, control: Control| (title.to_string(), description, control);
        let rows = match page {
            0 => vec![
                row("Theme", "Colors for the window and the text", Choice(0, vec!["One Dark", "One Light", "System"], st.theme as usize)),
                row("Prompt position", "Top: after the output. Bottom: at the pane edge.", Choice(1, vec!["Top", "Bottom"], self.bottom as usize)),
                row("Command popup", "Where the colon popup opens", Choice(2, vec!["Auto", "Always below", "Always above"], st.popup as usize)),
                row("Font", "All installed fonts. Fixed-width fonts are first.", Select(3, ui.gpu.mono().to_string())),
                row("Font size", "8 to 32 px. Ctrl+= and Ctrl+- also change it.", Step(4, format!("{}", st.font_px))),
                row("Cursor", "Shape of the caret on the prompt", Choice(5, vec!["Beam", "Block", "Underline"], st.cursor as usize)),
                row("Tab bar", "Auto: with one tab, the title bar shows the tab name", Choice(6, vec!["Auto", "Always", "Never"], st.tab_bar as usize)),
                row("Minimap", "Marks for failed commands and find matches", Toggle(7, st.minimap)),
                row("Clickable paths", "Ctrl+click opens a path or a link", Toggle(8, st.links)),
                row("Animations", "The caret and the popups move smoothly", Toggle(9, st.animations)),
            ],
            1 => {
                let names = self
                    .profiles
                    .iter()
                    .map(|p| match p {
                        Profile::Pwsh => "PowerShell 7",
                        Profile::PowerShell => "PowerShell 5.1",
                        Profile::Cmd => "Command Prompt",
                        Profile::Wsl => "WSL",
                    })
                    .collect();
                let default = st.profile.unwrap_or_else(Profile::default_profile);
                let current = self.profiles.iter().position(|p| *p == default).unwrap_or(0);
                vec![row("Default shell", "The shell of the first tab. It applies at the next start.", Choice(10, names, current))]
            },
            _ => self.entries().into_iter().filter(|e| !e.keys.is_empty()).map(|e| (e.title, "", Label(e.keys.to_string()))).collect(),
        };

        let row_h = if page == 2 { 28.0 * s } else { 48.0 * s };
        let mut y = m.bar + 60.0 * s;
        let mut font_box = None;
        for (title, description, control) in rows {
            if y + row_h > ui.h {
                break;
            }
            let mid = y + row_h / 2.0;
            let box_y = mid - 13.0 * s;
            let box_h = 26.0 * s;
            let control_left = match control {
                Choice(id, labels, current) => {
                    let widths: Vec<f32> = labels.iter().map(|l| ui.width(l, Font::Ui, 11.5) + 24.0 * s).collect();
                    let x0 = right - widths.iter().sum::<f32>();
                    ui.panel(Rect { x: x0, y: box_y, w: right - x0, h: box_h }, t.bg, 5.0 * s);
                    let mut x = x0;
                    for (i, label) in labels.iter().enumerate() {
                        let r = Rect { x, y: box_y, w: widths[i], h: box_h };
                        let inner = Rect { x: r.x + 2.0, y: r.y + 2.0, w: r.w - 4.0, h: r.h - 4.0 };
                        if i == current {
                            ui.rect(inner, t.panel_hi, 255, 4.0 * s);
                        } else if ui.hot(r) {
                            ui.rect(inner, mix(t.bg, t.panel_hi, 0.5), 255, 4.0 * s);
                        }
                        let color = if i == current { t.fg } else { t.dim };
                        ui.text(r.x + r.w / 2.0, mid, label, color, Font::Ui, 11.5, Align::Center);
                        ui.hit(r, Hit::Setting(id, i as u8));
                        x += widths[i];
                    }
                    x0
                },
                Step(id, value) => {
                    let cell = 30.0 * s;
                    let x0 = right - 3.0 * cell;
                    ui.panel(Rect { x: x0, y: box_y, w: 3.0 * cell, h: box_h }, t.bg, 5.0 * s);
                    for (i, icon) in [Some(Icon::Minus), None, Some(Icon::Plus)].into_iter().enumerate() {
                        let r = Rect { x: x0 + i as f32 * cell, y: box_y, w: cell, h: box_h };
                        match icon {
                            Some(icon) => {
                                let hot = ui.hot(r);
                                if hot {
                                    ui.rect(Rect { x: r.x + 2.0, y: r.y + 2.0, w: r.w - 4.0, h: r.h - 4.0 }, t.panel_hi, 255, 4.0 * s);
                                }
                                ui.icon(r.x + r.w / 2.0, mid, icon, if hot { t.fg } else { t.dim }, 12.0);
                                ui.hit(r, Hit::Setting(id, (i / 2) as u8));
                            },
                            None => {
                                ui.rect(Rect { x: r.x, y: r.y + 2.0, w: r.w, h: r.h - 4.0 }, t.panel_hi, 255, 3.0 * s);
                                ui.text(r.x + r.w / 2.0, mid, &value, t.fg, Font::Ui, 12.0, Align::Center);
                            },
                        }
                    }
                    x0
                },
                Toggle(id, on) => {
                    let r = Rect { x: right - 30.0 * s, y: mid - 8.0 * s, w: 30.0 * s, h: 16.0 * s };
                    let knob = 12.0 * s;
                    // The knob moves from one side to the other side.
                    let progress = ui.anim.to(A_SWITCH + id as u32, on as u8 as f32, 0.02);
                    let knob_x = r.x + 2.0 * s + progress * (r.w - knob - 4.0 * s);
                    ui.rect(r, mix(mix(t.bg, t.fg, 0.22), t.accent, progress), 255, 8.0 * s);
                    ui.rect(Rect { x: knob_x, y: r.y + 2.0 * s, w: knob, h: knob }, mix(t.dim, t.bg, progress), 255, knob / 2.0);
                    ui.hit(Rect { x: r.x - 6.0 * s, y: box_y, w: r.w + 6.0 * s, h: box_h }, Hit::Setting(id, 0));
                    r.x
                },
                Select(id, name) => {
                    let width = (ui.width(&name, Font::Ui, 12.0) + 48.0 * s).max(200.0 * s);
                    let r = Rect { x: right - width, y: box_y, w: width, h: box_h };
                    let open = self.font_menu.is_some();
                    ui.panel(r, if ui.hot(r) || open { t.panel_hi } else { t.bg }, 5.0 * s);
                    ui.text(r.x + 11.0 * s, mid, &name, t.fg, Font::Ui, 12.0, Align::Left);
                    ui.icon(r.x + r.w - 15.0 * s, mid, if open { Icon::ChevronUp } else { Icon::ChevronDown }, t.dim, 13.0);
                    ui.hit(r, Hit::Setting(id, 0));
                    font_box = Some(r);
                    r.x
                },
                Label(keys) => {
                    ui.text(right, mid, &keys, t.dim, Font::Ui, 11.0, Align::Right);
                    right - 110.0 * s
                },
            };
            ui.clip = [left, y, control_left - 12.0 * s, y + row_h];
            if description.is_empty() {
                ui.text(left, mid, &title, t.fg, Font::Ui, 12.0, Align::Left);
            } else {
                ui.text(left, mid - 8.0 * s, &title, t.fg, Font::Ui, 12.5, Align::Left);
                ui.text(left, mid + 9.0 * s, description, t.dim, Font::Ui, 11.0, Align::Left);
            }
            ui.clip = [0.0, 0.0, ui.w, ui.h];
            y += row_h;
            ui.rect(Rect { x: left, y: y - 1.0, w: right - left, h: 1.0 }, line, 255, 0.0);
        }

        // Preview of the theme, the font, and the cursor.
        if preview {
            let x = right + 40.0 * s;
            ui.text(x, m.bar + 34.0 * s, "PREVIEW", t.dim, Font::UiBold, 10.0, Align::Left);
            let r = Rect { x, y: m.bar + 60.0 * s, w: left + content - x, h: 4.0 * m.ch + 24.0 * s };
            ui.panel(r, t.bg, 6.0 * s);
            ui.clip = [r.x, r.y, r.x + r.w - 6.0 * s, r.y + r.h];
            let size = ui.gpu.font_px / s;
            let span = |text: &str, color: Rgb| Span { text: text.to_string(), color, bold: false, italic: false };
            let prompt = || vec![span("PS ", t.dim), span(r"C:\Users\dev> ", t.cyan)];
            let mut first = prompt();
            first.extend([span("Get-Item", t.ansi[4]), span(r" .\a.txt", t.fg)]);
            let lines = [first, vec![span("Get-Item: Cannot find path", t.red)], vec![span(r"'C:\Users\dev\a.txt'.", t.red)], prompt()];
            for (i, spans) in lines.into_iter().enumerate() {
                ui.spans(r.x + 12.0 * s, r.y + 12.0 * s + (i as f32 + 0.5) * m.ch, spans, Font::Mono, size, Align::Left);
            }
            let (cx, cy) = (r.x + 12.0 * s + 17.0 * m.cw, r.y + 12.0 * s + 3.0 * m.ch);
            let beam = (2.0 * s).max(1.0);
            let cursor = match st.cursor {
                1 => Rect { x: cx, y: cy, w: m.cw, h: m.ch },
                2 => Rect { x: cx, y: cy + m.ch - beam, w: m.cw, h: beam },
                _ => Rect { x: cx, y: cy, w: beam, h: m.ch },
            };
            ui.rect(cursor, t.accent, 255, 0.0);
            ui.clip = [0.0, 0.0, ui.w, ui.h];
        }

        // The font list opens below the font box.
        if let (Some(scroll), Some(anchor)) = (self.font_menu, font_box) {
            let families = ui.gpu.families().to_vec();
            let current = ui.gpu.mono().to_string();
            let count = families.len();
            let first = (scroll as usize).min(count.saturating_sub(FONT_ROWS));
            let row_h = 25.0 * s;
            let width = (300.0 * s).max(anchor.w);
            let r = Rect { x: anchor.x + anchor.w - width, y: anchor.y + anchor.h + 4.0 * s, w: width, h: count.min(FONT_ROWS) as f32 * row_h + 8.0 * s };
            ui.pop(A_FONTS);
            ui.panel(r, t.panel, 6.0 * s);
            ui.hit(r, Hit::Block);
            for (i, (name, fixed)) in families.iter().enumerate().skip(first).take(FONT_ROWS) {
                let row = Rect { x: r.x + 4.0 * s, y: r.y + 4.0 * s + (i - first) as f32 * row_h, w: r.w - 16.0 * s, h: row_h };
                let mid = row.y + row_h / 2.0;
                if ui.hot(row) {
                    ui.rect(row, t.panel_hi, 255, 4.0 * s);
                }
                if *name == current {
                    ui.icon(row.x + 13.0 * s, mid, Icon::Check, t.accent, 12.0);
                }
                ui.clip = [row.x, r.y, row.x + row.w - 44.0 * s, r.y + r.h];
                ui.text(row.x + 27.0 * s, mid, name, t.fg, Font::Ui, 12.0, Align::Left);
                ui.clip = [0.0, 0.0, ui.w, ui.h];
                if *fixed {
                    ui.text(row.x + row.w - 8.0 * s, mid, "fixed", t.dim, Font::Ui, 10.0, Align::Right);
                }
                ui.hit(row, Hit::Font(i));
            }
            if count > FONT_ROWS {
                let track = r.h - 8.0 * s;
                let thumb = (FONT_ROWS as f32 / count as f32 * track).max(18.0 * s);
                let top = r.y + 4.0 * s + first as f32 / (count - FONT_ROWS) as f32 * (track - thumb);
                ui.rect(Rect { x: r.x + r.w - 8.0 * s, y: top, w: 4.0 * s, h: thumb }, mix(t.panel, t.fg, 0.3), 255, 2.0 * s);
            }
            ui.unpop();
        }
    }

    fn draw_palette(&self, ui: &mut Ui, m: &Metrics) {
        let Some(palette) = &self.palette else { return };
        let (t, s) = (ui.t, m.s);
        let rows = if palette.rename { Vec::new() } else { self.palette_rows() };
        let visible = 9;
        let first = palette.selected.saturating_sub(visible - 1);
        let shown = rows.len().saturating_sub(first).min(visible);
        let (input_h, row_h, footer_h) = (38.0 * s, 26.0 * s, 26.0 * s);
        let width = (500.0 * s).min(ui.w - 40.0 * s);
        let list_h = if palette.rename { 0.0 } else { shown as f32 * row_h + 8.0 * s + footer_h };
        let r = Rect { x: ((ui.w - width) / 2.0).round(), y: m.bar + 12.0 * s, w: width, h: input_h + list_h };

        // The window behind the palette is darker.
        let dy = std::mem::take(&mut ui.dy);
        ui.rect(Rect { x: 0.0, y: 0.0, w: ui.w, h: ui.h }, [0, 0, 0], if t.dark { 90 } else { 60 }, 0.0);
        ui.dy = dy;
        ui.panel(r, t.panel, 8.0 * s);
        ui.hit(r, Hit::Block);

        let mid = r.y + input_h / 2.0;
        ui.icon(r.x + 20.0 * s, mid, if palette.rename { Icon::Pencil } else { Icon::Search }, t.accent, 13.0);
        if palette.query.is_empty() {
            let hint = if palette.rename { "New tab name. Enter with no text removes the name." } else { "Search all actions" };
            ui.text(r.x + 32.0 * s, mid, hint, t.dim, Font::Ui, 13.0, Align::Left);
        } else {
            ui.text(r.x + 32.0 * s, mid, &palette.query, t.fg, Font::Ui, 13.0, Align::Left);
        }
        if palette.rename {
            return;
        }
        let line = mix(t.border, t.panel, 0.3);
        ui.rect(Rect { x: r.x + 1.0, y: r.y + input_h, w: r.w - 2.0, h: 1.0 }, line, 255, 0.0);

        for (i, (entry, positions)) in rows.iter().enumerate().skip(first).take(visible) {
            let row = Rect { x: r.x + 4.0 * s, y: r.y + input_h + 4.0 * s + (i - first) as f32 * row_h, w: r.w - 8.0 * s, h: row_h };
            let mid = row.y + row_h / 2.0;
            if i == palette.selected {
                ui.rect(row, t.panel_hi, 255, 5.0 * s);
            }
            ui.text(row.x + 10.0 * s, mid, entry.category, t.dim, Font::Ui, 10.5, Align::Left);
            // The matched characters have the accent color.
            let mut spans: Vec<Span> = Vec::new();
            for (index, c) in entry.title.char_indices() {
                let color = if positions.contains(&index) { t.accent } else { t.fg };
                match spans.last_mut() {
                    Some(span) if span.color == color => span.text.push(c),
                    _ => spans.push(Span { text: c.to_string(), color, bold: false, italic: false }),
                }
            }
            ui.spans(row.x + 68.0 * s, mid, spans, Font::Ui, 12.5, Align::Left);
            let keys = if entry.current { "Current" } else { entry.keys };
            ui.text(row.x + row.w - 10.0 * s, mid, keys, t.dim, Font::Ui, 10.5, Align::Right);
            ui.hit(row, Hit::PaletteItem(i));
        }

        let footer = r.y + r.h - footer_h;
        ui.rect(Rect { x: r.x + 1.0, y: footer, w: r.w - 2.0, h: 1.0 }, line, 255, 0.0);
        let mid = footer + footer_h / 2.0;
        ui.icon(r.x + 19.0 * s, mid, Icon::ArrowUp, t.dim, 11.0);
        ui.icon(r.x + 31.0 * s, mid, Icon::ArrowDown, t.dim, 11.0);
        ui.text(r.x + 41.0 * s, mid, "move", t.dim, Font::Ui, 10.5, Align::Left);
        ui.icon(r.x + 90.0 * s, mid, Icon::CornerDownLeft, t.dim, 11.0);
        ui.text(r.x + 100.0 * s, mid, "run     Esc close", t.dim, Font::Ui, 10.5, Align::Left);
        ui.text(r.x + r.w - 14.0 * s, mid, &format!("{} actions", rows.len()), t.dim, Font::Ui, 10.5, Align::Right);
    }
}

/// The message at the prompt after a restart.
fn draw_badge(ui: &mut Ui, m: &Metrics, rect: Rect, pane: &Pane) {
    let (Some(badge), Some((_, y))) = (&pane.badge, pane.cursor_px) else { return };
    let (t, s) = (ui.t, m.s);
    let color = if badge.lost { t.yellow } else { t.green };
    let width = ui.width(&badge.text, Font::Ui, 10.5) + 30.0 * s;
    let r = Rect { x: rect.x + rect.w - m.lane - 14.0 * s - width, y: y + (m.ch - 18.0 * s) / 2.0, w: width, h: 18.0 * s };
    let mid = r.y + r.h / 2.0;
    ui.rect(r, mix(color, t.bg, 0.82), 255, 9.0 * s);
    ui.icon(r.x + 13.0 * s, mid, if badge.lost { Icon::TriangleAlert } else { Icon::RotateCw }, color, 10.5);
    ui.text(r.x + 22.0 * s, mid, &badge.text, color, Font::Ui, 10.5, Align::Left);
}

fn draw_find(ui: &mut Ui, m: &Metrics, rect: Rect, find: &Find) {
    let (t, s) = (ui.t, m.s);
    let width = (340.0 * s).min(rect.w - m.lane - 16.0 * s);
    let r = Rect { x: rect.x + rect.w - m.lane - width - 12.0 * s, y: rect.y + 8.0 * s, w: width, h: 38.0 * s };
    ui.panel(r, t.panel, 8.0 * s);
    ui.hit(r, Hit::Block);
    let input = Rect { x: r.x + 6.0 * s, y: r.y + 6.0 * s, w: r.w - 12.0 * s, h: r.h - 12.0 * s };
    ui.rect(input, t.accent, 255, 5.0 * s);
    ui.rect(Rect { x: input.x + 1.0, y: input.y + 1.0, w: input.w - 2.0, h: input.h - 2.0 }, t.bg, 255, 4.0 * s);
    let mid = r.y + r.h / 2.0;

    // The controls go from right to left.
    let mut x = input.x + input.w - 4.0 * s;
    for (icon, hit) in [(Icon::X, Hit::FindClose), (Icon::ChevronDown, Hit::FindNext), (Icon::ChevronUp, Hit::FindPrev)] {
        let b = Rect { x: x - 20.0 * s, y: mid - 9.0 * s, w: 20.0 * s, h: 18.0 * s };
        if ui.hot(b) {
            ui.rect(b, t.panel_hi, 255, 4.0 * s);
        }
        ui.icon(b.x + b.w / 2.0, mid, icon, t.fg, 12.0);
        ui.hit(b, hit);
        x -= 20.0 * s;
    }
    let count = match (find.query.is_empty(), find.matches.len()) {
        (true, _) => String::new(),
        (false, 0) => "No results".to_string(),
        (false, n) => format!("{} of {n}", find.current + 1),
    };
    ui.text(x - 6.0 * s, mid, &count, t.fg, Font::Ui, 10.5, Align::Right);
    x -= 66.0 * s;
    for (i, (label, on)) in [(".*", find.regex), ("ab", find.word), ("Aa", find.case)].into_iter().enumerate() {
        let b = Rect { x: x - 22.0 * s, y: mid - 9.0 * s, w: 21.0 * s, h: 18.0 * s };
        if on {
            ui.rect(b, mix(t.accent, t.bg, 0.7), 255, 4.0 * s);
        } else if ui.hot(b) {
            ui.rect(b, t.panel_hi, 255, 4.0 * s);
        }
        ui.text(b.x + b.w / 2.0, mid, label, if on { t.accent } else { t.dim }, Font::Mono, 10.5, Align::Center);
        ui.hit(b, Hit::FindToggle(2 - i as u8));
        x -= 22.0 * s;
    }
    // A long text moves to the left, so that its end and the caret stay in view.
    let text = format!("{}\u{2502}", find.query);
    let (left, right) = (input.x + 8.0 * s, x - 6.0 * s);
    ui.clip = [input.x + 4.0 * s, r.y, x - 4.0 * s, r.y + r.h];
    if ui.width(&text, Font::Mono, 11.5) > right - left {
        ui.text(right, mid, &text, t.fg, Font::Mono, 11.5, Align::Right);
    } else {
        ui.text(left, mid, &text, t.fg, Font::Mono, 11.5, Align::Left);
    }
    ui.clip = [0.0, 0.0, ui.w, ui.h];
}

/// The list of colon commands at the prompt. It opens below the prompt, or above if there is no room.
fn draw_colon(ui: &mut Ui, m: &Metrics, rect: Rect, cursor: (f32, f32), colon: &Colon, popup: u8) {
    let items = colon_items(&colon.text);
    if items.is_empty() {
        return;
    }
    let (t, s) = (ui.t, m.s);
    let row_h = 23.0 * s;
    let width = (440.0 * s).min(rect.w - 2.0 * m.pad - m.lane);
    let height = items.len() as f32 * row_h + 8.0 * s;
    let below = cursor.1 + m.ch + 5.0 * s;
    let above = cursor.1 - height - 5.0 * s;
    let y = match popup {
        1 => below,
        2 => above,
        _ if below + height > rect.y + rect.h => above,
        _ => below,
    };
    let r = Rect { x: rect.x + m.pad, y, w: width, h: height };
    ui.panel(r, t.panel, 6.0 * s);
    ui.hit(r, Hit::Block);
    for (i, (name, description, keys)) in items.iter().enumerate() {
        let row = Rect { x: r.x + 4.0 * s, y: r.y + 4.0 * s + i as f32 * row_h, w: r.w - 8.0 * s, h: row_h };
        let mid = row.y + row_h / 2.0;
        if i == colon.selected || ui.hot(row) {
            ui.rect(row, t.panel_hi, 255, 4.0 * s);
        }
        ui.text(row.x + 8.0 * s, mid, name, t.accent, Font::Mono, 11.5, Align::Left);
        ui.text(row.x + 80.0 * s, mid, description, t.fg, Font::Ui, 12.0, Align::Left);
        ui.text(row.x + row.w - 8.0 * s, mid, keys, t.dim, Font::Ui, 10.5, Align::Right);
        ui.hit(row, Hit::ColonItem(i));
    }
}

/// The question before a restart that stops a command.
fn draw_confirm(ui: &mut Ui, m: &Metrics, rect: Rect, pane: &Pane, clean: bool) {
    let (t, s) = (ui.t, m.s);
    let width = (460.0 * s).min(rect.w - 2.0 * m.pad - m.lane);
    let r = Rect { x: rect.x + m.pad, y: rect.y + rect.h - 98.0 * s - m.ch, w: width, h: 92.0 * s };
    ui.panel(r, t.panel, 6.0 * s);
    ui.hit(r, Hit::Block);
    ui.clip = [r.x, r.y, r.x + r.w - 8.0 * s, r.y + r.h];
    let x = r.x + 12.0 * s;
    let line = |i: f32| r.y + 16.0 * s + i * 20.0 * s;
    ui.text(x, line(0.0), "Restart this session", t.fg, Font::UiBold, 12.0, Align::Left);
    let folder = pane.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let offset = ui.width("Restart this session", Font::UiBold, 12.0) + 12.0 * s;
    ui.text(x + offset, line(0.0), &folder, t.cyan, Font::Ui, 10.5, Align::Left);
    let effect = if clean { "The content will be lost. The shell opens in the home folder." } else { "Scrollback and folder will be restored" };
    ui.text(x, line(1.0), effect, t.fg, Font::Ui, 11.5, Align::Left);
    let command = pane.blocks.last().map_or("", |b| b.command.as_str());
    ui.text(x, line(2.0), &format!("1 job will stop: {command}"), t.yellow, Font::Ui, 11.5, Align::Left);
    ui.text(x, line(3.0), "\u{21b5} restart     esc cancel", t.dim, Font::Ui, 10.5, Align::Left);
    ui.clip = [0.0, 0.0, ui.w, ui.h];
}

fn rgba(c: Rgb) -> [u8; 4] {
    [c[0], c[1], c[2], 255]
}

/// How `draw_pane` shows one pane in this frame.
struct View<'a> {
    rect: Rect,
    focused: bool,
    /// The tab has two or more panes.
    multi: bool,
    bottom: bool,
    find: Option<&'a Find>,
    colon: Option<&'a Colon>,
    /// The link below the mouse: grid line, first column, last column.
    link: Option<(i32, usize, usize)>,
    /// The mouse is on the minimap.
    lane_hot: bool,
}

/// Draws the terminal text, the cursor, the find matches, and the minimap of one pane.
/// Returns the grid line at the top of the view and the pixel position of the cursor cell.
#[allow(clippy::too_many_arguments)]
fn draw_pane(
    frame: &mut Frame,
    anim: &mut Anim,
    id: usize,
    pane: &Pane,
    view: &View,
    m: &Metrics,
    t: &Theme,
    settings: &Settings,
) -> (i32, Option<(f32, f32)>) {
    let View { rect, focused, multi, bottom, find, colon, link, lane_hot } = *view;
    let term = pane.term.lock();
    let grid = term.grid();
    let (cw, ch, s) = (m.cw, m.ch, m.s);
    let (rows, cols) = (pane.rows.min(grid.screen_lines()) as i32, grid.columns());
    let top = view_top(&term, rows as usize, bottom);
    let history = grid.history_size() as i32;
    let (ox, oy) = (rect.x + m.pad, rect.y + (m.pad * 0.6).round());
    let lane_x = rect.x + rect.w - m.lane;
    let clip = [rect.x, rect.y, lane_x, rect.y + rect.h];
    let quad = |x: f32, y: f32, w: f32, h: f32, color: Rgb| Quad { x, y, w, h, color: rgba(color), radius: 0.0 };

    let alt = term.mode().contains(TermMode::ALT_SCREEN);
    let selection = term.selection.as_ref().and_then(|s| s.to_range(&term));
    let cursor = grid.cursor.point;
    let cursor_row = cursor.line.0 - top;
    let region = Region::new(pane, &term);
    // The shell hides the cursor for a moment while it draws the command line again.
    // At the prompt, bebo shows the cursor always. This prevents a flicker.
    let cursor_shown = term.mode().contains(TermMode::SHOW_CURSOR) || region.is_some();
    let cursor_visible = cursor_shown && (0..rows).contains(&cursor_row);
    // After a character in the last column, the cursor stays in that column until the next character wraps the line.
    // The caret then shows after that character, not on it.
    let wrap = grid.cursor.input_needs_wrap as usize;
    let cursor_px = (ox + (cursor.column.0 + wrap) as f32 * cw, oy + cursor_row as f32 * ch);
    // The prompt has a beam cursor. A full-screen program selects its own shape.
    let prompt_shape = [CursorShape::Beam, CursorShape::Block, CursorShape::Underline][settings.cursor.min(2) as usize];
    let shape = if alt { term.cursor_style().shape } else { prompt_shape };

    // Find matches. The current match is brighter.
    if let Some(find) = find {
        for (i, (a, b)) in find.matches.iter().enumerate() {
            if b.line.0 < top || a.line.0 >= top + rows {
                continue;
            }
            let color = if i == find.current { mix(t.yellow, t.bg, 0.45) } else { t.find };
            for line in a.line.0.max(top)..=b.line.0.min(top + rows - 1) {
                let first = if line == a.line.0 { a.column.0 } else { 0 };
                let last = if line == b.line.0 { b.column.0 } else { cols - 1 };
                let y = oy + (line - top) as f32 * ch;
                frame.quads.push(quad(ox + first as f32 * cw, y, (last + 1 - first) as f32 * cw, ch, color));
            }
        }
    }

    for row in (-history - top).max(0)..rows {
        let line = Line(top + row);
        let y = oy + row as f32 * ch;
        let mut spans: Vec<Span> = Vec::new();
        // A fallback font has its own character width. Each wide character is a separate text row at its cell,
        // so that the text after it stays on the cell grid. `seg_x` is the start of the current text row.
        let mut seg_x = ox;
        for col in 0..cols {
            let cell = &grid[line][Column(col)];
            let flags = cell.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let point = Point::new(line, Column(col));
            let mut fg = resolve(cell.fg, term.colors(), t);
            let mut bg = resolve(cell.bg, term.colors(), t);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) {
                fg = mix(fg, t.bg, 0.35);
            }
            if selection.is_some_and(|s| s.contains(point)) {
                bg = t.selection;
            }
            let x = ox + col as f32 * cw;
            let w = if flags.contains(Flags::WIDE_CHAR) { cw * 2.0 } else { cw };
            if cursor_visible && point == cursor && focused && shape == CursorShape::Block {
                bg = t.accent;
                fg = t.bg;
            }
            if bg != t.bg {
                // Extend the last rectangle if it has the same color and touches this cell.
                match frame.quads.last_mut() {
                    Some(q) if q.y == y && q.color == rgba(bg) && (q.x + q.w - x).abs() < 0.5 => q.w += w,
                    _ => frame.quads.push(quad(x, y, w, ch, bg)),
                }
            }
            if flags.intersects(Flags::ALL_UNDERLINES) {
                frame.overlay.push(quad(x, y + ch - 2.0, w, 1.0, fg));
            }
            if flags.contains(Flags::STRIKEOUT) {
                frame.overlay.push(quad(x, y + ch / 2.0, w, 1.0, fg));
            }
            let c = if flags.contains(Flags::HIDDEN) || cell.c == '\t' { ' ' } else { cell.c };
            let (bold, italic) = (flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC));
            if flags.contains(Flags::WIDE_CHAR) {
                if spans.iter().any(|s| !s.text.trim().is_empty()) {
                    frame.rows.push(Row { x: seg_x, y, clip, spans: std::mem::take(&mut spans), ..Default::default() });
                }
                spans.clear();
                let mut text = c.to_string();
                text.extend(cell.zerowidth().into_iter().flatten());
                let span = Span { text, color: fg, bold, italic };
                frame.rows.push(Row { x, y, clip, spans: vec![span], ..Default::default() });
                seg_x = x + w;
                continue;
            }
            match spans.last_mut() {
                Some(span) if span.color == fg && span.bold == bold && span.italic == italic => span.text.push(c),
                _ => spans.push(Span { text: c.to_string(), color: fg, bold, italic }),
            }
            if let Some(extra) = cell.zerowidth() {
                spans.last_mut().unwrap().text.extend(extra);
            }
        }
        while let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }
        if !spans.is_empty() {
            frame.rows.push(Row { x: seg_x, y, clip, spans, ..Default::default() });
        }
    }

    let mono = |x: f32, y: f32, text: String, color: Rgb| Row {
        x,
        y,
        clip,
        spans: vec![Span { text, color, bold: false, italic: false }],
        ..Default::default()
    };
    if cursor_visible {
        let (x, y) = cursor_px;
        let color = if focused { t.accent } else { t.dim };
        let beam = (2.0 * s).max(1.0);
        // The colon command is not in the shell. bebo draws it at the prompt.
        let colon_text = colon.map(|colon| format!(":{}", colon.text));
        let target = x + colon_text.as_ref().map_or(0.0, |text| text.chars().count() as f32 * cw);
        // The caret moves smoothly along its row. A different row is an immediate change.
        let (key_x, key_y) = (A_CARET + 2 * id as u32, A_CARET + 2 * id as u32 + 1);
        let caret = if anim.caret_live && anim.values.get(&key_y) == Some(&y) { anim.to(key_x, target, 0.4) } else { target };
        anim.set(key_x, caret);
        anim.set(key_y, y);
        if let Some(text) = colon_text {
            frame.rows.push(mono(x, y, text, t.fg));
            frame.overlay.push(quad(caret, y, beam, ch, color));
        } else {
            match shape {
                CursorShape::Block if focused => {},
                CursorShape::Underline => frame.overlay.push(quad(caret, y + ch - beam, cw, beam, color)),
                CursorShape::Hidden => {},
                CursorShape::Beam => frame.overlay.push(quad(caret, y, beam, ch, color)),
                _ => {
                    for (qx, qy, qw, qh) in [(x, y, cw, 1.0), (x, y + ch - 1.0, cw, 1.0), (x, y, 1.0, ch), (x + cw - 1.0, y, 1.0, ch)] {
                        frame.overlay.push(quad(qx, qy, qw, qh, color));
                    }
                },
            }
        }
        // The current prompt has an accent bar at the left edge, from its first row to the cursor row.
        if let (Some(region), true) = (&region, focused) {
            let first = (region.start.line.0 - top).clamp(0, cursor_row);
            let bar_y = oy + first as f32 * ch;
            frame.overlay.push(quad(rect.x + 1.0, bar_y, beam, y + ch - bar_y, t.accent));
        }
    }

    // The path or the link below the mouse has an underline.
    if let Some((line, first, last)) = link
        && (0..rows).contains(&(line - top))
    {
        let y = oy + (line - top + 1) as f32 * ch - (2.0 * s).max(1.0);
        frame.overlay.push(quad(ox + first as f32 * cw, y, (last + 1 - first) as f32 * cw, s.max(1.0), t.accent));
    }

    // Command blocks: the duration and the exit code of a command, at the right side of its prompt line.
    // ponytail: when the scrollback is full, old lines drop and the block lines are wrong. Blocks are off then.
    let blocks_ok = history < 10_000 && !alt;
    if blocks_ok {
        for block in &pane.blocks {
            let row = block.line - history - top;
            let failed = block.failed();
            if let (Some(duration), true) = (block.duration, (0..rows).contains(&row))
                && (duration >= Duration::from_secs(1) || failed)
            {
                let secs = duration.as_secs();
                let mut label = match secs {
                    0 => String::new(),
                    1..60 => format!("{:.1}s", duration.as_secs_f32()),
                    _ => format!("{}m{}s", secs / 60, secs % 60),
                };
                if block.exit == Some(CTRL_C_EXIT) {
                    label = format!("stopped  {label}").trim_end().to_string();
                } else if let Some(code) = block.exit.filter(|_| failed) {
                    label = format!("exit {code}  {label}").trim_end().to_string();
                }
                // The label shows only if it does not cover the text of its line.
                let start = cols.saturating_sub(label.chars().count() + 1);
                let line = Line(block.line - history);
                if (start..cols).any(|col| grid[line][Column(col)].c != ' ') {
                    continue;
                }
                let x = ox + (start + 1) as f32 * cw;
                let color = if failed { t.red } else { t.dim };
                frame.rows.push(mono(x, oy + row as f32 * ch, label, color));
            }
        }
    }

    // Minimap: the view position, failed commands, and find matches.
    if m.lane > 0.0 {
    let total = (history + rows) as f32;
    let lane_y = |line: i32| rect.y + (line + history) as f32 / total * rect.h;
    frame.quads.push(quad(lane_x, rect.y, 1.0, rect.h, mix(t.bg, t.border, 0.5)));
    let thumb_h = (rows as f32 / total * rect.h).max(24.0 * s);
    if lane_hot {
        frame.quads.push(quad(lane_x + 1.0, rect.y, m.lane - 1.0, rect.h, mix(t.bg, t.fg, 0.04)));
    }
    let thumb = mix(t.bg, t.fg, if lane_hot { 0.32 } else { 0.18 });
    frame.quads.push(quad(lane_x + 1.0, lane_y(top).min(rect.y + rect.h - thumb_h).max(rect.y), m.lane - 1.0, thumb_h, thumb));
    let mark = (2.0 * s).max(1.0);
    if blocks_ok {
        for block in pane.blocks.iter().filter(|b| b.failed()) {
            frame.quads.push(quad(lane_x + 3.0 * s, lane_y(block.line - history), m.lane - 5.0 * s, mark, t.red));
        }
    }
    if let Some(find) = find {
        for (a, _) in &find.matches {
            frame.quads.push(quad(lane_x + 3.0 * s, lane_y(a.line.0), m.lane - 5.0 * s, mark, t.yellow));
        }
    }
    }

    if multi {
        if focused {
            frame.overlay.push(quad(rect.x, rect.y, rect.w, (2.0 * s).max(1.0), t.accent));
        } else {
            // A pane with no focus is dimmer.
            frame.overlay.push(Quad { x: rect.x, y: rect.y, w: rect.w, h: rect.h, color: [t.bg[0], t.bg[1], t.bg[2], 90], radius: 0.0 });
        }
    }
    (top, cursor_visible.then_some(cursor_px))
}
