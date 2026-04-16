use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::time::interval;

//  Configuration

#[derive(Debug, Clone, Deserialize)]
struct Config {
    routes: Vec<Route>,
}

#[derive(Debug, Clone, Deserialize)]
struct Route {
    /// URL prefix this proxy listens on ("/api", "/static")
    path_prefix: String,
    /// Load-balancing strategy: "round_robin" | "least_connections"
    strategy: String,
    backends: Vec<String>,
}

//  Backend State

#[derive(Debug, Clone)]
struct BackendState {
    url: String,
    healthy: bool,
    active_connections: usize,
    last_checked: Instant,
    total_requests: u64,
    failed_requests: u64,
}

impl BackendState {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            healthy: true, // optimistic start
            active_connections: 0,
            last_checked: Instant::now(),
            total_requests: 0,
            failed_requests: 0,
        }
    }

    fn success_rate(&self) -> f64 {
        if self.total_requests == 0 {
            return 100.0;
        }
        ((self.total_requests - self.failed_requests) as f64 / self.total_requests as f64) * 100.0
    }
}

//  Route Runtime

struct RouteRuntime {
    path_prefix: String,
    strategy: String,
    backends: RwLock<Vec<BackendState>>,
    rr_counter: AtomicUsize,
}

impl RouteRuntime {
    fn from_config(route: &Route) -> Self {
        Self {
            path_prefix: route.path_prefix.clone(),
            strategy: route.strategy.clone(),
            backends: RwLock::new(
                route
                    .backends
                    .iter()
                    .map(|u| BackendState::new(u))
                    .collect(),
            ),
            rr_counter: AtomicUsize::new(0),
        }
    }

    /// Pick a healthy backend based on strategy
    fn pick_backend(&self) -> Option<String> {
        let backends = self.backends.read().unwrap();
        let healthy: Vec<&BackendState> = backends.iter().filter(|b| b.healthy).collect();
        if healthy.is_empty() {
            return None;
        }

        match self.strategy.as_str() {
            "least_connections" => healthy
                .iter()
                .min_by_key(|b| b.active_connections)
                .map(|b| b.url.clone()),
            _ => {
                // round_robin (default)
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
                Some(healthy[idx].url.clone())
            }
        }
    }

    fn increment_connections(&self, url: &str) {
        let mut backends = self.backends.write().unwrap();
        if let Some(b) = backends.iter_mut().find(|b| b.url == url) {
            b.active_connections += 1;
            b.total_requests += 1;
        }
    }

    fn decrement_connections(&self, url: &str, failed: bool) {
        let mut backends = self.backends.write().unwrap();
        if let Some(b) = backends.iter_mut().find(|b| b.url == url) {
            if b.active_connections > 0 {
                b.active_connections -= 1;
            }
            if failed {
                b.failed_requests += 1;
            }
        }
    }
}

//  App State

struct AppState {
    client: Client,
    routes: Vec<Arc<RouteRuntime>>,
}

impl AppState {
    fn find_route(&self, path: &str) -> Option<Arc<RouteRuntime>> {
        // Longest prefix match
        self.routes
            .iter()
            .filter(|r| path.starts_with(&r.path_prefix))
            .max_by_key(|r| r.path_prefix.len())
            .cloned()
    }
}

//  Health Checker

async fn run_health_checks(routes: Vec<Arc<RouteRuntime>>, client: Client) {
    let mut ticker = interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        for route in &routes {
            let urls: Vec<String> = {
                route
                    .backends
                    .read()
                    .unwrap()
                    .iter()
                    .map(|b| b.url.clone())
                    .collect()
            };

            for url in urls {
                let health_url = format!("{}/health", url.trim_end_matches('/'));
                let result = client
                    .get(&health_url)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await;

                let is_healthy = match result {
                    Ok(r) => r.status().is_success() || r.status().as_u16() == 404,
                    Err(_) => false,
                };

                let mut backends = route.backends.write().unwrap();
                if let Some(b) = backends.iter_mut().find(|b| b.url == url) {
                    let was_healthy = b.healthy;
                    b.healthy = is_healthy;
                    b.last_checked = Instant::now();

                    if was_healthy != b.healthy {
                        if b.healthy {
                            println!("[HEALTH] ✅  {} is back ONLINE", b.url);
                        } else {
                            println!("[HEALTH] ❌  {} went OFFLINE", b.url);
                        }
                    }
                }
            }
        }
    }
}

// ─── Proxy Handler ────────────────────────────────────────────────────────────

