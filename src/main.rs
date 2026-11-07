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

// ---- CLI ----

const CLI_USAGE: &str = "\
tunna [command]

no command             start the proxy + manager server (reads env)

routes                 list all routes
expose <port> [host]   expose a local port (host defaults to p<port>.<base_domain>)
remove <host>-or-port  delete a route
on <target>            enable an existing route
off <target>           disable an existing route

target: [host] | [-p | --port] <port> — a bare or -p port finds the route
using that port (when unique), else p<port>.<base_domain>.

Commands talk to the running manager over HTTP (env TUNNA_MANAGER,
default http://127.0.0.1:8080). Short hosts for on/off/remove resolve
against the configured base domain.
";

fn resolve_host(arg: &str, base: &str, routes: &[Route]) -> Result<String, String> {
    let arg = arg.trim().trim_end_matches('.').to_lowercase();
    // a bare or -p port: an unambiguous port match wins, else fall through
    // to the p<port>.<base_domain> pattern
    let arg = if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
        let port = arg.parse::<u64>().unwrap_or(0);
        let mut on_port = routes.iter().filter(|r| r.port == port);
        if let (Some(only), None) = (on_port.next(), on_port.next()) {
            return Ok(only.host.clone());
        }
        format!("p{port}")
    } else {
        arg
    };
    if routes.iter().any(|r| r.host == arg) {
        return Ok(arg);
    }
    let full = format!("{arg}.{base}");
    if routes.iter().any(|r| r.host == full) {
        return Ok(full);
    }
    Err(format!("route not found: {arg}"))
}

/// target after the command: "-p 3000", "-p3000", "--port 3000",
/// "--port=3000" or a plain host/port
fn target_of(rest: &[String], cmd: &str) -> Result<String, String> {
    let usage = || format!("usage: tunna {cmd} [-p <port>] [host]");
    let mut target: Option<String> = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next().map(String::as_str) {
        match flag {
            "-p" | "--port" => target = Some(it.next().ok_or_else(usage)?.to_string()),
            s if let Some(v) = s.strip_prefix("--port=") => target = Some(v.to_string()),
            s if let Some(v) = s.strip_prefix("-p") => target = Some(v.to_string()),
            _ => {
                if target.is_some() {
                    return Err(usage());
                }
                target = Some(flag.to_string());
            }
        }
    }
    target.ok_or_else(usage)
}

async fn manager(method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
    let base = env("TUNNA_MANAGER", "http://127.0.0.1:8080");
    let uri = format!("{}{}", base.trim_end_matches('/'), path);
    let client = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method(method)
        .uri(&uri)
        .header("content-type", "application/json")
        .body(Body::from(match body {
            Some(v) => serde_json::to_vec(&v).unwrap(),
            None => Vec::new(),
        }))
        .expect("static cli request parts are valid");
    let res = client
        .request(req)
        .await
        .map_err(|_| format!("manager unreachable at {base}"))?;
    let status = res.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .map_err(|_| "invalid manager response")?
        .to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    Ok((status, v))
}

async fn cmd_routes() -> Result<(), String> {
    let (status, v) = manager("GET", "/api/routes", None).await?;
    if status != 200 {
        return Err(v["error"].as_str().unwrap_or("request failed").to_owned());
    }
    let base = v["base_domain"].as_str().unwrap_or("");
    let routes: Vec<Route> =
        serde_json::from_value(v["routes"].clone()).map_err(|e| e.to_string())?;
    println!("base domain: {base}");
    for r in routes {
        println!(
            "{:<44} :{:<5} {}",
            r.host,
            r.port,
            if r.enabled { "live" } else { "off" }
        );
    }
    Ok(())
}

async fn cmd_expose(args: &[String]) -> Result<(), String> {
    let port: u64 = args
        .first()
        .ok_or("usage: tunna expose <port> [host]")?
        .parse()
        .map_err(|_| "port must be a number")?;
    let host = match args.get(1).map(|s| s.trim().to_lowercase()) {
        None => Value::Null,
        Some(h) if h.contains('.') => Value::from(h),
        Some(h) => {
            let (_, v) = manager("GET", "/api/routes", None).await?;
            let base = v["base_domain"].as_str().unwrap_or("");
            Value::from(format!("{h}.{base}"))
        }
    };
    let (status, v) =
        manager("POST", "/api/routes", Some(json!({ "port": port, "host": host }))).await?;
    if status == 201 {
        println!(
            "exposed {} to local :{}",
            v["host"].as_str().unwrap_or("?"),
            v["port"]
        );
        Ok(())
    } else {
        Err(v["error"].as_str().unwrap_or("request failed").to_owned())
    }
}

