//! Panel web server: embedded page and JSON API.

use crate::auth::{Auth, Login};
use crate::config::{parse_request, parse_schedule};
use crate::tasks::{TaskError, TaskManager};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use serde_json::json;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const INDEX: &str = include_str!("../static/index.html");
const LOGIN: &str = include_str!("../static/login.html");
const SCRIPT: &str = include_str!("../static/app.js");
const STYLE: &str = include_str!("../static/style.css");
const ICON: &str = include_str!("../static/favicon.svg");

const BODY_LIMIT: usize = 1024 * 1024;
const IMPORT_LIMIT: usize = 16 * 1024 * 1024;

type Reply = Response<Full<Bytes>>;

pub async fn serve(listener: TcpListener, manager: Arc<TaskManager>, auth: Arc<Auth>, shutdown: CancellationToken) {
    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("接受连接失败：{error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let (manager, auth) = (manager.clone(), auth.clone());
        let ip = peer.ip().to_canonical();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let (manager, auth) = (manager.clone(), auth.clone());
                async move { Ok::<_, Infallible>(handle(request, &manager, &auth, ip).await) }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
            let _ = builder.serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

fn json_reply(status: StatusCode, value: &impl Serialize) -> Reply {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    respond(status, "application/json; charset=utf-8", body)
}

fn error_reply(status: StatusCode, message: impl Into<String>) -> Reply {
    json_reply(status, &json!({ "error": message.into() }))
}

fn board_reply(manager: &TaskManager, auth: &Auth) -> Reply {
    let mut board = manager.board();
    board.auth = auth.enabled();
    json_reply(StatusCode::OK, &board)
}

fn with_cookie(mut reply: Reply, cookie: &str) -> Reply {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        reply.headers_mut().insert(header::SET_COOKIE, value);
    }
    reply
}

fn task_error(error: TaskError) -> Reply {
    let status = match error {
        TaskError::NotFound => StatusCode::NOT_FOUND,
        TaskError::Invalid(_) => StatusCode::BAD_REQUEST,
        TaskError::Conflict(_) => StatusCode::CONFLICT,
        TaskError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_reply(status, error.to_string())
}

/// Reject state-changing requests sent by another site's page in the user's
/// browser. Non-browser clients (curl, scripts) send neither header and pass.
fn cross_site(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return !matches!(site.as_bytes(), b"same-origin" | b"none");
    }
    let Some(origin) = headers.get(header::ORIGIN) else { return false };
    let origin_host = origin.to_str().ok().and_then(|o| o.split_once("://")).map(|(_, host)| host);
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    origin_host.is_none() || origin_host != host
}

#[allow(clippy::result_large_err)] // the error is the ready-made reply, returned once
async fn read_body(body: Incoming, limit: usize) -> Result<Bytes, Reply> {
    let collected = tokio::time::timeout(Duration::from_secs(60), Limited::new(body, limit).collect()).await;
    match collected {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(error)) if error.downcast_ref::<http_body_util::LengthLimitError>().is_some() => {
            Err(error_reply(StatusCode::PAYLOAD_TOO_LARGE, "请求内容过大"))
        }
        _ => Err(error_reply(StatusCode::BAD_REQUEST, "读取请求内容失败")),
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, for export file names.
fn timestamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

async fn handle(request: Request<Incoming>, manager: &Arc<TaskManager>, auth: &Auth, ip: IpAddr) -> Reply {
    let (parts, body) = request.into_parts();
    let method = parts.method.as_str();
    let path = parts.uri.path();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let public =
        matches!(segments.as_slice(), ["style.css"] | ["favicon.svg"] | ["healthz"] | ["api", "login" | "logout"]);
    let signed_in = auth.allowed(&parts.headers, ip);

    if matches!(method, "GET" | "HEAD") {
        if !signed_in && !public {
            return match segments.as_slice() {
                [""] => respond(StatusCode::OK, "text/html; charset=utf-8", LOGIN),
                _ => error_reply(StatusCode::UNAUTHORIZED, "请先登录"),
            };
        }
        return match segments.as_slice() {
            [""] => respond(StatusCode::OK, "text/html; charset=utf-8", INDEX),
            ["app.js"] => respond(StatusCode::OK, "text/javascript; charset=utf-8", SCRIPT),
            ["style.css"] => respond(StatusCode::OK, "text/css; charset=utf-8", STYLE),
            ["favicon.svg"] => respond(StatusCode::OK, "image/svg+xml", ICON),
            ["healthz"] => json_reply(StatusCode::OK, &json!({"ok": true})),
            ["api", "state"] => board_reply(manager, auth),
            ["api", "export"] => {
                let mut reply = respond(StatusCode::OK, "application/json; charset=utf-8", manager.export());
                let name =
                    format!("attachment; filename=\"flow-panel-tasks-{}.json\"", timestamp(crate::tasks::now() as u64));
                if let Ok(value) = HeaderValue::from_str(&name) {
                    reply.headers_mut().insert(header::CONTENT_DISPOSITION, value);
                }
                reply
            }
            _ => error_reply(StatusCode::NOT_FOUND, "页面不存在"),
        };
    }
    if !matches!(method, "POST" | "PUT" | "DELETE") {
        return error_reply(StatusCode::METHOD_NOT_ALLOWED, "不支持的请求方法");
    }
    if cross_site(&parts.headers) {
        return error_reply(StatusCode::FORBIDDEN, "已拒绝跨站请求");
    }
    if method == "POST" && segments.as_slice() == ["api", "logout"] {
        return with_cookie(json_reply(StatusCode::OK, &json!({"ok": true})), &Auth::logout_cookie());
    }
    if !signed_in && !public {
        return error_reply(StatusCode::UNAUTHORIZED, "请先登录");
    }
    let is_import = method == "POST" && segments.as_slice() == ["api", "import"];
    let body = match read_body(body, if is_import { IMPORT_LIMIT } else { BODY_LIMIT }).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let result = match (method, segments.as_slice()) {
        ("POST", ["api", "login"]) => return login(&parts.headers, &body, auth, ip).await,
        ("POST", ["api", "tasks"]) => {
            parse_request(&body).map_err(TaskError::Invalid).and_then(|request| manager.add(request)).map(|_| ())
        }
        ("POST", ["api", "tasks", "start-all"]) => manager.start_all(),
        ("POST", ["api", "tasks", "stop-all"]) => manager.stop_all(),
        ("PUT", ["api", "schedule"]) => serde_json::from_slice(&body)
            .map_err(|_| "请求内容不是有效的 JSON".to_owned())
            .and_then(|value| parse_schedule(&value))
            .map_err(TaskError::Invalid)
            .and_then(|schedule| manager.set_schedule(schedule)),
        ("PUT", ["api", "tasks", id]) => {
            parse_request(&body).map_err(TaskError::Invalid).and_then(|request| manager.update(id, request))
        }
        ("DELETE", ["api", "tasks", id]) => manager.delete(id),
        ("POST", ["api", "tasks", id, "start"]) => manager.start(id),
        ("POST", ["api", "tasks", id, "stop"]) => manager.stop(id),
        ("POST", ["api", "import"]) => {
            let replace = parts.uri.query().is_some_and(|q| q.split('&').any(|pair| pair == "mode=replace"));
            manager.import(&body, replace).map(|_| ())
        }
        _ => return error_reply(StatusCode::NOT_FOUND, "接口不存在"),
    };
    match result {
        Ok(()) => board_reply(manager, auth),
        Err(error) => task_error(error),
    }
}

async fn login(headers: &HeaderMap, body: &[u8], auth: &Auth, ip: IpAddr) -> Reply {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let password = value.get("password").and_then(|p| p.as_str()).unwrap_or_default();
    match auth.login(ip, password) {
        Login::Ok => {
            let https = headers.get("x-forwarded-proto").is_some_and(|proto| proto.as_bytes() == b"https");
            with_cookie(json_reply(StatusCode::OK, &json!({"ok": true})), &auth.session_cookie(https))
        }
        Login::Wrong => {
            eprintln!("登录失败：{ip} 密码错误");
            tokio::time::sleep(auth.fail_delay).await;
            error_reply(StatusCode::UNAUTHORIZED, "密码错误")
        }
        Login::Locked(minutes) => {
            error_reply(StatusCode::TOO_MANY_REQUESTS, format!("密码错误次数过多，请 {minutes} 分钟后再试"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::test_engine;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn panel() -> (u16, Arc<TaskManager>) {
        panel_with(Auth::open()).await
    }

    async fn panel_with(auth: Auth) -> (u16, Arc<TaskManager>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let manager = TaskManager::memory(test_engine());
        tokio::spawn(serve(listener, manager.clone(), Arc::new(auth), CancellationToken::new()));
        (port, manager)
    }

    /// Minimal HTTP/1.1 client: returns (status, headers, body).
    async fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        stream.write_all(format!("{head}\r\n{body}").as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head.to_owned(), body.to_owned())
    }

    #[tokio::test]
    async fn serves_embedded_page_and_assets() {
        let (port, _) = panel().await;
        for path in ["/", "/app.js", "/style.css", "/favicon.svg", "/healthz", "/api/state"] {
            let (status, _, body) = call(port, "GET", path, &[], "").await;
            assert_eq!(status, 200, "{path}");
            assert!(!body.is_empty(), "{path}");
        }
        assert!(call(port, "GET", "/", &[], "").await.2.contains("Flow Panel"));
        assert_eq!(call(port, "GET", "/missing", &[], "").await.0, 404);
    }

    #[tokio::test]
    async fn api_workflow() {
        let (port, _) = panel().await;
        let json = [("Content-Type", "application/json")];
        let create = r#"{"urls":"http://127.0.0.1:9/a\nhttp://127.0.0.1:9/b","repeat":0}"#;
        let (status, _, body) = call(port, "POST", "/api/tasks", &json, create).await;
        assert_eq!(status, 200, "{body}");
        let board: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(board["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(board["tasks"][0]["busy"], true);
        assert_eq!(call(port, "DELETE", "/api/tasks/task-1", &json, "").await.0, 409);
        assert_eq!(call(port, "POST", "/api/tasks/stop-all", &json, "{}").await.0, 200);
        assert_eq!(call(port, "POST", "/api/tasks/task-1/start", &json, "{}").await.0, 200);
        assert_eq!(call(port, "POST", "/api/tasks/task-1/stop", &json, "{}").await.0, 200);
        assert_eq!(call(port, "POST", "/api/tasks/start-all", &json, "{}").await.0, 200);
        assert_eq!(call(port, "POST", "/api/tasks/stop-all", &json, "{}").await.0, 200);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let edit = r#"{"urls":["http://127.0.0.1:9/c"],"name":"改名","max_gib":1.5}"#;
        let (status, _, body) = call(port, "PUT", "/api/tasks/task-1", &json, edit).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("改名") && body.contains("\"max_gib\":1.5"));
        assert_eq!(call(port, "DELETE", "/api/tasks/task-2", &json, "").await.0, 200);
        assert_eq!(call(port, "DELETE", "/api/tasks/task-2", &json, "").await.0, 404);
        assert_eq!(call(port, "POST", "/api/tasks", &json, r#"{"urls":"ftp://x"}"#).await.0, 400);
        let (status, _, body) = call(port, "PUT", "/api/schedule", &json, r#"{"enabled":true,"hours":[0,1]}"#).await;
        assert_eq!(status, 200, "{body}");
        let board: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(board["schedule"], serde_json::json!({"enabled": true, "hours": [0, 1]}));
        assert_eq!(call(port, "PUT", "/api/schedule", &json, r#"{"enabled":true,"hours":[]}"#).await.0, 400);

        let (status, head, exported) = call(port, "GET", "/api/export", &[], "").await;
        assert_eq!(status, 200);
        assert!(head.to_ascii_lowercase().contains("content-disposition: attachment; filename=\"flow-panel-tasks-"));
        let (status, _, body) = call(port, "POST", "/api/import?mode=replace", &json, &exported).await;
        assert_eq!(status, 200, "{body}");
        let board: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(board["tasks"][0]["name"], "改名");
        assert_eq!(board["tasks"][0]["id"], "task-3");
        assert_eq!(call(port, "POST", "/api/import", &json, "not json").await.0, 400);
    }

    #[tokio::test]
    async fn rejects_cross_site_writes() {
        let (port, manager) = panel().await;
        let body = r#"{"urls":"http://127.0.0.1:9/a"}"#;
        let evil = [("Origin", "http://evil.example"), ("Content-Type", "text/plain")];
        assert_eq!(call(port, "POST", "/api/tasks", &evil, body).await.0, 403);
        let fetch = [("Sec-Fetch-Site", "cross-site")];
        assert_eq!(call(port, "POST", "/api/tasks", &fetch, body).await.0, 403);
        assert_eq!(manager.board().tasks.len(), 0);
        let origin = format!("http://127.0.0.1:{port}");
        let same = [("Origin", origin.as_str()), ("Sec-Fetch-Site", "same-origin")];
        assert_eq!(call(port, "POST", "/api/tasks", &same, body).await.0, 200);
        manager.stop_all().unwrap();
    }

    #[tokio::test]
    async fn password_protects_the_panel() {
        let mut auth = Auth::new("open sesame", &[3; 32]);
        auth.fail_delay = Duration::ZERO;
        let (port, manager) = panel_with(auth).await;
        let json = [("Content-Type", "application/json")];
        let (status, _, page) = call(port, "GET", "/", &[], "").await;
        assert!(status == 200 && page.contains("id=\"login\""), "login page not shown");
        for path in ["/app.js", "/api/state", "/api/export"] {
            assert_eq!(call(port, "GET", path, &[], "").await.0, 401, "{path}");
        }
        for path in ["/style.css", "/favicon.svg", "/healthz"] {
            assert_eq!(call(port, "GET", path, &[], "").await.0, 200, "{path}");
        }
        let body = r#"{"urls":"http://127.0.0.1:9/a"}"#;
        assert_eq!(call(port, "POST", "/api/tasks", &json, body).await.0, 401);
        assert_eq!(call(port, "PUT", "/api/schedule", &json, r#"{"enabled":false}"#).await.0, 401);
        assert_eq!(manager.board().tasks.len(), 0);

        assert_eq!(call(port, "POST", "/api/login", &json, r#"{"password":"nope"}"#).await.0, 401);
        let (status, head, _) = call(port, "POST", "/api/login", &json, r#"{"password":"open sesame"}"#).await;
        assert_eq!(status, 200);
        let cookie = head
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: ").or_else(|| line.strip_prefix("Set-Cookie: ")))
            .expect("no session cookie")
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(head.contains("HttpOnly; SameSite=Strict"));
        let signed_in = [("Cookie", cookie.as_str()), ("Content-Type", "application/json")];
        let (status, _, page) = call(port, "GET", "/", &signed_in, "").await;
        assert!(status == 200 && page.contains("task-body"));
        let (status, _, state) = call(port, "GET", "/api/state", &signed_in, "").await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&state).unwrap()["auth"], true);
        assert_eq!(call(port, "POST", "/api/tasks", &signed_in, body).await.0, 200);
        let bearer = [("Authorization", "Bearer open sesame")];
        assert_eq!(call(port, "GET", "/api/state", &bearer, "").await.0, 200);
        let (_, head, _) = call(port, "POST", "/api/logout", &[], "").await;
        assert!(head.contains("Max-Age=0"));
        manager.stop_all().unwrap();

        // Repeated wrong passwords lock the address out, even for the right one.
        for _ in 0..5 {
            call(port, "POST", "/api/login", &json, r#"{"password":"guess"}"#).await;
        }
        assert_eq!(call(port, "POST", "/api/login", &json, r#"{"password":"open sesame"}"#).await.0, 429);
    }

    #[test]
    fn export_timestamps() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(951_868_800), "20000301-000000");
        assert_eq!(timestamp(1_709_210_096), "20240229-123456");
    }
}
