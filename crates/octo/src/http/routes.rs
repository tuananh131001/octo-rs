//! Route registration with ASP.NET Core's matching rules.
//!
//! Octo's controllers relied on three things axum does differently:
//!
//! - literal segments match ignoring case (`/REST/Ping.VIEW` reaches `ping`) and a trailing
//!   slash is ignored;
//! - a request whose path matches a route but whose method does not falls through to the
//!   catch-all (`{**endpoint}` takes every method), never a 405;
//! - anything unmatched goes to the catch-all, which relays to Navidrome.
//!
//! [`RouteSet`] collects every template as it builds the axum router, and [`PathCanon`] then
//! rewrites an incoming path onto the template's own spelling before axum matches it.

use axum::Router;
use axum::extract::Request;
use axum::handler::Handler;
use axum::routing::MethodRouter;
use std::sync::Arc;

use crate::app::AppState;

/// A set of routes being built, with the templates kept for path canonicalisation.
pub struct RouteSet {
    routes: Vec<(String, MethodRouter<AppState>)>,
}

impl Default for RouteSet {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteSet {
    pub fn new() -> Self {
        RouteSet { routes: Vec::new() }
    }

    /// One template (axum syntax: `/api/admin/genre/{id}`) and the methods it answers.
    pub fn route(mut self, template: &str, methods: MethodRouter<AppState>) -> Self {
        self.routes.push((template.to_string(), methods));
        self
    }

    /// A Subsonic endpoint, at both `rest/{name}` and `rest/{name}.view`, as every
    /// SubsonicController action was declared.
    pub fn subsonic(self, name: &str, methods: MethodRouter<AppState>) -> Self {
        self.route(&format!("/rest/{name}"), methods.clone()).route(&format!("/rest/{name}.view"), methods)
    }

    pub fn merge(mut self, other: RouteSet) -> Self {
        self.routes.extend(other.routes);
        self
    }

    /// Builds the router. Every route falls back to `catch_all` for methods it does not
    /// answer, and so does every unmatched path.
    pub fn finish<H, T>(self, catch_all: H) -> (Router<AppState>, PathCanon)
    where
        H: Handler<T, AppState> + Clone,
        T: 'static,
    {
        let mut router = Router::new();
        let mut templates = Vec::with_capacity(self.routes.len());
        for (template, methods) in self.routes {
            router = router.route(&template, methods.fallback(catch_all.clone()));
            templates.push(Template::parse(&template));
        }
        (router.fallback(catch_all), PathCanon { templates: Arc::new(templates) })
    }
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    Param,
    CatchAll,
}

#[derive(Debug, Clone)]
struct Template {
    segments: Vec<Segment>,
}

impl Template {
    fn parse(template: &str) -> Self {
        let segments = template
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.starts_with("{*") {
                    Segment::CatchAll
                } else if s.starts_with('{') {
                    Segment::Param
                } else {
                    Segment::Literal(s.to_string())
                }
            })
            .collect();
        Template { segments }
    }

    /// The path spelled as this template spells its literals, when the path matches it.
    fn canonical(&self, path_segments: &[&str]) -> Option<String> {
        let mut out = Vec::with_capacity(path_segments.len());
        for (i, seg) in self.segments.iter().enumerate() {
            match seg {
                Segment::CatchAll => {
                    out.extend(path_segments[i..].iter().map(|s| s.to_string()));
                    return Some(format!("/{}", out.join("/")));
                }
                Segment::Param => out.push(path_segments.get(i)?.to_string()),
                Segment::Literal(lit) => {
                    let s = path_segments.get(i)?;
                    if !s.eq_ignore_ascii_case(lit) {
                        return None;
                    }
                    out.push(lit.clone());
                }
            }
        }
        (path_segments.len() == self.segments.len()).then(|| format!("/{}", out.join("/")))
    }
}

/// Rewrites request paths onto the registered templates' spelling.
#[derive(Clone)]
pub struct PathCanon {
    templates: Arc<Vec<Template>>,
}

impl PathCanon {
    /// The canonical spelling of `path`, or `None` when no template matches (the path then
    /// reaches the catch-all as sent).
    pub fn canonical(&self, path: &str) -> Option<String> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        // Templates with only literals win over ones with parameters, as ASP.NET orders them.
        let literal_first = self
            .templates
            .iter()
            .filter(|t| t.segments.iter().all(|s| matches!(s, Segment::Literal(_))))
            .chain(self.templates.iter().filter(|t| t.segments.iter().any(|s| !matches!(s, Segment::Literal(_)))));
        for t in literal_first {
            if let Some(c) = t.canonical(&segments) {
                return Some(c);
            }
        }
        None
    }

    /// Applies [`PathCanon::canonical`] to a request in place, keeping the query string.
    pub fn rewrite(&self, req: &mut Request) {
        let path = req.uri().path();
        let Some(canonical) = self.canonical(path) else { return };
        if canonical == path {
            return;
        }
        let pq = match req.uri().query() {
            Some(q) => format!("{canonical}?{q}"),
            None => canonical,
        };
        let mut parts = req.uri().clone().into_parts();
        if let Ok(pq) = pq.parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(templates: &[&str]) -> PathCanon {
        PathCanon { templates: Arc::new(templates.iter().map(|t| Template::parse(t)).collect()) }
    }

    #[test]
    fn literals_match_ignoring_case_and_trailing_slash() {
        let c = canon(&["/rest/ping", "/rest/ping.view", "/radio/stream/{token}"]);
        assert_eq!(c.canonical("/REST/Ping.VIEW").as_deref(), Some("/rest/ping.view"));
        assert_eq!(c.canonical("/rest/ping/").as_deref(), Some("/rest/ping"));
        assert_eq!(c.canonical("/Radio/Stream/AbC").as_deref(), Some("/radio/stream/AbC"));
        assert_eq!(c.canonical("/rest/getArtists"), None);
    }

    #[test]
    fn literal_templates_win_over_parameter_templates() {
        let c = canon(&["/api/admin/{id}", "/api/admin/Settings"]);
        assert_eq!(c.canonical("/api/admin/settings").as_deref(), Some("/api/admin/Settings"));
    }
}
