use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, join_all};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchLike, FetchResponse};
use crate::wire::record::WireAsyncError;

use super::source::decode_uri_component;

pub const KIMI_CODE_PLUGIN_MARKETPLACE_URL_ENV: &str = "KIMI_CODE_PLUGIN_MARKETPLACE_URL";
pub const PLUGIN_MARKETPLACE_CLASSIFICATIONS: &[&str] = &["official", "third_party"];
pub const PLUGIN_MARKETPLACE_CLIENTS: &[&str] = &["desktop", "cli"];
pub const PLUGIN_MARKETPLACE_TIERS: &[&str] = &["official", "curated"];
pub const PLUGIN_MARKETPLACE_PLATFORMS: &[&str] = &["macos", "windows", "linux", "cloud"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginMarketplaceClassification {
    Official,
    ThirdParty,
}

impl PluginMarketplaceClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::ThirdParty => "third_party",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginClient {
    Desktop,
    Cli,
}

impl PluginClient {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginMarketplaceTier {
    Official,
    Curated,
}

impl PluginMarketplaceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Curated => "curated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginMarketplacePlatform {
    Darwin,
    Win32,
    Linux,
    Cloud,
}

impl PluginMarketplacePlatform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Darwin => "darwin",
            Self::Win32 => "win32",
            Self::Linux => "linux",
            Self::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceLocalizedText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginMarketplaceLocaleValue {
    Text(String),
    Localized(PluginMarketplaceLocalizedText),
}

pub type PluginMarketplaceLocale = HashMap<String, PluginMarketplaceLocaleValue>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceDistribution {
    pub category: PluginMarketplaceClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_install: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_level: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceEntry {
    pub id: String,
    pub display_name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<PluginMarketplaceClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<Vec<PluginClient>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<PluginMarketplaceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256sum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<PluginMarketplaceLocale>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginMarketplacePlatform>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<PluginMarketplaceDistribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_in: Option<bool>,
}

pub trait MarketplaceHostEntry {
    fn host_clients(&self) -> Option<&[PluginClient]>;
    fn host_platforms(&self) -> Option<&[PluginMarketplacePlatform]>;
}

impl MarketplaceHostEntry for PluginMarketplaceEntry {
    fn host_clients(&self) -> Option<&[PluginClient]> {
        self.clients.as_deref()
    }

    fn host_platforms(&self) -> Option<&[PluginMarketplacePlatform]> {
        self.platforms.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplace {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub plugins: Vec<PluginMarketplaceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MarketplaceUpdateStatus {
    NotInstalled,
    UpToDate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Update {
        local: String,
        latest: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketplaceLocationKind {
    Remote,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceLocation {
    pub raw: String,
    pub kind: MarketplaceLocationKind,
    pub resolved: String,
}

pub type SourceCheckoutLocation = Arc<
    dyn Fn() -> BoxFuture<'static, Result<Option<MarketplaceLocation>, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ReadPluginMarketplaceOptions {
    pub source: String,
    pub work_dir: String,
    pub home_dir: Option<String>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub source_checkout_location: Option<SourceCheckoutLocation>,
}

pub fn plugin_client_for_identity(identity: &str) -> PluginClient {
    if identity == "kimi_code_desktop" {
        PluginClient::Desktop
    } else {
        PluginClient::Cli
    }
}

pub fn marketplace_entry_supports_client(
    entry: &impl MarketplaceHostEntry,
    client: PluginClient,
) -> bool {
    entry
        .host_clients()
        .is_none_or(|clients| clients.contains(&client))
}

pub fn marketplace_entry_installable_on_host(
    entry: &impl MarketplaceHostEntry,
    client: PluginClient,
    platform: &str,
) -> bool {
    if !marketplace_entry_supports_client(entry, client) {
        return false;
    }
    entry.host_platforms().is_none_or(|platforms| {
        platforms
            .iter()
            .any(|candidate| candidate.as_str() == platform)
    })
}

pub fn marketplace_entry_visible_on_host(
    entry: &impl MarketplaceHostEntry,
    client: PluginClient,
    platform: &str,
) -> bool {
    if !marketplace_entry_supports_client(entry, client) {
        return false;
    }
    let Some(platforms) = entry.host_platforms() else {
        return true;
    };
    platforms
        .iter()
        .any(|candidate| candidate.as_str() == platform)
        || platforms
            .iter()
            .all(|candidate| *candidate == PluginMarketplacePlatform::Cloud)
}

pub fn compute_update_status(
    latest: Option<&str>,
    local: Option<&str>,
    installed: bool,
) -> MarketplaceUpdateStatus {
    if !installed {
        return MarketplaceUpdateStatus::NotInstalled;
    }
    if let (Some(latest), Some(local)) = (latest, local)
        && let (Some(latest_version), Some(local_version)) = (parse_semver(latest), parse_semver(local))
        && latest_version > local_version
    {
        return MarketplaceUpdateStatus::Update {
            local: local.to_string(),
            latest: latest.to_string(),
        };
    }
    MarketplaceUpdateStatus::UpToDate {
        version: local.map(str::to_string),
    }
}

pub fn resolve_marketplace_location(
    source: &str,
    work_dir: &str,
    home: Option<&str>,
) -> Result<MarketplaceLocation, WireAsyncError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(format!("{KIMI_CODE_PLUGIN_MARKETPLACE_URL_ENV} cannot be empty.").into());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(MarketplaceLocation {
            raw: trimmed.to_string(),
            kind: MarketplaceLocationKind::Remote,
            resolved: trimmed.to_string(),
        });
    }
    if trimmed.starts_with("file://") {
        let path = file_url_to_path(trimmed)?;
        return Ok(MarketplaceLocation {
            raw: trimmed.to_string(),
            kind: MarketplaceLocationKind::Local,
            resolved: path,
        });
    }
    Ok(MarketplaceLocation {
        raw: trimmed.to_string(),
        kind: MarketplaceLocationKind::Local,
        resolved: resolve_local_path(trimmed, work_dir, home.unwrap_or_else(|| default_home().leak())),
    })
}

pub async fn read_plugin_marketplace(
    options: ReadPluginMarketplaceOptions,
) -> Result<(String, MarketplaceLocation), WireAsyncError> {
    let location = resolve_marketplace_location(
        &options.source,
        &options.work_dir,
        options.home_dir.as_deref(),
    )?;
    let default_fetch;
    let fetch_impl = match options.fetch_impl {
        Some(fetch) => fetch,
        None => {
            default_fetch = Arc::new(ReqwestFetch);
            default_fetch
        }
    };
    match read_marketplace_text(&location, fetch_impl.as_ref()).await {
        Ok(raw) => Ok((raw, location)),
        Err(error) => {
            let fallback = match options.source_checkout_location {
                Some(callback) => callback().await?,
                None => None,
            };
            let Some(fallback) = fallback else {
                return Err(error);
            };
            let raw = read_marketplace_text(&fallback, fetch_impl.as_ref()).await?;
            Ok((raw, fallback))
        }
    }
}

pub fn parse_plugin_marketplace(
    raw: &str,
    location: &MarketplaceLocation,
    home: Option<&str>,
) -> Result<PluginMarketplace, WireAsyncError> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|error| format!("Plugin marketplace is not valid JSON: {error}"))?;
    let Some(parsed) = parsed.as_object() else {
        return Err("Plugin marketplace must be an object.".into());
    };
    let Some(raw_plugins) = parsed.get("plugins").and_then(Value::as_array) else {
        return Err("Plugin marketplace must contain a \"plugins\" array.".into());
    };
    let home = home.map(str::to_string).unwrap_or_else(default_home);
    let mut plugins = Vec::with_capacity(raw_plugins.len());
    for (index, entry) in raw_plugins.iter().enumerate() {
        plugins.push(parse_marketplace_entry(entry, index, location, &home)?);
    }
    Ok(PluginMarketplace {
        source: location.resolved.clone(),
        version: string_field(parsed, "version"),
        plugins,
    })
}

pub fn with_built_in_entries(
    marketplace: &PluginMarketplace,
    built_ins: &[PluginMarketplaceEntry],
) -> PluginMarketplace {
    let built_in_ids = built_ins
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let catalog_by_id = marketplace
        .plugins
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut plugins = marketplace
        .plugins
        .iter()
        .filter(|entry| !built_in_ids.contains(entry.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    plugins.extend(built_ins.iter().map(|entry| {
        let mut entry = entry.clone();
        if let Some(version) = catalog_by_id
            .get(entry.id.as_str())
            .and_then(|catalog| catalog.version.clone())
        {
            entry.version = Some(version);
        }
        entry
    }));
    PluginMarketplace {
        source: marketplace.source.clone(),
        version: marketplace.version.clone(),
        plugins,
    }
}

pub async fn with_latest_versions(
    marketplace: &PluginMarketplace,
    fetch_impl: Arc<dyn FetchLike>,
) -> PluginMarketplace {
    let plugins = join_all(marketplace.plugins.iter().cloned().map(|entry| {
        let fetch_impl = fetch_impl.clone();
        async move {
            if entry.version.is_some() {
                return entry;
            }
            let latest = resolve_latest_github_release(&entry.source, fetch_impl.as_ref()).await;
            match latest {
                Some(latest) => PluginMarketplaceEntry {
                    version: Some(latest),
                    ..entry
                },
                None => entry,
            }
        }
    }))
    .await;
    PluginMarketplace {
        source: marketplace.source.clone(),
        version: marketplace.version.clone(),
        plugins,
    }
}

async fn read_marketplace_text(
    location: &MarketplaceLocation,
    fetch_impl: &dyn FetchLike,
) -> Result<String, WireAsyncError> {
    if location.kind == MarketplaceLocationKind::Local {
        return Ok(tokio::fs::read_to_string(&location.resolved).await?);
    }
    let response = fetch_impl
        .fetch(&location.resolved, FetchInit::default())
        .await?;
    if !response.ok {
        return Err(format!("Plugin marketplace returned HTTP {}", response.status).into());
    }
    response.text().await
}

fn parse_marketplace_entry(
    value: &Value,
    index: usize,
    location: &MarketplaceLocation,
    home: &str,
) -> Result<PluginMarketplaceEntry, WireAsyncError> {
    let Some(value) = value.as_object() else {
        return Err(format!("Plugin marketplace entry {} must be an object.", index + 1).into());
    };
    let id = required_string(value, "id", index)?;
    validate_marketplace_entry_type(value, &id)?;
    let source = string_field(value, "source")
        .or_else(|| string_field(value, "url"))
        .or_else(|| string_field(value, "downloadUrl"))
        .ok_or_else(|| format!("Plugin marketplace entry {id} must define \"source\"."))?;
    let resolved_source = resolve_entry_source(&source, location, home)?;
    let distribution = parse_distribution(value.get("distribution"), &id)?;
    let tier = match &distribution {
        None => parse_marketplace_tier(value, &id)?,
        Some(distribution) => Some(match distribution.category {
            PluginMarketplaceClassification::Official => PluginMarketplaceTier::Official,
            PluginMarketplaceClassification::ThirdParty => PluginMarketplaceTier::Curated,
        }),
    };
    let classification = distribution.as_ref().map(|distribution| distribution.category).or_else(|| {
        tier.map(|tier| match tier {
            PluginMarketplaceTier::Official => PluginMarketplaceClassification::Official,
            PluginMarketplaceTier::Curated => PluginMarketplaceClassification::ThirdParty,
        })
    });
    Ok(PluginMarketplaceEntry {
        id: id.clone(),
        display_name: string_field(value, "displayName")
            .or_else(|| string_field(value, "name"))
            .unwrap_or(id.clone()),
        source: resolved_source.clone(),
        classification,
        clients: parse_clients(value.get("clients")),
        tier,
        version: string_field(value, "version")
            .or_else(|| derive_version_from_github_source(&resolved_source)),
        description: string_field(value, "description")
            .or_else(|| string_field(value, "shortDescription")),
        homepage: string_field(value, "homepage").or_else(|| string_field(value, "websiteURL")),
        keywords: string_array_field(value, "keywords"),
        icon_url: string_field(value, "iconUrl"),
        sha256sum: parse_sha256sum(value, &id)?,
        filename: string_field(value, "filename"),
        cli_description: string_field(value, "cliDescription"),
        locale: parse_locale(value.get("locale"), &id)?,
        platforms: parse_platforms(value.get("platforms"), &id)?,
        distribution,
        billing: value.get("billing").cloned(),
        built_in: None,
    })
}

fn validate_marketplace_entry_type(value: &Map<String, Value>, id: &str) -> Result<(), WireAsyncError> {
    let Some(raw) = value.get("type") else {
        return Ok(());
    };
    let Some(raw) = raw.as_str() else {
        return Err(format!("Plugin marketplace entry {id} \"type\" must be a string.").into());
    };
    let kind = raw.trim();
    if matches!(kind, "plugin" | "managed" | "guide") {
        return Ok(());
    }
    Err(format!(
        "Plugin marketplace entry {id} \"type\" must be \"plugin\". Legacy aliases \"managed\" and \"guide\" are also accepted."
    )
    .into())
}

fn parse_marketplace_tier(
    value: &Map<String, Value>,
    id: &str,
) -> Result<Option<PluginMarketplaceTier>, WireAsyncError> {
    let Some(raw) = value.get("tier") else {
        return Ok(None);
    };
    let Some(raw) = raw.as_str() else {
        return Err(format!("Plugin marketplace entry {id} \"tier\" must be a string.").into());
    };
    let tier = raw.trim();
    if tier.is_empty() {
        return Ok(None);
    }
    match tier {
        "official" => Ok(Some(PluginMarketplaceTier::Official)),
        "curated" => Ok(Some(PluginMarketplaceTier::Curated)),
        _ => Err(format!(
            "Plugin marketplace entry {id} \"tier\" must be one of: {}.",
            PLUGIN_MARKETPLACE_TIERS.join(", ")
        )
        .into()),
    }
}

fn parse_clients(value: Option<&Value>) -> Option<Vec<PluginClient>> {
    let value = value?;
    let Some(values) = value.as_array() else {
        return Some(Vec::new());
    };
    let mut output = Vec::new();
    for value in values {
        let client = match value.as_str() {
            Some("desktop") => Some(PluginClient::Desktop),
            Some("cli") => Some(PluginClient::Cli),
            _ => None,
        };
        if let Some(client) = client
            && !output.contains(&client)
        {
            output.push(client);
        }
    }
    Some(output)
}

fn parse_distribution(
    value: Option<&Value>,
    id: &str,
) -> Result<Option<PluginMarketplaceDistribution>, WireAsyncError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(value) = value.as_object() else {
        return Err(format!("Plugin marketplace entry {id} must define \"distribution\".").into());
    };
    let category = string_field(value, "category");
    let category = match category.as_deref() {
        Some("official") => PluginMarketplaceClassification::Official,
        Some("third_party") => PluginMarketplaceClassification::ThirdParty,
        _ => {
            return Err(format!(
                "Plugin marketplace entry {id} \"distribution.category\" must be one of: {}.",
                PLUGIN_MARKETPLACE_CLASSIFICATIONS.join(", ")
            )
            .into());
        }
    };
    Ok(Some(PluginMarketplaceDistribution {
        category,
        region: string_field(value, "region"),
        default_install: boolean_field(value, "defaultInstall", id)?,
        weight: number_field(value, "weight", id)?,
        user_level: value.get("userLevel").cloned(),
    }))
}

fn parse_platforms(
    value: Option<&Value>,
    id: &str,
) -> Result<Option<Vec<PluginMarketplacePlatform>>, WireAsyncError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(values) = value.as_array().filter(|values| !values.is_empty()) else {
        return Err(format!(
            "Plugin marketplace entry {id} \"platforms\" must be a non-empty array."
        )
        .into());
    };
    let mut output = Vec::new();
    for value in values {
        let platform = match value.as_str() {
            Some("macos" | "darwin") => PluginMarketplacePlatform::Darwin,
            Some("windows" | "win32") => PluginMarketplacePlatform::Win32,
            Some("linux") => PluginMarketplacePlatform::Linux,
            Some("cloud") => PluginMarketplacePlatform::Cloud,
            _ => {
                let mut allowed = PLUGIN_MARKETPLACE_PLATFORMS.to_vec();
                allowed.extend(["darwin", "win32"]);
                return Err(format!(
                    "Plugin marketplace entry {id} \"platforms\" must contain only: {}.",
                    allowed.join(", ")
                )
                .into());
            }
        };
        if !output.contains(&platform) {
            output.push(platform);
        }
    }
    Ok(Some(output))
}

fn parse_sha256sum(value: &Map<String, Value>, id: &str) -> Result<Option<String>, WireAsyncError> {
    let sha256sum = string_field(value, "sha256sum");
    if sha256sum.as_ref().is_some_and(|value| {
        value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(format!(
            "Plugin marketplace entry {id} \"sha256sum\" must be a SHA-256 hex digest."
        )
        .into());
    }
    Ok(sha256sum)
}

fn parse_locale(
    value: Option<&Value>,
    id: &str,
) -> Result<Option<PluginMarketplaceLocale>, WireAsyncError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(value) = value.as_object() else {
        return Err(format!("Plugin marketplace entry {id} \"locale\" must be an object.").into());
    };
    let mut locale = HashMap::new();
    for (key, entry) in value {
        if let Some(text) = entry.as_str() {
            locale.insert(key.clone(), PluginMarketplaceLocaleValue::Text(text.to_string()));
            continue;
        }
        let Some(entry) = entry.as_object() else {
            return Err(format!(
                "Plugin marketplace entry {id} \"locale.{key}\" must be a string or an object."
            )
            .into());
        };
        locale.insert(
            key.clone(),
            PluginMarketplaceLocaleValue::Localized(PluginMarketplaceLocalizedText {
                cli_description: string_field(entry, "cliDescription"),
                long_description: string_field(entry, "longDescription"),
                short_description: string_field(entry, "shortDescription"),
            }),
        );
    }
    Ok(Some(locale))
}

fn boolean_field(
    value: &Map<String, Value>,
    field: &str,
    id: &str,
) -> Result<Option<bool>, WireAsyncError> {
    let Some(raw) = value.get(field) else {
        return Ok(None);
    };
    let Some(raw) = raw.as_bool() else {
        return Err(format!("Plugin marketplace entry {id} \"{field}\" must be a boolean.").into());
    };
    Ok(Some(raw))
}

fn number_field(
    value: &Map<String, Value>,
    field: &str,
    id: &str,
) -> Result<Option<f64>, WireAsyncError> {
    let Some(raw) = value.get(field) else {
        return Ok(None);
    };
    let Some(raw) = raw.as_f64().filter(|value| value.is_finite()) else {
        return Err(format!("Plugin marketplace entry {id} \"{field}\" must be a number.").into());
    };
    Ok(Some(raw))
}

fn resolve_entry_source(
    source: &str,
    location: &MarketplaceLocation,
    home: &str,
) -> Result<String, WireAsyncError> {
    let trimmed = source.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_string());
    }
    if trimmed.starts_with("file://") {
        return file_url_to_path(trimmed);
    }
    if trimmed == "~" || trimmed.starts_with("~/") {
        return Ok(resolve_local_path(trimmed, "", home));
    }
    if Path::new(trimmed).is_absolute() {
        return Ok(trimmed.to_string());
    }
    if location.kind == MarketplaceLocationKind::Remote {
        return Ok(Url::parse(&location.resolved)?.join(trimmed)?.to_string());
    }
    let parent = Path::new(&location.resolved)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    Ok(resolve_path(parent, trimmed).to_string_lossy().into_owned())
}

fn derive_version_from_github_source(source: &str) -> Option<String> {
    let url = Url::parse(source).ok()?;
    if !matches!(url.host_str()?, "github.com" | "www.github.com") {
        return None;
    }
    let segments = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let kind = segments.get(2).copied();
    let a = segments.get(3).copied();
    let b = segments.get(4).copied();
    let ref_ = if kind == Some("releases") && a == Some("tag") {
        b?
    } else if matches!(kind, Some("tree" | "commit")) {
        a?
    } else {
        return None;
    };
    let decoded = decode_uri_component(ref_).unwrap_or_else(|| ref_.to_string());
    let candidate = decoded
        .strip_prefix('v')
        .or_else(|| decoded.strip_prefix('V'))
        .unwrap_or(&decoded);
    parse_semver(candidate).map(|_| candidate.to_string())
}

async fn resolve_latest_github_release(source: &str, fetch_impl: &dyn FetchLike) -> Option<String> {
    let (owner, repo) = parse_github_repo(source)?;
    let tag = fetch_latest_release_tag(&owner, &repo, fetch_impl)
        .await
        .ok()
        .flatten()?;
    let candidate = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(&tag);
    parse_semver(candidate).map(|_| candidate.to_string())
}

fn parse_github_repo(source: &str) -> Option<(String, String)> {
    let url = Url::parse(source).ok()?;
    if !matches!(url.host_str()?, "github.com" | "www.github.com") {
        return None;
    }
    let segments = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 2 {
        return None;
    }
    Some((segments[0].to_string(), segments[1].to_string()))
}

async fn fetch_latest_release_tag(
    owner: &str,
    repo: &str,
    fetch_impl: &dyn FetchLike,
) -> Result<Option<String>, WireAsyncError> {
    let url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let response = fetch_impl
        .fetch(
            &url,
            FetchInit {
                manual_redirect: true,
                ..Default::default()
            },
        )
        .await?;
    if response.status == 404 {
        return Ok(None);
    }
    if response.status != 301 && response.status != 302 {
        return Err(format!(
            "Could not look up latest release of {owner}/{repo}: HTTP {} ({url}).",
            response.status
        )
        .into());
    }
    let Some(location) = response.header("location") else {
        return Ok(None);
    };
    let Some(tag) = location
        .split("/releases/tag/")
        .nth(1)
        .and_then(|tail| tail.split(['/', '?', '#']).next())
        .filter(|tag| !tag.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(
        decode_uri_component(tag).unwrap_or_else(|| tag.to_string()),
    ))
}

fn resolve_local_path(input: &str, work_dir: &str, home: &str) -> String {
    if input == "~" {
        return home.to_string();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Path::new(home).join(rest).to_string_lossy().into_owned();
    }
    if Path::new(input).is_absolute() {
        return input.to_string();
    }
    resolve_path(Path::new(work_dir), input)
        .to_string_lossy()
        .into_owned()
}

fn required_string(
    value: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<String, WireAsyncError> {
    string_field(value, field).ok_or_else(|| {
        format!("Plugin marketplace entry {} must define \"{field}\".", index + 1).into()
    })
}

fn string_field(value: &Map<String, Value>, field: &str) -> Option<String> {
    let trimmed = value.get(field)?.as_str()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn string_array_field(value: &Map<String, Value>, field: &str) -> Option<Vec<String>> {
    let output = value
        .get(field)?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!output.is_empty()).then_some(output)
}

fn file_url_to_path(value: &str) -> Result<String, WireAsyncError> {
    let url = Url::parse(value)?;
    let path = url
        .to_file_path()
        .map_err(|_| format!("Invalid file URL: {value}"))?;
    Ok(path.to_string_lossy().into_owned())
}

fn default_home() -> String {
    std::env::home_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn resolve_path(base: &Path, input: &str) -> PathBuf {
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Semver {
    major: String,
    minor: String,
    patch: String,
    prerelease: Vec<SemverIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemverIdentifier {
    Numeric(String),
    Text(String),
}

impl Ord for Semver {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_numeric(&self.major, &other.major)
            .then_with(|| compare_numeric(&self.minor, &other.minor))
            .then_with(|| compare_numeric(&self.patch, &other.patch))
            .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

impl PartialOrd for Semver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_semver(value: &str) -> Option<Semver> {
    let mut value = value.trim();
    if let Some(rest) = value.strip_prefix('v').or_else(|| value.strip_prefix('V')) {
        value = rest;
    } else if let Some(rest) = value.strip_prefix('=') {
        value = rest.trim_start();
    }
    let (without_build, build) = value.split_once('+').unwrap_or((value, ""));
    if !build.is_empty() && !valid_identifiers(build, false) {
        return None;
    }
    if value.contains('+') && without_build.contains('+') {
        return None;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .unwrap_or((without_build, ""));
    let core = core.split('.').collect::<Vec<_>>();
    if core.len() != 3 || core.iter().any(|part| !valid_numeric_identifier(part)) {
        return None;
    }
    let prerelease = if prerelease.is_empty() {
        Vec::new()
    } else {
        if !valid_identifiers(prerelease, true) {
            return None;
        }
        prerelease
            .split('.')
            .map(|identifier| {
                if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
                    SemverIdentifier::Numeric(identifier.to_string())
                } else {
                    SemverIdentifier::Text(identifier.to_string())
                }
            })
            .collect()
    };
    Some(Semver {
        major: core[0].to_string(),
        minor: core[1].to_string(),
        patch: core[2].to_string(),
        prerelease,
    })
}

fn valid_numeric_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_identifiers(value: &str, prerelease: bool) -> bool {
    value.split('.').all(|identifier| {
        !identifier.is_empty()
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!prerelease
                || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                || valid_numeric_identifier(identifier))
    })
}

fn compare_numeric(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn compare_prerelease(left: &[SemverIdentifier], right: &[SemverIdentifier]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (true, true) => return Ordering::Equal,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = match (left, right) {
            (SemverIdentifier::Numeric(left), SemverIdentifier::Numeric(right)) => {
                compare_numeric(left, right)
            }
            (SemverIdentifier::Numeric(_), SemverIdentifier::Text(_)) => Ordering::Less,
            (SemverIdentifier::Text(_), SemverIdentifier::Numeric(_)) => Ordering::Greater,
            (SemverIdentifier::Text(left), SemverIdentifier::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

struct ReqwestFetch;

#[async_trait]
impl FetchLike for ReqwestFetch {
    async fn fetch(&self, url: &str, init: FetchInit) -> Result<FetchResponse, FetchError> {
        let mut client = reqwest::Client::builder();
        if init.manual_redirect {
            client = client.redirect(reqwest::redirect::Policy::none());
        }
        let client = client.build()?;
        let method = init
            .method
            .as_deref()
            .unwrap_or("GET")
            .parse::<reqwest::Method>()?;
        let mut request = client.request(method, url);
        if let Some(timeout_ms) = init.timeout_ms {
            request = request.timeout(std::time::Duration::from_millis(timeout_ms));
        }
        for (name, value) in init.headers {
            request = request.header(&name, &value);
        }
        if let Some(body) = init.body {
            request = request.body(body);
        }
        let send = request.send();
        let response = if let Some(signal) = init.signal {
            tokio::select! {
                response = send => response?,
                _ = signal.cancelled() => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Fetch aborted",
                    )));
                }
            }
        } else {
            send.await?
        };
        let status = response.status();
        let status_text = status.canonical_reason().unwrap_or_default().to_string();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let body = response.bytes().await?.to_vec();
        Ok(FetchResponse::from_bytes(
            status.as_u16(),
            status_text,
            headers,
            body,
        ))
    }
}
