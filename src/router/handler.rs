/// Request dispatcher.
///
/// `dispatch` is the single decision point that routes a fully-parsed request
/// to the correct handler based on:
/// 1. Allowed methods check → 405 Method Not Allowed
/// 2. Redirect configured   → 3xx redirect
/// 3. CGI extension match   → (stub, wired in CGI step)
/// 4. Body size limit check → 413 Payload Too Large
/// 5. Method-specific handler (static file / upload / delete)
use crate::config::types::{RouteConfig, ServerConfig};
use crate::http::request::types::Request;
use crate::http::response::{builder, types::{Response, StatusCode}};
use crate::config::types::Method;
use crate::handlers::{delete, redirect as redirect_handler, static_file, upload};
use crate::cgi::{self, CgiTarget};

/// What `dispatch` decided to do with a request.
///
/// CGI can't be resolved synchronously into a `Response` — spawning the
/// child and pumping its pipes spans multiple `epoll_wait` ticks — so this
/// enum lets the dispatcher act on either outcome.
pub enum DispatchOutcome {
    Response(Response),
    StartCgi(CgiTarget),
}

impl DispatchOutcome {
    /// Convenience for call sites (tests, mainly) that don't expect CGI.
    /// Panics if a CGI target was returned instead of a response.
    #[cfg(test)]
    pub fn unwrap_response(self) -> Response {
        match self {
            DispatchOutcome::Response(r) => r,
            DispatchOutcome::StartCgi(_) => panic!("expected Response, got StartCgi"),
        }
    }
}


// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Dispatch `request` to the appropriate handler and return a `Response`.
///
/// `active_session_id` is the resolved session for this request (set by the
/// dispatcher during cookie resolution). Handlers use it to store/retrieve
/// per-session data via [`crate::session::store::SessionStore`].
///
/// This function is synchronous and infallible: every code path returns a
/// valid `Response`.
pub fn dispatch(
    request:           &Request,
    route:             &RouteConfig,
    server:            &ServerConfig,
    _active_session_id: Option<&str>,
) -> DispatchOutcome {
    // ------------------------------------------------------------------
    // 1. Method check
    // ------------------------------------------------------------------
    if !route.allows_method(&request.method) {
        return DispatchOutcome::Response(builder::method_not_allowed(&route.methods));
    }

    // ------------------------------------------------------------------
    // 2. Redirect
    // ------------------------------------------------------------------
    if let Some(redir) = &route.redirect {
        return DispatchOutcome::Response(redirect_handler::redirect(StatusCode(redir.code), &redir.target));
    }

    // ------------------------------------------------------------------
    // 3. Body size limit
    // ------------------------------------------------------------------
    let limit = route.client_body_limit
        .unwrap_or(server.body_limit());
    if request.body.len() > limit {
        return DispatchOutcome::Response(builder::payload_too_large());
    }

    // ------------------------------------------------------------------
    // 4. CGI extension check
    // ------------------------------------------------------------------
    if let Some(target) = cgi::cgi_target(request, route) {
        return DispatchOutcome::StartCgi(target);
    }

    // ------------------------------------------------------------------
    // 5. Method dispatch
    // ------------------------------------------------------------------
    let error_page_404 = server
        .error_page(404)
        .and_then(|path| std::fs::read_to_string(path).ok());

    let response = match request.method {
        Method::Get | Method::Head => {
            let mut resp = static_file::serve(
                request,
                route,
                error_page_404.as_deref(),
            );
            if request.method == Method::Head {
                resp.body.clear();
            }
            resp
        }

        Method::Post => {
            let limit = route.client_body_limit.unwrap_or(server.client_body_limit);
            upload::handle(request, route, limit)
        }

        Method::Delete => {
            delete::handle(request, route)
        }

        Method::Put => {
            builder::method_not_allowed(&[Method::Get, Method::Post, Method::Delete])
        }

        Method::Options => {
            options_response(route)
        }
    };

    DispatchOutcome::Response(response)
}

