//! Cookie parsing/building for the transitional auth model C → B (D6, S8).
//!
//! Cookie names:
//! * [`LAMPA_SESSION_COOKIE`] — `lampa_session`, target B cookie (HttpOnly);
//! * [`DEVICE_TOKEN_COOKIE`] — `device_token`, readable legacy cookie; it is
//!   **never** HttpOnly (the client JS still reads/writes it during C);
//! * [`ADMIN_TOKEN_COOKIE`] — `auth_token`, legacy admin cookie (HttpOnly in
//!   the Node backend; validation is handled by the guard/T07 login).
//!
//! The `Secure` flag is only added for HTTPS origins (tunnel/Tailscale): LAN
//! HTTP browsers drop `Secure` cookies, which would force a PIN on every start
//! (S8 §3.1). `SameSite=Lax` is always used; `SameSite=None` is never emitted.

use axum::http::{header, HeaderMap};

/// Target B session cookie (HttpOnly).
pub const LAMPA_SESSION_COOKIE: &str = "lampa_session";
/// Legacy readable device cookie (transitional C; never HttpOnly).
pub const DEVICE_TOKEN_COOKIE: &str = "device_token";
/// Legacy admin cookie (`settings.admin_token`).
pub const ADMIN_TOKEN_COOKIE: &str = "auth_token";
/// Cookie lifetime (one year, like the legacy JS `max-age`).
pub const SESSION_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 365;

/// One cookie lookup result, preserving "absent" vs "present but blank".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieValue<'a> {
    /// No cookie with any of the requested names.
    Missing,
    /// The cookie exists but its value is empty (`name=` or `name=""`).
    Empty,
    /// First non-empty value, in name-precedence order.
    Value(&'a str),
}

/// Finds the first non-empty cookie among `names`, in the order given.
///
/// All `Cookie` request headers are scanned (HTTP/2 clients may split them).
/// Empty values of a higher-precedence name fall through to the next name, and
/// are reported as [`CookieValue::Empty`] only when no name has a value — this
/// matches Node's `cookies.a || cookies.b` fallback.
pub fn find<'a>(headers: &'a HeaderMap, names: &[&str]) -> CookieValue<'a> {
    let mut seen_empty = false;
    for name in names {
        match find_one(headers, name) {
            Some(CookieValue::Value(value)) => return CookieValue::Value(value),
            Some(CookieValue::Empty) => seen_empty = true,
            Some(CookieValue::Missing) | None => {}
        }
    }
    if seen_empty {
        CookieValue::Empty
    } else {
        CookieValue::Missing
    }
}

/// Looks up exactly one cookie name.
fn find_one<'a>(headers: &'a HeaderMap, name: &str) -> Option<CookieValue<'a>> {
    for header in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = header.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let Some((cookie_name, cookie_value)) = pair.split_once('=') else {
                continue;
            };
            if cookie_name.trim() != name {
                continue;
            }
            let value = cookie_value.trim().trim_matches('"');
            if value.is_empty() {
                return Some(CookieValue::Empty);
            }
            return Some(CookieValue::Value(value));
        }
    }
    None
}

/// True when the request arrived over HTTPS (direct TLS is not terminated by
/// this server; the tunnel/ingress sets `X-Forwarded-Proto: https`).
pub fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

/// `Set-Cookie` values for a successful PIN verify: the HttpOnly session
/// cookie and — only in transitional C — the readable legacy device cookie
/// carrying the **same** token.
pub fn device_cookies(token: &str, secure: bool, include_legacy: bool) -> Vec<String> {
    let mut cookies = vec![build(
        LAMPA_SESSION_COOKIE,
        token,
        SESSION_MAX_AGE_SECONDS,
        true,
        secure,
    )];
    if include_legacy {
        cookies.push(build(
            DEVICE_TOKEN_COOKIE,
            token,
            SESSION_MAX_AGE_SECONDS,
            false,
            secure,
        ));
    }
    cookies
}

/// Re-issues the HttpOnly session cookie with an already-validated legacy token
/// (no rotation: S8 §7.10 — the session bootstrap must not invalidate devices).
pub fn session_cookie(token: &str, secure: bool) -> String {
    build(
        LAMPA_SESSION_COOKIE,
        token,
        SESSION_MAX_AGE_SECONDS,
        true,
        secure,
    )
}

/// `Set-Cookie` values that expire both device cookies on `401`.
pub fn clear_device_cookies() -> Vec<String> {
    vec![
        build(LAMPA_SESSION_COOKIE, "", 0, true, false),
        build(DEVICE_TOKEN_COOKIE, "", 0, false, false),
    ]
}

/// `Set-Cookie` value that expires the legacy admin cookie.
pub fn clear_admin_cookie() -> String {
    build(ADMIN_TOKEN_COOKIE, "", 0, true, false)
}

