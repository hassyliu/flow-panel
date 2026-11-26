//! Optional access password. The browser signs in once and keeps a signed
//! session cookie; scripts can send `Authorization: Bearer <password>`.
//!
//! Sessions are not stored: a cookie carries its expiry and an HMAC over it.
//! The HMAC key is derived from a random secret kept in the data directory and
//! from the password, so sessions survive a restart and changing the password
//! signs everyone out.

use hyper::header::{self, HeaderMap};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COOKIE: &str = "flow_panel_session";
const SESSION_SECONDS: u64 = 30 * 86_400;
const SECRET_BYTES: usize = 32;
/// Wrong passwords allowed per client address within `LOCKOUT`.
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
const MAX_TRACKED: usize = 4096;

struct Keys {
    /// Keys the password comparison, which is then constant-time.
    check: hmac::Key,
    password_tag: hmac::Tag,
    /// Signs session cookies.
    session: hmac::Key,
}

pub struct Auth {
    keys: Option<Keys>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    /// Pause before answering a wrong password.
    pub fail_delay: Duration,
}

pub enum Login {
    Ok,
    Wrong,
    /// Too many wrong passwords; minutes until the client may try again.
    Locked(u64),
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

impl Auth {
    /// No password: every request is allowed.
    pub fn open() -> Auth {
        Auth { keys: None, failures: Mutex::default(), fail_delay: Duration::from_secs(1) }
    }

    pub fn new(password: &str, secret: &[u8]) -> Auth {
        let check = hmac::Key::new(hmac::HMAC_SHA256, secret);
        let password_tag = hmac::sign(&check, password.as_bytes());
        let mut material = b"session\0".to_vec();
        material.extend_from_slice(password.as_bytes());
        let session = hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&check, &material).as_ref());
        Auth { keys: Some(Keys { check, password_tag, session }), ..Auth::open() }
    }

    pub fn enabled(&self) -> bool {
        self.keys.is_some()
    }

    fn failures(&self) -> MutexGuard<'_, HashMap<IpAddr, (u32, Instant)>> {
        self.failures.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn password_matches(&self, given: &str) -> bool {
        self.keys.as_ref().is_none_or(|k| hmac::verify(&k.check, given.as_bytes(), k.password_tag.as_ref()).is_ok())
    }

    fn locked_minutes(&self, ip: IpAddr) -> Option<u64> {
        let failures = self.failures();
        let (count, since) = failures.get(&ip)?;
        let left = LOCKOUT.checked_sub(since.elapsed())?;
        (*count >= MAX_FAILURES).then(|| left.as_secs().div_ceil(60).max(1))
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut failures = self.failures();
        if failures.len() >= MAX_TRACKED && !failures.contains_key(&ip) {
            failures.retain(|_, (_, since)| since.elapsed() < LOCKOUT);
            if failures.len() >= MAX_TRACKED {
                failures.clear();
            }
        }
        let entry = failures.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= LOCKOUT {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    /// Check a password typed into the login page.
    pub fn login(&self, ip: IpAddr, password: &str) -> Login {
        if let Some(minutes) = self.locked_minutes(ip) {
            return Login::Locked(minutes);
        }
        if self.password_matches(password) {
            self.failures().remove(&ip);
            Login::Ok
        } else {
            self.record_failure(ip);
            Login::Wrong
        }
    }

    fn sign(&self, expires: u64) -> Option<String> {
        let keys = self.keys.as_ref()?;
        let tag = hmac::sign(&keys.session, expires.to_string().as_bytes());
        Some(format!("{expires}.{}", hex(tag.as_ref())))
    }

    fn session_valid(&self, token: &str) -> bool {
        let Some(keys) = &self.keys else { return true };
        let Some((expires, tag)) = token.split_once('.') else { return false };
        let (Ok(seconds), Some(tag)) = (expires.parse::<u64>(), unhex(tag)) else { return false };
        seconds > unix_now() && hmac::verify(&keys.session, expires.as_bytes(), &tag).is_ok()
    }

    /// `Set-Cookie` value for a new session. `secure` when served over HTTPS.
    pub fn session_cookie(&self, secure: bool) -> String {
        let token = self.sign(unix_now() + SESSION_SECONDS).unwrap_or_default();
        let secure = if secure { "; Secure" } else { "" };
        format!("{COOKIE}={token}; Path=/; Max-Age={SESSION_SECONDS}; HttpOnly; SameSite=Strict{secure}")
    }

    pub fn logout_cookie() -> String {
        format!("{COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
    }

    /// Whether the request may use the panel: a valid session cookie or the
    /// password as a bearer token. A wrong bearer password counts as a failure.
    pub fn allowed(&self, headers: &HeaderMap, ip: IpAddr) -> bool {
        if self.keys.is_none() {
            return true;
        }
        let cookies = headers.get_all(header::COOKIE).into_iter().filter_map(|v| v.to_str().ok());
        let mut sessions = cookies.flat_map(|line| line.split(';')).filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then_some(value)
        });
        if sessions.any(|token| self.session_valid(token)) {
            return true;
        }
        let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| {
            let (scheme, password) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(password)
        });
        let Some(password) = bearer else { return false };
        matches!(self.login(ip, password), Login::Ok)
    }
}

