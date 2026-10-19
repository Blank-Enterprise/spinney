//! GitHub: list the repositories a space's token can see; sign in with the OAuth device flow.

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer};
use std::time::Duration;

/// Pages followed at most (100 repos each).
const MAX_PAGES: usize = 50;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Repo {
    pub full_name: String,
    pub name: String,
    #[serde(deserialize_with = "login")]
    pub owner: String,
    pub private: bool,
    pub description: Option<String>,
    pub clone_url: String,
    pub pushed_at: Option<String>,
}

/// `owner: { "login": "..." }` → the login.
fn login<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }
    Ok(Owner::deserialize(d)?.login)
}

/// The repos a token can see; `truncated` when listing stopped before the last page.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoList {
    pub repos: Vec<Repo>,
    pub truncated: bool,
}

/// HTTP agent for GitHub calls: system trust store (corporate CAs, TLS-inspecting proxies)
/// and a timeout so a hung host cannot spin forever.
pub fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// Every repo the token can see (owned, collaborator, org member), most recently pushed first.
pub fn list_repos(api_base: &str, token: &str) -> Result<RepoList> {
    let agent = agent();
    let mut url = Some(format!(
        "{api_base}/user/repos?per_page=100&affiliation=owner,collaborator,organization_member&sort=pushed"
    ));
    let (mut repos, mut truncated) = (vec![], false);
    // ponytail: stops after 5000 repos (flagged as truncated); nobody browses more in a list.
    for _ in 0..MAX_PAGES {
        let Some(u) = url.take() else { break };
        let mut resp = match agent
            .get(&u)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "spinney")
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => {
                bail!("Token rejected — check the space's personal access token")
            }
            Err(ureq::Error::StatusCode(code)) => bail!("GitHub answered HTTP {code}"),
            Err(e) => bail!("Could not reach GitHub: {e}"),
        };
        let link = resp
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        url = next_page(link, api_base);
        truncated = url.is_none() && parse_next_link(link).is_some();
        let page: Vec<Repo> = resp.body_mut().read_json()?;
        repos.extend(page);
    }
    Ok(RepoList {
        truncated: truncated || url.is_some(),
        repos,
    })
}

/// The next page to fetch, only if it is on the space's API: the token must go nowhere else.
pub fn next_page(link_header: &str, api_base: &str) -> Option<String> {
    parse_next_link(link_header).filter(|next| next.starts_with(&format!("{api_base}/")))
}

/// The `rel="next"` URL of a `Link` header.
pub fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let next = params.split(';').any(|p| p.trim() == r#"rel="next""#);
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        next.then(|| url.to_string())
    })
}

/// Repos whose full name or description contains `query`, ignoring case; blank → all.
pub fn filter<'a>(repos: &'a [Repo], query: &str) -> Vec<&'a Repo> {
    let q = query.trim().to_lowercase();
    repos
        .iter()
        .filter(|r| {
            r.full_name.to_lowercase().contains(&q)
                || r.description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&q))
        })
        .collect()
}

/// Spinney's OAuth App (public; the device flow needs no client secret).
pub const GITHUB_CLIENT_ID: &str = "Ov23liCPm15HJJ4KHBlA";
const DEVICE_SCOPE: &str = "repo read:org";
/// The device flow only ever talks to github.com.
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// GitHub's documented defaults, used when a reply omits (or zeroes) them.
const DEFAULT_INTERVAL: u64 = 5;
const DEFAULT_EXPIRES_IN: u64 = 900;

/// A started device flow. `device_code` is a secret: never shown, logged or put in errors.
#[derive(Clone, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// One token poll's outcome. The token is a secret.
#[derive(Clone, PartialEq)]
pub enum Poll {
    Pending,
    /// Polled too fast: wait this many seconds from now on.
    SlowDown(u64),
    Token(String),
    Denied,
    Expired,
}

