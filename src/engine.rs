//! Download engine: one `Run` per started task, with `concurrency` workers that
//! repeatedly download the task URL and discard the body.

use crate::config::{Config, host_key, normalize_url};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{ACCEPT_ENCODING, CACHE_CONTROL, CONNECTION, HOST, LOCATION, USER_AGENT};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, timeout};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

pub const AGENT: &str = concat!("FlowPanel/", env!("CARGO_PKG_VERSION"));
/// Fixed per-connection read buffer. Bounds memory to roughly this much per
/// active download (plus TLS buffers), independent of file size.
const READ_BUFFER: usize = 32 * 1024;
const SPEED_SAMPLES: usize = 5;
const MAX_REDIRECTS: u32 = 5;
const MAX_ERROR_CHARS: usize = 240;

/// Process-wide download context.
pub struct Engine {
    tls: Arc<ClientConfig>,
}

impl Engine {
    pub fn new(tls: Arc<ClientConfig>) -> Arc<Engine> {
        Arc::new(Engine { tls })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Stopping,
    Stopped,
    Completed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Stopping => "stopping",
            Phase::Stopped => "stopped",
            Phase::Completed => "completed",
        }
    }
}

/// What the task list shows for a run.
pub struct RunView {
    pub phase: Phase,
    pub speed: f64,
    pub active: u32,
    pub last_error: String,
    pub server_ip: String,
    pub connection_host: String,
    pub ip_overridden: bool,
}

/// Counters used by tests and logs; not exposed in the API.
#[derive(Debug)]
#[allow(dead_code)]
pub struct RunStats {
    pub phase: Phase,
    pub reason: String,
    pub total: u64,
    pub success: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub elapsed: Duration,
    pub server_ip: String,
    pub connection_host: String,
    pub ip_overridden: bool,
}

pub struct Run {
    config: Config,
    server_ips: Vec<IpAddr>,
    engine: Arc<Engine>,
    cancel: CancellationToken,
    done: CancellationToken,
    started: Instant,
    state: Mutex<State>,
}

struct State {
    phase: Phase,
    reason: String,
    attempts: u64,
    active: u32,
    success: u64,
    failed: u64,
    cancelled: u64,
    total: u64,
    last_error: String,
    consecutive_errors: u32,
    ready_at: Option<Instant>,
    next_ip: usize,
    next_read: Option<Instant>,
    speeds: VecDeque<f64>,
    server_ip: String,
    connection_host: String,
    ip_overridden: bool,
    ended: Option<Instant>,
}

enum Claim {
    Go,
    Wait(Duration),
    Done,
}

fn seconds(value: f64) -> Duration {
    Duration::from_secs_f64(value.max(0.0))
}

