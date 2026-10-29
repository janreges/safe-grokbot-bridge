//! Bearer-token authentication, failed-attempt lockout and per-token rate limiting.
//!
//! Tokens are 256-bit random values; the config stores only their SHA-256. Brute force is
//! therefore infeasible, and the lockout mainly keeps scanners from filling the logs and from
//! burning CPU. All state is in memory and bounded.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::{Config, Limits};
use crate::error::ApiError;

/// Prefix of every token, so a leaked token is easy to recognise, e.g. by secret scanners.
pub const TOKEN_PREFIX: &str = "sgb_";

/// Upper bound on the number of client addresses tracked for failed attempts.
const MAX_TRACKED_IPS: usize = 10_000;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Generates a new token: the prefix followed by 256 random bits in hex.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator failed");
    format!("{TOKEN_PREFIX}{}", hex(&bytes))
}

/// The value stored in the config file: the hex SHA-256 of the token.
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

pub struct Auth {
    tokens: Vec<(String, [u8; 32])>,
    limits: Limits,
    client_ip_header: Option<HeaderName>,
    failures: Mutex<HashMap<IpAddr, Failures>>,
    windows: Mutex<HashMap<String, RateWindow>>,
}

struct Failures {
    count: u32,
    since: Instant,
    blocked_until: Option<Instant>,
}

