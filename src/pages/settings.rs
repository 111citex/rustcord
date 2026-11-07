//! User Settings, laid out like Discord's: sections on the left, the chosen
//! section on the right, a close button (or Escape) back to where you were.

use cord_core::cdn;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, IconName, Sizable, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AnyView, App, AppContext, Context, ElementId, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    div,
};

use crate::ui::u;
use crate::router::RouteContext;
use std::rc::Rc;

use crate::settings::{CHAT_TEXT_MAX, CHAT_TEXT_MIN, Settings, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, step_big_memory, step_image_screens};
use crate::store::Services;

gpui_kit::actions!(rustcord, [CloseSettings]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Chat,
    Voice,
    Performance,
    Account,
    About,
}

impl Section {
    const ALL: [Section; 6] = [Section::Appearance, Section::Chat, Section::Voice, Section::Performance, Section::Account, Section::About];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Chat => "Chat",
            Section::Voice => "Voice & Video",
            Section::Performance => "Performance",
            Section::Account => "My Account",
            Section::About => "About Rustcord",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeFilter {
    All,
    Dark,
    Light,
}

pub struct SettingsPage {
    ctx: RouteContext,
    section: Section,
    filter: ThemeFilter,
    focus: FocusHandle,
    scroll: gpui_kit::ScrollHandle,
    scroller: crate::smooth_scroll::SmoothScroller,
    /// The mic, open for the meter while Voice & Video shows and we're not in a call.
    mic_test: Option<cord_voice::Mic>,
    /// Redraws the meter while it shows.
    meter: Option<gpui_kit::Task<()>>,
    /// Microphones and speakers, looked up on opening the section: asking is slow.
    devices: (Vec<String>, Vec<String>),
    /// Waiting for the key to use for push to talk.
    recording_key: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    fn new(ctx: RouteContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let subscriptions = vec![
            cx.observe(&ctx.services.settings, |_, _, cx| cx.notify()),
            cx.observe(&ctx.services.store, |_, _, cx| cx.notify()),
        ];
        let scroll = gpui_kit::ScrollHandle::new();
        let scroller = crate::smooth_scroll::SmoothScroller::new(crate::smooth_scroll::ScrollTarget::Handle(scroll.clone()));
        SettingsPage { ctx, section: Section::Appearance, filter: ThemeFilter::All, focus, scroll, scroller, mic_test: None, meter: None, devices: (Vec::new(), Vec::new()), recording_key: false, _subscriptions: subscriptions }
    }