/// Builds one `Set-Cookie` value; `HttpOnly` is applied before `SameSite`.
fn build(name: &str, value: &str, max_age_seconds: i64, http_only: bool, secure: bool) -> String {
    let mut cookie = format!("{name}={value}; Path=/; Max-Age={max_age_seconds}");
    if http_only {
        cookie.push_str("; HttpOnly");
    }
    cookie.push_str("; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::COOKIE,
                HeaderValue::from_str(value).expect("cookie header"),
            );
        }
        headers
    }

    fn device_headers(cookie: &str) -> HeaderMap {
        headers_with(&[cookie])
    }

    #[test]
    fn missing_empty_and_present_are_distinguished() {
        assert_eq!(
            find(&HeaderMap::new(), &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Missing
        );
        assert_eq!(
            find(&device_headers("device_token="), &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Empty
        );
        assert_eq!(
            find(&device_headers("device_token=\"\""), &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Empty
        );
        assert_eq!(
            find(
                &device_headers("other=1; device_token=abc; auth_token=xyz"),
                &[DEVICE_TOKEN_COOKIE]
            ),
            CookieValue::Value("abc")
        );
        assert_eq!(
            find(&device_headers("device_token=abc"), &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Value("abc")
        );
        assert_eq!(
            find(
                &device_headers("device_token=\"abc\""),
                &[DEVICE_TOKEN_COOKIE]
            ),
            CookieValue::Value("abc")
        );
    }

    #[test]
    fn name_precedence_prefers_lampa_session_then_falls_back() {
        let names = [LAMPA_SESSION_COOKIE, DEVICE_TOKEN_COOKIE];
        let both = device_headers("device_token=legacy; lampa_session=session");
        assert_eq!(find(&both, &names), CookieValue::Value("session"));
        let legacy_only = device_headers("device_token=legacy");
        assert_eq!(find(&legacy_only, &names), CookieValue::Value("legacy"));
        let empty_session = device_headers("lampa_session=; device_token=legacy");
        assert_eq!(find(&empty_session, &names), CookieValue::Value("legacy"));
        let empty_both = device_headers("lampa_session=; device_token=");
        assert_eq!(find(&empty_both, &names), CookieValue::Empty);
        assert_eq!(find(&HeaderMap::new(), &names), CookieValue::Missing);
    }

    #[test]
    fn multiple_cookie_headers_are_scanned() {
        let headers = headers_with(&["a=1; lampa_session=sess", "b=2"]);
        assert_eq!(
            find(&headers, &[LAMPA_SESSION_COOKIE]),
            CookieValue::Value("sess")
        );
    }

    #[test]
    fn malformed_pairs_are_ignored() {
        let headers = device_headers("no-equals; =blank; device_token=ok");
        assert_eq!(
            find(&headers, &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Value("ok")
        );
        assert_eq!(
            find(&device_headers("no-equals; =blank"), &[DEVICE_TOKEN_COOKIE]),
            CookieValue::Missing
        );
    }

    #[test]
    fn https_detection_uses_the_first_forwarded_proto_token() {
        let mut headers = HeaderMap::new();
        assert!(!is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http, https"));
        assert!(!is_https(&headers));
    }

    #[test]
    fn transitional_c_sets_both_cookies_with_the_same_token() {
        let cookies = device_cookies("abc123", false, true);
        assert_eq!(cookies.len(), 2);
        assert!(cookies[0].starts_with("lampa_session=abc123; Path=/; Max-Age=31536000"));
        assert!(cookies[0].contains("; HttpOnly"));
        assert!(cookies[0].contains("; SameSite=Lax"));
        assert!(!cookies[0].contains("; Secure"));
        assert!(cookies[1].starts_with("device_token=abc123; Path=/; Max-Age=31536000"));
        assert!(
            !cookies[1].contains("HttpOnly"),
            "device_token must stay readable during C (B-naive is forbidden)"
        );

        let secure = device_cookies("abc123", true, true);
        assert!(secure[0].contains("; Secure"));
        assert!(secure[1].contains("; Secure"));

        let strict_b = device_cookies("abc123", false, false);
        assert_eq!(strict_b.len(), 1, "B-strict sets only lampa_session");
        assert!(strict_b[0].starts_with("lampa_session="));
    }

    #[test]
    fn clearing_expires_both_device_cookies_and_admin_cookie() {
        let cleared = clear_device_cookies();
        assert_eq!(cleared.len(), 2);
        assert!(cleared[0].starts_with("lampa_session=; Path=/; Max-Age=0"));
        assert!(cleared[0].contains("HttpOnly"));
        assert!(cleared[1].starts_with("device_token=; Path=/; Max-Age=0"));
        assert!(!cleared[1].contains("HttpOnly"));
        assert!(clear_admin_cookie().starts_with("auth_token=; Path=/; Max-Age=0"));
        assert!(clear_admin_cookie().contains("HttpOnly"));
    }

    #[test]
    fn session_reissue_keeps_the_value_and_httponly() {
        let cookie = session_cookie("s8-known-token", false);
        assert!(cookie.contains("lampa_session=s8-known-token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Max-Age=31536000"));
    }
}
