use gpui_kit::{EventEmitter, SharedString};

/// Fire-and-forget notifications. Stores have no window, so they emit a
/// [`Toast`] here and the window's root view turns it into a notification.
pub struct Toasts;

impl EventEmitter<Toast> for Toasts {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    #[allow(dead_code)] // info toasts arrive with later plans' features
    Info,
    #[allow(dead_code)] // success toasts arrive with later plans' features
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: SharedString,
    pub message: SharedString,
}

impl Toast {
    fn new(
        kind: ToastKind,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
        }
    }

    #[allow(dead_code)] // part of the toast API later plans use
    pub fn info(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Info, title, message)
    }

    #[allow(dead_code)] // part of the toast API later plans use
    pub fn success(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Success, title, message)
    }

    pub fn warning(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Warning, title, message)
    }

    pub fn error(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Error, title, message)
    }
}