    /// Opens `section`: Voice & Video opens the mic for its meter, and anything else closes it.
    fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.recording_key = false;
        if section == Section::Voice {
            self.devices = (cord_voice::input_devices(), cord_voice::output_devices());
            self.meter = Some(cx.spawn(async |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    let Ok(()) = this.update(cx, |page, cx| {
                        // In a call the call's mic feeds the meter; otherwise one of our own.
                        let in_call = page.ctx.services.store.read(cx).voice_status().is_some();
                        if in_call {
                            page.mic_test = None;
                        } else if page.mic_test.is_none() {
                            page.mic_test = Some(cord_voice::Mic::start(None));
                        }
                        cx.notify();
                    }) else {
                        return;
                    };
                }
            }));
        } else {
            self.meter = None;
            self.mic_test = None;
            cord_voice::mic().set_monitor(false);
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cord_voice::mic().set_monitor(false);
        self.ctx.router.update(cx, |router, cx| router.back("/channels/@me", cx));
    }

    fn edit(services: &Services, cx: &mut App, edit: impl FnOnce(&mut Settings)) {
        services.settings.update(cx, |store, cx| store.update(cx, edit));
    }

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .w(u(232.))
            .h_full()
            .flex_shrink_0()
            .py_8()
            .px_3()
            .gap_0p5()
            .bg(theme.sidebar)
            .child(div().px_2().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child("USER SETTINGS"))
            .children(Section::ALL.map(|section| {
                let active = section == self.section;
                div()
                    .id(ElementId::Name(SharedString::from(format!("section-{}", section.label()))))
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(active, |this| this.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                    .when(!active, |this| this.text_color(theme.sidebar_foreground.opacity(0.75)))
                    .hover(|style| style.bg(theme.sidebar_accent.opacity(0.6)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show(section, cx);
                    }))
                    .child(section.label())
            }))
    }

    fn appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.ctx.services.settings.read(cx).get().clone();
        let active = Theme::global(cx).theme_name().to_string();
        let filter = self.filter;
        let themes: Vec<_> = crate::theme::available(cx)
            .into_iter()
            .filter(|t| match filter {
                ThemeFilter::All => true,
                ThemeFilter::Dark => t.mode.is_dark(),
                ThemeFilter::Light => !t.mode.is_dark(),
            })
            .collect();
        let services = self.ctx.services.clone();

        let theme_cards = h_flex().flex_wrap().gap_3().children(themes.into_iter().map(|config| {
            let name = config.name.to_string();
            let selected = name == active;
            let swatch = crate::theme::swatch(&config, cx);
            let services = services.clone();
            let apply = name.clone();
            let theme = cx.theme();
            v_flex()
                .id(ElementId::Name(SharedString::from(format!("theme-{name}"))))
                .w(u(156.))
                .gap_2()
                .p_2()
                .rounded(theme.radius_lg)
                .border_2()
                .border_color(if selected { theme.primary } else { theme.border })
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
                .on_click(move |_, _, cx| {
                    crate::theme::apply(&apply, cx);
                    let name = apply.clone();
                    Self::edit(&services, cx, |s| s.theme = name);
                })
                .child(
                    // A tiny window in the theme's own colors.
                    h_flex()
                        .h(u(64.))
                        .rounded(theme.radius)
                        .bg(swatch.background)
                        .child(div().w(u(28.)).h_full().rounded_l(theme.radius).bg(swatch.surface))
                        .child(
                            v_flex()
                                .flex_1()
                                .p_2()
                                .gap_1p5()
                                .child(div().w(u(64.)).h(u(6.)).rounded_full().bg(swatch.foreground))
                                .child(div().w(u(40.)).h(u(6.)).rounded_full().bg(swatch.foreground.opacity(0.5)))
                                .child(div().mt_1().w(u(34.)).h(u(12.)).rounded(u(4.)).bg(swatch.primary)),
                        ),
                )
                .child(div().text_sm().truncate().child(name))
        }));

        let s = |services: &Services| services.clone();
        let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
        let slider_colors = (blurple, cx.theme().muted_foreground.opacity(0.35), cx.theme().foreground);
        let zoom = {
            let services = s(&services);
            // Applied on letting go: zooming resizes the slider under the pointer mid-drag.
            crate::slider::ticked_on_release(
                "zoom",
                f32::from(settings.zoom),
                (f32::from(ZOOM_MIN), f32::from(ZOOM_MAX), f32::from(ZOOM_STEP)),
                |at| format!("{at:.0}%"),
                slider_colors,
                Rc::new(move |value, cx| {
                    let value = value as u16;
                    if services.settings.read(cx).get().zoom != value {
                        Self::edit(&services, cx, |s| s.zoom = value);
                    }
                }),
            )
        };
        let chat_text = {
            let services = s(&services);
            crate::slider::ticked(
                "chat-text",
                f32::from(settings.chat_text),
                (f32::from(CHAT_TEXT_MIN), f32::from(CHAT_TEXT_MAX), 1.),
                format!("{}px", settings.chat_text),
                slider_colors,
                Rc::new(move |value, cx| {
                    let value = value as u8;
                    if services.settings.read(cx).get().chat_text != value {
                        Self::edit(&services, cx, |s| s.chat_text = value);
                    }
                }),
            )
        };
        let compact = Switch::new("compact").checked(settings.compact).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.compact = value);
            }
        });

        v_flex()
            .gap_6()
            .child(title("Appearance"))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(heading("Theme", cx))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        TabBar::new("theme-filter")
                                            .segmented()
                                            .selected_index(match filter {
                                                ThemeFilter::All => 0,
                                                ThemeFilter::Dark => 1,
                                                ThemeFilter::Light => 2,
                                            })
                                            .children(["All", "Dark", "Light"].map(|label| Tab::new().label(label)))
                                            .on_click(cx.listener(|this, index: &usize, _, cx| {
                                                this.filter = [ThemeFilter::All, ThemeFilter::Dark, ThemeFilter::Light][*index];
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("open-themes")
                                            .outline()
                                            .small()
                                            .icon(IconName::FolderOpen)
                                            .label("Themes folder")
                                            .on_click(|_, _, cx| cx.open_with_system(&crate::theme::themes_dir())),
                                    ),
                            ),
                    )
                    .child(theme_cards),
            )
            .child(divider(cx))
            .child(setting_row("Zoom", "Makes everything bigger or smaller.", zoom, cx))
            .child(setting_row("Chat text size", "Size of the text in messages.", chat_text, cx))
            .child(divider(cx))
            .child(setting_row("Compact mode", "Fit more messages on screen: no avatars, one line per message.", compact, cx))
            .into_any_element()
    }

    fn chat(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.ctx.services.settings.read(cx).get().clone();
        let services = self.ctx.services.clone();
        let s = |services: &Services| services.clone();
        let smooth = Switch::new("smooth-scrolling").checked(settings.smooth_scrolling).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.smooth_scrolling = value);
            }
        });
        let select_across = Switch::new("select-across").checked(settings.select_across).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.select_across = value);
            }
        });
        let copy_raw = Switch::new("copy-raw").checked(settings.copy_raw).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.copy_raw = value);
            }
        });
        let select_names = Switch::new("select-names").checked(settings.select_names).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.select_names = value);
            }
        });

        v_flex()
            .gap_6()
            .child(title("Chat"))
            .child(setting_row("Smooth scrolling", "Glide when scrolling with a mouse wheel.", smooth, cx))
            .child(divider(cx))
            .child(setting_row("Select across messages", "Dragging over message text can run on into the next messages. Off, a selection stays in the message it started in.", select_across, cx))
            .child(setting_row("Copy raw markdown", "Copying selected messages takes each message as it was written, markdown and all, instead of the text as shown.", copy_raw, cx))
            .child(setting_row("Include names and times in selections", "Lets a selection take in who wrote each message and when. Off, only the message text can be selected.", select_names, cx))
            .into_any_element()
    }

    fn performance(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.ctx.services.settings.read(cx).get().clone();
        let services = self.ctx.services.clone();
        let s = |services: &Services| services.clone();
        let stepper = |id: &'static str, value: String, minus: Box<dyn Fn(&mut App)>, plus: Box<dyn Fn(&mut App)>| {
            h_flex()
                .gap_2()
                .items_center()
                .child(Button::new(SharedString::from(format!("{id}-minus"))).outline().small().icon(IconName::Minus).on_click(move |_, _, cx| minus(cx)))
                .child(div().min_w(u(56.)).whitespace_nowrap().text_center().font_weight(FontWeight::SEMIBOLD).child(value))
                .child(Button::new(SharedString::from(format!("{id}-plus"))).outline().small().icon(IconName::Plus).on_click(move |_, _, cx| plus(cx)))
        };
        let image_screens = stepper(
            "image-screens",
            match settings.image_screens {
                1 => "1 screen".to_string(),
                n => format!("{n} screens"),
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.image_screens = step_image_screens(s.image_screens, -1)))
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.image_screens = step_image_screens(s.image_screens, 1)))
            },
        );
        let big_memory = stepper(
            "big-memory",
            match settings.big_memory_kb {
                kb if kb >= 1024 => format!("{} MB", kb / 1024),
                kb => format!("{kb} KB"),
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.big_memory_kb = step_big_memory(s.big_memory_kb, -1)))
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.big_memory_kb = step_big_memory(s.big_memory_kb, 1)))
            },
        );
        let on_screen_mb = crate::anim::ON_SCREEN_BYTES.load(std::sync::atomic::Ordering::Relaxed) / (1 << 20);
        let image_note = if on_screen_mb > 0 {
            format!("Pictures kept ready above and below what's on screen. Each screen uses about {} MB more in the channel you were just in. 0 uses the least, and scrolling back loads them again.", on_screen_mb * 2)
        } else {
            "Pictures kept ready above and below what's on screen. More uses more memory. 0 uses the least, and scrolling back loads them again.".to_string()
        };

        v_flex()
            .gap_6()
            .child(title("Performance"))
            .child(setting_row("Pictures to keep ready", &image_note, image_screens, cx))
            .child(setting_row("Free memory right away from", "Anything this size or bigger goes back to the system as soon as it's done with. Lower uses less memory. Higher can stutter less in videos and GIFs.", big_memory, cx))
            .into_any_element()
    }

    fn account(&self, cx: &mut Context<Self>) -> AnyElement {
        let me = self.ctx.services.store.read(cx).cache().me().cloned();
        let store = self.ctx.services.store.clone();
        let theme = cx.theme();
        v_flex()
            .gap_6()
            .child(title("My Account"))
            .when_some(me, |this, me| {
                this.child(
                    h_flex()
                        .gap_4()
                        .items_center()
                        .p_4()
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .child(Avatar::new().border_0().src(cdn::avatar_url(me.id, me.avatar.as_deref(), 128)).name(me.display_name().to_string()).size(u(64.)))
                        .child(
                            v_flex()
                                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(me.display_name().to_string()))
                                .child(div().text_sm().text_color(theme.muted_foreground).child(me.username.clone())),
                        ),
                )
            })
            .child(setting_row(
                "Log out",
                "Ends this session on this device and forgets it.",
                Button::new("log-out")
                    .danger()
                    .label("Log Out")
                    .on_click(move |_, _, cx| store.update(cx, |store, cx| store.sign_out(cx))),
                cx,
            ))
            .into_any_element()
    }

    fn about(&self, cx: &mut Context<Self>) -> AnyElement {
        let build = self.ctx.services.core.http().identity().build_number;
        let config = cord_core::paths::config_dir();
        let logs = cord_core::paths::logs_dir();
        let open = |id: &'static str, path: std::path::PathBuf| {
            Button::new(id).outline().small().icon(IconName::FolderOpen).label("Open").on_click(move |_, _, cx| cx.open_with_system(&path))
        };
        v_flex()
            .gap_6()
            .child(title("About Rustcord"))
            .child(setting_row("Version", "", div().child(env!("CARGO_PKG_VERSION")), cx))
            .child(setting_row("Discord client build", "The web client build this session presents as.", div().child(build.to_string()), cx))
            .child(divider(cx))
            .child(setting_row("Settings folder", &config.display().to_string(), open("open-config", config.clone()), cx))
            .child(setting_row("Logs folder", &logs.display().to_string(), open("open-logs", logs.clone()), cx))
            .into_any_element()
    }
}

