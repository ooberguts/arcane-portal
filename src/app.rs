use crate::Frame;
use crate::capture::{self, Capture, FrameRate, Resolution, Source, SourceKind, VideoSettings};
use crate::network::{Discovery, Host, Room, Viewer};
use crate::update::{CURRENT_VERSION, RELEASES_URL, UpdateStatus, Updater};
use eframe::egui::{
    self, Align, Color32, FontId, Layout, Margin, Painter, Pos2, Rect, Response, RichText, Sense,
    Shape, Stroke, StrokeKind, TextureHandle, Ui, UiBuilder, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const BACKGROUND: Color32 = Color32::from_rgb(16, 17, 23);
const SIDEBAR: Color32 = Color32::from_rgb(21, 22, 30);
const SURFACE: Color32 = Color32::from_rgb(25, 26, 35);
const FIELD: Color32 = Color32::from_rgb(17, 18, 25);
const BORDER: Color32 = Color32::from_rgb(46, 47, 62);
const TEXT: Color32 = Color32::from_rgb(244, 243, 250);
const QUIET: Color32 = Color32::from_rgb(163, 163, 184);
const ACCENT: Color32 = Color32::from_rgb(201, 183, 255);
const ACCENT_DARK: Color32 = Color32::from_rgb(39, 30, 63);
const GREEN: Color32 = Color32::from_rgb(115, 223, 173);
const RED: Color32 = Color32::from_rgb(255, 156, 174);
const PREFS_KEY: &str = "arcane-portal-preferences";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Rooms,
    Broadcast,
    Settings,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Rooms => "Live Rooms",
            Self::Broadcast => "Broadcast",
            Self::Settings => "Settings",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Rooms => Icon::Rooms,
            Self::Broadcast => Icon::Broadcast,
            Self::Settings => Icon::Settings,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(from = "StoredPreferences")]
struct Preferences {
    name: String,
    video: VideoSettings,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            name: default_name(),
            video: VideoSettings::default(),
        }
    }
}

#[derive(Deserialize)]
struct StoredPreferences {
    name: Option<String>,
    video: Option<VideoSettings>,
    quality: Option<u8>,
}

impl From<StoredPreferences> for Preferences {
    fn from(stored: StoredPreferences) -> Self {
        let video = stored.video.unwrap_or_else(|| match stored.quality {
            Some(0) => VideoSettings {
                resolution: Resolution::P480,
                frame_rate: FrameRate::Fps15,
            },
            Some(1) => VideoSettings {
                resolution: Resolution::P720,
                frame_rate: FrameRate::Fps24,
            },
            Some(2) => VideoSettings {
                resolution: Resolution::P1080,
                frame_rate: FrameRate::Fps30,
            },
            _ => VideoSettings::default(),
        });
        Self {
            name: stored.name.unwrap_or_else(default_name),
            video,
        }
    }
}

enum Session {
    Idle,
    Hosting {
        host: Host,
        capture: Capture,
        name: String,
        title: String,
    },
    Watching {
        viewer: Viewer,
        selected_room: Room,
    },
}

impl Session {
    fn active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn frame(&self) -> Option<Arc<Frame>> {
        match self {
            Self::Hosting { capture, .. } => capture.frame(),
            Self::Watching { viewer, .. } => viewer.frame(),
            Self::Idle => None,
        }
    }
}

pub struct PortalApp {
    page: Page,
    preferences: Preferences,
    name_draft: String,
    title_draft: String,
    discovery: Option<Discovery>,
    discovery_error: Option<String>,
    sources: Vec<Source>,
    source_kind: SourceKind,
    selected_source: Option<u32>,
    source_request: Option<mpsc::Receiver<Result<Vec<Source>, String>>>,
    source_error: Option<String>,
    session: Session,
    texture: Option<(Arc<Frame>, TextureHandle)>,
    fullscreen: bool,
    share_audio: bool,
    audio_capture: Option<crate::audio::AudioCapture>,
    audio_capture_start: Option<mpsc::Receiver<Result<crate::audio::AudioCapture, String>>>,
    audio_capture_enabled: Option<Arc<std::sync::atomic::AtomicBool>>,
    play_audio: bool,
    audio_output: Option<crate::audio::AudioOutput>,
    audio_output_start: Option<mpsc::Receiver<Result<crate::audio::AudioOutput, String>>>,
    notice: Option<(String, bool)>,
    updater: Updater,
}

