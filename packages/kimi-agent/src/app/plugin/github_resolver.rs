use std::sync::LazyLock;
use std::time::Duration;

use kimi_core::primitives::errors::Error2;
use regex::Regex;
use serde_json::{Map, Value};
use url::Url;

use crate::wire::record::WireAsyncError;

use super::errors::PLUGIN_LOAD_FAILED;
use super::source::{GithubRef, GithubRefKind, decode_uri_component};
use super::types::{PluginGithubRef, PluginGithubRefKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubSourceKind {
    Github,
}

#[derive(Debug, Clone)]
pub struct GithubSourceInput {
    pub kind: GithubSourceKind,
    pub owner: String,
    pub repo: String,
    pub ref_: Option<GithubRef>,
}

#[derive(Debug, Clone)]
pub struct GithubSourceResolution {
    pub tarball_url: String,
    pub display_version: String,
    pub ref_: PluginGithubRef,
}

pub async fn resolve_github_commit_sha(
    owner: &str,
    repo: &str,
    ref_: &str,
) -> Result<String, WireAsyncError> {
    let encoded_ref = encode_codeload_ref_path(ref_);
    let url = format!("https://github.com/{owner}/{repo}/commits/{encoded_ref}.atom");
    let response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/atom+xml")
        .header(reqwest::header::RANGE, "bytes=0-4095")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(github_error(
            format!(
                "Could not resolve {owner}/{repo}@{ref_}: HTTP {} {}.",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            ),
            details(&[
                ("owner", owner.into()),
                ("repo", repo.into()),
                ("ref", ref_.into()),
                ("status", status.as_u16().into()),
            ]),
        ));
    }
    let feed = response.text().await?;
    static COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
        regex::RegexBuilder::new(r"Grit::Commit/([0-9a-f]{40})")
            .case_insensitive(true)
            .build()
            .unwrap()
    });
    let Some(sha) = COMMIT_RE
        .captures(&feed)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_lowercase())
    else {
        return Err(github_error(
            format!("Could not resolve {owner}/{repo}@{ref_} to a commit SHA."),
            details(&[("owner", owner.into()), ("repo", repo.into()), ("ref", ref_.into())]),
        ));
    };
    Ok(sha)
}

pub async fn resolve_github_source(
    input: GithubSourceInput,
) -> Result<GithubSourceResolution, WireAsyncError> {
    let owner = input.owner;
    let repo = input.repo;
    if let Some(ref_) = input.ref_ {
        return Ok(GithubSourceResolution {
            tarball_url: codeload_url(&owner, &repo, &ref_),
            display_version: ref_.value.clone(),
            ref_: to_plugin_ref(&ref_),
        });
    }
    if let Some(latest_tag) = try_resolve_latest_release_tag(&owner, &repo).await? {
        let ref_ = GithubRef {
            kind: GithubRefKind::Tag,
            value: latest_tag,
        };
        return Ok(GithubSourceResolution {
            tarball_url: codeload_url(&owner, &repo, &ref_),
            display_version: ref_.value.clone(),
            ref_: to_plugin_ref(&ref_),
        });
    }
    let tarball_url = format!("https://codeload.github.com/{owner}/{repo}/zip/HEAD");
    let response = reqwest::Client::new()
        .head(&tarball_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(github_error(
            format!("Repository `{owner}/{repo}` not found or not accessible."),
            details(&[("owner", owner.into()), ("repo", repo.into())]),
        ));
    }
    if !status.is_success() {
        return Err(github_error(
            format!(
                "Could not access `{owner}/{repo}`: HTTP {} {}.",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            ),
            details(&[
                ("owner", owner.into()),
                ("repo", repo.into()),
                ("status", status.as_u16().into()),
            ]),
        ));
    }
    Ok(GithubSourceResolution {
        tarball_url,
        display_version: "HEAD".to_string(),
        ref_: PluginGithubRef {
            kind: PluginGithubRefKind::Branch,
            value: "HEAD".to_string(),
        },
    })
}

async fn try_resolve_latest_release_tag(
    owner: &str,
    repo: &str,
) -> Result<Option<String>, WireAsyncError> {
    let url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status != reqwest::StatusCode::MOVED_PERMANENTLY
        && status != reqwest::StatusCode::FOUND
    {
        return Err(github_error(
            format!(
                "Could not look up latest release of `{owner}/{repo}`: HTTP {} {} ({url}). Pin a specific ref with `/tree/<branch|tag|sha>` to bypass release lookup.",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            ),
            details(&[
                ("owner", owner.into()),
                ("repo", repo.into()),
                ("status", status.as_u16().into()),
                ("url", url.clone().into()),
            ]),
        ));
    }
    let Some(location) = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(None);
    };
    static TAG_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"/releases/tag/([^/?#]+)").unwrap());
    let Some(tag) = TAG_RE
        .captures(location)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str())
    else {
        return Ok(None);
    };
    Ok(Some(
        decode_uri_component(tag).unwrap_or_else(|| tag.to_string()),
    ))
}

fn codeload_url(owner: &str, repo: &str, ref_: &GithubRef) -> String {
    let base = format!("https://codeload.github.com/{owner}/{repo}/zip");
    let encoded = encode_codeload_ref_path(&ref_.value);
    match ref_.kind {
        GithubRefKind::Tag => format!("{base}/refs/tags/{encoded}"),
        GithubRefKind::Branch | GithubRefKind::Sha => format!("{base}/{encoded}"),
    }
}

fn encode_codeload_ref_path(value: &str) -> String {
    value
        .split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn encode_uri_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
        {
            output.push(char::from(*byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn to_plugin_ref(ref_: &GithubRef) -> PluginGithubRef {
    PluginGithubRef {
        kind: match ref_.kind {
            GithubRefKind::Branch => PluginGithubRefKind::Branch,
            GithubRefKind::Tag => PluginGithubRefKind::Tag,
            GithubRefKind::Sha => PluginGithubRefKind::Sha,
        },
        value: ref_.value.clone(),
    }
}

fn github_error(message: String, details: Map<String, Value>) -> WireAsyncError {
    Box::new(Error2::new(PLUGIN_LOAD_FAILED, message).with_details(details))
}

fn details(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

pub(crate) fn valid_http_url(value: &str) -> Option<Url> {
    Url::parse(value).ok()
}