/// Read the session secret from `path`, creating it on first use.
pub fn load_secret(path: &Path) -> Result<Vec<u8>, String> {
    match fs::read(path) {
        Ok(secret) if secret.len() >= SECRET_BYTES => return Ok(secret),
        Ok(_) => return Err(format!("会话密钥文件 {} 已损坏，删除它后重启即可（所有人需要重新登录）", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("读取会话密钥 {} 失败：{error}", path.display())),
    }
    let mut secret = vec![0u8; SECRET_BYTES];
    SystemRandom::new().fill(&mut secret).map_err(|_| "生成会话密钥失败".to_owned())?;
    let write = || -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path)?;
        file.write_all(&secret)?;
        file.sync_all()
    };
    write().map_err(|e| format!("保存会话密钥 {} 失败：{e}", path.display()))?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    fn with_cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn sessions_are_signed_and_expire() {
        let auth = Auth::new("secret pass", &[7; 32]);
        let cookie = auth.session_cookie(false);
        let token = cookie.split(';').next().unwrap().split_once('=').unwrap().1.to_owned();
        assert!(auth.allowed(&with_cookie(&format!("other=1; {COOKIE}={token}")), IP));
        assert!(!auth.allowed(&HeaderMap::new(), IP));
        let (expires, tag) = token.split_once('.').unwrap();
        let later: u64 = expires.parse::<u64>().unwrap() + 1;
        assert!(!auth.allowed(&with_cookie(&format!("{COOKIE}={later}.{tag}")), IP), "extended expiry accepted");
        let past = auth.sign(unix_now() - 1).unwrap();
        assert!(!auth.allowed(&with_cookie(&format!("{COOKIE}={past}")), IP), "expired session accepted");
        // Another password (or another secret) does not accept the session.
        assert!(!Auth::new("new pass", &[7; 32]).allowed(&with_cookie(&format!("{COOKIE}={token}")), IP));
        assert!(!Auth::new("secret pass", &[8; 32]).allowed(&with_cookie(&format!("{COOKIE}={token}")), IP));
        assert!(auth.session_cookie(true).ends_with("; Secure"));
    }

    #[test]
    fn bearer_and_lockout() {
        let auth = Auth::new("pw", &[1; 32]);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer pw"));
        assert!(auth.allowed(&headers, IP));
        for _ in 0..MAX_FAILURES {
            assert!(matches!(auth.login(IP, "wrong"), Login::Wrong));
        }
        assert!(matches!(auth.login(IP, "pw"), Login::Locked(15)), "correct password accepted while locked");
        assert!(!auth.allowed(&headers, IP));
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(matches!(auth.login(other, "pw"), Login::Ok), "lockout spread to another address");
        assert!(Auth::open().allowed(&HeaderMap::new(), IP));
    }

    #[test]
    fn secret_is_created_once() {
        let dir = std::env::temp_dir().join(format!("flow-panel-secret-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("secret.key");
        let first = load_secret(&path).unwrap();
        assert_eq!(first.len(), SECRET_BYTES);
        assert_eq!(load_secret(&path).unwrap(), first);
        fs::write(&path, b"short").unwrap();
        assert!(load_secret(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
