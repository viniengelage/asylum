//! The cookies a collection's responses set, sent back on later requests the way a browser
//! would: by domain, path and expiry. Enough for session cookies; not a full RFC 6265 jar.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Without a leading dot. A cookie without `Domain` only goes back to the exact host.
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    /// Seconds since the epoch; `None` lives until the jar is cleared.
    pub expires_at: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
}

impl Cookie {
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    fn matches(&self, host: &str, path: &str, is_https: bool, now: i64) -> bool {
        if self.is_expired(now) || (self.secure && !is_https) {
            return false;
        }
        let domain_matches = if self.host_only {
            host == self.domain
        } else {
            host == self.domain || host.ends_with(&format!(".{}", self.domain))
        };
        domain_matches && path_matches(&self.path, path)
    }
}

fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || request_path[cookie_path.len()..].starts_with('/')))
}

/// The host, path and scheme of a URL, without pulling in a URL parser for three fields.
pub fn url_parts(url: &str) -> Option<(String, String, bool)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .split(':')
        .next()?
        .to_ascii_lowercase();
    let path_and_rest = &rest[authority_end..];
    let path_end = path_and_rest
        .find(['?', '#'])
        .unwrap_or(path_and_rest.len());
    let path = match &path_and_rest[..path_end] {
        "" => "/".to_string(),
        path => path.to_string(),
    };
    Some((host, path, scheme.eq_ignore_ascii_case("https")))
}

/// The directory of a request path, the default `Path` of a cookie it sets.
fn default_path(request_path: &str) -> String {
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => request_path[..index].to_string(),
    }
}

pub fn parse_set_cookie(header: &str, request_url: &str, now: i64) -> Option<Cookie> {
    let (host, request_path, _) = url_parts(request_url)?;
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.trim().trim_matches('"').to_string(),
        domain: host.clone(),
        host_only: true,
        path: default_path(&request_path),
        expires_at: None,
        secure: false,
        http_only: false,
    };
    let mut max_age = None;
    for attribute in parts {
        let (key, value) = attribute
            .split_once('=')
            .map_or((attribute.trim(), ""), |(key, value)| {
                (key.trim(), value.trim())
            });
        match key.to_ascii_lowercase().as_str() {
            "domain" if !value.is_empty() => {
                let domain = value.trim_start_matches('.').to_ascii_lowercase();
                // A server may only set cookies for itself or a parent domain.
                if host != domain && !host.ends_with(&format!(".{domain}")) {
                    return None;
                }
                cookie.domain = domain;
                cookie.host_only = false;
            }
            "path" if value.starts_with('/') => cookie.path = value.to_string(),
            "max-age" => max_age = value.parse::<i64>().ok(),
            "expires" => {
                if let Ok(expires) = DateTime::parse_from_rfc2822(value)
                    .or_else(|_| DateTime::parse_from_str(value, "%a, %d-%b-%Y %H:%M:%S GMT"))
                {
                    cookie.expires_at = Some(expires.with_timezone(&Utc).timestamp());
                }
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            _ => {}
        }
    }
    // Max-Age wins over Expires, and zero or less deletes the cookie.
    if let Some(max_age) = max_age {
        cookie.expires_at = Some(now + max_age);
    }
    Some(cookie)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Keeps what a response's `Set-Cookie` headers set, replacing cookies with the same
    /// name, domain and path. Returns the cookies it set.
    pub fn store<'a>(
        &mut self,
        headers: impl IntoIterator<Item = &'a (String, String)>,
        request_url: &str,
        now: i64,
    ) -> Vec<Cookie> {
        let mut set = Vec::new();
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let Some(cookie) = parse_set_cookie(value, request_url, now) else {
                continue;
            };
            self.cookies.retain(|existing| {
                !(existing.name == cookie.name
                    && existing.domain == cookie.domain
                    && existing.path == cookie.path)
            });
            if !cookie.is_expired(now) {
                self.cookies.push(cookie.clone());
            }
            set.push(cookie);
        }
        self.cookies.retain(|cookie| !cookie.is_expired(now));
        set
    }

    /// The `Cookie` header for a request, longest paths first as browsers send them.
    pub fn header_for(&self, url: &str, now: i64) -> Option<String> {
        let (host, path, is_https) = url_parts(url)?;
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches(&host, &path, is_https, now))
            .collect();
        if matching.is_empty() {
            return None;
        }
        matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        Some(
            matching
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const NOW: i64 = 1_790_000_000;

    fn headers(values: &[&str]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|value| ("set-cookie".to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn keeps_and_sends_back_cookies() {
        let mut jar = CookieJar::default();
        let set = jar.store(
            &headers(&[
                "sid=abc; Path=/; HttpOnly",
                "pref=dark; Domain=.trix.com.br; Path=/; Max-Age=3600",
                "tmp=1; Path=/auth",
                "secure=1; Path=/; Secure",
                "evil=1; Domain=google.com",
            ]),
            "https://api.trix.com.br/auth/login",
            NOW,
        );
        assert_eq!(set.len(), 4);
        assert_eq!(
            jar.header_for("https://api.trix.com.br/auth/me", NOW)
                .as_deref(),
            Some("tmp=1; sid=abc; pref=dark; secure=1")
        );
        assert_eq!(
            jar.header_for("http://api.trix.com.br/users", NOW)
                .as_deref(),
            Some("sid=abc; pref=dark")
        );
        assert_eq!(
            jar.header_for("https://web.trix.com.br/", NOW).as_deref(),
            Some("pref=dark")
        );
        assert_eq!(
            jar.header_for("https://pref.trix.com.br.evil.com/", NOW),
            None
        );
        assert_eq!(
            jar.header_for("https://api.trix.com.br/users", NOW + 7200)
                .as_deref(),
            Some("sid=abc; secure=1")
        );
    }

    #[test]
    fn replaces_and_deletes_cookies() {
        let mut jar = CookieJar::default();
        jar.store(&headers(&["sid=1"]), "https://a.test/x", NOW);
        jar.store(&headers(&["sid=2"]), "https://a.test/y", NOW);
        assert_eq!(jar.cookies.len(), 1);
        assert_eq!(jar.cookies[0].value, "2");
        jar.store(&headers(&["sid=; Max-Age=0"]), "https://a.test/", NOW);
        assert!(jar.is_empty());
        let cookie = parse_set_cookie(
            "a=b; Expires=Wed, 21 Oct 2026 07:28:00 GMT",
            "https://a.test/",
            NOW,
        )
        .unwrap();
        assert!(cookie.expires_at.is_some());
    }
}