impl PortalApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let preferences: Preferences = cc
            .storage
            .and_then(|storage| storage.get_string(PREFS_KEY))
            .and_then(|json| serde_json::from_str(&json).ok())
            .filter(|preferences: &Preferences| !preferences.name.trim().is_empty())
            .unwrap_or_default();
        let mut updater = Updater::default();
        let page = if let Some(error) = crate::install::previous_error() {
            updater.report_failure(error);
            Page::Settings
        } else {
            Page::Rooms
        };
        let mut app = Self {
            page,
            name_draft: preferences.name.clone(),
            preferences,
            title_draft: String::new(),
            discovery: None,
            discovery_error: None,
            sources: Vec::new(),
            source_kind: SourceKind::Display,
            selected_source: None,
            source_request: None,
            source_error: None,
            session: Session::Idle,
            texture: None,
            fullscreen: false,
            share_audio: false,
            audio_capture: None,
            audio_capture_start: None,
            audio_capture_enabled: None,
            play_audio: true,
            audio_output: None,
            audio_output_start: None,
            notice: None,
            updater,
        };
        app.refresh_discovery();
        app
    }

    fn refresh_discovery(&mut self) {
        self.discovery = None;
        match Discovery::start() {
            Ok(discovery) => {
                self.discovery = Some(discovery);
                self.discovery_error = None;
            }
            Err(error) => self.discovery_error = Some(error),
        }
    }

    fn navigate(&mut self, page: Page, ctx: &egui::Context) {
        if self.page == page {
            return;
        }
        self.page = page;
        self.notice = None;
        if page == Page::Broadcast
            && !matches!(self.session, Session::Hosting { .. })
            && self.source_request.is_none()
        {
            self.refresh_sources(ctx);
        }
    }

    fn refresh_sources(&mut self, ctx: &egui::Context) {
        self.source_error = None;
        let (send, receive) = mpsc::channel();
        let ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("portal-source-discovery".into())
            .spawn(move || {
                let _ = send.send(capture::sources());
                ctx.request_repaint();
            }) {
            Ok(_) => self.source_request = Some(receive),
            Err(error) => self.source_error = Some(error.to_string()),
        }
    }

    fn update_sources(&mut self) {
        let Some(receive) = self.source_request.as_ref() else {
            return;
        };
        match receive.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return,
            Ok(Ok(sources)) => {
                self.sources = sources;
                self.select_first_source();
            }
            Ok(Err(error)) => self.source_error = Some(error),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.source_error = Some("Could not list capture sources.".into());
            }
        }
        self.source_request = None;
    }

    fn select_first_source(&mut self) {
        let has_selected = self.sources.iter().any(|source| {
            source.kind == self.source_kind && self.selected_source == Some(source.id)
        });
        if !has_selected {
            self.selected_source = self
                .sources
                .iter()
                .find(|source| source.kind == self.source_kind)
                .map(|source| source.id);
        }
    }

    fn start_broadcast(&mut self) {
        let Some(source) = self
            .sources
            .iter()
            .find(|source| {
                source.kind == self.source_kind && self.selected_source == Some(source.id)
            })
            .cloned()
        else {
            return;
        };
        let title = match self.title_draft.trim() {
            "" => "Screen share".to_owned(),
            title => title.to_owned(),
        };
        let settings = self.preferences.video;
        let (height, fps) = settings.initial_format();
        let started = Host::start(&self.preferences.name, &title, height, fps).and_then(|host| {
            Capture::start(source, settings, host.publisher()).map(|capture| (host, capture))
        });
        match started {
            Ok((host, capture)) => {
                self.notice = None;
                self.session = Session::Hosting {
                    host,
                    capture,
                    name: self.preferences.name.clone(),
                    title,
                };
                self.update_audio_capture();
            }
            Err(error) => self.notice = Some((error, true)),
        }
    }

    fn update_audio_capture(&mut self) {
        if !self.share_audio {
            if let Some(enabled) = self.audio_capture_enabled.take() {
                enabled.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            self.audio_capture_start = None;
            self.audio_capture = None;
            if let Session::Hosting { host, .. } = &self.session {
                host.clear_audio();
            }
        } else if self.audio_capture.is_none()
            && self.audio_capture_start.is_none()
            && let Session::Hosting { host, .. } = &self.session
        {
            let publish = host.audio_publisher();
            let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
            self.audio_capture_enabled = Some(enabled.clone());
            let publish = move |packet| {
                if enabled.load(std::sync::atomic::Ordering::Relaxed) {
                    publish(packet);
                }
            };
            let (send, receive) = mpsc::channel();
            match std::thread::Builder::new()
                .name("portal-audio-capture-start".into())
                .spawn(move || {
                    let _ = send.send(crate::audio::AudioCapture::start(publish));
                }) {
                Ok(_) => self.audio_capture_start = Some(receive),
                Err(error) => {
                    self.share_audio = false;
                    self.notice = Some((format!("Could not share system audio: {error}"), true));
                }
            }
        }
    }

    fn start_audio_output(&mut self) {
        if !self.play_audio || self.audio_output.is_some() || self.audio_output_start.is_some() {
            return;
        }
        let (send, receive) = mpsc::channel();
        match std::thread::Builder::new()
            .name("portal-audio-output-start".into())
            .spawn(move || {
                let _ = send.send(crate::audio::AudioOutput::start());
            }) {
            Ok(_) => self.audio_output_start = Some(receive),
            Err(error) => {
                self.play_audio = false;
                self.notice = Some((format!("Could not play audio: {error}"), true));
            }
        }
    }

    fn audio_controls(&mut self, ui: &mut Ui) {
        if ui
            .checkbox(&mut self.share_audio, "Share system audio")
            .changed()
        {
            self.update_audio_capture();
        }
    }

    fn join(&mut self, selected_room: Room) {
        match Viewer::join(&selected_room) {
            Ok(viewer) => {
                self.notice = None;
                self.texture = None;
                self.audio_output_start = None;
                self.audio_output = None;
                self.session = Session::Watching {
                    viewer,
                    selected_room,
                };
            }
            Err(error) => self.notice = Some((error, true)),
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, fullscreen: bool) {
        let fullscreen = fullscreen && matches!(self.session, Session::Watching { .. });
        self.fullscreen = fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
    }

    fn end_session(&mut self, ctx: &egui::Context) {
        if let Some(enabled) = self.audio_capture_enabled.take() {
            enabled.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        self.audio_capture_start = None;
        self.audio_capture = None;
        if let Session::Hosting { host, .. } = &self.session {
            host.clear_audio();
        }
        self.audio_output_start = None;
        self.audio_output = None;
        self.share_audio = false;
        self.session = Session::Idle;
        self.texture = None;
        if self.fullscreen {
            self.set_fullscreen(ctx, false);
        }
    }

    fn update_session(&mut self, ctx: &egui::Context) {
        if let Some(result) = self
            .audio_capture_start
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Audio capture worker stopped.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            })
        {
            self.audio_capture_start = None;
            match result {
                Ok(capture) => self.audio_capture = Some(capture),
                Err(error) => {
                    self.share_audio = false;
                    self.notice = Some((format!("Could not share system audio: {error}"), true));
                }
            }
        }
        if let Some(result) = self
            .audio_output_start
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Audio playback worker stopped.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            })
        {
            self.audio_output_start = None;
            match result {
                Ok(output) => self.audio_output = Some(output),
                Err(error) => {
                    self.play_audio = false;
                    self.notice = Some((format!("Could not play audio: {error}"), true));
                }
            }
        }
        if let Some(error) = self
            .audio_capture
            .as_ref()
            .and_then(|capture| capture.error())
        {
            self.share_audio = false;
            self.update_audio_capture();
            self.notice = Some((format!("System audio stopped: {error}"), true));
        }
        if let Some(error) = self.audio_output.as_ref().and_then(|output| output.error()) {
            self.audio_output = None;
            self.play_audio = false;
            self.notice = Some((format!("Audio playback stopped: {error}"), true));
        }
        let packets = match &self.session {
            Session::Watching { viewer, .. } => viewer.take_audio_packets(),
            _ => Vec::new(),
        };
        if !packets.is_empty() {
            self.start_audio_output();
            if let Some(output) = &self.audio_output {
                for packet in packets {
                    output.push(&packet);
                }
            }
        }
        if let Session::Hosting { host, capture, .. } = &self.session {
            if let Some(error) = capture.error() {
                self.end_session(ctx);
                self.navigate(Page::Broadcast, ctx);
                self.notice = Some((error, true));
                return;
            }
            let (height, fps) = capture.format();
            host.set_format(height, fps);
            capture.set_network_fps_limit(host.network_fps_limit());
        }
        if self.fullscreen
            && (ctx.input(|input| input.key_pressed(egui::Key::Escape))
                || matches!(&self.session, Session::Watching { viewer, .. } if viewer.error().is_some()))
        {
            self.set_fullscreen(ctx, false);
        }
        if let Some(frame) = self.session.frame() {
            let already_visible = self
                .texture
                .as_ref()
                .is_some_and(|(visible, _)| Arc::ptr_eq(visible, &frame));
            if !already_visible {
                let pixels = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.rgba,
                );
                if let Some((visible, texture)) = self.texture.as_mut() {
                    texture.set(pixels, egui::TextureOptions::LINEAR);
                    *visible = frame;
                } else {
                    let texture =
                        ctx.load_texture("live-screen", pixels, egui::TextureOptions::LINEAR);
                    self.texture = Some((frame, texture));
                }
            }
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("portal-sidebar")
            .exact_width(222.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::same(17)),
            )
            .show(ctx, |ui| {
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    let (logo, _) = ui.allocate_exact_size(vec2(41.0, 41.0), Sense::hover());
                    let painter = ui.painter();
                    painter.rect_filled(logo, 11, ACCENT_DARK);
                    icon(painter, Icon::Portal, logo.center(), 24.0, ACCENT);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Arcane Portal")
                                .size(17.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new("Local screen sharing")
                                .size(11.0)
                                .color(QUIET),
                        );
                    });
                });
                ui.add_space(32.0);
                for page in [Page::Rooms, Page::Broadcast, Page::Settings] {
                    if nav_item(ui, page.title(), page.icon(), page == self.page).clicked() {
                        self.navigate(page, ctx);
                    }
                    ui.add_space(3.0);
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    let profile = egui::Frame::new()
                        .fill(SURFACE)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .corner_radius(10)
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let (avatar, _) =
                                    ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                                ui.painter()
                                    .circle_filled(avatar.center(), 16.0, ACCENT_DARK);
                                let first = self.preferences.name.chars().next().unwrap_or('A');
                                ui.painter().text(
                                    avatar.center(),
                                    egui::Align2::CENTER_CENTER,
                                    first.to_uppercase().collect::<String>(),
                                    FontId::proportional(15.0),
                                    ACCENT,
                                );
                                ui.add_space(2.0);
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&self.preferences.name).strong(),
                                        )
                                        .truncate(),
                                    );
                                    let identity = match self.session {
                                        Session::Idle => "Watch only",
                                        Session::Hosting { .. } => "Broadcasting",
                                        Session::Watching { .. } => "Watching",
                                    };
                                    ui.label(RichText::new(identity).size(11.0).color(QUIET));
                                });
                            });
                        });
                    if profile
                        .response
                        .interact(Sense::click())
                        .on_hover_text("Edit your display name")
                        .clicked()
                    {
                        self.navigate(Page::Settings, ctx);
                    }
                    if self.session.active() {
                        let page = if matches!(self.session, Session::Hosting { .. }) {
                            Page::Broadcast
                        } else {
                            Page::Rooms
                        };
                        ui.add_space(10.0);
                        if nav_item(ui, "Return to stream", Icon::Play, false).clicked() {
                            self.navigate(page, ctx);
                        }
                    }
                });
            });
    }

    fn header(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("portal-header")
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(Margin::symmetric(29, 0)),
            )
            .show(ctx, |ui| {
                ui.set_height(60.0);
                ui.horizontal_centered(|ui| {
                    let (marker, _) = ui.allocate_exact_size(vec2(19.0, 20.0), Sense::hover());
                    icon(ui.painter(), self.page.icon(), marker.center(), 17.0, QUIET);
                    ui.add_space(2.0);
                    ui.label(RichText::new(self.page.title()).size(15.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (label, color) = if self.discovery.is_some() {
                            ("LAN discovery active", GREEN)
                        } else {
                            ("LAN discovery unavailable", QUIET)
                        };
                        badge(ui, label, color);
                    });
                });
                let rect = ui.max_rect();
                ui.painter()
                    .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, BORDER));
            });
    }

    fn rooms(&mut self, ui: &mut Ui) {
        if matches!(self.session, Session::Watching { .. }) {
            self.watching(ui);
            return;
        }
        self.hero(ui);
        ui.add_space(28.0);
        let own_addresses = match &self.session {
            Session::Hosting { host, .. } => host.addresses(),
            _ => Vec::new(),
        };
        let mut rooms = self
            .discovery
            .as_ref()
            .map_or_else(Vec::new, Discovery::rooms);
        rooms.retain(|room| !own_addresses.contains(&room.address));
        rooms.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.address.cmp(&b.address))
        });
        ui.horizontal(|ui| {
            heading(ui, "Available rooms", 21.0);
            if !rooms.is_empty() {
                badge(ui, &rooms.len().to_string(), QUIET);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Refresh rooms", false).clicked() {
                    self.refresh_discovery();
                }
            });
        });
        ui.add_space(9.0);
        if rooms.is_empty() {
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    icon_tile(ui, Icon::Rooms, 42.0);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("No local broadcasts found")
                                .strong()
                                .size(15.0),
                        );
                        let description = self
                            .discovery_error
                            .as_deref()
                            .unwrap_or("Rooms appear here when someone on your network goes live.");
                        ui.label(RichText::new(description).color(QUIET).size(13.0));
                    });
                });
            });
        } else {
            for room in rooms {
                if display_room(ui, &room, !self.session.active()) {
                    self.join(room);
                    break;
                }
                ui.add_space(7.0);
            }
        }
        if matches!(self.session, Session::Hosting { .. }) {
            ui.add_space(9.0);
            ui.label(
                RichText::new("Stop your broadcast before joining another room.")
                    .size(12.0)
                    .color(QUIET),
            );
        }
    }

    fn hero(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, 244.0), Sense::hover());
        portal_art(ui.painter(), rect);
        let content = Rect::from_min_size(
            rect.min + vec2(24.0, 28.0),
            vec2((width * 0.63).max(245.0).min(width - 48.0), 190.0),
        );
        let mut content_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
        );
        content_ui.set_max_width(content.width());
        content_ui.label(
            RichText::new("Watch a live broadcast")
                .size(33.0)
                .strong()
                .color(TEXT),
        );
        content_ui.add_space(5.0);
        content_ui.label(
            RichText::new(
                "Watch someone’s screen or share your own with people on the same local network.",
            )
            .size(14.0)
            .color(QUIET),
        );
        content_ui.add_space(15.0);
        let title = if matches!(self.session, Session::Hosting { .. }) {
            "Return to broadcast"
        } else {
            "Set up a broadcast"
        };
        if button(&mut content_ui, title, true).clicked() {
            self.navigate(Page::Broadcast, content_ui.ctx());
        }
    }

    fn broadcast(&mut self, ui: &mut Ui) {
        if matches!(self.session, Session::Hosting { .. }) {
            self.hosting(ui);
            return;
        }
        heading(ui, "Share your screen", 31.0);
        ui.label(RichText::new("Choose what viewers on your local network will see.").color(QUIET));
        ui.add_space(19.0);
        if matches!(self.session, Session::Watching { .. }) {
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label("Leave the current room before starting a broadcast.");
                if button(ui, "Return to stream", false).clicked() {
                    self.navigate(Page::Rooms, ui.ctx());
                }
            });
            return;
        }
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                heading(ui, "Capture source", 18.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            self.source_request.is_none(),
                            egui::Button::new("Refresh sources"),
                        )
                        .clicked()
                    {
                        self.refresh_sources(ui.ctx());
                    }
                });
            });
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                for (kind, label) in [
                    (SourceKind::Display, "Displays"),
                    (SourceKind::Window, "Windows"),
                ] {
                    if button(ui, label, self.source_kind == kind).clicked()
                        && self.source_kind != kind
                    {
                        self.source_kind = kind;
                        self.selected_source = None;
                        self.select_first_source();
                    }
                }
            });
            ui.add_space(11.0);
            if self.source_request.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Finding capture sources…").color(QUIET));
                });
            } else if let Some(error) = &self.source_error {
                ui.label(RichText::new(error).color(RED));
            } else {
                self.source_picker(ui);
            }
        });
        ui.add_space(12.0);
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            heading(ui, "Broadcast details", 18.0);
            ui.add_space(7.0);
            ui.label(RichText::new("Room title").strong());
            ui.add_sized(
                vec2(ui.available_width().min(380.0), 34.0),
                egui::TextEdit::singleline(&mut self.title_draft)
                    .margin(Margin::symmetric(8, 5))
                    .vertical_align(Align::Center)
                    .hint_text("Screen share")
                    .char_limit(120),
            );
            ui.add_space(9.0);
            self.video_controls(ui);
            ui.add_space(8.0);
            self.audio_controls(ui);
        });
        ui.add_space(14.0);
        let can_start = self.selected_source.is_some()
            && self.source_request.is_none()
            && self.source_error.is_none();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled_ui(can_start, |ui| button(ui, "Go live", true))
                .inner
                .clicked()
            {
                self.start_broadcast();
            }
            ui.label(
                RichText::new("Anyone on your local network can watch while you’re live.")
                    .size(12.0)
                    .color(QUIET),
            );
        });
    }

    fn video_controls(&mut self, ui: &mut Ui) {
        let settings = &mut self.preferences.video;
        let previous = *settings;
        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Resolution").strong())
                    .on_hover_text("Auto adjusts the output size to keep the stream moving.");
                egui::ComboBox::from_id_salt("broadcast-resolution")
                    .width(130.0)
                    .selected_text(settings.resolution.label())
                    .show_ui(ui, |ui| {
                        for option in [
                            Resolution::Auto,
                            Resolution::P480,
                            Resolution::P720,
                            Resolution::P1080,
                        ] {
                            ui.selectable_value(&mut settings.resolution, option, option.label());
                        }
                    });
            });
            ui.vertical(|ui| {
                ui.label(RichText::new("Frame rate").strong())
                    .on_hover_text(
                        "Auto adjusts the frame rate to capture and viewer performance.",
                    );
                egui::ComboBox::from_id_salt("broadcast-frame-rate")
                    .width(130.0)
                    .selected_text(settings.frame_rate.label())
                    .show_ui(ui, |ui| {
                        for option in [
                            FrameRate::Auto,
                            FrameRate::Fps15,
                            FrameRate::Fps24,
                            FrameRate::Fps30,
                            FrameRate::Fps60,
                        ] {
                            ui.selectable_value(&mut settings.frame_rate, option, option.label());
                        }
                    });
            });
        });
        if *settings != previous
            && let Session::Hosting { capture, .. } = &self.session
        {
            capture.set_settings(*settings);
        }
    }

    fn source_picker(&mut self, ui: &mut Ui) {
        let selected_label = self.sources.iter().find(|source| {
            source.kind == self.source_kind && self.selected_source == Some(source.id)
        });
        let Some(selected_label) = selected_label else {
            ui.label(
                RichText::new(
                    "No sources found. Open a window or connect a display, then refresh.",
                )
                .color(QUIET),
            );
            return;
        };
        let width = ui.available_width().min(430.0);
        egui::ComboBox::from_id_salt("capture-source")
            .width(width)
            .selected_text(short_label(&selected_label.label, 52))
            .show_ui(ui, |ui| {
                for source in self
                    .sources
                    .iter()
                    .filter(|source| source.kind == self.source_kind)
                {
                    ui.selectable_value(
                        &mut self.selected_source,
                        Some(source.id),
                        short_label(&source.label, 58),
                    )
                    .on_hover_text(&source.label);
                }
            });
    }

    fn settings(&mut self, ui: &mut Ui) {
        heading(ui, "Your profile", 31.0);
        ui.label(RichText::new("Choose the name shown next to your room.").color(QUIET));
        ui.add_space(19.0);
        card().show(ui, |ui| {
            ui.set_width(ui.available_width().min(530.0));
            ui.label(RichText::new("Display name").strong());
            let input = ui.add_sized(
                vec2(ui.available_width(), 36.0),
                egui::TextEdit::singleline(&mut self.name_draft)
                    .margin(Margin::symmetric(8, 5))
                    .vertical_align(Align::Center)
                    .char_limit(120),
            );
            let submitted =
                input.lost_focus() && ui.input(|state| state.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            ui.label(
                RichText::new("Your computer name is used by default.")
                    .size(12.0)
                    .color(QUIET),
            );
            ui.add_space(13.0);
            ui.horizontal_wrapped(|ui| {
                if button(ui, "Save name", true).clicked() || submitted {
                    let name = self.name_draft.trim();
                    if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
                        self.notice = Some((
                            "Enter a display name of up to 120 UTF-8 bytes.".into(),
                            true,
                        ));
                    } else {
                        self.preferences.name = name.to_string();
                        self.name_draft = self.preferences.name.clone();
                        self.notice = Some(("Display name saved.".into(), false));
                    }
                }
                if button(ui, "Use computer name", false).clicked() {
                    self.name_draft = default_name();
                }
            });
        });
        ui.add_space(22.0);
        self.updates(ui);
        ui.add_space(22.0);
        heading(ui, "Network", 17.0);
        ui.label(
            RichText::new(
                "Arcane Portal opens in Watch only mode. Use Broadcast to share a screen.",
            )
            .color(QUIET),
        );
    }

    fn updates(&mut self, ui: &mut Ui) {
        heading(ui, "Updates", 17.0);
        ui.add_space(5.0);
        card().show(ui, |ui| {
            ui.set_width(ui.available_width().min(530.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new("Installed version").strong());
                ui.label(RichText::new(CURRENT_VERSION).monospace().color(ACCENT));
            });
            ui.add_space(6.0);
            match self.updater.status() {
                UpdateStatus::Idle => {
                    ui.label(RichText::new("Check GitHub for a new release.").color(QUIET));
                    ui.add_space(6.0);
                    self.update_links(ui, "Check for updates");
                }
                UpdateStatus::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Checking GitHub…").color(QUIET));
                    });
                }
                UpdateStatus::UpToDate => {
                    ui.label(RichText::new("You’re up to date.").color(GREEN));
                    ui.add_space(6.0);
                    self.update_links(ui, "Check again");
                }
                UpdateStatus::Available { version } => {
                    ui.label(
                        RichText::new(format!("Version {version} is available.")).color(GREEN),
                    );
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if button(ui, "Download update", true).clicked() {
                            self.updater.download();
                        }
                        ui.hyperlink_to("View releases on GitHub", RELEASES_URL);
                    });
                }
                UpdateStatus::Downloading => {
                    if let Some((received, total)) =
                        self.updater.progress().filter(|(_, total)| *total > 0)
                    {
                        let fraction = (received as f32 / total as f32).clamp(0.0, 1.0);
                        let status = if received >= total {
                            "Preparing update…"
                        } else {
                            "Downloading update…"
                        };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(status).color(QUIET));
                            ui.label(
                                RichText::new(format!("{:.0}%", fraction * 100.0)).monospace(),
                            );
                        });
                        ui.add(egui::ProgressBar::new(fraction).fill(ACCENT));
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Downloading and preparing update…").color(QUIET),
                            );
                        });
                    }
                }
                UpdateStatus::Ready { version } => {
                    ui.label(
                        RichText::new(format!("Version {version} is ready to install."))
                            .color(GREEN),
                    );
                    let session_active = self.session.active();
                    let message = if session_active {
                        "Stop your broadcast or leave the current room before installing."
                    } else {
                        "Arcane Portal will close and reopen to finish the update."
                    };
                    ui.label(RichText::new(message).color(QUIET));
                    ui.add_space(6.0);
                    let install = ui.add_enabled_ui(!session_active, |ui| {
                        button(ui, "Install and restart", true)
                    });
                    if install.inner.clicked() {
                        self.updater.launch();
                    }
                }
                UpdateStatus::Failed(error) => {
                    ui.label(RichText::new(error).color(RED));
                    ui.add_space(6.0);
                    self.update_links(ui, "Check again");
                }
                UpdateStatus::Launching | UpdateStatus::Restarting => {}
            }
        });
    }

    fn update_links(&mut self, ui: &mut Ui, check_label: &str) {
        ui.horizontal_wrapped(|ui| {
            if button(ui, check_label, false).clicked() {
                self.updater.check();
            }
            ui.hyperlink_to("View releases on GitHub", RELEASES_URL);
        });
    }

    fn hosting(&mut self, ui: &mut Ui) {
        let Session::Hosting {
            host,
            capture,
            name,
            title,
        } = &self.session
        else {
            return;
        };
        let name = name.clone();
        let title = title.clone();
        let viewers = host.viewers();
        let (_, fps) = capture.format();
        let actual_fps = host.stream_fps();
        let on_lan = host
            .addresses()
            .iter()
            .any(|address| !address.ip().is_loopback());
        let is_live = self.texture.is_some();
        ui.horizontal_wrapped(|ui| {
            badge(
                ui,
                if is_live { "Live" } else { "Starting" },
                if is_live { RED } else { ACCENT },
            );
            heading(ui, &title, 26.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Stop broadcast", false).clicked() {
                    self.end_session(ui.ctx());
                }
            });
        });
        let caption = if viewers == 1 {
            "1 viewer".to_string()
        } else {
            format!("{viewers} viewers")
        };
        ui.label(RichText::new(format!("{name}  ·  {caption}")).color(QUIET));
        ui.add_space(12.0);
        if self.session.active() {
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                self.video_controls(ui);
                ui.add_space(8.0);
                self.audio_controls(ui);
                ui.label(
                    RichText::new(format!("Published: {actual_fps:.1} fps"))
                        .strong()
                        .color(ACCENT),
                );
                if let Some((frame, _)) = self.texture.as_ref() {
                    ui.label(
                        RichText::new(format!(
                            "{} × {}  ·  Up to {fps} fps",
                            frame.width, frame.height
                        ))
                        .size(12.0)
                        .color(QUIET),
                    );
                }
            });
            ui.add_space(12.0);
            self.stage(ui, "Starting screen capture…");
            if !on_lan {
                ui.label(
                    RichText::new("Connect to a local network, then restart the broadcast.")
                        .color(QUIET),
                );
            } else if is_live {
                ui.label(
                    RichText::new("Viewers on this network can find you in Live Rooms.")
                        .color(QUIET),
                );
            }
        }
    }

    fn watching(&mut self, ui: &mut Ui) {
        let Session::Watching {
            viewer,
            selected_room,
        } = &self.session
        else {
            return;
        };
        let actual_fps = viewer.stream_fps();
        let connected_room = viewer.room();
        let error = viewer.error();
        let audio_error = viewer.audio_error();
        let connected = connected_room.is_some();
        let is_live = error.is_none() && self.texture.is_some();
        let room = connected_room.unwrap_or_else(|| selected_room.clone());
        let failure = error.map(|error| (error, selected_room.clone()));
        let (status, color) = if failure.is_some() {
            ("Unavailable", RED)
        } else if is_live {
            ("Live", RED)
        } else if connected {
            ("Awaiting video", ACCENT)
        } else {
            ("Connecting", ACCENT)
        };
        ui.horizontal_wrapped(|ui| {
            badge(ui, status, color);
            heading(ui, &room.title, 26.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Leave room", false).clicked() {
                    self.end_session(ui.ctx());
                }
            });
        });
        let info = match self.texture.as_ref() {
            Some((frame, _)) if is_live => {
                format!("{}  ·  {} × {}", room.name, frame.width, frame.height)
            }
            _ => room.name,
        };
        ui.label(RichText::new(info).color(QUIET));
        if ui
            .checkbox(&mut self.play_audio, "Play stream audio")
            .changed()
            && !self.play_audio
        {
            self.audio_output_start = None;
            self.audio_output = None;
        }
        if self.play_audio
            && let Some(message) = audio_error
        {
            ui.label(RichText::new(message).color(QUIET));
        }
        ui.label(
            RichText::new(format!("Received: {actual_fps:.1} fps"))
                .strong()
                .color(ACCENT),
        );
        ui.add_space(12.0);
        if self.session.active() {
            if let Some((error, retry_room)) = failure {
                card().show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(error).color(RED));
                    ui.add_space(5.0);
                    if button(ui, "Try again", true).clicked() {
                        self.join(retry_room);
                    }
                });
            } else {
                let waiting = if connected {
                    "Waiting for the first frame…"
                } else {
                    "Connecting to the broadcaster…"
                };
                self.stage(ui, waiting);
            }
        }
    }

    fn stage(&mut self, ui: &mut Ui, waiting: &str) {
        let width = ui.available_width();
        let height = (width * 0.5625).clamp(230.0, 460.0);
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        ui.painter()
            .rect_filled(rect, 12, Color32::from_rgb(10, 11, 15));
        draw_screen(
            ui.painter(),
            rect.shrink(1.0),
            self.texture.as_ref(),
            waiting,
        );
        ui.painter()
            .rect_stroke(rect, 12, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
        if self.texture.is_some() && matches!(self.session, Session::Watching { .. }) {
            let full_button = ui
                .with_layout(Layout::right_to_left(Align::Center), |ui| {
                    button(ui, "Full screen", false)
                })
                .inner;
            if response.double_clicked() || full_button.clicked() {
                self.set_fullscreen(ui.ctx(), true);
            }
        }
    }

    fn immersive(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::BLACK))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let response =
                    ui.interact(rect, ui.id().with("fullscreen-preview"), Sense::click());
                draw_screen(
                    ui.painter(),
                    rect,
                    self.texture.as_ref(),
                    "Waiting for the first frame…",
                );
                if let Session::Watching { viewer, .. } = &self.session {
                    let label = format!("Received: {:.1} fps", viewer.stream_fps());
                    let position = rect.left_top() + vec2(18.0, 18.0);
                    ui.painter().rect_filled(
                        Rect::from_min_size(position - vec2(8.0, 5.0), vec2(185.0, 32.0)),
                        6,
                        Color32::from_black_alpha(190),
                    );
                    ui.painter().text(
                        position,
                        egui::Align2::LEFT_TOP,
                        label,
                        FontId::proportional(18.0),
                        Color32::WHITE,
                    );
                }
                let controls = Rect::from_min_max(
                    pos2(rect.right() - 170.0, rect.top() + 14.0),
                    rect.right_top() + vec2(-14.0, 52.0),
                );
                let exit = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(controls)
                            .layout(Layout::right_to_left(Align::TOP)),
                        |ui| button(ui, "Exit full screen", false),
                    )
                    .inner;
                if response.double_clicked() || exit.clicked() {
                    self.set_fullscreen(ctx, false);
                }
            });
    }
}

