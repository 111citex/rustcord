pub mod channel;
pub mod login;
pub mod settings;

use gpui_kit::{AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, Styled, Window, div};

use crate::router::{Route, RouteContext};

pub fn routes() -> Vec<Route> {
    vec![
        Route::new("/login", login::page),
        Route::new("/settings", settings::page),
        Route::new("/channels/@me", channel::page),
        Route::new("/channels/@me/:channel", channel::page),
        Route::new("/channels/:guild", channel::page),
        Route::new("/channels/:guild/:channel", channel::page),
        Route::new("*", not_found),
    ]
}

struct NotFound;

impl Render for NotFound {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().items_center().justify_center().text_xl().child("Page not found")
    }
}

fn not_found(_: RouteContext, _: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| NotFound).into()
}
