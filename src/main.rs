use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{CONNECTION, UPGRADE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use hyper::upgrade::OnUpgrade;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Route {
    host: String,
    port: u64,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone)]
struct AppState {
    base_domain: String,
    data_dir: PathBuf,
    routes: Arc<RwLock<Vec<Route>>>,
    client: Client<HttpConnector, Body>,
}

type ApiResult = Result<Response, (StatusCode, Json<Value>)>;

fn api_err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.into() })))
}

// ---- host/port validation (behavior copied from python normalize_host / HOST_RE) ----

fn is_valid_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    labels.into_iter().all(|l| {
        !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// python int() over a JSON value
fn as_int(v: &Value) -> Result<u64, String> {
    match v {
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.fract() != 0.0 {
                Err(format!("cannot convert float {f} to integer"))
            } else if f < 0.0 {
                Err(format!(
                    "negative number cannot be converted to unsigned: {f}"
                ))
            } else {
                Ok(f as u64)
            }
        }
        Value::Bool(b) => Ok(*b as u64),
        Value::Null => Err("invalid literal for int() with base 10: 'None'".into()),
        other => {
            let s = other
                .as_str()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| other.to_string());
            let t = s.trim().to_owned();
            t.parse::<u64>().map_err(|_| {
                format!("invalid literal for int() with base 10: '{s}'")
            })
        }
    }
}

/// python bool() over a JSON value
fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn normalize_host(payload: &Value, base_domain: &str) -> Result<(String, u64), String> {
    let port = match payload.get("port") {
        None | Some(Value::Null) => return Err("port is required".into()),
        Some(Value::String(s)) if s.is_empty() => return Err("port is required".into()),
        Some(v) => as_int(v)?,
    };
    if !(1..=65535).contains(&port) {
        return Err("port must be 1-65535".into());
    }
    let mut host = payload
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if host.is_empty() {
        host = format!("p{port}.{base_domain}");
    }
    if !is_valid_host(&host) {
        return Err("invalid hostname".into());
    }
    Ok((host, port))
}

// ---- routes store ----

fn load_routes(data_dir: &PathBuf) -> Vec<Route> {
    let file = data_dir.join("routes.json");
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", file.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("cannot read {}: {e}", file.display()),
    }
}

fn persist(routes: &[Route], data_dir: &PathBuf) {
    std::fs::create_dir_all(data_dir).expect("cannot create data dir");
    let tmp = data_dir.join("routes.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(routes).unwrap())
        .expect("cannot write routes file");
    std::fs::rename(&tmp, data_dir.join("routes.json")).expect("cannot replace routes file");
}

fn route_for(routes: &[Route], host: &str) -> Option<u64> {
    routes
        .iter()
        .find(|r| r.host == host && r.enabled)
        .map(|r| r.port)
}

// ---- reverse proxy listener ----

fn strip_hop_headers(headers: &mut HeaderMap, upgrading: bool) {
    for h in [
        CONNECTION.as_str(),
        "transfer-encoding",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        UPGRADE.as_str(),
    ] {
        headers.remove(h);
    }
    if upgrading {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    }
}

fn plain(status: StatusCode, body: &str) -> Response {
    (
        status,
        [("content-type", "text/plain; charset=utf-8")],
        body.to_owned(),
    )
        .into_response()
}

async fn proxy_request(State(st): State<AppState>, mut req: Request<Body>) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let port = {
        let snapshot: Vec<Route> = {
            let guard = st.routes.read().unwrap();
            guard.as_slice().to_vec()
        };
        route_for(&snapshot, &host)
    };
    let Some(port) = port else {
        return plain(StatusCode::NOT_FOUND, "no tunnel route for this host\n");
    };

    let upgrading = req
        .headers()
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    let server_up = upgrading.then(|| hyper::upgrade::on(&mut req));

    let pq = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let mut headers = req.headers().clone();
    strip_hop_headers(&mut headers, upgrading);

    let method = req.method().clone();
    let body = Body::from_stream(req.into_body().into_data_stream());
    let outbound = Request::builder()
        .method(method)
        .uri(format!("http://localhost:{port}{pq}"))
        .body(body)
        .expect("static proxy request parts are valid");

    let fut = st.client.request(outbound);

    let Ok(mut res) = fut.await else {
        return plain(StatusCode::BAD_GATEWAY, "upstream unreachable\n");
    };

    // hyper puts the client-side OnUpgrade in the 101 response extensions
    let client_up = if res.status() == StatusCode::SWITCHING_PROTOCOLS {
        res.extensions().get::<OnUpgrade>().cloned()
    } else {
        None
    };

    let res_upgrading = client_up.is_some();
    strip_hop_headers(res.headers_mut(), res_upgrading);

    if let (Some(client_up), Some(server_up)) = (client_up, server_up) {
        tokio::spawn(async move {
            let (client_io, server_io) = match (client_up.await, server_up.await) {
                (Ok(c), Ok(s)) => (c, s),
                (e, f) => {
                    eprintln!("upgrade connect failed: {e:?} / {f:?}");
                    return;
                }
            };
            let mut a = TokioIo::new(server_io);
            let mut b = TokioIo::new(client_io);
            if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                eprintln!("upgrade pipe ended: {e:?}");
            }
        });
    }

    let (parts, body) = res.into_parts();
    Response::from_parts(parts, Body::new(body))
}