impl eframe::App for PortalApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_sources();
        self.update_session(ctx);
        self.updater.poll();
        ctx.request_repaint_after(if self.session.active() {
            Duration::from_millis(16)
        } else if matches!(
            self.updater.status(),
            UpdateStatus::Checking | UpdateStatus::Downloading | UpdateStatus::Launching
        ) {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(300)
        });
        if matches!(
            self.updater.status(),
            UpdateStatus::Launching | UpdateStatus::Restarting
        ) {
            if matches!(self.updater.status(), UpdateStatus::Restarting) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BACKGROUND))
                .show(ctx, |ui| {
                    ui.add_space(ui.available_height() * 0.38);
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        heading(ui, "Installing update", 24.0);
                        ui.label(
                            RichText::new("Arcane Portal will close and reopen.").color(QUIET),
                        );
                    });
                });
            return;
        }
        if self.fullscreen {
            self.immersive(ctx);
            return;
        }
        self.sidebar(ctx);
        self.header(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(self.page.title())
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(29, 24))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                if let Some((message, is_error)) = self.notice.as_ref() {
                                    let color = if *is_error { RED } else { GREEN };
                                    let dismiss = card()
                                        .show(ui, |ui| {
                                            ui.set_min_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                if button(ui, "Dismiss", false).clicked() {
                                                    return true;
                                                }
                                                ui.label(RichText::new(message).color(color));
                                                false
                                            })
                                            .inner
                                        })
                                        .inner;
                                    if dismiss {
                                        self.notice = None;
                                    }
                                    ui.add_space(10.0);
                                }
                                match self.page {
                                    Page::Rooms => self.rooms(ui),
                                    Page::Broadcast => self.broadcast(ui),
                                    Page::Settings => self.settings(ui),
                                }
                            });
                    });
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(json) = serde_json::to_string(&self.preferences) {
            storage.set_string(PREFS_KEY, json);
        }
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = FIELD;
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.selection.bg_fill = ACCENT_DARK;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.widgets.inactive.bg_fill = SURFACE;
    style.visuals.widgets.inactive.weak_bg_fill = SURFACE;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 45, 66);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 45, 66);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.widgets.active.bg_fill = ACCENT_DARK;
    style.visuals.widgets.active.weak_bg_fill = ACCENT_DARK;
    style.visuals.widgets.noninteractive.fg_stroke.color = QUIET;
    style.spacing.item_spacing = vec2(8.0, 5.0);
    style.spacing.button_padding = vec2(12.0, 7.0);
    ctx.set_style(style);
}