/// Aborts the spawned HTTP connection task when the download is dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Run {
    /// Start downloading in the background. Must be called inside a Tokio runtime.
    pub fn start(engine: Arc<Engine>, config: Config) -> Arc<Run> {
        let server_ips = config.settings.server_ips.iter().filter_map(|ip| ip.parse().ok()).collect();
        let run = Arc::new(Run {
            config,
            server_ips,
            engine,
            cancel: CancellationToken::new(),
            done: CancellationToken::new(),
            started: Instant::now(),
            state: Mutex::new(State {
                phase: Phase::Running,
                reason: "正在下载".into(),
                attempts: 0,
                active: 0,
                success: 0,
                failed: 0,
                cancelled: 0,
                total: 0,
                last_error: String::new(),
                consecutive_errors: 0,
                ready_at: None,
                next_ip: 0,
                next_read: None,
                speeds: VecDeque::with_capacity(SPEED_SAMPLES),
                server_ip: String::new(),
                connection_host: String::new(),
                ip_overridden: false,
                ended: None,
            }),
        });
        tokio::spawn(run.clone().supervise());
        run
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn stop(&self, reason: &str) {
        {
            let mut state = self.lock();
            if state.phase != Phase::Running {
                return;
            }
            state.phase = Phase::Stopping;
            state.reason = reason.to_owned();
        }
        self.cancel.cancel();
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase
    }

    /// Cancelled once the run has fully ended.
    pub fn done_token(&self) -> CancellationToken {
        self.done.clone()
    }

    #[cfg(test)]
    pub async fn wait(&self) {
        self.done.cancelled().await;
    }

    pub fn view(&self) -> RunView {
        let state = self.lock();
        let speed = if matches!(state.phase, Phase::Running | Phase::Stopping) && !state.speeds.is_empty() {
            state.speeds.iter().sum::<f64>() / state.speeds.len() as f64
        } else {
            0.0
        };
        RunView {
            phase: state.phase,
            speed,
            active: state.active,
            last_error: state.last_error.clone(),
            server_ip: state.server_ip.clone(),
            connection_host: state.connection_host.clone(),
            ip_overridden: state.ip_overridden,
        }
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> RunStats {
        let state = self.lock();
        RunStats {
            phase: state.phase,
            reason: state.reason.clone(),
            total: state.total,
            success: state.success,
            failed: state.failed,
            cancelled: state.cancelled,
            elapsed: state.ended.unwrap_or_else(Instant::now) - self.started,
            server_ip: state.server_ip.clone(),
            connection_host: state.connection_host.clone(),
            ip_overridden: state.ip_overridden,
        }
    }

    async fn supervise(self: Arc<Self>) {
        let mut workers = JoinSet::new();
        for _ in 0..self.config.settings.concurrency {
            workers.spawn(self.clone().worker());
        }
        let period = Duration::from_secs(1);
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let duration = self.config.settings.duration;
        let deadline = tokio::time::sleep(seconds(duration));
        tokio::pin!(deadline);
        let mut deadline_armed = duration > 0.0;
        let (mut previous_time, mut previous_total) = (Instant::now(), 0u64);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let mut state = self.lock();
                    let elapsed = now.duration_since(previous_time).as_secs_f64();
                    if elapsed > 0.0 {
                        if state.speeds.len() == SPEED_SAMPLES {
                            state.speeds.pop_front();
                        }
                        let speed = (state.total - previous_total) as f64 / elapsed;
                        state.speeds.push_back(speed);
                    }
                    previous_time = now;
                    previous_total = state.total;
                }
                _ = &mut deadline, if deadline_armed => {
                    deadline_armed = false;
                    self.stop("已达到运行时长上限");
                }
                joined = workers.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
            }
        }
        self.cancel.cancel();
        {
            let mut state = self.lock();
            state.ended = Some(Instant::now());
            if state.phase == Phase::Stopping {
                state.phase = Phase::Stopped;
            } else {
                state.phase = Phase::Completed;
                state.reason =
                    if state.failed > 0 { "下载已结束，部分请求失败" } else { "所有下载已结束" }.into();
            }
        }
        self.done.cancel();
    }

    fn claim(&self) -> Claim {
        let mut state = self.lock();
        if self.cancel.is_cancelled() {
            return Claim::Done;
        }
        let repeat = self.config.settings.repeat;
        if repeat > 0 && state.attempts >= repeat {
            return Claim::Done;
        }
        if let Some(ready_at) = state.ready_at {
            let now = Instant::now();
            if ready_at > now {
                return Claim::Wait(ready_at - now);
            }
        }
        state.attempts += 1;
        state.active += 1;
        Claim::Go
    }

    fn finish(&self, result: Result<(), String>) {
        let mut state = self.lock();
        state.active -= 1;
        if self.cancel.is_cancelled() {
            state.cancelled += 1;
            return;
        }
        match result {
            Ok(()) => {
                state.success += 1;
                state.last_error.clear();
                state.consecutive_errors = 0;
                state.ready_at = None;
            }
            Err(error) => {
                state.failed += 1;
                state.last_error = error.chars().take(MAX_ERROR_CHARS).collect();
                state.consecutive_errors += 1;
                // Back off 1, 2, 4 ... 30 s so a dead URL does not spin.
                let delay = (1u64 << (state.consecutive_errors - 1).min(5)).min(30);
                state.ready_at = Some(Instant::now() + Duration::from_secs(delay));
            }
        }
    }

    /// Sleep unless cancelled first; returns false when cancelled.
    async fn pause(&self, duration: Duration) -> bool {
        if duration.is_zero() {
            return !self.cancel.is_cancelled();
        }
        tokio::select! {
            _ = tokio::time::sleep(duration) => true,
            _ = self.cancel.cancelled() => false,
        }
    }

    async fn worker(self: Arc<Self>) {
        let interval = seconds(self.config.settings.interval);
        loop {
            match self.claim() {
                Claim::Done => return,
                Claim::Wait(duration) => {
                    if !self.pause(duration).await {
                        return;
                    }
                    continue;
                }
                Claim::Go => {}
            }
            let result = tokio::select! {
                result = self.download() => result,
                _ = self.cancel.cancelled() => Err("已取消".into()),
            };
            self.finish(result);
            if !self.pause(interval).await {
                return;
            }
        }
    }

    async fn download(&self) -> Result<(), String> {
        let limit = seconds(self.config.settings.timeout);
        let mut url = Url::parse(&self.config.url).map_err(|e| e.to_string())?;
        // One IP per download, chosen in list order; redirects to the same host keep it.
        let route = (!self.server_ips.is_empty()).then(|| {
            let mut state = self.lock();
            let ip = self.server_ips[state.next_ip % self.server_ips.len()];
            state.next_ip = (state.next_ip + 1) % self.server_ips.len();
            (host_key(&url), ip)
        });
        let mut redirects = 0;
        loop {
            let key = host_key(&url);
            let pinned = route.as_ref().filter(|(host, _)| *host == key).map(|(_, ip)| *ip);
            let (response, peer, _connection) = self.request(&url, pinned, limit).await?;
            {
                let mut state = self.lock();
                state.connection_host = key;
                state.server_ip = peer;
                state.ip_overridden = pinned.is_some();
            }
            let status = response.status();
            let location = response.headers().get(LOCATION);
            if let (301 | 302 | 303 | 307 | 308, Some(location)) = (status.as_u16(), location) {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(format!("重定向超过 {MAX_REDIRECTS} 次"));
                }
                let location = location.to_str().map_err(|_| "重定向地址无效")?;
                let next = url.join(location).map_err(|_| "重定向地址无效")?;
                url = Url::parse(&normalize_url(next.as_str())?).map_err(|e| e.to_string())?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            return self.consume(response.into_body(), limit).await;
        }
    }

    async fn request(
        &self,
        url: &Url,
        pinned: Option<IpAddr>,
        limit: Duration,
    ) -> Result<(Response<Incoming>, String, AbortOnDrop), String> {
        let port = url.port_or_known_default().ok_or("链接缺少端口")?;
        let addresses: Vec<SocketAddr> = match (pinned, url.host()) {
            (Some(ip), _) => vec![SocketAddr::new(ip, port)],
            (None, Some(Host::Ipv4(ip))) => vec![SocketAddr::new(ip.into(), port)],
            (None, Some(Host::Ipv6(ip))) => vec![SocketAddr::new(ip.into(), port)],
            (None, Some(Host::Domain(domain))) => match timeout(limit, tokio::net::lookup_host((domain, port))).await {
                Err(_) => return Err(format!("域名解析超时：{domain}")),
                Ok(Err(error)) => return Err(format!("域名解析失败：{domain}（{error}）")),
                Ok(Ok(found)) => found.collect(),
            },
            (None, None) => return Err("链接缺少域名".into()),
        };
        let mut failure = String::from("没有可用的服务器地址");
        let mut connected = None;
        for address in addresses {
            match timeout(limit, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => {
                    connected = Some((stream, address));
                    break;
                }
                Ok(Err(error)) => failure = format!("连接 {address} 失败：{error}"),
                Err(_) => failure = format!("连接 {address} 超时"),
            }
        }
        let (stream, address) = connected.ok_or(failure)?;
        let _ = stream.set_nodelay(true);
        let host = url.host_str().unwrap_or_default();
        let host_header = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let target = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let request = Request::get(if target.is_empty() { "/" } else { target })
            .header(HOST, host_header)
            .header(USER_AGENT, AGENT)
            .header(ACCEPT_ENCODING, "identity")
            .header(CACHE_CONTROL, "no-cache")
            .header(CONNECTION, "close")
            .body(Empty::<Bytes>::new())
            .map_err(|e| e.to_string())?;
        let (response, connection) = if url.scheme() == "https" {
            let name = match url.host() {
                Some(Host::Domain(domain)) => ServerName::try_from(domain.trim_end_matches('.').to_owned())
                    .map_err(|_| format!("域名无法用于 TLS：{domain}"))?,
                Some(Host::Ipv4(ip)) => ServerName::IpAddress(IpAddr::from(ip).into()),
                Some(Host::Ipv6(ip)) => ServerName::IpAddress(IpAddr::from(ip).into()),
                None => return Err("链接缺少域名".into()),
            };
            let tls = timeout(limit, TlsConnector::from(self.engine.tls.clone()).connect(name, stream))
                .await
                .map_err(|_| "TLS 握手超时".to_owned())?
                .map_err(|e| format!("TLS 握手失败：{e}"))?;
            send(TokioIo::new(tls), request, limit).await?
        } else {
            send(TokioIo::new(stream), request, limit).await?
        };
        Ok((response, address.ip().to_string(), connection))
    }

    async fn consume(&self, mut body: Incoming, limit: Duration) -> Result<(), String> {
        let rate = self.config.settings.rate_mib * 1_048_576.0;
        let quota = self.config.settings.max_gib * 1_073_741_824.0;
        loop {
            let frame = match timeout(limit, body.frame()).await {
                Err(_) => return Err("读取超时".into()),
                Ok(None) => return Ok(()),
                Ok(Some(Err(error))) => return Err(format!("读取失败：{error}")),
                Ok(Some(Ok(frame))) => frame,
            };
            let Ok(data) = frame.into_data() else { continue };
            let read = data.len() as u64;
            drop(data);
            let (wake, reached) = {
                let mut state = self.lock();
                state.total += read;
                let reached = quota > 0.0 && state.total as f64 >= quota;
                // Charge the bytes actually read against a schedule shared by all
                // workers of this task, then wait until that slot has passed.
                let wake = (rate > 0.0).then(|| {
                    let now = Instant::now();
                    let start = state.next_read.filter(|at| *at > now).unwrap_or(now);
                    let next = start + Duration::from_secs_f64(read as f64 / rate);
                    state.next_read = Some(next);
                    next
                });
                (wake, reached)
            };
            if reached {
                self.stop("已达到累计流量上限");
            }
            if let Some(wake) = wake {
                tokio::time::sleep_until(wake.into()).await;
            }
        }
    }
}