impl SettingsPage {
    fn voice(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.ctx.services.settings.read(cx).get().clone();
        let services = self.ctx.services.clone();
        let theme = cx.theme().clone();
        let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
        let (radius, active, hover, faint) = (theme.radius, theme.list_active, theme.list_hover, theme.muted_foreground);
        // A list to pick one from: rows with a dot.
        let choices = |id: &'static str, options: Vec<(String, bool)>, pick: Rc<dyn Fn(usize, &mut App)>| {
            v_flex().gap_1().children(options.into_iter().enumerate().map(move |(i, (label, on))| {
                let pick = pick.clone();
                h_flex()
                    .id(ElementId::Name(SharedString::from(format!("{id}-{i}"))))
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded(radius)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active))
                    .hover(|d| d.bg(hover))
                    .on_click(move |_, _, cx| pick(i, cx))
                    .child(
                        div()
                            .size(u(18.))
                            .flex_shrink_0()
                            .rounded_full()
                            .border_2()
                            .border_color(if on { blurple } else { faint })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(on, |d| d.child(div().size(u(8.)).rounded_full().bg(blurple))),
                    )
                    .child(div().truncate().child(label))
            }))
        };
        let device_list = |id: &'static str, names: &[String], chosen: &Option<String>, set: fn(&mut Settings, Option<String>)| {
            let mut options = vec![("Default".to_string(), chosen.is_none())];
            options.extend(names.iter().map(|n| (n.clone(), chosen.as_deref() == Some(n.as_str()))));
            let names = names.to_vec();
            let services = services.clone();
            (&choices)(id, options, Rc::new(move |i, cx| {
                let name = if i == 0 { None } else { names.get(i - 1).cloned() };
                Self::edit(&services, cx, |s| set(s, name));
            }))
        };
        let inputs = device_list("input", &self.devices.0, &settings.input_device, |s, name| s.input_device = name);
        let outputs = device_list("output", &self.devices.1, &settings.output_device, |s, name| s.output_device = name);

        let mode = {
            let services = services.clone();
            choices(
                "input-mode",
                vec![("Voice Activity".into(), !settings.push_to_talk), ("Push to Talk".into(), settings.push_to_talk)],
                Rc::new(move |i, cx| Self::edit(&services, cx, |s| s.push_to_talk = i == 1)),
            )
        };

        // The meter, with the threshold marked on it: green past it, as Discord does.
        let mic = cord_voice::mic();
        let level = ((mic.level_db() + 100.) / 100.).clamp(0., 1.);
        let mark = ((settings.voice_threshold_db as f32 + 100.) / 100.).clamp(0., 1.);
        let loud = mic.level_db() >= settings.voice_threshold_db as f32;
        let meter = div()
            .relative()
            .w_full()
            .h(u(10.))
            .rounded_full()
            .bg(theme.muted_foreground.opacity(0.25))
            .overflow_hidden()
            .child(div().absolute().top_0().left_0().h_full().w(gpui_kit::relative(level)).bg(if loud { gpui_kit::Hsla::from(gpui_kit::rgb(0x23a55a)) } else { gpui_kit::Hsla::from(gpui_kit::rgb(0xf0b232)) }))
            .child(div().absolute().top_0().h_full().w(u(2.)).left(gpui_kit::relative(mark)).bg(theme.foreground));
        let threshold = {
            let services = services.clone();
            crate::slider::ticked(
                "voice-threshold",
                settings.voice_threshold_db as f32,
                (-100., 0., 1.),
                format!("{} dB", settings.voice_threshold_db),
                (blurple, theme.muted_foreground.opacity(0.35), theme.foreground),
                Rc::new(move |value, cx| {
                    let value = value.round() as i32;
                    if services.settings.read(cx).get().voice_threshold_db != value {
                        Self::edit(&services, cx, |s| s.voice_threshold_db = value);
                    }
                }),
            )
        };

        let volume = |id: &'static str, value: u16, set: fn(&mut Settings, u16)| {
            let services = services.clone();
            crate::slider::ticked(
                id,
                f32::from(value),
                (0., 200., 1.),
                format!("{value}%"),
                (blurple, theme.muted_foreground.opacity(0.35), theme.foreground),
                Rc::new(move |at, cx| {
                    let at = at.round() as u16;
                    Self::edit(&services, cx, |s| set(s, at));
                }),
            )
        };
        let input_volume = volume("input-volume", settings.input_volume, |s, v| s.input_volume = v);
        let output_volume = volume("output-volume", settings.output_volume, |s, v| s.output_volume = v);
        let checking = cord_voice::mic().is_monitoring();
        let lets_check = Button::new("lets-check")
            .when(!checking, |b| b.primary())
            .when(checking, |b| b.outline())
            .label(if checking { "Stop Checking" } else { "Let's Check" })
            .on_click(cx.listener(move |_, _, _, cx| {
                cord_voice::mic().set_monitor(!checking);
                cx.notify();
            }));

        let key_label = if self.recording_key {
            "Press a key…".to_string()
        } else if settings.ptt_key.is_empty() {
            "Record Keybind".to_string()
        } else {
            settings.ptt_key.to_uppercase()
        };
        let keybind = Button::new("ptt-key")
            .outline()
            .label(key_label)
            .on_click(cx.listener(|this, _, _, cx| {
                this.recording_key = !this.recording_key;
                cx.notify();
            }));

        let suppression = {
            let services = services.clone();
            let chosen = settings.noise_suppression.clone();
            choices(
                "noise",
                vec![
                    ("None".into(), chosen == "off"),
                    ("RNNoise (light)".into(), chosen == "rnnoise"),
                    ("DeepFilterNet (stronger, more CPU)".into(), chosen == "deepfilter"),
                ],
                Rc::new(move |i, cx| {
                    let name = ["off", "rnnoise", "deepfilter"][i].to_string();
                    Self::edit(&services, cx, |s| s.noise_suppression = name);
                }),
            )
        };

        let mute_shows = Switch::new("mute-shows").checked(!settings.mute_stays_local).on_change({
            let services = services.clone();
            move |value: &bool, _, cx| {
                let shows = *value;
                Self::edit(&services, cx, |s| s.mute_stays_local = !shows);
            }
        });

        v_flex()
            .gap_6()
            .child(title("Voice & Video"))
            .child(
                h_flex()
                    .gap_6()
                    .items_start()
                    .child(v_flex().flex_1().min_w_0().gap_2().child(heading("Input Device", cx)).child(inputs))
                    .child(v_flex().flex_1().min_w_0().gap_2().child(heading("Output Device", cx)).child(outputs)),
            )
            .child(
                Button::new("refresh-devices").ghost().small().label("Look for devices again").on_click(cx.listener(|this, _, _, cx| {
                    this.devices = (cord_voice::input_devices(), cord_voice::output_devices());
                    cx.notify();
                })),
            )
            .child(
                h_flex()
                    .gap_6()
                    .items_start()
                    .child(v_flex().flex_1().min_w_0().gap_2().child(heading("Input Volume", cx)).child(input_volume))
                    .child(v_flex().flex_1().min_w_0().gap_2().child(heading("Output Volume", cx)).child(output_volume)),
            )
            .child(divider(cx))
            .child(v_flex().gap_2().child(heading("Mic Test", cx)).child(h_flex().gap_4().items_center().child(lets_check).child(div().flex_1().child(meter))).child(
                div().text_sm().text_color(theme.muted_foreground).child(if self.mic_test.is_some() || self.ctx.services.store.read(cx).voice_status().is_some() {
                    "Talk to see how loud you are. Past the white line, you're heard."
                } else {
                    "Opening the mic…"
                }),
            ))
            .child(divider(cx))
            .child(v_flex().gap_2().child(heading("Input Mode", cx)).child(mode))
            .when(!settings.push_to_talk, |d| {
                d.child(setting_row("Input Sensitivity", "How loud you need to be to be heard. The white line on the meter.", threshold, cx))
            })
            .when(settings.push_to_talk, |d| {
                d.child(setting_row("Shortcut", "Hold it to talk. Works while Rustcord's window has focus.", keybind, cx))
            })
            .child(divider(cx))
            .child(v_flex().gap_2().child(heading("Noise Suppression", cx)).child(suppression))
            .child(divider(cx))
            .child(setting_row(
                "Show me as muted",
                "Off: muting only stops your mic, and others don't see the muted icon.",
                mute_shows,
                cx,
            ))
            .into_any_element()
    }
}