fn default_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok();
    #[cfg(target_os = "macos")]
    let name = name.or_else(|| {
        let result = std::process::Command::new("/bin/hostname")
            .arg("-s")
            .output()
            .ok()?;
        result
            .status
            .success()
            .then(|| String::from_utf8_lossy(&result.stdout).into_owned())
    });
    name.map(|name| {
        name.trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(40)
            .collect::<String>()
    })
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| "Local host".to_owned())
}

fn short_label(label: &str, limit: usize) -> String {
    let mut chars = label.chars();
    let mut shortened: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        shortened.push('…');
    }
    shortened
}

fn heading(ui: &mut Ui, title: &str, size: f32) {
    ui.label(RichText::new(title).strong().size(size).color(TEXT));
}

fn button(ui: &mut Ui, label: &str, primary: bool) -> Response {
    let text = RichText::new(label).size(13.0).color(if primary {
        Color32::from_rgb(35, 24, 56)
    } else {
        TEXT
    });
    let button = egui::Button::new(text)
        .corner_radius(7)
        .min_size(vec2(0.0, 33.0));
    let response = if primary {
        ui.scope(|ui| {
            let widgets = &mut ui.style_mut().visuals.widgets;
            widgets.inactive.weak_bg_fill = ACCENT;
            widgets.inactive.bg_stroke = Stroke::new(1.0_f32, ACCENT);
            widgets.hovered.weak_bg_fill = Color32::from_rgb(218, 205, 255);
            widgets.hovered.bg_stroke = Stroke::new(1.0_f32, TEXT);
            widgets.active.weak_bg_fill = Color32::from_rgb(182, 155, 249);
            widgets.active.bg_stroke = Stroke::new(1.0_f32, TEXT);
            ui.add(button)
        })
        .inner
    } else {
        ui.add(button)
    };
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .inner_margin(Margin::same(14))
        .corner_radius(11)
}