impl std::fmt::Debug for Poll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Poll::Pending => f.write_str("Pending"),
            Poll::SlowDown(s) => write!(f, "SlowDown({s})"),
            Poll::Token(_) => f.write_str("Token(<redacted>)"),
            Poll::Denied => f.write_str("Denied"),
            Poll::Expired => f.write_str("Expired"),
        }
    }
}

/// Raw `/login/device/code` reply: the codes, or an `error`.
#[derive(Deserialize, Default)]
pub struct DeviceCodeReply {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    error: Option<String>,
}

/// Raw `/login/oauth/access_token` reply: a token, or an `error` (with `interval` on slow_down).
#[derive(Deserialize, Default)]
pub struct PollReply {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

/// Ask github.com for a device and user code.
pub fn start_device_flow(client_id: &str) -> Result<DeviceCode> {
    let reply = post_form(
        DEVICE_CODE_URL,
        &[("client_id", client_id), ("scope", DEVICE_SCOPE)],
    )?;
    parse_device_code(reply)
}

/// Ask github.com once whether the user approved; `interval` is the current poll interval.
pub fn poll_once(client_id: &str, device_code: &str, interval: u64) -> Result<Poll> {
    let form = [
        ("client_id", client_id),
        ("device_code", device_code),
        ("grant_type", DEVICE_GRANT),
    ];
    parse_poll(&post_form(ACCESS_TOKEN_URL, &form)?, interval)
}

/// POST a form to github.com and decode its JSON reply. Errors name only the HTTP status or
/// transport failure, never the form (which carries the device code).
fn post_form<T: serde::de::DeserializeOwned>(url: &str, form: &[(&str, &str)]) -> Result<T> {
    let mut resp = match agent()
        .post(url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("Accept", "application/json")
        .header("User-Agent", "spinney")
        .send_form(form.iter().copied())
    {
        Ok(r) => r,
        Err(e) => bail!("Could not reach GitHub: {e}"),
    };
    let status = resp.status().as_u16();
    match resp.body_mut().read_json::<T>() {
        Ok(v) => Ok(v),
        Err(_) if status >= 400 => bail!("GitHub answered HTTP {status}"),
        Err(_) => bail!("GitHub sent an unreadable reply"),
    }
}

/// Validate a device-code reply, falling back to GitHub's defaults for a missing timing.
pub fn parse_device_code(r: DeviceCodeReply) -> Result<DeviceCode> {
    if let Some(code) = r.error {
        bail!("GitHub refused sign-in: {code}");
    }
    let (Some(device_code), Some(user_code), Some(verification_uri)) =
        (r.device_code, r.user_code, r.verification_uri)
    else {
        bail!("GitHub sent an incomplete sign-in reply");
    };
    Ok(DeviceCode {
        device_code,
        user_code,
        verification_uri,
        expires_in: r
            .expires_in
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_EXPIRES_IN),
        interval: r.interval.filter(|&s| s > 0).unwrap_or(DEFAULT_INTERVAL),
    })
}

/// Map a token-poll reply to its outcome. `slow_down` uses the returned interval, or adds 5s.
pub fn parse_poll(r: &PollReply, interval: u64) -> Result<Poll> {
    match (r.access_token.as_deref(), r.error.as_deref()) {
        (Some(t), _) if !t.is_empty() => Ok(Poll::Token(t.to_string())),
        (_, Some("authorization_pending")) => Ok(Poll::Pending),
        (_, Some("slow_down")) => Ok(Poll::SlowDown(r.interval.unwrap_or(0).max(interval + 5))),
        (_, Some("expired_token" | "token_expired")) => Ok(Poll::Expired),
        (_, Some("access_denied")) => Ok(Poll::Denied),
        (_, Some(code)) => bail!("GitHub sign-in failed: {code}"),
        _ => bail!("GitHub sent an empty sign-in reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(full_name: &str, description: Option<&str>) -> Repo {
        let (owner, name) = full_name.split_once('/').unwrap();
        Repo {
            full_name: full_name.into(),
            name: name.into(),
            owner: owner.into(),
            private: false,
            description: description.map(Into::into),
            clone_url: format!("https://example.com/{full_name}.git"),
            pushed_at: None,
        }
    }

    #[test]
    fn next_link_is_found_anywhere() {
        let base = "https://api.example.com/user/repos?per_page=100";
        let first = format!(r#"<{base}&page=2>; rel="next", <{base}&page=5>; rel="last""#);
        assert_eq!(parse_next_link(&first), Some(format!("{base}&page=2")));
        let middle = format!(
            r#"<{base}&page=1>; rel="prev", <{base}&page=3>; rel="next", <{base}&page=5>; rel="last""#
        );
        assert_eq!(parse_next_link(&middle), Some(format!("{base}&page=3")));
        let last = format!(r#"<{base}&page=4>; rel="prev", <{base}&page=1>; rel="first""#);
        assert_eq!(parse_next_link(&last), None);
        assert_eq!(parse_next_link(""), None);
    }

    #[test]
    fn next_page_stays_on_the_api_host() {
        let gh = "https://api.github.com";
        let link = |url: &str| format!(r#"<{url}>; rel="next""#);
        let ok = "https://api.github.com/user/repos?page=2";
        assert_eq!(next_page(&link(ok), gh), Some(ok.to_string()));
        for evil in [
            "https://api.github.com.evil.com/user/repos?page=2",
            "https://evil.com/?u=https://api.github.com/",
            "https://api.github.com@evil.com/user/repos",
            "http://api.github.com/user/repos?page=2",
        ] {
            assert_eq!(next_page(&link(evil), gh), None, "{evil}");
        }
        let ghe = "https://ghe.example.com/api/v3";
        let ghe_next = "https://ghe.example.com/api/v3/user/repos?page=2";
        assert_eq!(next_page(&link(ghe_next), ghe), Some(ghe_next.to_string()));
        let outside = "https://ghe.example.com/other/user/repos?page=2";
        assert_eq!(next_page(&link(outside), ghe), None);
        let last = r#"<https://api.github.com/user/repos?page=1>; rel="prev""#;
        assert_eq!(next_page(last, gh), None);
        assert_eq!(next_page("", gh), None);
    }

    #[test]
    fn filter_matches_name_or_description_ignoring_case() {
        let repos = [
            repo("octo-org/Widget", None),
            repo("octo-org/tools", Some("Build WIDGETS fast")),
            repo("someone/other", Some("unrelated")),
        ];
        let names = |q: &str| -> Vec<&str> {
            filter(&repos, q)
                .iter()
                .map(|r| r.full_name.as_str())
                .collect()
        };
        assert_eq!(names("widget"), ["octo-org/Widget", "octo-org/tools"]);
        assert_eq!(names("OCTO-ORG/w"), ["octo-org/Widget"]);
        assert_eq!(names("nothing"), Vec::<&str>::new());
        assert_eq!(names("").len(), 3);
        assert_eq!(names("  ").len(), 3);
    }

    #[test]
    fn deserializes_rest_shape() {
        let json = r#"[{"id": 1, "full_name": "octo-org/widget", "name": "widget",
            "owner": {"login": "octo-org", "id": 2}, "private": true, "description": null,
            "clone_url": "https://example.com/octo-org/widget.git", "pushed_at": null}]"#;
        let repos: Vec<Repo> = serde_json::from_str(json).unwrap();
        let mut want = repo("octo-org/widget", None);
        want.private = true;
        assert_eq!(repos, [want]);
    }

    const SECRET: &str = "sentinel-secret-value";

    fn poll(json: &str, interval: u64) -> Result<Poll> {
        parse_poll(&serde_json::from_str(json).unwrap(), interval)
    }

    #[test]
    fn poll_maps_every_documented_reply() {
        let pending = r#"{"error":"authorization_pending","error_description":"x"}"#;
        assert_eq!(poll(pending, 5).unwrap(), Poll::Pending);
        let token =
            format!(r#"{{"access_token":"{SECRET}","token_type":"bearer","scope":"repo"}}"#);
        assert_eq!(poll(&token, 5).unwrap(), Poll::Token(SECRET.into()));
        for expired in [
            r#"{"error":"expired_token"}"#,
            r#"{"error":"token_expired"}"#,
        ] {
            assert_eq!(poll(expired, 5).unwrap(), Poll::Expired, "{expired}");
        }
        assert_eq!(
            poll(r#"{"error":"access_denied"}"#, 5).unwrap(),
            Poll::Denied
        );
    }

    #[test]
    fn slow_down_uses_returned_interval_or_adds_five() {
        let returned = r#"{"error":"slow_down","interval":10}"#;
        assert_eq!(poll(returned, 5).unwrap(), Poll::SlowDown(10));
        assert_eq!(
            poll(r#"{"error":"slow_down"}"#, 5).unwrap(),
            Poll::SlowDown(10)
        );
        assert_eq!(
            poll(r#"{"error":"slow_down"}"#, 10).unwrap(),
            Poll::SlowDown(15)
        );
        // A returned interval never makes polling faster than the documented +5s.
        let lower = r#"{"error":"slow_down","interval":1}"#;
        assert_eq!(poll(lower, 10).unwrap(), Poll::SlowDown(15));
    }

    #[test]
    fn unknown_or_empty_poll_reply_is_an_error_naming_only_the_code() {
        for code in [
            "incorrect_device_code",
            "device_flow_disabled",
            "unsupported_grant_type",
            "incorrect_client_credentials",
        ] {
            let json = format!(r#"{{"error":"{code}","error_description":"{SECRET}"}}"#);
            let err = format!("{:#}", poll(&json, 5).unwrap_err());
            assert!(err.contains(code), "{err}");
            assert!(!err.contains(SECRET), "{err}");
        }
        assert!(poll("{}", 5).is_err());
        assert!(poll(r#"{"access_token":""}"#, 5).is_err());
    }

    #[test]
    fn device_code_reply_is_validated_with_default_timing() {
        let json = format!(
            r#"{{"device_code":"{SECRET}","user_code":"WDJB-MJHT",
            "verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}}"#
        );
        let dc = parse_device_code(serde_json::from_str(&json).unwrap()).unwrap();
        assert_eq!(dc.device_code, SECRET);
        assert_eq!(dc.user_code, "WDJB-MJHT");
        assert_eq!(dc.verification_uri, "https://github.com/login/device");
        assert_eq!((dc.expires_in, dc.interval), (900, 5));

        let zero = format!(
            r#"{{"device_code":"{SECRET}","user_code":"WDJB-MJHT",
            "verification_uri":"https://github.com/login/device","expires_in":0,"interval":0}}"#
        );
        let dc = parse_device_code(serde_json::from_str(&zero).unwrap()).unwrap();
        assert_eq!((dc.expires_in, dc.interval), (900, 5));

        let err =
            parse_device_code(serde_json::from_str(r#"{"error":"device_flow_disabled"}"#).unwrap())
                .unwrap_err();
        assert!(format!("{err:#}").contains("device_flow_disabled"));
        let partial = format!(r#"{{"device_code":"{SECRET}"}}"#);
        let err = parse_device_code(serde_json::from_str(&partial).unwrap()).unwrap_err();
        assert!(!format!("{err:#}").contains(SECRET));
    }

    #[test]
    fn debug_redacts_secrets() {
        let dc = DeviceCode {
            device_code: SECRET.into(),
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 900,
            interval: 5,
        };
        let shown = format!("{dc:?} {:?}", Poll::Token(SECRET.into()));
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(shown.contains("WDJB-MJHT"));
    }
}