// ---- boot ----

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli(args).await);
    }
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

fn report(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

async fn cli(args: Vec<String>) -> i32 {
    let (cmd, rest) = args.split_first().unwrap();
    match cmd.as_str() {
        "routes" | "list" => report(cmd_routes().await),
        "expose" => report(cmd_expose(rest).await),
        "remove" => report(remove_target(rest, "remove").await),
        "on" => report(toggle_target(rest, "on", true).await),
        "off" => report(toggle_target(rest, "off", false).await),
        "help" | "-h" | "--help" => {
            print!("{CLI_USAGE}");
            0
        }
        _ => {
            eprintln!("unknown command: {cmd}\n\n{CLI_USAGE}");
            2
        }
    }
}

async fn toggle_target(rest: &[String], cmd: &str, enabled: bool) -> Result<(), String> {
    let target = target_of(rest, cmd)?;
    host_action(&target, |host| async move { set_enabled(&host, enabled).await }).await
}

async fn remove_target(rest: &[String], cmd: &str) -> Result<(), String> {
    let target = target_of(rest, cmd)?;
    host_action(&target, |host| async move {
        let path = format!("/api/routes/{host}");
        let (status, v) = manager("DELETE", &path, None).await?;
        if status == 200 {
            println!("removed {}", v["deleted"].as_str().unwrap_or(&host));
            Ok(())
        } else {
            Err(v["error"].as_str().unwrap_or("request failed").to_owned())
        }
    })
    .await
}

async fn set_enabled(host: &str, enabled: bool) -> Result<(), String> {
    let (status, v) =
        manager("PATCH", &format!("/api/routes/{host}"), Some(json!({ "enabled": enabled }))).await?;
    if status == 200 {
        println!("{} is {}", host, if enabled { "live" } else { "off" });
        Ok(())
    } else {
        Err(v["error"].as_str().unwrap_or("request failed").to_owned())
    }
}

/// resolve a short host ("p3000") to its stored form, then run the action
async fn host_action<F, Fut>(arg: &str, action: F) -> Result<(), String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let (_, v) = manager("GET", "/api/routes", None).await?;
    if v["base_domain"].is_null() {
        return Err("manager unreachable: no routes response".to_owned());
    }
    let base = v["base_domain"].as_str().unwrap_or("");
    let routes: Vec<Route> =
        serde_json::from_value(v["routes"].clone()).map_err(|e| e.to_string())?;
    let host = resolve_host(arg, base, &routes)?;
    action(host).await
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

    #[test]
    fn host_resolution() {
        let routes = vec![route("p3000.example.com", 3000, true)];
        assert_eq!(
            resolve_host("p3000.example.com", "example.com", &routes),
            Ok("p3000.example.com".to_owned())
        );
        assert_eq!(
            resolve_host("p3000", "example.com", &routes),
            Ok("p3000.example.com".to_owned())
        );
        assert!(resolve_host("p3001", "example.com", &routes)
            .unwrap_err()
            .starts_with("route not found"));
        assert!(resolve_host("p3000.other.com", "example.com", &routes).is_err());
    }

    #[test]
    fn port_target_resolves() {
        let routes = vec![
            route("app.example.com", 3000, true),
            route("p8000.example.com", 8000, true),
        ];
        // unique port anywhere in the table wins over the p<port> pattern
        assert_eq!(
            resolve_host("3000", "example.com", &routes),
            Ok("app.example.com".to_owned())
        );
        assert_eq!(
            resolve_host("8000", "example.com", &routes),
            Ok("p8000.example.com".to_owned())
        );
        // ambiguous port: falls back to the p<port> pattern, which is missing
        let dup = vec![
            route("a.example.com", 3000, true),
            route("b.example.com", 3000, true),
        ];
        assert!(resolve_host("3000", "example.com", &dup).is_err());
    }

    #[test]
    fn target_flag_parsing() {
        let arg = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(target_of(&arg(&["-p", "3000"]), "on"), Ok("3000".into()));
        assert_eq!(target_of(&arg(&["-p3000"]), "on"), Ok("3000".into()));
        assert_eq!(target_of(&arg(&["--port", "3000"]), "off"), Ok("3000".into()));
        assert_eq!(target_of(&arg(&["--port=3000"]), "off"), Ok("3000".into()));
        assert_eq!(target_of(&arg(&["p3000"]), "on"), Ok("p3000".into()));
        assert_eq!(target_of(&arg(&["-p", "3000", "host"]), "on").unwrap_err(), "usage: tunna on [-p <port>] [host]");
        assert_eq!(target_of(&arg(&[]), "on").unwrap_err(), "usage: tunna on [-p <port>] [host]");
    }
}