fn badge(ui: &mut Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(FIELD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if color != QUIET {
                    let (area, _) = ui.allocate_exact_size(vec2(6.0, 12.0), Sense::hover());
                    ui.painter().circle_filled(area.center(), 3.0, color);
                }
                ui.label(RichText::new(label).size(11.0).color(color));
            });
        });
}

fn display_room(ui: &mut Ui, room: &Room, can_join: bool) -> bool {
    card()
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                icon_tile(ui, Icon::Broadcast, 45.0);
                ui.add_space(4.0);
                let details_width = (ui.available_width() - 106.0).max(90.0);
                ui.vertical(|ui| {
                    ui.set_width(details_width);
                    ui.set_max_width(details_width);
                    ui.add(
                        egui::Label::new(RichText::new(&room.title).strong().size(15.0)).truncate(),
                    )
                    .on_hover_text(&room.title);
                    let viewers = if room.viewers == 1 {
                        "1 viewer".to_owned()
                    } else {
                        format!("{} viewers", room.viewers)
                    };
                    let details = format!(
                        "{}  ·  {}p at {} fps  ·  {viewers}",
                        room.name, room.height, room.fps
                    );
                    ui.add(
                        egui::Label::new(RichText::new(&details).color(QUIET).size(12.0))
                            .truncate(),
                    )
                    .on_hover_text(details);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled_ui(can_join, |ui| button(ui, "Watch", false))
                        .inner
                        .clicked()
                })
                .inner
            })
            .inner
        })
        .inner
}

