//! Task configuration: request parsing, validation and URL normalization.

use serde::Serialize;
use serde_json::{Map, Value};
use std::net::IpAddr;
use url::{Host, Url};

pub const MAX_CONCURRENCY: u32 = 64;
pub const MAX_TASKS: usize = 500;
pub const MAX_SERVER_IPS: usize = 500;
pub const MAX_NAME_CHARS: usize = 80;
const MAX_URL_BYTES: usize = 8192;

/// Download settings shared by every task created from one request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Settings {
    /// Parallel download requests for this task (1–64).
    pub concurrency: u32,
    /// Request attempts in total, failures included. 0 repeats until stopped.
    pub repeat: u64,
    /// Pause in seconds after each attempt.
    pub interval: f64,
    /// Connect / response / idle-read timeout in seconds.
    pub timeout: f64,
    /// Aggregate rate limit for the task in MiB/s, 0 = unlimited.
    pub rate_mib: f64,
    /// Stop the task after this much traffic in GiB, 0 = unlimited.
    pub max_gib: f64,
    /// Stop the task after this many seconds, 0 = unlimited.
    pub duration: f64,
    /// Server IPs used in order for the URL's own host, empty = normal DNS.
    pub server_ips: Vec<String>,
    /// Reuse connections between downloads (HTTP keep-alive).
    pub keep_alive: bool,
    /// Hours of the day (0–23, panel time zone) in which the task runs; empty = all day.
    pub hours: Vec<u8>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            concurrency: 8,
            repeat: 1,
            interval: 0.0,
            timeout: 15.0,
            rate_mib: 0.0,
            max_gib: 0.0,
            duration: 0.0,
            server_ips: Vec::new(),
            keep_alive: true,
            hours: Vec::new(),
        }
    }
}

/// The complete configuration of one task.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Config {
    pub url: String,
    #[serde(flatten)]
    pub settings: Settings,
}

/// A create or update request: one task per URL.
#[derive(Debug)]
pub struct TaskRequest {
    pub urls: Vec<String>,
    pub name: String,
    pub settings: Settings,
}

pub fn parse_request(body: &[u8]) -> Result<TaskRequest, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "请求内容必须为 JSON 对象")?;
    let object = value.as_object().ok_or("请求内容必须为 JSON 对象")?;
    let urls = parse_urls(object.get("urls"))?;
    let name = parse_name(object.get("name"))?;
    let settings = parse_settings(object)?;
    Ok(TaskRequest { urls, name, settings })
}

/// Validate a stored or imported task configuration object.
pub fn parse_config(value: &Value) -> Result<Config, String> {
    let object = value.as_object().ok_or("任务配置必须为 JSON 对象")?;
    let url = match object.get("url") {
        Some(Value::String(url)) => normalize_url(url)?,
        _ => return Err("任务配置缺少下载链接".into()),
    };
    Ok(Config { url, settings: parse_settings(object)? })
}

fn parse_urls(value: Option<&Value>) -> Result<Vec<String>, String> {
    let lines: Vec<String> = match value {
        Some(Value::String(text)) => text.lines().map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or("下载链接格式错误"))
            .collect::<Result<_, _>>()?,
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err("下载链接格式错误".into()),
    };
    let lines: Vec<&str> = lines.iter().map(|line| line.trim()).filter(|line| !line.is_empty()).collect();
    if lines.is_empty() || lines.len() > MAX_TASKS {
        return Err(format!("请填写 1–{MAX_TASKS} 条下载链接"));
    }
    let mut urls: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let url = normalize_url(line)?;
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    Ok(urls)
}

pub fn parse_name(value: Option<&Value>) -> Result<String, String> {
    let name = match value {
        None | Some(Value::Null) => "",
        Some(Value::String(name)) => name.trim(),
        Some(_) => return Err("任务名称格式错误".into()),
    };
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("任务名称最多 {MAX_NAME_CHARS} 个字符"));
    }
    Ok(name.to_owned())
}