// ---- manager API listener (localhost only) ----

async fn ui() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_routes(State(st): State<AppState>) -> Json<Value> {
    let snapshot: Vec<Route> = {
        let guard = st.routes.read().unwrap();
        guard.as_slice().to_vec()
    };
    Json(json!({ "base_domain": st.base_domain, "routes": snapshot }))
}

async fn post_route(State(st): State<AppState>, body: Body) -> ApiResult {
    let payload: Value = match read_json(body).await {
        Ok(v) => v,
        Err(e) => return Err(api_err(StatusCode::BAD_REQUEST, e)),
    };
    let (host, port) = match normalize_host(&payload, &st.base_domain) {
        Ok(v) => v,
        Err(e) => return Err(api_err(StatusCode::BAD_REQUEST, e)),
    };
    let mut routes = st.routes.write().unwrap();
    if routes.iter().any(|r| r.host == host) {
        return Err(api_err(StatusCode::CONFLICT, format!("{host} already mapped")));
    }
    let route = Route {
        host: host.clone(),
        port,
        enabled: true,
    };
    routes.push(route.clone());
    persist(&routes, &st.data_dir);
    Ok((StatusCode::CREATED, Json(route)).into_response())
}

async fn patch_route(State(st): State<AppState>, Path(host): Path<String>, body: Body) -> ApiResult {
    let host = host.trim_end_matches('/').to_string();
    let payload: Value = match read_json(body).await {
        Ok(v) => v,
        Err(e) => return Err(api_err(StatusCode::BAD_REQUEST, e)),
    };
    let mut routes = st.routes.write().unwrap();
    let Some(route) = routes.iter_mut().find(|r| r.host == host) else {
        return Err(api_err(StatusCode::NOT_FOUND, "route not found"));
    };
    if let Some(v) = payload.get("enabled") {
        route.enabled = truthy(v);
    }
    if let Some(v) = payload.get("port") {
        route.port = match as_int(v) {
            Ok(p) => p,
            Err(e) => return Err(api_err(StatusCode::BAD_REQUEST, e)),
        };
    }
    let updated = route.clone();
    persist(&routes, &st.data_dir);
    drop(routes);
    Ok(Json(updated).into_response())
}

async fn delete_route(State(st): State<AppState>, Path(host): Path<String>) -> ApiResult {
    let host = host.trim_end_matches('/').to_string();
    let mut routes = st.routes.write().unwrap();
    let before = routes.len();
    routes.retain(|r| r.host != host);
    if routes.len() == before {
        return Err(api_err(StatusCode::NOT_FOUND, "route not found"));
    }
    persist(&routes, &st.data_dir);
    drop(routes);
    Ok(Json(json!({ "deleted": host })).into_response())
}

async fn read_json(body: Body) -> Result<Value, &'static str> {
    match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "invalid JSON body"),
        Err(_) => Err("invalid JSON body"),
    }
}