// ---------------------------------------------------------------------------
// OPTIONS response
// ---------------------------------------------------------------------------

fn options_response(route: &RouteConfig) -> Response {
    let methods = if route.methods.is_empty() {
        vec![Method::Get, Method::Head, Method::Post,
             Method::Delete, Method::Options]
    } else {
        let mut m = route.methods.clone();
        if !m.contains(&Method::Options) {
            m.push(Method::Options);
        }
        m
    };

    let allow: String = methods
        .iter()
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ");

    let mut resp = builder::no_content();
    resp.headers.set("Allow", allow);
    resp
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Method, RouteConfig, ServerConfig};
    use crate::http::request::types::{HeaderMap, Request, Version};
    use std::fs;

    fn get(path: &str) -> Request {
        Request {
            method:  Method::Get,
            path:    path.into(),
            query:   String::new(),
            version: Version::Http11,
            headers: {
                let mut h = HeaderMap::new();
                h.insert("Host", "localhost");
                h
            },
            body:    Vec::new(),
        }
    }

    fn server() -> ServerConfig {
        ServerConfig::default()
    }

    fn route_get_only(root: &str) -> RouteConfig {
        RouteConfig {
            path:    "/".into(),
            methods: vec![Method::Get, Method::Head],
            root:    Some(root.into()),
            ..Default::default()
        }
    }

    fn open_route(root: &str) -> RouteConfig {
        RouteConfig {
            path: "/".into(),
            root: Some(root.into()),
            ..Default::default()
        }
    }

    fn tmp_dir() -> String {
        let dir = format!("/tmp/handler_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- method check -------------------------------------------------------

    #[test]
    fn post_on_get_only_route_returns_405() {
        let dir = tmp_dir();
        let route = route_get_only(&dir);
        let req   = Request { method: Method::Post, ..get("/") };
        
        let resp  = dispatch(&req, &route, &server(), None).unwrap_response();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 405);
        assert!(resp.headers.get("Allow").is_some());
    }

    // ---- redirect -----------------------------------------------------------

    #[test]
    fn redirect_route_returns_3xx() {
        use crate::config::types::Redirect;
        let route = RouteConfig {
            path:     "/old".into(),
            redirect: Some(Redirect { code: 301, target: "/new".into() }),
            ..Default::default()
        };
        let resp = dispatch(&get("/old"), &route, &server(), None).unwrap_response();
        assert_eq!(resp.status.code(), 301);
        assert_eq!(resp.headers.get("Location"), Some("/new"));
    }

    // ---- static file --------------------------------------------------------

    #[test]
    fn get_existing_file_returns_200() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/hello.txt"), b"world").unwrap();
        let route = open_route(&dir);
        let resp  = dispatch(&get("/hello.txt"), &route, &server(), None).unwrap_response();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"world");
    }

    #[test]
    fn head_request_has_empty_body_but_content_length() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/data.txt"), b"12345").unwrap();
        let route = open_route(&dir);
        let req   = Request { method: Method::Head, ..get("/data.txt") };
        let resp  = dispatch(&req, &route, &server(), None).unwrap_response();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 200);
        assert!(resp.body.is_empty(), "HEAD must have no body");
        assert_eq!(resp.headers.get("Content-Length"), Some("5"));
    }

    // ---- body size limit ----------------------------------------------------

    #[test]
    fn body_exceeding_limit_returns_413() {
        let dir  = tmp_dir();
        let mut route = open_route(&dir);
        route.client_body_limit = Some(4);
        let mut req = get("/");
        req.method = Method::Post;
        req.body   = b"toolong".to_vec();
        let resp = dispatch(&req, &route, &server(), None).unwrap_response();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 413);
    }

    // ---- options ------------------------------------------------------------

    #[test]
    fn options_returns_allow_header() {
        let dir = tmp_dir();
        let route = open_route(&dir);
        let req = Request { method: Method::Options, ..get("/") };
        let resp = dispatch(&req, &route, &server(), None).unwrap_response();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 204);
        assert!(resp.headers.get("Allow").is_some());
    }
}