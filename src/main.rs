mod config;
mod engine;
mod http;
mod storage;
mod tasks;
mod tls;

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const USAGE: &str = "\
Flow Panel — 局域网下载任务面板

用法: flow-panel [选项]

选项（括号内为对应环境变量）:
  --host <地址>       监听地址，默认 0.0.0.0           (FLOW_PANEL_HOST)
  --port <端口>       监听端口，默认 8089              (FLOW_PANEL_PORT)
  --data-dir <目录>   任务配置目录，默认 ./data         (FLOW_PANEL_DATA_DIR)
  --threads <数量>    工作线程数，默认 CPU 核数，最多 4  (FLOW_PANEL_THREADS)
  --ca-file <文件>    额外信任的 CA 证书（PEM）         (FLOW_PANEL_CA_FILE)
  --healthcheck       检查本机面板是否正常，用于容器健康检查
  --version           显示版本
  --help              显示帮助
";

struct Options {
    host: String,
    port: u16,
    data_dir: PathBuf,
    threads: usize,
    ca_file: Option<PathBuf>,
    healthcheck: bool,
}

enum Command {
    Run(Options),
    Print(String),
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(4);
    let mut host = env("FLOW_PANEL_HOST").unwrap_or_else(|| "0.0.0.0".into());
    let mut port = env("FLOW_PANEL_PORT").unwrap_or_else(|| "8089".into());
    let mut data_dir = env("FLOW_PANEL_DATA_DIR").unwrap_or_else(|| "data".into());
    let mut threads = env("FLOW_PANEL_THREADS").unwrap_or_else(|| cores.to_string());
    let mut ca_file = env("FLOW_PANEL_CA_FILE");
    let mut healthcheck = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| args.next()).ok_or_else(|| format!("{flag} 需要一个值"));
        match flag.as_str() {
            "--host" => host = value()?,
            "--port" => port = value()?,
            "--data-dir" => data_dir = value()?,
            "--threads" => threads = value()?,
            "--ca-file" => ca_file = Some(value()?),
            "--healthcheck" => healthcheck = true,
            "--version" | "-V" => return Ok(Command::Print(format!("Flow Panel {}", env!("CARGO_PKG_VERSION")))),
            "--help" | "-h" => return Ok(Command::Print(USAGE.to_owned())),
            _ => return Err(format!("未知参数：{arg}\n\n{USAGE}")),
        }
    }
    let port: u16 = port.trim().parse().map_err(|_| format!("端口无效：{port}"))?;
    let threads: usize = threads.trim().parse().ok().filter(|n| (1..=64).contains(n)).ok_or("线程数应为 1–64")?;
    Ok(Command::Run(Options {
        host,
        port,
        data_dir: PathBuf::from(data_dir),
        threads,
        ca_file: ca_file.map(PathBuf::from),
        healthcheck,
    }))
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Print(text)) => {
            println!("{}", text.trim_end());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(options)) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if options.healthcheck {
        return healthcheck(&options);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(options.threads)
        .max_blocking_threads(16)
        .thread_name("flow-panel")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("启动运行时失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(options));
    runtime.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: Options) -> Result<(), String> {
    let tls = tls::client_config(options.ca_file.as_deref())?;
    let engine = engine::Engine::new(tls);
    let manager = tasks::TaskManager::load(engine, options.data_dir.join("tasks.json"))?;
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port))
        .await
        .map_err(|e| format!("监听 {}:{} 失败：{e}", options.host, options.port))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    println!("Flow Panel {} 已启动：http://{address}", env!("CARGO_PKG_VERSION"));
    if let Some(file) = manager.data_file() {
        println!("任务配置：{}（{} 条任务）", file.display(), manager.task_count());
    }
    println!(
        "工作线程 {}；TLS {}",
        options.threads,
        if tls::has_aes_hardware() {
            "AES-GCM 优先（CPU 支持 AES-NI）"
        } else {
            "ChaCha20 优先（CPU 无 AES-NI）"
        }
    );
    manager.resume();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(http::serve(listener, manager.clone(), shutdown.clone()));
    wait_for_signal().await;
    println!("正在停止…");
    shutdown.cancel();
    manager.close(Duration::from_secs(5)).await;
    let _ = server.await;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `GET /healthz` on the local listener; exit code 0 when it answers 200.
fn healthcheck(options: &Options) -> ExitCode {
    let ip = match options.host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Ok(IpAddr::V6(ip)) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        Ok(ip) => ip,
        Err(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let check = || -> std::io::Result<bool> {
        let timeout = Duration::from_secs(3);
        let mut stream = TcpStream::connect_timeout(&SocketAddr::new(ip, options.port), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head)?;
        Ok(head.ends_with(b" 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Options {
        match parse_args(args.iter().map(|s| s.to_string())).unwrap() {
            Command::Run(options) => options,
            Command::Print(_) => panic!("expected options"),
        }
    }

    #[test]
    fn arguments() {
        let defaults = options(&[]);
        assert_eq!(defaults.port, 8089);
        assert_eq!(defaults.data_dir, PathBuf::from("data"));
        let custom = options(&["--port", "9090", "--host=127.0.0.1", "--data-dir", "/data", "--threads", "2"]);
        assert_eq!((custom.port, custom.host.as_str(), custom.threads), (9090, "127.0.0.1", 2));
        assert!(parse_args(["--port", "70000"].iter().map(|s| s.to_string())).is_err());
        assert!(parse_args(["--bogus"].iter().map(|s| s.to_string())).is_err());
        assert!(parse_args(["--port"].iter().map(|s| s.to_string())).is_err());
    }
}
