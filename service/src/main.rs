use axum::{extract::{Path, State}, routing::get, Json, Router};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use reqwest::StatusCode as HttpStatusCode;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, env, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{signal, time};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
enum InternetStatus {
    Normal,
    Degraded,
    Failed,
}

impl Default for InternetStatus {
    fn default() -> Self { InternetStatus::Failed }
}

#[derive(Debug, Clone, Serialize)]
struct StatusPayload {
    status: InternetStatus,
    score: u8,
    latency_ms: Option<u128>,
    p95_ms: Option<u128>,
    ok_ratio: f32,
    window: usize,
    last_checked: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct Config {
    interval: Duration,
    timeout: Duration,
    degraded_latency_ms: u128,
    window_size: usize,
    targets: Vec<String>,
    links: Vec<LinkConfig>,
    mqtt: Option<MqttConfig>,
    burst_count: usize,
    burst_interval: Duration,
}

impl Config {
    fn from_env() -> Self {
        let interval = env::var("INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let timeout = env::var("REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5000);
        let degraded_latency = env::var("DEGRADED_LATENCY_MS")
            .ok()
            .and_then(|v| v.parse::<u128>().ok())
            .unwrap_or(750);
        let window_size = env::var("WINDOW_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10);
        let targets: Vec<String> = env::var("TARGETS_JSON")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| vec![
                "http://www.google.com/generate_204".to_string(),
                "http://cp.cloudflare.com/generate_204".to_string(),
            ]);

        let links: Vec<LinkConfig> = env::var("LINKS_JSON")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| vec![LinkConfig::default()]);

        let mqtt = MqttConfig::from_env();
        let burst_count = env::var("BURST_COUNT").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        let burst_interval = env::var("BURST_INTERVAL_MS").ok().and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis).unwrap_or(Duration::from_millis(5000));

