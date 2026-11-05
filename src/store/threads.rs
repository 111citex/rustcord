//! What the app does with threads and forum posts: join, leave, start, post.

use cord_core::http::{HttpError, Upload};
use cord_core::model::Channel;
use cord_core::{Id, NewPost};
use gpui_kit::{Context, Task};

use super::{Store, Toast};

impl Store {
    /// Joins a thread: shown as joined at once, and put back if Discord refuses.
    pub fn join_thread(&mut self, thread: Id, cx: &mut Context<Self>) {
        self.cache.set_joined(thread, true);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.join_thread(thread).await;
            if result.is_err() {
                let _ = this.update(cx, |store, cx| {
                    store.cache.set_joined(thread, false);
                    cx.notify();
                });
            }
            Self::report(this, result, "Couldn't join the thread", cx);
        })
        .detach();
    }

    pub fn leave_thread(&mut self, thread: Id, cx: &mut Context<Self>) {
        self.cache.set_joined(thread, false);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.leave_thread(thread).await;
            if result.is_err() {
                let _ = this.update(cx, |store, cx| {
                    store.cache.set_joined(thread, true);
                    cx.notify();
                });
            }
            Self::report(this, result, "Couldn't leave the thread", cx);
        })
        .detach();
    }

    /// Starts a thread from `message`; the task gives its id once it exists.
    pub fn start_thread(&mut self, channel: Id, message: Id, name: String, cx: &mut Context<Self>) -> Task<Option<Id>> {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| match core.start_thread(channel, message, name).await {
            Ok(thread) => {
                let id = thread.id;
                this.update(cx, |store, cx| {
                    store.cache.add_thread(thread);
                    cx.notify();
                })
                .ok()?;
                Some(id)
            }
            Err(error) => {
                if !matches!(error, HttpError::Unauthorized) {
                    let _ = this.update(cx, |store, cx| {
                        store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Couldn't start the thread", error.to_string())))
                    });
                }
                None
            }
        })
    }

    /// Makes a forum post; the task gives its id once it exists.
    pub fn create_post(&mut self, forum: Id, post: NewPost, files: Vec<Upload>, cx: &mut Context<Self>) -> Task<Option<Id>> {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| match core.create_post(forum, post, files).await {
            Ok((thread, _first)) => {
                let id = thread.id;
                this.update(cx, |store, cx| {
                    store.cache.add_thread(thread);
                    cx.notify();
                })
                .ok()?;
                Some(id)
            }
            Err(error) => {
                if !matches!(error, HttpError::Unauthorized) {
                    let _ = this.update(cx, |store, cx| {
                        store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Couldn't make the post", error.to_string())))
                    });
                }
                None
            }
        })
    }

    /// Keeps threads learned over REST (a forum's posts, archived threads) so they can be opened.
    pub fn add_threads(&mut self, threads: Vec<Channel>, joined: &[Id], cx: &mut Context<Self>) {
        self.cache.add_threads(threads, joined);
        cx.notify();
    }
}
