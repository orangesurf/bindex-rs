use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

const RECENT_LIMIT: usize = 200;
const LATENCY_LIMIT: usize = 512;

pub struct Monitor {
    path: PathBuf,
    started_at: Instant,
    started_unix: u64,
    state: Mutex<MonitorState>,
}

#[derive(Default)]
struct MonitorState {
    active: BTreeMap<String, ClientMetrics>,
    methods: BTreeMap<String, MethodMetrics>,
    recent: VecDeque<RequestSample>,
    total_sessions: u64,
    total_requests: u64,
    total_errors: u64,
}

#[derive(Default, Serialize)]
struct ClientMetrics {
    connected_at: u64,
    last_seen_at: u64,
    requests: u64,
    errors: u64,
    last_method: Option<String>,
    last_error: Option<String>,
}

#[derive(Default)]
struct MethodMetrics {
    count: u64,
    errors: u64,
    total_ms: f64,
    max_ms: f64,
    samples_ms: VecDeque<f64>,
}

struct RequestSample {
    unix: u64,
    peer: String,
    method: String,
    ok: bool,
    duration_ms: f64,
    error: Option<String>,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    ok: bool,
    started_at: u64,
    updated_at: u64,
    uptime_secs: u64,
    active_sessions: usize,
    total_sessions: u64,
    total_requests: u64,
    total_errors: u64,
    methods: Vec<MethodSnapshot<'a>>,
    clients: Vec<ClientSnapshot<'a>>,
    recent: Vec<RequestSnapshot<'a>>,
}

#[derive(Serialize)]
struct MethodSnapshot<'a> {
    method: &'a str,
    count: u64,
    errors: u64,
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
struct ClientSnapshot<'a> {
    peer: &'a str,
    connected_at: u64,
    last_seen_at: u64,
    requests: u64,
    errors: u64,
    last_method: Option<&'a str>,
    last_error: Option<&'a str>,
}

#[derive(Serialize)]
struct RequestSnapshot<'a> {
    time: u64,
    peer: &'a str,
    method: &'a str,
    ok: bool,
    duration_ms: f64,
    error: Option<&'a str>,
}

pub struct SessionGuard<'a> {
    monitor: &'a Monitor,
    peer: String,
}

impl Monitor {
    pub fn new(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let monitor = Self {
            path,
            started_at: Instant::now(),
            started_unix: unix_now(),
            state: Mutex::new(MonitorState::default()),
        };
        monitor.write_snapshot()?;
        Ok(monitor)
    }

    pub fn session(&self, peer: impl Into<String>) -> SessionGuard<'_> {
        let peer = peer.into();
        {
            let mut state = self.state.lock().expect("monitor lock");
            let now = unix_now();
            state.total_sessions += 1;
            state.active.insert(
                peer.clone(),
                ClientMetrics {
                    connected_at: now,
                    last_seen_at: now,
                    ..ClientMetrics::default()
                },
            );
        }
        self.write_snapshot_best_effort();
        SessionGuard {
            monitor: self,
            peer,
        }
    }

    pub fn record_request(
        &self,
        peer: &str,
        method: impl Into<String>,
        ok: bool,
        duration: Duration,
        error: Option<String>,
    ) {
        let method = method.into();
        let duration_ms = duration.as_secs_f64() * 1000.0;
        {
            let mut state = self.state.lock().expect("monitor lock");
            state.total_requests += 1;
            if !ok {
                state.total_errors += 1;
            }

            let metrics = state.methods.entry(method.clone()).or_default();
            metrics.count += 1;
            if !ok {
                metrics.errors += 1;
            }
            metrics.total_ms += duration_ms;
            metrics.max_ms = metrics.max_ms.max(duration_ms);
            metrics.samples_ms.push_back(duration_ms);
            while metrics.samples_ms.len() > LATENCY_LIMIT {
                metrics.samples_ms.pop_front();
            }

            if let Some(client) = state.active.get_mut(peer) {
                client.last_seen_at = unix_now();
                client.requests += 1;
                if !ok {
                    client.errors += 1;
                }
                client.last_method = Some(method.clone());
                client.last_error = error.clone();
            }

            state.recent.push_front(RequestSample {
                unix: unix_now(),
                peer: peer.to_string(),
                method,
                ok,
                duration_ms,
                error,
            });
            while state.recent.len() > RECENT_LIMIT {
                state.recent.pop_back();
            }
        }
        self.write_snapshot_best_effort();
    }

    pub fn reset(&self) {
        {
            let mut state = self.state.lock().expect("monitor lock");
            state.methods.clear();
            state.recent.clear();
            state.total_sessions = state.active.len() as u64;
            state.total_requests = 0;
            state.total_errors = 0;
            for client in state.active.values_mut() {
                client.requests = 0;
                client.errors = 0;
                client.last_method = None;
                client.last_error = None;
                client.last_seen_at = unix_now();
            }
        }
        self.write_snapshot_best_effort();
    }

    fn end_session(&self, peer: &str) {
        self.state.lock().expect("monitor lock").active.remove(peer);
        self.write_snapshot_best_effort();
    }

    fn write_snapshot_best_effort(&self) {
        if let Err(err) = self.write_snapshot() {
            log::warn!("failed to write electrum monitor snapshot: {err}");
        }
    }

    fn write_snapshot(&self) -> anyhow::Result<()> {
        let state = self.state.lock().expect("monitor lock");
        let snapshot = self.snapshot(&state);
        let bytes = serde_json::to_vec(&snapshot)?;
        write_atomic(&self.path, &bytes)?;
        Ok(())
    }

    fn snapshot<'a>(&'a self, state: &'a MonitorState) -> Snapshot<'a> {
        let mut methods = state
            .methods
            .iter()
            .map(|(method, metrics)| {
                let mut samples = metrics.samples_ms.iter().copied().collect::<Vec<_>>();
                samples.sort_by(|a, b| a.total_cmp(b));
                MethodSnapshot {
                    method,
                    count: metrics.count,
                    errors: metrics.errors,
                    avg_ms: if metrics.count == 0 {
                        0.0
                    } else {
                        metrics.total_ms / metrics.count as f64
                    },
                    p50_ms: percentile(&samples, 0.50),
                    p95_ms: percentile(&samples, 0.95),
                    max_ms: metrics.max_ms,
                }
            })
            .collect::<Vec<_>>();
        methods.sort_by(|left, right| right.count.cmp(&left.count));

        let clients = state
            .active
            .iter()
            .map(|(peer, client)| ClientSnapshot {
                peer,
                connected_at: client.connected_at,
                last_seen_at: client.last_seen_at,
                requests: client.requests,
                errors: client.errors,
                last_method: client.last_method.as_deref(),
                last_error: client.last_error.as_deref(),
            })
            .collect::<Vec<_>>();

        let recent = state
            .recent
            .iter()
            .map(|item| RequestSnapshot {
                time: item.unix,
                peer: &item.peer,
                method: &item.method,
                ok: item.ok,
                duration_ms: item.duration_ms,
                error: item.error.as_deref(),
            })
            .collect::<Vec<_>>();

        Snapshot {
            ok: true,
            started_at: self.started_unix,
            updated_at: unix_now(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            active_sessions: state.active.len(),
            total_sessions: state.total_sessions,
            total_requests: state.total_requests,
            total_errors: state.total_errors,
            methods,
            clients,
            recent,
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.monitor.end_session(&self.peer);
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("json")
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