        Self {
            interval: Duration::from_secs(interval),
            timeout: Duration::from_millis(timeout),
            degraded_latency_ms: degraded_latency,
            window_size,
            targets,
            links,
            mqtt,
            burst_count,
            burst_interval,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ProbeWindow {
    outcomes: VecDeque<(bool, Option<u128>)>,
    capacity: usize,
}

impl ProbeWindow {
    fn new(capacity: usize) -> Self {
        Self { outcomes: VecDeque::with_capacity(capacity), capacity }
    }

    fn push(&mut self, ok: bool, latency_ms: Option<u128>) {
        if self.outcomes.len() == self.capacity {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back((ok, latency_ms));
    }

    fn ok_ratio(&self) -> f32 {
        if self.outcomes.is_empty() {
            return 0.0;
        }
        let ok = self.outcomes.iter().filter(|(ok, _)| *ok).count();
        ok as f32 / self.outcomes.len() as f32
    }

    fn last_latency(&self) -> Option<u128> {
        self.outcomes.back().and_then(|(_, l)| *l)
    }

    fn len(&self) -> usize { self.outcomes.len() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LinkConfig {
    name: String,
    socks5: Option<String>,
    #[serde(default)]
    weights: Option<Weights>,
    #[serde(default)]
    latency_baseline_ms: Option<u128>,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self { name: "default".to_string(), socks5: None, weights: None, latency_baseline_ms: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Weights {
    availability: Option<f32>, // default 60
    latency: Option<f32>,      // default 30
    jitter: Option<f32>,       // default 10
}

#[derive(Debug, Clone, Default)]
struct LinkState {
    window: ProbeWindow,
    last_checked: DateTime<Utc>,
    last_p95: Option<u128>,
    score: u8,
    status: InternetStatus,
}

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    // per-link state
    links: Arc<RwLock<std::collections::HashMap<String, LinkState>>>,
    mqtt: Option<MqttPublisher>,
}

async fn status_handler(Path(name): Path<String>, State(state): State<Arc<AppState>>) -> Result<Json<StatusPayload>, axum::http::StatusCode> {
    let map = state.links.read();
    let ls = match map.get(&name) { Some(v) => v, None => return Err(axum::http::StatusCode::NOT_FOUND) };
    let payload = StatusPayload {
        status: ls.status,
        score: ls.score,
        latency_ms: ls.window.last_latency(),
        p95_ms: ls.last_p95,
        ok_ratio: ls.window.ok_ratio(),
        window: state.cfg.window_size,
        last_checked: ls.last_checked,
    };
    Ok(Json(payload))
}

#[derive(Debug, Clone, Serialize)]
struct ScoreboardEntry {
    name: String,
    status: InternetStatus,
    score: u8,
    p95_ms: Option<u128>,
    ok_ratio: f32,
    last_checked: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
struct ScoreboardPayload {
    links: Vec<ScoreboardEntry>,
    recommended: Option<String>,
}

async fn scoreboard_handler(State(state): State<Arc<AppState>>) -> Json<ScoreboardPayload> {
    let map = state.links.read();
    let mut entries: Vec<ScoreboardEntry> = map
        .iter()
        .map(|(name, s)| ScoreboardEntry {
            name: name.clone(),
            status: s.status,
            score: s.score,
            p95_ms: s.last_p95,
            ok_ratio: s.window.ok_ratio(),
            last_checked: s.last_checked,
        })
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.score));
    let recommended = entries.first().map(|e| e.name.clone());
    Json(ScoreboardPayload { links: entries, recommended })
}

async fn health_handler() -> &'static str { "ok" }

fn eval_status(cfg: &Config, window: &ProbeWindow) -> InternetStatus {
    if window.len() == 0 {
        return InternetStatus::Failed; // conservative until first probe
    }
    let ratio = window.ok_ratio();
    let last_latency = window.last_latency();

    if ratio == 0.0 {
        return InternetStatus::Failed;
    }
    if ratio < 0.8 {
        return InternetStatus::Degraded;
    }
    if let Some(lat) = last_latency {
        if lat > cfg.degraded_latency_ms {
            return InternetStatus::Degraded;
        }
    }
    InternetStatus::Normal
}

fn p95(latencies: &[u128]) -> Option<u128> {
    if latencies.is_empty() { return None; }
    let mut v = latencies.to_vec();
    v.sort_unstable();
    let idx = ((v.len() as f64) * 0.95).ceil() as usize - 1;
    v.get(idx).copied()
}

fn stddev(latencies: &[u128]) -> Option<f64> {
    if latencies.len() < 2 { return None; }
    let mean = latencies.iter().map(|x| *x as f64).sum::<f64>() / (latencies.len() as f64);
    let var = latencies.iter().map(|x| {
        let d = (*x as f64) - mean;
        d*d
    }).sum::<f64>() / ((latencies.len() - 1) as f64);
    Some(var.sqrt())
}

fn score_link(cfg: &Config, link_cfg: &LinkConfig, window: &ProbeWindow, lat_hist: &[u128]) -> (u8, InternetStatus, Option<u128>) {
    let weights = link_cfg.weights.clone().unwrap_or_default();
    let w_avail = weights.availability.unwrap_or(60.0);
    let w_lat = weights.latency.unwrap_or(30.0);
    let w_jit = weights.jitter.unwrap_or(10.0);
    let total_w = w_avail + w_lat + w_jit;

    let avail = window.ok_ratio().clamp(0.0, 1.0);
    let p95v = p95(lat_hist);
    let baseline = link_cfg.latency_baseline_ms.unwrap_or(cfg.degraded_latency_ms);
    let lat_score = match p95v {
        Some(p) => {
            if p <= baseline { 1.0 } else { (baseline as f64 / p as f64) as f32 }
        }
        None => 0.0,
    };
    let jitter = stddev(lat_hist).unwrap_or(0.0);
    let jitter_score = if let Some(p) = p95v { (1.0 - (jitter / (p as f64)).min(1.0)) as f32 } else { 0.0 };

    let composite = (avail * w_avail + lat_score * w_lat + jitter_score * w_jit) / total_w;
    let score = (composite * 100.0).round().clamp(0.0, 100.0) as u8;

    let status = if score >= 80 { InternetStatus::Normal } else if score >= 40 { InternetStatus::Degraded } else { InternetStatus::Failed };
    (score, status, p95v)
}

#[derive(Debug, Clone)]
struct MqttConfig {
    enabled: bool,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    base_topic: String,
    discovery: bool,
    qos: QoS,
    retain: bool,
}

impl MqttConfig {
    fn from_env() -> Option<Self> {
        let host = env::var("MQTT_HOST").ok()?;
        let port = env::var("MQTT_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(1883);
        let username = env::var("MQTT_USERNAME").ok();
        let password = env::var("MQTT_PASSWORD").ok();
        let base_topic = env::var("MQTT_BASE_TOPIC").unwrap_or_else(|_| "home/internet_health".to_string());
        let discovery = env::var("MQTT_DISCOVERY").ok().map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(true);
        let qos_u8 = env::var("MQTT_QOS").ok().and_then(|v| v.parse::<u8>().ok()).unwrap_or(1);
        let qos = match qos_u8 { 2 => QoS::ExactlyOnce, 1 => QoS::AtLeastOnce, _ => QoS::AtMostOnce };
        let retain = env::var("MQTT_RETAIN").ok().map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(true);
        Some(Self { enabled: true, host, port, username, password, base_topic, discovery, qos, retain })
    }
}

#[derive(Clone)]
struct MqttPublisher {
    client: AsyncClient,
    base_topic: String,
    qos: QoS,
    retain: bool,
}

impl MqttPublisher {
    async fn new(cfg: &MqttConfig, device_id: &str) -> (Self, EventLoop) {
        let mut opts = MqttOptions::new(device_id, cfg.host.clone(), cfg.port);
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) { opts.set_credentials(u.clone(), p.clone()); }
        opts.set_keep_alive(Duration::from_secs(30));
        let avail_topic = format!("{}/availability", cfg.base_topic);
        opts.set_last_will(rumqttc::LastWill::new(avail_topic.clone(), "offline", cfg.qos, cfg.retain));

        let (client, eventloop) = AsyncClient::new(opts, 10);
        let me = Self { client, base_topic: cfg.base_topic.clone(), qos: cfg.qos, retain: cfg.retain };
        // publish online
        me.publish_str(&format!("{}/availability", &me.base_topic), "online").await.ok();
        (me, eventloop)
    }

    async fn publish_json<T: Serialize>(&self, topic: &str, value: &T) -> Result<(), rumqttc::ClientError> {
        let payload = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        self.client.publish(topic, self.qos, self.retain, payload).await
    }

    async fn publish_str(&self, topic: &str, value: &str) -> Result<(), rumqttc::ClientError> {
        self.client.publish(topic, self.qos, self.retain, value.as_bytes()).await
    }

    async fn publish_discovery_for_link(&self, link: &str) {
        // Home Assistant MQTT Discovery topics
        let dev_id = "internet_health_checker";
        let node = "internet_health";
        let status_obj = format!("{}_{}_status", node, link);
        let score_obj = format!("{}_{}_score", node, link);
        let avail_t = format!("{}/availability", self.base_topic);
        let state_t = format!("{}/{}/state", self.base_topic, link);

        let device = serde_json::json!({
            "identifiers": [dev_id],
            "name": "Internet Health Checker",
            "manufacturer": "Gnattu",
            "model": "internet-health-checker",
        });

        let status_cfg = serde_json::json!({
            "name": format!("Internet {} Status", link),
            "uniq_id": status_obj,
            "stat_t": state_t,
            "val_tpl": "{{ value_json.status }}",
            "avty_t": avail_t,
            "json_attr_t": state_t,
            "device": device,
        });

        let score_cfg = serde_json::json!({
            "name": format!("Internet {} Score", link),
            "uniq_id": score_obj,
            "stat_t": state_t,
            "val_tpl": "{{ value_json.score }}",
            "avty_t": avail_t,
            "device": device,
            "unit_of_meas": "",
        });

        let status_topic = format!("homeassistant/sensor/{}/config", status_obj);
        let score_topic = format!("homeassistant/sensor/{}/config", score_obj);

        let _ = self.publish_json(&status_topic, &status_cfg).await;
        let _ = self.publish_json(&score_topic, &score_cfg).await;
    }
}

#[tokio::main]
async fn main() {
    // Logging
    let filter = env::var("RUST_LOG").unwrap_or_else(|_| "info,axum=info,reqwest=warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cfg = Arc::new(Config::from_env());
    info!(
        interval=?cfg.interval,
        timeout_ms=?cfg.timeout.as_millis(),
        degraded_latency_ms=?cfg.degraded_latency_ms,
        window_size=?cfg.window_size,
        targets=?cfg.targets,
        links=?cfg.links.iter().map(|l| &l.name).collect::<Vec<_>>(),
        "starting internet-health-checker"
    );
    // MQTT setup
    let mqtt_pub = if let Some(mq) = &cfg.mqtt { 
        use rand::{distr::Alphanumeric, Rng};
        let suffix: String = rand::rng().sample_iter(&Alphanumeric).take(6).map(char::from).collect();
        let client_id = format!("internet-health-{}", suffix);
        let (pubr, mut ev) = MqttPublisher::new(mq, &client_id).await;
        // drive event loop
        tokio::spawn(async move {
            loop {
                if let Err(e) = ev.poll().await { warn!(error=%e, "mqtt eventloop error"); time::sleep(Duration::from_secs(1)).await; }
            }
        });
        Some(pubr)
    } else { None };

    let links_map = Arc::new(RwLock::new(std::collections::HashMap::<String, LinkState>::new()));

    // Spawn a prober per link
    for link_cfg in cfg.links.clone() {
        let cfg2 = cfg.clone();
        let links_map2 = links_map.clone();
        let mqtt_clone = mqtt_pub.clone();
        tokio::spawn(async move {
            // Build reqwest client with optional SOCKS5 proxy
            let mut builder = reqwest::Client::builder().timeout(cfg2.timeout);
            if let Some(socks) = &link_cfg.socks5 {
                match reqwest::Proxy::all(socks) {
                    Ok(proxy) => { builder = builder.proxy(proxy); }
                    Err(e) => warn!(link=%link_cfg.name, error=%e, "invalid socks5 proxy, proceeding without proxy"),
                }
            }
            let client = builder.build().expect("reqwest client");

            let mut ticker = time::interval(cfg2.interval);
            // keep a vector of successful latencies for p95/jitter
            let mut lat_hist: Vec<u128> = Vec::new();
            let name = link_cfg.name.clone();
            {
                let mut m = links_map2.write();
                m.insert(name.clone(), LinkState { window: ProbeWindow::new(cfg2.window_size), last_checked: Utc::now(), last_p95: None, score: 0, status: InternetStatus::Failed });
            }
            // Publish discovery config once per link
            if let (Some(p), Some(mq)) = (&mqtt_clone, &cfg2.mqtt) { if mq.discovery { p.publish_discovery_for_link(&name).await; } }
            loop {
                ticker.tick().await;
                let mut any_ok = false;
                let mut last_latency: Option<u128> = None;
                for target in &cfg2.targets {
                    let started = time::Instant::now();
                    match client.get(target).send().await {
                        Ok(resp) => {
                            let elapsed = started.elapsed().as_millis();
                            let st = resp.status();
                            if st == HttpStatusCode::NO_CONTENT { any_ok = true; last_latency = Some(elapsed); lat_hist.push(elapsed); }
                        }
                        Err(err) => {
                            warn!(link=%name, target=%target, error=%err, "probe error");
                        }
                    }
                }

                // Rapid recheck burst on failure
                if !any_ok && cfg2.burst_count > 0 {
                    for _ in 0..cfg2.burst_count {
                        time::sleep(cfg2.burst_interval).await;
                        for target in &cfg2.targets {
                            let started = time::Instant::now();
                            if let Ok(resp) = client.get(target).send().await {
                                let elapsed = started.elapsed().as_millis();
                                if resp.status() == HttpStatusCode::NO_CONTENT { any_ok = true; last_latency = Some(elapsed); lat_hist.push(elapsed); }
                            }
                        }
                        if any_ok { break; }
                    }
                }
                let mut to_publish: Option<(String, StatusPayload)> = None;
                let mut best_snapshot: Option<(String, u8)> = None;
                {
                    let mut map = links_map2.write();
                    if let Some(state) = map.get_mut(&name) {
                        state.window.push(any_ok, last_latency);
                        state.last_checked = Utc::now();
                        // Keep last K latencies to a reasonable cap (e.g., 500)
                        if lat_hist.len() > 500 { let drop = lat_hist.len() - 500; lat_hist.drain(0..drop); }
                        let (score, status, p95v) = score_link(&cfg2, &link_cfg, &state.window, &lat_hist);
                        state.score = score;
                        state.status = status;
                        state.last_p95 = p95v;

                        if mqtt_clone.is_some() {
                            let topic = format!("{}/{}/state", mqtt_clone.as_ref().unwrap().base_topic, name);
                            let payload = StatusPayload { status: state.status, score: state.score, latency_ms: state.window.last_latency(), p95_ms: state.last_p95, ok_ratio: state.window.ok_ratio(), window: cfg2.window_size, last_checked: state.last_checked };
                            to_publish = Some((topic, payload));
                            // compute recommended (best score) at this moment (copy minimal snapshot to avoid holding borrow across iteration)
                            best_snapshot = map.iter().map(|(n, s)| (n.clone(), s.score)).max_by_key(|(_, sc)| *sc);
                        }
                    }
                }
                if let Some(p) = &mqtt_clone {
                    if let Some((topic, payload)) = to_publish { let _ = p.publish_json(&topic, &payload).await; }
                    if let Some((best, _)) = best_snapshot { let _ = p.publish_str(&format!("{}/recommended", p.base_topic), &best).await; }
                }
            }
        });
    }

    // HTTP server
    let app_state = Arc::new(AppState { cfg: cfg.clone(), links: links_map, mqtt: mqtt_pub.clone() });
    let app = Router::new()
        .route("/status/{:name}", get(status_handler))
        .route("/scoreboard", get(scoreboard_handler))
        .route("/healthz", get(health_handler))
        .with_state(app_state.clone());

    let port: u16 = env::var("PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind listener");
    let server = axum::serve(listener, app);

    // graceful shutdown
    let with_shutdown = server.with_graceful_shutdown(async {
        let _ = signal::ctrl_c().await;
        info!("shutdown signal received");
    });

    if let Err(e) = with_shutdown.await {
        error!(error=%e, "server error");
    }
}