async fn not_found() -> Response {
    api_err(StatusCode::NOT_FOUND, "not found").into_response()
}

fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/index.html", get(ui))
        .route("/api/routes", get(get_routes).post(post_route))
        .route(
            "/api/routes/{host}",
            patch(patch_route).delete(delete_route),
        )
        .fallback(not_found)
        .with_state(state)
}

// ---- boot ----

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(env("DATA_DIR", "./data"));
    let st = AppState {
        base_domain: env("BASE_DOMAIN", "example.com").to_lowercase(),
        data_dir: data_dir.clone(),
        routes: Arc::new(RwLock::new(load_routes(&data_dir))),
        client: Client::builder(TokioExecutor::new()).build_http(),
    };

    let manager_addr: std::net::SocketAddr = format!(
        "{}:{}",
        env("MANAGER_BIND", "127.0.0.1"),
        env("MANAGER_PORT", "8080")
    )
    .parse()
    .expect("invalid MANAGER_BIND/MANAGER_PORT");
    let proxy_addr: std::net::SocketAddr = format!("0.0.0.0:{}", env("LISTEN_PORT", "8088"))
        .parse()
        .expect("invalid LISTEN_PORT");

    let manager = {
        let st = st.clone();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(manager_addr)
                .await
                .expect("cannot bind manager listener");
            eprintln!("manager listening on {manager_addr}");
            axum::serve(listener, api_router(st)).await.unwrap();
        })
    };

    let listener = tokio::net::TcpListener::bind(proxy_addr)
        .await
        .expect("cannot bind proxy listener");
    eprintln!(
        "proxy listening on {proxy_addr} (base domain {})",
        st.base_domain
    );
    let proxy = Router::new().fallback(proxy_request).with_state(st);
    axum::serve(listener, proxy).await.unwrap();
    manager.abort();
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: &str, port: u64, enabled: bool) -> Route {
        Route {
            host: host.to_owned(),
            port,
            enabled,
        }
    }

    #[test]
    fn host_auto_from_port() {
        let (host, port) =
            normalize_host(&json!({ "port": 3000 }), "example.com").unwrap();
        assert_eq!(host, "p3000.example.com");
        assert_eq!(port, 3000);
    }

    #[test]
    fn host_normalized_and_validated() {
        let (host, _) =
            normalize_host(&json!({ "port": 80, "host": "  Foo.Bar.COM " }), "example.com")
                .unwrap();
        assert_eq!(host, "foo.bar.com");
        assert_eq!(
            normalize_host(&json!({ "port": 80, "host": "not a host!" }), "x.com"),
            Err("invalid hostname".to_owned())
        );
        // single label is invalid (regex requires a dot-separated suffix)
        assert_eq!(
            normalize_host(&json!({ "port": 80, "host": "justhost" }), "x.com"),
            Err("invalid hostname".to_owned())
        );
    }

    #[test]
    fn port_validation() {
        assert_eq!(
            normalize_host(&json!({}), "x.com"),
            Err("port is required".to_owned())
        );
        assert_eq!(
            normalize_host(&json!({ "port": 0, "host": "a.b" }), "x.com"),
            Err("port must be 1-65535".to_owned())
        );
        assert_eq!(
            normalize_host(&json!({ "port": 65536, "host": "a.b" }), "x.com"),
            Err("port must be 1-65535".to_owned())
        );
        assert_eq!(
            normalize_host(&json!({ "port": "70000", "host": "a.b" }), "x.com"),
            Err("port must be 1-65535".to_owned())
        );
    }

    #[test]
    fn proxy_route_matching() {
        let routes = vec![
            route("a.example.com", 8000, true),
            route("b.example.com", 8001, false),
        ];
        assert_eq!(route_for(&routes, "a.example.com"), Some(8000));
        assert_eq!(route_for(&routes, "b.example.com"), None);
        assert_eq!(route_for(&routes, "c.example.com"), None);
        assert_eq!(route_for(&routes, "a.example.com."), None);
    }

    #[test]
    fn serde_defaults_enabled_true_like_python() {
        let r: Route = serde_json::from_str(r#"{"host":"h","port":1}"#).unwrap();
        assert!(r.enabled);
    }
}