fn parse_settings(object: &Map<String, Value>) -> Result<Settings, String> {
    let d = Settings::default();
    let number = |key: &str, label: &str, min: f64, max: f64, default: f64, integral: bool| -> Result<f64, String> {
        let value = match object.get(key) {
            None => default,
            Some(Value::Number(number)) => number.as_f64().ok_or_else(|| format!("{label}必须为数字"))?,
            Some(_) => return Err(format!("{label}必须为数字")),
        };
        if !value.is_finite() || value < min || value > max || (integral && value.fract() != 0.0) {
            return Err(format!("{label}取值范围 {min}–{max}"));
        }
        Ok(value)
    };
    Ok(Settings {
        concurrency: number("concurrency", "并发数", 1.0, MAX_CONCURRENCY as f64, d.concurrency as f64, true)? as u32,
        repeat: number("repeat", "下载次数", 0.0, 1_000_000.0, d.repeat as f64, true)? as u64,
        interval: number("interval", "重复间隔", 0.0, 3600.0, d.interval, false)?,
        timeout: number("timeout", "超时", 1.0, 120.0, d.timeout, false)?,
        rate_mib: number("rate_mib", "限速", 0.0, 100_000.0, d.rate_mib, false)?,
        max_gib: number("max_gib", "流量上限", 0.0, 1_000_000.0, d.max_gib, false)?,
        duration: number("duration", "运行时长", 0.0, 31_536_000.0, d.duration, false)?,
        server_ips: parse_server_ips(object.get("server_ips"))?,
        keep_alive: match object.get("keep_alive") {
            None | Some(Value::Null) => d.keep_alive,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("keep_alive 必须为 true 或 false".into()),
        },
        hours: parse_hours(object.get("hours"))?,
    })
}

/// Hours 0–23, sorted and deduplicated. All 24 hours is stored as empty (all day).
pub fn parse_hours(value: Option<&Value>) -> Result<Vec<u8>, String> {
    const INVALID: &str = "运行时段必须是 0–23 的小时列表";
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(INVALID.into()),
    };
    let mut hours = [false; 24];
    for item in items {
        let hour = item.as_u64().filter(|h| *h < 24).ok_or(INVALID)?;
        hours[hour as usize] = true;
    }
    if hours.iter().all(|on| *on) {
        return Ok(Vec::new());
    }
    Ok((0..24u8).filter(|h| hours[*h as usize]).collect())
}

pub fn parse_server_ips(value: Option<&Value>) -> Result<Vec<String>, String> {
    let lines: Vec<String> = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(text)) => text.lines().map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("服务器 IP 必须是每行一个 IP 的文本或字符串数组")?,
        Some(_) => return Err("服务器 IP 必须是每行一个 IP 的文本或字符串数组".into()),
    };
    let mut result: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let mut text = line.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            text = inner;
        }
        let ip: IpAddr = text
            .parse()
            .map_err(|_| format!("第 {} 行 IP 格式错误：每行只填一个 IPv4 / IPv6 地址，不含域名或端口", index + 1))?;
        let ip = ip.to_string();
        if !result.contains(&ip) {
            result.push(ip);
        }
        if result.len() > MAX_SERVER_IPS {
            return Err(format!("每条任务最多 {MAX_SERVER_IPS} 个服务器 IP"));
        }
    }
    Ok(result)
}

pub fn normalize_url(raw: &str) -> Result<String, String> {
    const INVALID: &str = "请填写完整的 HTTP / HTTPS 链接，不包含账号密码";
    let raw = raw.trim();
    if raw.len() > MAX_URL_BYTES {
        return Err("下载链接过长".into());
    }
    if raw.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        return Err("下载链接包含控制字符".into());
    }
    let mut url = Url::parse(raw).map_err(|_| INVALID)?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return Err(INVALID.into());
    }
    let trimmed = match url.host() {
        Some(Host::Domain(domain)) => {
            let trimmed = domain.trim_end_matches('.');
            if trimmed.is_empty() {
                return Err(INVALID.into());
            }
            (trimmed.len() != domain.len()).then(|| trimmed.to_owned())
        }
        Some(_) => None,
        None => return Err(INVALID.into()),
    };
    if let Some(host) = trimmed {
        url.set_host(Some(&host)).map_err(|_| "域名格式错误")?;
    }
    if url.port() == Some(0) {
        return Err("端口范围为 1–65535".into());
    }
    url.set_fragment(None);
    Ok(url.into())
}

/// Lower-case host without brackets or trailing dot, used to match redirects.
pub fn host_key(url: &Url) -> String {
    match url.host() {
        Some(Host::Domain(domain)) => domain.trim_end_matches('.').to_ascii_lowercase(),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        None => String::new(),
    }
}

/// The file name of the URL, falling back to the host name.
pub fn default_task_name(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return truncate(raw, MAX_NAME_CHARS);
    };
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(|segment| percent_encoding::percent_decode_str(segment).decode_utf8_lossy().into_owned())
        .unwrap_or_default();
    let name = if segment.is_empty() { host_key(&url) } else { segment };
    truncate(&name, MAX_NAME_CHARS)
}

