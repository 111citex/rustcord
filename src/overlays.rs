//! What floats over everything and moves a lot: the ringing panel, the video window and
//! a live person's card. A view of its own beside the shell, not inside it, so dragging
//! one or playing its video redraws it alone, and the shell's last frame is used as it is.

use gpui_kit::{AnyView, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, StyleRefinement, Styled as _, Window, div};

use crate::router::Router;
use crate::shell::Shell;
use crate::store::Services;

pub struct Overlays {
    services: Services,
    router: Entity<Router>,
}

impl Overlays {
    pub fn new(services: Services, router: Entity<Router>, cx: &mut Context<Self>) -> Self {
        cx.observe(&services.store, |_, _, cx| cx.notify()).detach();
        cx.observe(&router, |_, _, cx| cx.notify()).detach();
        Overlays { services, router }
    }
}

impl Render for Overlays {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::note_overlay_view(cx.entity_id());
        let store = self.services.store.clone();
        let looking_at = crate::nav::parse_location(self.router.read(cx).current_path()).and_then(|l| l.channel);
        div()
            .children(crate::call_ring::overlay(&store, window, cx))
            .children(crate::video_pip::overlay(&store, looking_at, window, cx))
            .children(crate::stream_card::overlay(&store, window, cx))
    }
}

/// The window's own view: the shell, and the overlays over it, each drawn only when it changed.
pub struct Root {
    shell: Entity<Shell>,
    overlays: Entity<Overlays>,
    services: Services,
}

impl Root {
    pub fn new(shell: Entity<Shell>, services: Services, router: Entity<Router>, cx: &mut Context<Self>) -> Self {
        let overlays = cx.new(|cx| Overlays::new(services.clone(), router, cx));
        Root { shell, overlays, services }
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The zoom, set before anything's drawn. gpui-kit's own root, around this one, puts it
        // back to the theme's 16px each time it draws; the shell sets it too, but the shell's
        // often not drawn again (its last frame is reused), and then whatever is drawn afresh
        // that frame (a floating window, a tooltip) came out at 100% while the rest was at 145%.
        let rem = gpui_kit::px(self.services.settings.read(cx).get().rem_px());
        if window.rem_size() != rem {
            window.set_rem_size(rem);
        }
        div()
            .relative()
            .size_full()
            .child(AnyView::from(self.shell.clone()).cached(StyleRefinement::default().size_full()))
            .child(AnyView::from(self.overlays.clone()).cached(StyleRefinement::default().absolute().top_0().left_0().size_full()))
    }
}