struct RateWindow {
    start: Instant,
    count: u32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Auth {
    pub fn new(config: &Config) -> Self {
        Self {
            tokens: config
                .tokens
                .iter()
                .map(|t| (t.name.clone(), t.sha256))
                .collect(),
            limits: config.limits.clone(),
            client_ip_header: config.server.client_ip_header.clone(),
            failures: Mutex::new(HashMap::new()),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// The address used for lockout and logging. The client-IP header (e.g. `CF-Connecting-IP`
    /// set by cloudflared) is trusted only when the TCP peer is on this machine; anyone else
    /// could forge it to dodge the lockout.
    pub fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if peer.ip().is_loopback()
            && let Some(name) = &self.client_ip_header
            && let Some(ip) = headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
        peer.ip()
    }

    /// Checks an `Authorization` header value and returns the name of the matching token.
    pub fn check(
        &self,
        ip: IpAddr,
        authorization: Option<&str>,
        now: Instant,
    ) -> Result<String, ApiError> {
        if let Some(until) = self.blocked_until(ip, now) {
            return Err(blocked(until.duration_since(now)));
        }
        let presented = authorization.and_then(|value| {
            let (scheme, token) = value.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
        let Some(name) = presented.and_then(|token| self.find(token)) else {
            return Err(self.record_failure(ip, now));
        };
        lock(&self.failures).remove(&ip);
        self.rate_limit(&name, now)?;
        Ok(name)
    }

    fn find(&self, token: &str) -> Option<String> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // Compare against every token without exiting early.
        let mut found = None;
        for (name, hash) in &self.tokens {
            if bool::from(hash.ct_eq(&digest)) {
                found = Some(name.clone());
            }
        }
        found
    }

    fn blocked_until(&self, ip: IpAddr, now: Instant) -> Option<Instant> {
        let mut failures = lock(&self.failures);
        let until = failures.get(&ip)?.blocked_until?;
        if until > now {
            return Some(until);
        }
        failures.remove(&ip);
        None
    }

    fn record_failure(&self, ip: IpAddr, now: Instant) -> ApiError {
        let window = Duration::from_secs(self.limits.auth_window_secs);
        let mut failures = lock(&self.failures);
        if failures.len() >= MAX_TRACKED_IPS && !failures.contains_key(&ip) {
            failures.retain(|_, f| {
                f.blocked_until.is_some_and(|until| until > now)
                    || now.duration_since(f.since) < window
            });
            if failures.len() >= MAX_TRACKED_IPS {
                failures.retain(|_, f| f.blocked_until.is_some_and(|until| until > now));
            }
            if failures.len() >= MAX_TRACKED_IPS {
                failures.clear();
            }
        }
        let entry = failures.entry(ip).or_insert(Failures {
            count: 0,
            since: now,
            blocked_until: None,
        });
        if now.duration_since(entry.since) >= window {
            entry.count = 0;
            entry.since = now;
        }
        entry.count += 1;
        if entry.count >= self.limits.auth_failures {
            let block = Duration::from_secs(self.limits.auth_block_secs);
            entry.blocked_until = Some(now + block);
            tracing::warn!(%ip, attempts = entry.count, "blocking client after failed authentication attempts");
            return blocked(block);
        }
        ApiError::unauthorized()
    }

    fn rate_limit(&self, name: &str, now: Instant) -> Result<(), ApiError> {
        let mut windows = lock(&self.windows);
        let window = windows.entry(name.to_string()).or_insert(RateWindow {
            start: now,
            count: 0,
        });
        if now.duration_since(window.start) >= RATE_WINDOW {
            window.start = now;
            window.count = 0;
        }
        window.count += 1;
        if window.count > self.limits.requests_per_minute {
            let retry = RATE_WINDOW.saturating_sub(now.duration_since(window.start));
            return Err(ApiError::too_many(
                "rate_limited",
                format!(
                    "Rate limit of {} requests per minute exceeded for token `{name}`. Retry after {} seconds; long-poll with `wait` instead of polling in a tight loop.",
                    self.limits.requests_per_minute,
                    retry.as_secs().max(1)
                ),
                retry.as_secs(),
            ));
        }
        Ok(())
    }
}

fn blocked(remaining: Duration) -> ApiError {
    ApiError::too_many(
        "too_many_failures",
        format!(
            "Too many failed authentication attempts from this address. Try again in {} seconds with a valid token.",
            remaining.as_secs().max(1)
        ),
        remaining.as_secs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn auth(limits: &str) -> (Auth, String) {
        let dir = tempfile::tempdir().unwrap();
        let token = generate_token();
        let text = format!(
            "[[tokens]]\nname = \"grok\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n[limits]\n{limits}",
            hash_token(&token),
            toml::Value::String(dir.path().display().to_string())
        );
        let config = Config::parse(&text, Path::new("t.toml")).unwrap();
        (Auth::new(&config), format!("Bearer {token}"))
    }

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));

    #[test]
    fn valid_token_is_accepted_and_wrong_token_rejected() {
        let (auth, header) = auth("");
        let now = Instant::now();
        assert_eq!(auth.check(IP, Some(&header), now).unwrap(), "grok");
        assert_eq!(
            auth.check(IP, Some("bearer sgb_wrong"), now)
                .unwrap_err()
                .code,
            "unauthorized"
        );
        assert_eq!(auth.check(IP, None, now).unwrap_err().code, "unauthorized");
        assert_eq!(
            auth.check(IP, Some(&header.replace("Bearer", "Basic")), now)
                .unwrap_err()
                .code,
            "unauthorized"
        );
    }

    #[test]
    fn repeated_failures_block_the_address_until_the_block_expires() {
        let (auth, header) = auth("auth_failures = 3\nauth_block_secs = 60\n");
        let now = Instant::now();
        assert_eq!(
            auth.check(IP, Some("Bearer x"), now).unwrap_err().code,
            "unauthorized"
        );
        assert_eq!(
            auth.check(IP, Some("Bearer x"), now).unwrap_err().code,
            "unauthorized"
        );
        let third = auth.check(IP, Some("Bearer x"), now).unwrap_err();
        assert_eq!(
            (third.code, third.retry_after),
            ("too_many_failures", Some(60))
        );
        // Even a valid token is refused while blocked; another address is unaffected.
        assert_eq!(
            auth.check(IP, Some(&header), now).unwrap_err().code,
            "too_many_failures"
        );
        let other = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 1));
        assert!(auth.check(other, Some(&header), now).is_ok());
        let later = now + Duration::from_secs(61);
        assert!(auth.check(IP, Some(&header), later).is_ok());
    }

    #[test]
    fn rate_limit_applies_per_minute() {
        let (auth, header) = auth("requests_per_minute = 2\n");
        let now = Instant::now();
        assert!(auth.check(IP, Some(&header), now).is_ok());
        assert!(auth.check(IP, Some(&header), now).is_ok());
        assert_eq!(
            auth.check(IP, Some(&header), now).unwrap_err().code,
            "rate_limited"
        );
        assert!(auth.check(IP, Some(&header), now + RATE_WINDOW).is_ok());
    }

    #[test]
    fn client_ip_header_is_trusted_only_from_loopback() {
        let (auth, _) = auth("");
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "198.51.100.2:5000".parse().unwrap();
        assert_eq!(auth.client_ip(local, &headers).to_string(), "203.0.113.9");
        assert_eq!(auth.client_ip(remote, &headers).to_string(), "198.51.100.2");
        headers.insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
        assert_eq!(auth.client_ip(local, &headers).to_string(), "127.0.0.1");
    }
}
