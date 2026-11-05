use gpui_kit::{
    AnyView, App, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
};
use std::collections::HashMap;

use crate::store::Services;

pub type Page = fn(RouteContext, &mut Window, &mut App) -> AnyView;

#[derive(Clone)]
pub struct RouteContext {
    pub router: Entity<Router>,
    pub params: HashMap<String, String>,
    pub services: Services,
}

impl RouteContext {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }
}

pub fn navigate(router: &Entity<Router>, path: impl Into<String>, cx: &mut App) {
    let path = path.into();
    router.update(cx, |router, cx| router.navigate(path, cx));
}

pub struct Route {
    pattern: String,
    page: Page,
    cached_path: Option<String>,
    cached_view: Option<AnyView>,
}

impl Route {
    pub fn new(pattern: impl Into<String>, page: Page) -> Self {
        Self {
            pattern: normalize_path(&pattern.into()),
            page,
            cached_path: None,
            cached_view: None,
        }
    }
}

pub struct Router {
    routes: Vec<Route>,
    current_path: String,
    history: Vec<String>,
    /// Pages left with Back, for Forward. Any other navigation clears it.
    forward: Vec<String>,
    services: Services,
}

impl Router {
    pub fn new(routes: Vec<Route>, services: Services, initial_path: impl Into<String>) -> Self {
        Self {
            routes,
            current_path: normalize_path(&initial_path.into()),
            history: Vec::new(),
            forward: Vec::new(),
            services,
        }
    }

    /// Returns to the previous page, or `fallback` when there is none.
    pub fn back(&mut self, fallback: &str, cx: &mut Context<Self>) {
        let target = self.history.pop().unwrap_or_else(|| fallback.to_string());
        let left = self.current_path.clone();
        if self.go(target, cx) {
            // `go` recorded the page we just left; going back must not.
            self.history.pop();
            self.forward.push(left);
        }
    }

    /// Undoes the last Back.
    pub fn forward(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.forward.pop() {
            self.go(target, cx);
        }
    }

    pub fn can_go_back(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub fn navigate(&mut self, path: impl Into<String>, cx: &mut Context<Self>) {
        let path = path.into();
        if normalize_path(&path) != self.current_path {
            self.forward.clear();
        }
        self.go(path, cx);
    }

    /// Shows `path`; false when it was already showing.
    fn go(&mut self, path: String, cx: &mut Context<Self>) -> bool {
        let path = normalize_path(&path);

        if self.current_path == path {
            return false;
        }

        if path != "/login" {
            self.history.push(self.current_path.clone());
            if self.history.len() > 50 {
                self.history.remove(0);
            }
        } else {
            self.history.clear();
        }

        tracing::info!(
            from = %self.current_path,
            to = %path,
            "navigation",
        );

        self.current_path = path;

        // The cached view belongs to the previous route.
        // Force the new route to be constructed.
        for route in &mut self.routes {
            route.cached_path = None;
            route.cached_view = None;
        }

        cx.notify();
        true
    }

    pub fn current_path(&self) -> &str {
        &self.current_path
    }

    fn resolve(&self) -> Option<(usize, HashMap<String, String>)> {
        let mut best: Option<(usize, HashMap<String, String>, i32)> = None;

        for (index, route) in self.routes.iter().enumerate() {
            let Some(params) = match_route(&route.pattern, &self.current_path) else {
                continue;
            };

            let score = route_score(&route.pattern);

            if best
                .as_ref()
                .map(|(_, _, current)| score > *current)
                .unwrap_or(true)
            {
                best = Some((index, params, score));
            }
        }

        best.map(|(index, params, _)| (index, params))
    }
}

impl Render for Router {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((index, params)) = self.resolve() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_3xl()
                .child("404 - Not Found")
                .into_any_element();
        };

        let path = self.current_path.clone();
        let router = cx.entity();

        let route = &mut self.routes[index];

        if route.cached_path.as_deref() != Some(path.as_str()) {
            let view = (route.page)(
                RouteContext {
                    router,
                    params,
                    services: self.services.clone(),
                },
                window,
                cx,
            );

            route.cached_path = Some(path);
            route.cached_view = Some(view);
        }

        // Cached: something redrawing beside the page (a GIF in the server list) has
        // no reason to rebuild every row in it.
        route.cached_view.clone().unwrap().cached(gpui_kit::StyleRefinement::default().size_full()).into_any_element()
    }
}

fn normalize_path(path: &str) -> String {
    let mut path = path.trim().to_string();

    if path.is_empty() {
        return "/".into();
    }

    if !path.starts_with('/') {
        path.insert(0, '/');
    }

    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }

    path
}

fn route_score(pattern: &str) -> i32 {
    pattern
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| match segment {
            "*" => 0,
            s if s.starts_with(':') => 1,
            _ => 2,
        })
        .sum()
}

fn match_route(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pattern_segments = pattern
        .split('/')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>();

    let path_segments = path
        .split('/')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>();

    let mut params = HashMap::new();
    let mut path_index = 0;

    for segment in pattern_segments {
        if segment == "*" {
            params.insert("wildcard".into(), path_segments[path_index..].join("/"));

            return Some(params);
        }

        let value = path_segments.get(path_index)?;

        if let Some(name) = segment.strip_prefix(':') {
            params.insert(name.to_string(), (*value).to_string());
        } else if segment != *value {
            return None;
        }

        path_index += 1;
    }

    (path_index == path_segments.len()).then_some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn best<'a>(patterns: &[&'a str], path: &str) -> Option<(&'a str, HashMap<String, String>)> {
        patterns
            .iter()
            .filter_map(|p| match_route(p, path).map(|params| (*p, params, route_score(p))))
            .max_by_key(|(_, _, score)| *score)
            .map(|(p, params, _)| (p, params))
    }

    const ROUTES: &[&str] = &["/channels/@me", "/channels/@me/:channel", "/channels/:guild", "/channels/:guild/:channel", "*"];

    #[test]
    fn dm_routes_beat_guild_routes() {
        let (p, params) = best(ROUTES, "/channels/@me/42").unwrap();
        assert_eq!(p, "/channels/@me/:channel");
        assert_eq!(params["channel"], "42");
        assert_eq!(best(ROUTES, "/channels/@me").unwrap().0, "/channels/@me");
    }

    #[test]
    fn guild_channel_route_captures_both() {
        let (p, params) = best(ROUTES, "/channels/300/311").unwrap();
        assert_eq!(p, "/channels/:guild/:channel");
        assert_eq!((params["guild"].as_str(), params["channel"].as_str()), ("300", "311"));
    }

    #[test]
    fn unknown_paths_fall_through_to_wildcard() {
        assert_eq!(best(ROUTES, "/nope/1/2/3").unwrap().0, "*");
    }
}