async fn send<T>(
    io: T,
    request: Request<Empty<Bytes>>,
    limit: Duration,
) -> Result<(Response<Incoming>, AbortOnDrop), String>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let mut builder = hyper::client::conn::http1::Builder::new();
    builder.read_buf_exact_size(Some(READ_BUFFER));
    let (mut sender, connection) = timeout(limit, builder.handshake(io))
        .await
        .map_err(|_| "连接超时".to_owned())?
        .map_err(|e| format!("连接失败：{e}"))?;
    let guard = AbortOnDrop(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let response = timeout(limit, sender.send_request(request))
        .await
        .map_err(|_| "等待响应超时".to_owned())?
        .map_err(|e| format!("请求失败：{e}"))?;
    Ok((response, guard))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{TaskRequest, parse_request};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// How the test origin answers a path.
    #[derive(Clone)]
    pub enum Reply {
        /// 64 KiB in 8 KiB writes, optional delay between writes, optional chunked encoding.
        File {
            delay: Duration,
            chunked: bool,
        },
        Status(u16),
        Redirect(String),
        /// Headers promise 64 KiB, then nothing is sent.
        Hang,
        /// Headers promise 128 KiB, 64 KiB is sent, then the connection closes.
        Truncated,
        Text(&'static str),
    }

    pub const FILE: u64 = 65536;

    pub struct Origin {
        pub port: u16,
        pub hits: Arc<AtomicUsize>,
        pub peak: Arc<AtomicUsize>,
        pub hosts: Arc<Mutex<Vec<String>>>,
    }

    pub fn default_reply(path: &str) -> Reply {
        match path {
            "/slow" => Reply::File { delay: Duration::from_millis(15), chunked: false },
            "/stream" => Reply::File { delay: Duration::ZERO, chunked: true },
            "/error" => Reply::Status(503),
            "/redirect" => Reply::Redirect("/file".into()),
            "/loop" => Reply::Redirect("/loop".into()),
            "/hang" => Reply::Hang,
            "/truncated" => Reply::Truncated,
            _ => Reply::File { delay: Duration::ZERO, chunked: false },
        }
    }

    pub async fn origin(bind: &str, route: fn(&str, &str) -> Reply) -> Origin {
        try_origin(bind, route).await.expect("bind test origin")
    }

    pub async fn try_origin(bind: &str, route: fn(&str, &str) -> Reply) -> Option<Origin> {
        let listener = TcpListener::bind(bind).await.ok()?;
        let origin = Origin {
            port: listener.local_addr().unwrap().port(),
            hits: Arc::default(),
            peak: Arc::default(),
            hosts: Arc::default(),
        };
        let (hits, peak, hosts) = (origin.hits.clone(), origin.peak.clone(), origin.hosts.clone());
        let active = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (hits, peak, hosts, active) = (hits.clone(), peak.clone(), hosts.clone(), active.clone());
                tokio::spawn(async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // `serve` returns once the response is written, before the client
                    // has necessarily read it, so a new request never overlaps this count.
                    serve(stream, route, &hits, &hosts).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Some(origin)
    }

    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        route: fn(&str, &str) -> Reply,
        hits: &AtomicUsize,
        hosts: &Mutex<Vec<String>>,
    ) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte).await {
                Ok(1) => head.push(byte[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
        let host = head
            .lines()
            .find_map(|line| line.strip_prefix("host: ").or_else(|| line.strip_prefix("Host: ")))
            .unwrap_or_default()
            .to_owned();
        hits.fetch_add(1, Ordering::SeqCst);
        hosts.lock().unwrap().push(host.clone());
        let block = vec![b'x'; 8192];
        match route(&path, &host) {
            Reply::File { delay, chunked } => {
                let header = if chunked { "Transfer-Encoding: chunked" } else { "Content-Length: 65536" };
                let _ = stream
                    .write_all(format!("HTTP/1.1 200 OK\r\n{header}\r\nConnection: close\r\n\r\n").as_bytes())
                    .await;
                for index in 0..8 {
                    if index > 0 && !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    if chunked {
                        let _ = stream.write_all(b"2000\r\n").await;
                    }
                    if stream.write_all(&block).await.is_err() {
                        return;
                    }
                    if chunked {
                        let _ = stream.write_all(b"\r\n").await;
                    }
                    let _ = stream.flush().await;
                }
                if chunked {
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                }
                // Return as soon as the body is written; dropping the stream closes it.
                return;
            }
            Reply::Status(code) => {
                let _ = stream
                    .write_all(
                        format!("HTTP/1.1 {code} Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await;
            }
            Reply::Redirect(location) => {
                let _ = stream
                    .write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
                    .await;
            }
            Reply::Hang => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\n\r\n").await;
                let _ = stream.flush().await;
                let mut sink = [0u8; 64];
                let _ = stream.read(&mut sink).await;
            }
            Reply::Truncated => {
                let _ =
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 131072\r\nConnection: close\r\n\r\n").await;
                for _ in 0..8 {
                    let _ = stream.write_all(&block).await;
                }
            }
            Reply::Text(text) => {
                let _ = stream
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())
                            .as_bytes(),
                    )
                    .await;
            }
        }
        let _ = stream.shutdown().await;
    }

    pub fn test_engine() -> Arc<Engine> {
        Engine::new(crate::tls::with_roots(rustls::RootCertStore::empty()))
    }

    pub fn request(urls: &[String], extra: Value) -> TaskRequest {
        let mut body = json!({"urls": urls, "interval": 0, "concurrency": 4});
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        parse_request(body.to_string().as_bytes()).unwrap()
    }

    pub fn config(url: &str, extra: Value) -> Config {
        let request = request(&[url.to_owned()], extra);
        Config { url: request.urls[0].clone(), settings: request.settings }
    }

    async fn run_to_end(engine: Arc<Engine>, config: Config) -> RunStats {
        let run = Run::start(engine, config);
        if timeout(Duration::from_secs(8), run.wait()).await.is_err() {
            run.stop("test timeout");
            panic!("download timed out: {:?}", run.stats());
        }
        run.stats()
    }

    async fn run_url(url: String, extra: Value) -> RunStats {
        run_to_end(test_engine(), config(&url, extra)).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeat_and_concurrency() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let stats = run_url(format!("http://127.0.0.1:{}/slow", o.port), json!({"repeat": 6})).await;
        assert_eq!((stats.success, stats.total, stats.phase), (6, 6 * FILE, Phase::Completed));
        let peak = o.peak.load(Ordering::SeqCst);
        assert!((2..=4).contains(&peak), "peak {peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_repeat_not_multiplied_by_concurrency() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let stats = run_url(format!("http://127.0.0.1:{}/file", o.port), json!({"concurrency": 64})).await;
        assert_eq!(stats.success, 1);
        assert_eq!(o.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_infinite_and_blocked_downloads() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        for path in ["/slow", "/hang"] {
            let run =
                Run::start(test_engine(), config(&format!("http://127.0.0.1:{}{path}", o.port), json!({"repeat": 0})));
            tokio::time::sleep(Duration::from_millis(150)).await;
            run.stop("手动停止");
            timeout(Duration::from_secs(1), run.wait()).await.expect("stop timed out");
            let stats = run.stats();
            assert_eq!(stats.phase, Phase::Stopped);
            assert!(stats.cancelled >= 1, "{stats:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duration_and_traffic_limits() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let url = format!("http://127.0.0.1:{}/slow", o.port);
        let stats = run_url(url.clone(), json!({"repeat": 0, "duration": 0.2})).await;
        assert!(stats.reason.contains("时长") && stats.phase == Phase::Stopped, "{stats:?}");
        let stats = run_url(url, json!({"repeat": 0, "max_gib": 0.0001})).await;
        assert!(stats.reason.contains("流量") && stats.phase == Phase::Stopped, "{stats:?}");
        assert!(stats.total <= 107_375 + 4 * READ_BUFFER as u64, "quota overshoot {}", stats.total);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rate_limit_is_reached_but_not_exceeded() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        // 4 x 64 KiB at 0.25 MiB/s should take about one second.
        let stats = run_url(format!("http://127.0.0.1:{}/file", o.port), json!({"repeat": 4, "rate_mib": 0.25})).await;
        assert_eq!(stats.success, 4);
        let elapsed = stats.elapsed.as_secs_f64();
        assert!((0.7..1.6).contains(&elapsed), "elapsed {elapsed}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_handling() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        for (path, success, failed, total) in [
            ("/redirect", 1, 0, FILE),
            ("/stream", 1, 0, FILE),
            ("/error", 0, 1, 0),
            ("/truncated", 0, 1, FILE),
            ("/loop", 0, 1, 0),
        ] {
            let stats = run_url(format!("http://127.0.0.1:{}{path}", o.port), json!({})).await;
            assert_eq!((stats.success, stats.failed, stats.total), (success, failed, total), "{path}: {stats:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_read_timeout() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let stats = run_url(format!("http://127.0.0.1:{}/hang", o.port), json!({"timeout": 1})).await;
        assert_eq!(stats.failed, 1);
        assert!(stats.elapsed < Duration::from_millis(2500), "{stats:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failures_back_off() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let stats = run_url(
            format!("http://127.0.0.1:{}/error", o.port),
            json!({"repeat": 0, "duration": 0.5, "concurrency": 1}),
        )
        .await;
        assert_eq!(stats.failed, 1, "{stats:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ip_rotation_in_order_and_redirects_keep_ip() {
        let o = origin("127.0.0.1:0", |path, host| {
            if path == "/redirect" {
                let port = host.rsplit(':').next().unwrap();
                Reply::Redirect(format!("http://DOWNLOAD.TEST.:{port}/file"))
            } else {
                Reply::Text("ok")
            }
        })
        .await;
        let url = format!("http://download.test:{}/redirect", o.port);
        let stats = run_url(url, json!({"server_ips": "127.0.0.1", "concurrency": 1})).await;
        assert_eq!(stats.success, 1);
        assert_eq!(
            (stats.server_ip.as_str(), stats.connection_host.as_str(), stats.ip_overridden),
            ("127.0.0.1", "download.test", true)
        );
        let hosts = o.hosts.lock().unwrap().clone();
        assert_eq!(hosts, [format!("download.test:{}", o.port), format!("download.test:{}", o.port)]);

        // Two loopback addresses on one port, used in order: .1, .2, .1, .2, .1
        // (skipped where 127.0.0.2 is not routable, e.g. macOS).
        let a = origin("127.0.0.1:0", |_, _| Reply::Text("a")).await;
        let Some(b) = try_origin(&format!("127.0.0.2:{}", a.port), |_, _| Reply::Text("b")).await else { return };
        let url = format!("http://rotate.test:{}/file", a.port);
        let stats =
            run_url(url, json!({"server_ips": ["127.0.0.1", "127.0.0.2"], "concurrency": 1, "repeat": 5})).await;
        assert_eq!(stats.success, 5);
        assert_eq!((a.hits.load(Ordering::SeqCst), b.hits.load(Ordering::SeqCst)), (3, 2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_ipv6() {
        let Some(o) = try_origin("[::1]:0", |_, _| Reply::Text("ipv6")).await else { return };
        let stats = run_url(format!("http://download.test:{}/file", o.port), json!({"server_ips": "[::1]"})).await;
        assert_eq!((stats.success, stats.server_ip.as_str()), (1, "::1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normal_dns_without_ip_list() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let stats = run_url(format!("http://localhost:{}/file", o.port), json!({})).await;
        assert_eq!(stats.success, 1);
        assert!(!stats.ip_overridden);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_tls_keeps_sni_and_verifies_certificate() {
        use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["download.test".to_owned()]).unwrap().signed_by(&leaf_key, &ca).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (acceptor, record) = (acceptor.clone(), record.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(stream).await else { return };
                    let sni = tls.get_ref().1.server_name().unwrap_or_default().to_owned();
                    let hosts = Mutex::new(Vec::new());
                    serve(tls, |_, _| Reply::Text("tls"), &AtomicUsize::new(0), &hosts).await;
                    let host = hosts.lock().unwrap().first().cloned().unwrap_or_default();
                    record.lock().unwrap().push((sni, host));
                });
            }
        });

        let mut trusted = rustls::RootCertStore::empty();
        trusted.add(ca.der().clone()).unwrap();
        for (domain, roots, success) in [
            ("download.test", trusted.clone(), 1),
            ("wrong.test", trusted, 0),
            ("download.test", rustls::RootCertStore::empty(), 0),
        ] {
            seen.lock().unwrap().clear();
            let engine = Engine::new(crate::tls::with_roots(roots));
            let url = format!("https://{domain}:{port}/file");
            let stats = run_to_end(engine, config(&url, json!({"server_ips": ["127.0.0.1"]}))).await;
            assert_eq!(stats.success, success, "{domain}: {stats:?}");
            if success == 1 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let seen = seen.lock().unwrap().clone();
                assert_eq!(seen, [(domain.to_owned(), format!("{domain}:{port}"))]);
            }
        }
    }
}
