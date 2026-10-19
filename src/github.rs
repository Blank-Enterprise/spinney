//! GitHub REST: list the repositories a space's token can see.

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
}
