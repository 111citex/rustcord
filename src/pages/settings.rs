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
use crate::settings::{Settings, step_big_memory, step_chat_text, step_image_screens, step_zoom};
use crate::store::Services;

gpui_kit::actions!(rustcord, [CloseSettings]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Account,
    About,
}

impl Section {
    const ALL: [Section; 3] = [Section::Appearance, Section::Account, Section::About];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
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
        SettingsPage { ctx, section: Section::Appearance, filter: ThemeFilter::All, focus, scroll, scroller, _subscriptions: subscriptions }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
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
                        this.section = section;
                        cx.notify();
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

        let stepper = |id: &'static str, value: String, minus: Box<dyn Fn(&mut App)>, plus: Box<dyn Fn(&mut App)>| {
            h_flex()
                .gap_2()
                .items_center()
                .child(Button::new(SharedString::from(format!("{id}-minus"))).outline().small().icon(IconName::Minus).on_click(move |_, _, cx| minus(cx)))
                .child(div().min_w(u(56.)).whitespace_nowrap().text_center().font_weight(FontWeight::SEMIBOLD).child(value))
                .child(Button::new(SharedString::from(format!("{id}-plus"))).outline().small().icon(IconName::Plus).on_click(move |_, _, cx| plus(cx)))
        };
        let s = |services: &Services| services.clone();
        let zoom = stepper(
            "zoom",
            format!("{}%", settings.zoom),
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.zoom = step_zoom(s.zoom, -1)))
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.zoom = step_zoom(s.zoom, 1)))
            },
        );
        let chat_text = stepper(
            "chat-text",
            format!("{}px", settings.chat_text),
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.chat_text = step_chat_text(s.chat_text, -1)))
            },
            {
                let services = s(&services);
                Box::new(move |cx| Self::edit(&services, cx, |s| s.chat_text = step_chat_text(s.chat_text, 1)))
            },
        );
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
        let compact = Switch::new("compact").checked(settings.compact).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.compact = value);
            }
        });
        let smooth = Switch::new("smooth-scrolling").checked(settings.smooth_scrolling).on_change({
            let services = s(&services);
            move |value: &bool, _, cx| {
                let value = *value;
                Self::edit(&services, cx, |s| s.smooth_scrolling = value);
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
            .child(setting_row("Smooth scrolling", "Glide when scrolling with a mouse wheel.", smooth, cx))
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
            Section::Account => self.account(cx),
            Section::About => self.about(cx),
        };
        h_flex()
            .key_context("Settings")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CloseSettings, _, cx| this.close(cx)))
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