async fn proxy(req: HttpRequest, body: web::Bytes, data: web::Data<Arc<AppState>>) -> HttpResponse {
    let path = req.uri().to_string();

    let route = match data.find_route(&path) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "No route configured for this path",
                "path": path
            }));
        }
    };

    let backend_url = match route.pick_backend() {
        Some(url) => url,
        None => {
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "All backends are currently unavailable",
                "route": route.path_prefix
            }));
        }
    };

    // Strip prefix so backend receives clean path
    let backend_path = path.strip_prefix(&route.path_prefix).unwrap_or(&path);
    let target = format!(
        "{}{}",
        backend_url.trim_end_matches('/'),
        if backend_path.is_empty() {
            "/"
        } else {
            backend_path
        }
    );

    route.increment_connections(&backend_url);

    // Forward request
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let mut forward = data.client.request(method, &target);

    // Forward headers (skip hop-by-hop)
    for (key, value) in req.headers() {
        let name = key.as_str().to_lowercase();
        if !["host", "connection", "transfer-encoding"].contains(&name.as_str()) {
            if let Ok(v) = value.to_str() {
                forward = forward.header(key.as_str(), v);
            }
        }
    }

    // Add X-Forwarded-For
    if let Some(peer) = req.peer_addr() {
        forward = forward.header("X-Forwarded-For", peer.ip().to_string());
    }
    forward = forward.header("X-Forwarded-Host", req.connection_info().host().to_string());

    let response = forward.body(body).send().await;

    match response {
        Ok(res) => {
            let status = actix_web::http::StatusCode::from_u16(res.status().as_u16())
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);

            let mut builder = HttpResponse::build(status);

            // Forward response headers
            for (key, value) in res.headers() {
                let name = key.as_str().to_lowercase();
                if !["transfer-encoding", "connection"].contains(&name.as_str()) {
                    if let Ok(v) = value.to_str() {
                        builder.insert_header((key.as_str(), v));
                    }
                }
            }

            let bytes = res.bytes().await.unwrap_or_default();
            route.decrement_connections(&backend_url, false);
            println!(
                "[PROXY] {} {} -> {} ({})",
                req.method(),
                path,
                target,
                status
            );
            builder.body(bytes)
        }
        Err(e) => {
            route.decrement_connections(&backend_url, true);
            println!(
                "[ERROR] {} {} -> {} failed: {}",
                req.method(),
                path,
                target,
                e
            );
            HttpResponse::BadGateway().json(serde_json::json!({
                "error": "Backend request failed",
                "backend": backend_url,
                "details": e.to_string()
            }))
        }
    }
}

//  Admin API

#[derive(Serialize)]
struct StatusResponse {
    routes: Vec<RouteStatus>,
}

#[derive(Serialize)]
struct RouteStatus {
    path_prefix: String,
    strategy: String,
    backends: Vec<BackendStatus>,
}

#[derive(Serialize)]
struct BackendStatus {
    url: String,
    healthy: bool,
    active_connections: usize,
    total_requests: u64,
    failed_requests: u64,
    success_rate_percent: f64,
    last_checked_secs_ago: u64,
}

async fn admin_status(data: web::Data<Arc<AppState>>) -> HttpResponse {
    let routes = data
        .routes
        .iter()
        .map(|r| {
            let backends = r.backends.read().unwrap();
            RouteStatus {
                path_prefix: r.path_prefix.clone(),
                strategy: r.strategy.clone(),
                backends: backends
                    .iter()
                    .map(|b| BackendStatus {
                        url: b.url.clone(),
                        healthy: b.healthy,
                        active_connections: b.active_connections,
                        total_requests: b.total_requests,
                        failed_requests: b.failed_requests,
                        success_rate_percent: (b.success_rate() * 100.0).round() / 100.0,
                        last_checked_secs_ago: b.last_checked.elapsed().as_secs(),
                    })
                    .collect(),
            }
        })
        .collect();

    HttpResponse::Ok().json(StatusResponse { routes })
}

async fn proxy_health() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "Lightweight Reverse Proxy"
    }))
}

//  Main

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    //  Inline config (edit to add your backends)
    let config = Config {
        routes: vec![
            Route {
                path_prefix: "/api".to_string(),
                strategy: "round_robin".to_string(),
                backends: vec![
                    "http://127.0.0.1:9001".to_string(),
                    "http://127.0.0.1:9002".to_string(),
                    "http://127.0.0.1:9003".to_string(),
                ],
            },
            Route {
                path_prefix: "/static".to_string(),
                strategy: "least_connections".to_string(),
                backends: vec![
                    "http://127.0.0.1:9010".to_string(),
                    "http://127.0.0.1:9011".to_string(),
                ],
            },
        ],
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(50)
        .build()
        .expect("Failed to build HTTP client");

    let routes: Vec<Arc<RouteRuntime>> = config
        .routes
        .iter()
        .map(|r| Arc::new(RouteRuntime::from_config(r)))
        .collect();

    // Spawn background health checker
    let health_routes = routes.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        run_health_checks(health_routes, health_client).await;
    });

    let state = Arc::new(AppState { client, routes });

    println!("Reverse Proxy");
    println!("Listening on http://127.0.0.1:8080");
    println!("Admin status -> GET http://127.0.0.1:8080/_proxy/status");
    println!("Health check -> GET http://127.0.0.1:8080/_proxy/health");
    println!();
    for route in &state.routes {
        println!(
            "  Route: {} -> {:?} [{}]",
            route.path_prefix,
            route
                .backends
                .read()
                .unwrap()
                .iter()
                .map(|b| &b.url)
                .collect::<Vec<_>>(),
            route.strategy
        );
    }

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            // Admin routes
            .route("/_proxy/health", web::get().to(proxy_health))
            .route("/_proxy/status", web::get().to(admin_status))
            // Catch-all proxy
            .default_service(web::to(proxy))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}
