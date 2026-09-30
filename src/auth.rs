//! Password login, session cookies and login rate limiting.

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::Engine;
use rand::RngCore;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "strymek_session";

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?
        .to_string())
}

pub fn verify_password(hash: &str, pw: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(pw.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

struct Session {
    last_seen: Instant,
}

pub struct Auth {
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<HashMap<IpAddr, (u32, Option<Instant>)>>,
    idle: Duration,
}

const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(300);

pub enum LoginCheck {
    Allowed,
    LockedOut,
}

impl Auth {
    pub fn new(idle_hours: u64) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            idle: Duration::from_secs(idle_hours.max(1) * 3600),
        }
    }

    pub fn check_lockout(&self, ip: IpAddr) -> LoginCheck {
        let mut f = self.failures.lock().unwrap();
        if let Some((_, Some(until))) = f.get(&ip) {
            let now = Instant::now();
            if *until > now {
                return LoginCheck::LockedOut;
            }
            f.remove(&ip);
        }
        LoginCheck::Allowed
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert((0, None));
        e.0 += 1;
        if e.0 >= MAX_FAILURES {
            e.1 = Some(Instant::now() + LOCKOUT);
            tracing::warn!(%ip, "too many failed logins; locked out for {}s", LOCKOUT.as_secs());
        }
    }

    pub fn create_session(&self, ip: IpAddr) -> String {
        self.failures.lock().unwrap().remove(&ip);
        let mut buf = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Session { last_seen: Instant::now() });
        token
    }

    /// Validate a session token and refresh its idle timer.
    pub fn validate(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, v| now.duration_since(v.last_seen) < self.idle);
        match s.get_mut(token) {
            Some(sess) => {
                sess.last_seen = now;
                true
            }
            None => false,
        }
    }

    pub fn destroy(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

/// Extract the session token from a Cookie header value.
pub fn token_from_cookie_header(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}