/// Names for a batch: "name · 1", "name · 2" ... within the length limit.
pub fn batch_names(name: &str, urls: &[String]) -> Vec<String> {
    urls.iter()
        .enumerate()
        .map(|(index, url)| match (name.is_empty(), urls.len()) {
            (true, _) => default_task_name(url),
            (false, 1) => name.to_owned(),
            (false, _) => {
                let suffix = format!(" · {}", index + 1);
                format!("{}{suffix}", truncate(name, MAX_NAME_CHARS - suffix.chars().count()))
            }
        })
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(value: Value) -> Result<TaskRequest, String> {
        parse_request(value.to_string().as_bytes())
    }

    #[test]
    fn rejects_invalid_requests() {
        for bad in [
            json!(null),
            json!([]),
            json!({"urls": []}),
            json!({"urls": "  \n "}),
            json!({"urls": ["file:///tmp/x"]}),
            json!({"urls": ["http://user:pass@a/"]}),
            json!({"urls": ["http://a:0/"]}),
            json!({"urls": ["http://./"]}),
            json!({"urls": ["http://a"], "concurrency": 65}),
            json!({"urls": ["http://a"], "repeat": 1.2}),
            json!({"urls": ["http://a"], "timeout": null}),
            json!({"urls": ["http://a"], "concurrency": true}),
            json!({"urls": ["http://a"], "interval": "1"}),
            json!({"urls": ["http://a"], "name": 3}),
            json!({"urls": ["http://a"], "name": "x".repeat(81)}),
        ] {
            assert!(request(bad.clone()).is_err(), "accepted {bad}");
        }
        assert!(normalize_url("http://a/\u{7}").is_err());
        assert!(normalize_url(&format!("http://a/{}", "x".repeat(MAX_URL_BYTES))).is_err());
    }

    #[test]
    fn normalizes_urls_and_defaults() {
        let parsed = request(json!({
            "urls": "http://下载.test/file#frag\nhttp://下载.test/file\n\nHTTPS://Example.COM.:8443",
            "server_ips": "2001:DB8::1\n[2001:db8::1]\n10.0.0.1",
        }))
        .unwrap();
        assert_eq!(parsed.urls, ["http://xn--ghq880n.test/file", "https://example.com:8443/"]);
        assert_eq!(parsed.settings.server_ips, ["2001:db8::1", "10.0.0.1"]);
        assert_eq!(parsed.settings.concurrency, 8);
        assert_eq!(parsed.settings.repeat, 1);
        assert_eq!(parsed.name, "");
    }

    #[test]
    fn rejects_invalid_server_ips() {
        for bad in ["a.test=127.0.0.1", "127.0.0.1:80", "fe80::1%eth0", "wrong", "10.0.0.1\nexample.com"] {
            assert!(parse_server_ips(Some(&json!(bad))).is_err(), "accepted {bad}");
        }
        assert!(parse_server_ips(Some(&json!([1]))).is_err());
        let many: Vec<String> = (0..=MAX_SERVER_IPS).map(|i| format!("10.0.{}.{}", i / 256, i % 256)).collect();
        assert!(parse_server_ips(Some(&json!(many))).is_err());
    }

    #[test]
    fn hours_and_keep_alive() {
        let parsed = request(json!({"urls": ["http://a/"], "hours": [23, 8, 8, 9], "keep_alive": false})).unwrap();
        assert_eq!(parsed.settings.hours, [8, 9, 23]);
        assert!(!parsed.settings.keep_alive);
        let all: Vec<u8> = (0..24).collect();
        assert!(request(json!({"urls": ["http://a/"], "hours": all})).unwrap().settings.hours.is_empty());
        let defaults = request(json!({"urls": ["http://a/"]})).unwrap().settings;
        assert!(defaults.keep_alive && defaults.hours.is_empty() && defaults.interval == 0.0);
        for bad in [json!([24]), json!([-1]), json!("8-9"), json!([1.5])] {
            assert!(request(json!({"urls": ["http://a/"], "hours": bad})).is_err(), "accepted {bad}");
        }
        assert!(request(json!({"urls": ["http://a/"], "keep_alive": 1})).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(default_task_name("http://a.test/dir/%E4%B8%AD.bin"), "中.bin");
        assert_eq!(default_task_name("http://a.test/dir/"), "dir");
        assert_eq!(default_task_name("http://A.test/"), "a.test");
        assert_eq!(default_task_name("http://[::1]:8080/"), "::1");
        let urls = vec!["http://a/1".to_owned(), "http://a/2".to_owned()];
        assert_eq!(batch_names("下载", &urls), ["下载 · 1", "下载 · 2"]);
        assert_eq!(batch_names("", &urls), ["1", "2"]);
        let long = "x".repeat(MAX_NAME_CHARS);
        assert!(batch_names(&long, &urls).iter().all(|n| n.chars().count() == MAX_NAME_CHARS));
    }

    #[test]
    fn stored_config_round_trip() {
        let config = Config {
            url: "http://a.test/file".into(),
            settings: Settings {
                repeat: 0,
                server_ips: vec!["10.0.0.1".into()],
                max_gib: 2.5,
                keep_alive: false,
                hours: vec![1, 2],
                ..Settings::default()
            },
        };
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(parse_config(&value).unwrap(), config);
        assert!(parse_config(&json!({"concurrency": 1})).is_err());
    }
}