fn draw_screen(
    painter: &Painter,
    rect: Rect,
    texture: Option<&(Arc<Frame>, TextureHandle)>,
    waiting: &str,
) {
    if let Some((frame, texture)) = texture {
        let size = vec2(frame.width as f32, frame.height as f32);
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        painter.image(
            texture.id(),
            Rect::from_center_size(rect.center(), size * scale),
            Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        icon(
            painter,
            Icon::Broadcast,
            rect.center() - vec2(0.0, 17.0),
            31.0,
            ACCENT,
        );
        painter.text(
            rect.center() + vec2(0.0, 17.0),
            egui::Align2::CENTER_CENTER,
            waiting,
            FontId::proportional(14.0),
            QUIET,
        );
    }
}

fn nav_item(ui: &mut Ui, label: &str, symbol: Icon, active: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, label)
    });
    if active || response.hovered() || response.has_focus() {
        let fill = if active { ACCENT_DARK } else { SURFACE };
        ui.painter().rect_filled(rect, 8, fill);
    }
    let color = if active { ACCENT } else { QUIET };
    icon(
        ui.painter(),
        symbol,
        pos2(rect.left() + 19.0, rect.center().y),
        17.0,
        color,
    );
    ui.painter().text(
        pos2(rect.left() + 38.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        if active { ACCENT } else { TEXT },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn icon_tile(ui: &mut Ui, symbol: Icon, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().rect_filled(rect, 9, ACCENT_DARK);
    icon(ui.painter(), symbol, rect.center(), size * 0.46, ACCENT);
}

#[derive(Clone, Copy)]
enum Icon {
    Portal,
    Rooms,
    Broadcast,
    Settings,
    Play,
}

fn icon(painter: &Painter, symbol: Icon, center: Pos2, size: f32, color: Color32) {
    let point = |x, y| center + vec2(x * size, y * size);
    let stroke = Stroke::new(1.5_f32, color);
    let line = |a, b| painter.line_segment([a, b], stroke);
    let path = |points| painter.add(Shape::line(points, stroke));
    match symbol {
        Icon::Portal => {
            path(vec![
                point(0.0, -0.46),
                point(0.39, 0.0),
                point(0.0, 0.46),
                point(-0.39, 0.0),
                point(0.0, -0.46),
            ]);
            path(vec![
                point(0.0, -0.27),
                point(0.21, 0.0),
                point(0.0, 0.27),
                point(-0.21, 0.0),
                point(0.0, -0.27),
            ]);
            painter.circle_filled(center, 1.8, color);
        }
        Icon::Rooms => {
            for (x, y) in [(-0.29, -0.29), (0.08, -0.29), (-0.29, 0.08), (0.08, 0.08)] {
                painter.rect_stroke(
                    Rect::from_min_size(point(x, y), Vec2::splat(size * 0.23)),
                    1,
                    stroke,
                    StrokeKind::Inside,
                );
            }
        }
        Icon::Broadcast => {
            painter.rect_stroke(
                Rect::from_min_max(point(-0.42, -0.35), point(0.42, 0.23)),
                3,
                stroke,
                StrokeKind::Inside,
            );
            line(point(-0.24, 0.39), point(0.24, 0.39));
            line(point(0.0, 0.23), point(0.0, 0.39));
            painter.circle_filled(point(0.0, -0.06), size * 0.08, color);
        }
        Icon::Settings => {
            for (y, x) in [(-0.27, -0.12), (0.0, 0.17), (0.27, -0.04)] {
                line(point(-0.39, y), point(x - 0.11, y));
                line(point(x + 0.11, y), point(0.39, y));
                painter.circle_stroke(point(x, y), size * 0.10, stroke);
            }
        }
        Icon::Play => {
            path(vec![
                point(-0.26, -0.39),
                point(0.36, 0.0),
                point(-0.26, 0.39),
                point(-0.26, -0.39),
            ]);
        }
    }
}

fn portal_art(painter: &Painter, rect: Rect) {
    let painter = painter.with_clip_rect(rect.shrink(1.0));
    painter.rect_filled(rect, 13, Color32::from_rgb(25, 23, 39));
    let center = pos2(rect.left() + rect.width() * 0.77, rect.center().y + 5.0);
    for step in (0..18).rev() {
        let radius = 52.0 + step as f32 * 9.0;
        painter.circle_filled(
            center,
            radius,
            Color32::from_rgba_unmultiplied(117, 75, 218, 4),
        );
    }
    for column in 0..16 {
        let x = rect.right() - 18.0 - column as f32 * 22.0;
        if x > rect.center().x {
            painter.vline(
                x,
                rect.y_range(),
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(159, 136, 205, 14)),
            );
        }
    }
    for row in 1..11 {
        let y = rect.top() + row as f32 * 22.0;
        painter.hline(
            center.x - 129.0..=rect.right(),
            y,
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(159, 136, 205, 14)),
        );
    }
    let rotation = -0.49_f32;
    let (sin, cos) = rotation.sin_cos();
    let orbit_point = |angle: f32, width: f32, height: f32| {
        let x = angle.cos() * width;
        let y = angle.sin() * height;
        center + vec2(x * cos - y * sin, x * sin + y * cos)
    };
    for (width, height, alpha) in [
        (150.0, 57.0, 38),
        (123.0, 48.0, 95),
        (98.0, 37.0, 68),
        (75.0, 67.0, 140),
        (69.0, 61.0, 65),
        (44.0, 41.0, 50),
    ] {
        let orbit = (0..=100)
            .map(|index| orbit_point(index as f32 * std::f32::consts::TAU / 100.0, width, height))
            .collect();
        painter.add(Shape::line(
            orbit,
            Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(177, 147, 255, alpha),
            ),
        ));
    }
    let diamond = [
        center - vec2(0.0, 36.0),
        center + vec2(25.0, 0.0),
        center + vec2(0.0, 36.0),
        center - vec2(25.0, 0.0),
    ];
    painter.add(Shape::convex_polygon(
        diamond.to_vec(),
        Color32::from_rgb(50, 36, 79),
        Stroke::new(1.4_f32, ACCENT),
    ));
    painter.line_segment(
        [diamond[0], diamond[2]],
        Stroke::new(1.0_f32, Color32::from_rgb(137, 107, 199)),
    );
    painter.line_segment(
        [diamond[1], diamond[3]],
        Stroke::new(1.0_f32, Color32::from_rgb(137, 107, 199)),
    );
    painter.circle_filled(center, 3.0, ACCENT);
    for (angle, width, height) in [(0.38, 123.0, 48.0), (3.9, 98.0, 37.0), (5.15, 75.0, 67.0)] {
        painter.circle_filled(orbit_point(angle, width, height), 3.0, ACCENT);
    }
    for index in 0..25 {
        let x = ((index * 53 + 11) % 291) as f32 - 132.0;
        let y = ((index * 37 + 57) % 191) as f32 - 95.0;
        if x * x + y * y > 1250.0 {
            painter.circle_filled(
                center + vec2(x, y),
                if index % 7 == 0 { 1.6 } else { 0.8 },
                Color32::from_rgba_unmultiplied(210, 191, 255, 110),
            );
        }
    }
    painter.rect_stroke(rect, 13, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_saved_quality_presets_to_independent_settings() {
        for (quality, resolution, frame_rate) in [
            (0, Resolution::P480, FrameRate::Fps15),
            (1, Resolution::P720, FrameRate::Fps24),
            (2, Resolution::P1080, FrameRate::Fps30),
        ] {
            let preferences: Preferences = serde_json::from_value(serde_json::json!({
                "name": "Broadcaster",
                "quality": quality,
            }))
            .unwrap();
            assert_eq!(preferences.name, "Broadcaster");
            assert_eq!(preferences.video.resolution, resolution);
            assert_eq!(preferences.video.frame_rate, frame_rate);
        }
    }

    #[test]
    fn defaults_to_auto_and_keeps_explicit_video_choices() {
        let mut preferences: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(preferences.video.resolution, Resolution::Auto);
        assert_eq!(preferences.video.frame_rate, FrameRate::Auto);

        preferences.video.frame_rate = FrameRate::Fps60;
        let saved = serde_json::to_string(&preferences).unwrap();
        let restored: Preferences = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.video, preferences.video);
    }
}