fn title(text: &str) -> impl IntoElement {
    div().text_xl().font_weight(FontWeight::SEMIBOLD).child(text.to_string())
}

fn heading(text: &str, cx: &App) -> impl IntoElement {
    div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().muted_foreground).child(text.to_uppercase())
}

fn divider(cx: &App) -> impl IntoElement {
    div().h(u(1.)).w_full().bg(cx.theme().border)
}

fn setting_row(label: &str, description: &str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_6()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().font_weight(FontWeight::MEDIUM).child(label.to_string()))
                .when(!description.is_empty(), |this| {
                    this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(description.to_string()))
                }),
        )
        .child(control)
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.section {
            Section::Appearance => self.appearance(cx),
            Section::Chat => self.chat(cx),
            Section::Voice => self.voice(cx),
            Section::Performance => self.performance(cx),
            Section::Account => self.account(cx),
            Section::About => self.about(cx),
        };
        h_flex()
            .key_context("Settings")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CloseSettings, _, cx| this.close(cx)))
            // Recording the push-to-talk key: the next key, or a modifier on its own.
            .capture_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, _, cx| {
                if !this.recording_key {
                    return;
                }
                cx.stop_propagation();
                this.recording_key = false;
                let key = event.keystroke.key.clone();
                if key != "escape" {
                    Self::edit(&this.ctx.services, cx, |s| s.ptt_key = key);
                }
                cx.notify();
            }))
            .on_modifiers_changed(cx.listener(|this, event: &gpui_kit::ModifiersChangedEvent, _, cx| {
                if !this.recording_key {
                    return;
                }
                let m = event.modifiers;
                let key = if m.shift { "shift" } else if m.control { "control" } else if m.alt { "alt" } else if m.platform { "super" } else { return };
                this.recording_key = false;
                Self::edit(&this.ctx.services, cx, |s| s.ptt_key = key.to_string());
                cx.notify();
            }))
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .child(self.nav(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .child(
                        div()
                            .id("settings-content")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(v_flex().max_w(u(740.)).px_10().py_8().child(content)),
                    )
                    .when(self.ctx.services.settings.read(cx).get().smooth_scrolling, |d| d.child(self.scroller.layer())),
            )
            .child(
                v_flex()
                    .absolute()
                    .top(u(24.))
                    .right(u(24.))
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("close-settings")
                            .outline()
                            .icon(IconName::Close)
                            .rounded_full()
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("ESC")),
            )
    }
}

pub fn page(ctx: RouteContext, window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| SettingsPage::new(ctx, window, cx)).into()
}
