use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::region::KimiRegion;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedQuotaEntry {
    pub used_ratio: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedQuotaUsages {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit5h: Option<ManagedQuotaEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit7d: Option<ManagedQuotaEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month_total: Option<ManagedQuotaEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month_code: Option<ManagedQuotaEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoosterWalletInfo {
    pub balance_cents: i64,
    pub total_cents: i64,
    pub monthly_charge_limit_enabled: bool,
    pub monthly_charge_limit_cents: i64,
    pub monthly_used_cents: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedQuota {
    pub usages: ManagedQuotaUsages,
    pub extra_usage: Option<BoosterWalletInfo>,
}

static MANAGED_QUOTA_ENTRY_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["usedRatio"],
        "properties": {
            "usedRatio": {"type": "number"},
            "resetAt": {"type": "string"}
        }
    })
});

static MANAGED_QUOTA_USAGES_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "limit5h": MANAGED_QUOTA_ENTRY_SCHEMA.clone(),
            "limit7d": MANAGED_QUOTA_ENTRY_SCHEMA.clone(),
            "monthTotal": MANAGED_QUOTA_ENTRY_SCHEMA.clone(),
            "monthCode": MANAGED_QUOTA_ENTRY_SCHEMA.clone()
        }
    })
});

static BOOSTER_WALLET_INFO_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": [
            "balanceCents",
            "totalCents",
            "monthlyChargeLimitEnabled",
            "monthlyChargeLimitCents",
            "monthlyUsedCents",
            "currency"
        ],
        "properties": {
            "balanceCents": {"type": "integer"},
            "totalCents": {"type": "integer"},
            "monthlyChargeLimitEnabled": {"type": "boolean"},
            "monthlyChargeLimitCents": {"type": "integer"},
            "monthlyUsedCents": {"type": "integer"},
            "currency": {"type": "string"}
        }
    })
});

static MANAGED_QUOTA_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["usages", "extraUsage"],
        "properties": {
            "usages": MANAGED_QUOTA_USAGES_SCHEMA.clone(),
            "extraUsage": {
                "anyOf": [BOOSTER_WALLET_INFO_SCHEMA.clone(), {"type": "null"}]
            }
        }
    })
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ManagedUsageResult {
    #[serde(rename = "ok")]
    Ok { quota: ManagedQuota },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<i64>,
    },
}

pub static MANAGED_USAGE_RESULT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "quota"],
                "properties": {
                    "kind": {"const": "ok"},
                    "quota": MANAGED_QUOTA_SCHEMA.clone()
                }
            },
            {
                "type": "object",
                "required": ["kind", "message"],
                "properties": {
                    "kind": {"const": "error"},
                    "message": {"type": "string"},
                    "status": {"type": "integer"}
                }
            }
        ],
        "discriminator": {"propertyName": "kind"}
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedUserInfoPhone {
    pub country_code: String,
    pub number: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedUserInfo {
    pub user_id: String,
    pub nickname: String,
    pub status: String,
    pub region: String,
    pub user_level: i64,
    pub user_level_name: String,
    pub domain: i64,
    pub domain_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goods_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<ManagedUserInfoPhone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_time: Option<String>,
}

static MANAGED_USER_INFO_PHONE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["countryCode", "number"],
        "properties": {
            "countryCode": {"type": "string"},
            "number": {"type": "string"}
        }
    })
});

static MANAGED_USER_INFO_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": [
            "userId",
            "nickname",
            "status",
            "region",
            "userLevel",
            "userLevelName",
            "domain",
            "domainName"
        ],
        "properties": {
            "userId": {"type": "string"},
            "nickname": {"type": "string"},
            "status": {"type": "string"},
            "region": {"type": "string"},
            "userLevel": {"type": "integer"},
            "userLevelName": {"type": "string"},
            "domain": {"type": "integer"},
            "domainName": {"type": "string"},
            "goodsVersion": {"type": "integer"},
            "globalId": {"type": "string"},
            "bio": {"type": "string"},
            "avatar": {"type": "string"},
            "username": {"type": "string"},
            "email": {"type": "string"},
            "phone": MANAGED_USER_INFO_PHONE_SCHEMA.clone(),
            "createdTime": {"type": "string"},
            "lastLoginTime": {"type": "string"}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ManagedUserInfoResult {
    #[serde(rename = "ok")]
    Ok {
        #[serde(rename = "userInfo")]
        user_info: ManagedUserInfo,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<i64>,
    },
}

pub static MANAGED_USER_INFO_RESULT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "userInfo"],
                "properties": {
                    "kind": {"const": "ok"},
                    "userInfo": MANAGED_USER_INFO_SCHEMA.clone()
                }
            },
            {
                "type": "object",
                "required": ["kind", "message"],
                "properties": {
                    "kind": {"const": "error"},
                    "message": {"type": "string"},
                    "status": {"type": "integer"}
                }
            }
        ],
        "discriminator": {"propertyName": "kind"}
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthFlowStatus {
    Pending,
    Authenticated,
    Denied,
    Expired,
    Cancelled,
    Failed,
}

impl OAuthFlowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Authenticated => "authenticated",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

pub static OAUTH_FLOW_STATUS_ENUM: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "enum": ["pending", "authenticated", "denied", "expired", "cancelled", "failed"]
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthFlowStartPendingStatus {
    #[serde(rename = "pending")]
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthFlowStartPending {
    pub flow_id: String,
    pub provider: String,
    pub status: OAuthFlowStartPendingStatus,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub user_code: String,
    pub expires_in: u64,
    pub interval: u64,
    #[serde(deserialize_with = "deserialize_iso_date_time")]
    pub expires_at: String,
}

pub static OAUTH_FLOW_START_PENDING_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": [
            "flow_id",
            "provider",
            "status",
            "verification_uri",
            "verification_uri_complete",
            "user_code",
            "expires_in",
            "interval",
            "expires_at"
        ],
        "properties": {
            "flow_id": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "status": {"const": "pending"},
            "verification_uri": {"type": "string", "format": "uri"},
            "verification_uri_complete": {"type": "string", "format": "uri"},
            "user_code": {"type": "string", "minLength": 1},
            "expires_in": {"type": "integer", "minimum": 1},
            "interval": {"type": "integer", "minimum": 1},
            "expires_at": {"type": "string", "format": "date-time"}
        }
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthFlowStartAuthenticatedStatus {
    #[serde(rename = "authenticated")]
    Authenticated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthFlowStartAuthenticated {
    pub flow_id: String,
    pub provider: String,
    pub status: OAuthFlowStartAuthenticatedStatus,
}

pub static OAUTH_FLOW_START_AUTHENTICATED_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["flow_id", "provider", "status"],
        "properties": {
            "flow_id": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "status": {"const": "authenticated"}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OAuthFlowStart {
    Pending(OAuthFlowStartPending),
    Authenticated(OAuthFlowStartAuthenticated),
}

pub static OAUTH_FLOW_START_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "oneOf": [
            OAUTH_FLOW_START_PENDING_SCHEMA.clone(),
            OAUTH_FLOW_START_AUTHENTICATED_SCHEMA.clone()
        ],
        "discriminator": {"propertyName": "status"}
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthFlowSnapshot {
    pub flow_id: String,
    pub provider: String,
    pub status: OAuthFlowStatus,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub user_code: String,
    pub expires_in: u64,
    #[serde(deserialize_with = "deserialize_iso_date_time")]
    pub expires_at: String,
    pub interval: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_iso_date_time"
    )]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

pub static OAUTH_FLOW_SNAPSHOT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": [
            "flow_id",
            "provider",
            "status",
            "verification_uri",
            "verification_uri_complete",
            "user_code",
            "expires_in",
            "expires_at",
            "interval"
        ],
        "properties": {
            "flow_id": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "status": OAUTH_FLOW_STATUS_ENUM.clone(),
            "verification_uri": {"type": "string", "format": "uri"},
            "verification_uri_complete": {"type": "string", "format": "uri"},
            "user_code": {"type": "string", "minLength": 1},
            "expires_in": {"type": "integer", "minimum": 1},
            "expires_at": {"type": "string", "format": "date-time"},
            "interval": {"type": "integer", "minimum": 1},
            "resolved_at": {"type": "string", "format": "date-time"},
            "error_message": {"type": "string"}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthLoginCancelResponse {
    pub cancelled: bool,
    pub status: OAuthFlowStatus,
}

pub static OAUTH_LOGIN_CANCEL_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["cancelled", "status"],
        "properties": {
            "cancelled": {"type": "boolean"},
            "status": OAUTH_FLOW_STATUS_ENUM.clone()
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthLogoutResponse {
    pub logged_out: bool,
    pub provider: String,
}

pub static OAUTH_LOGOUT_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["logged_out", "provider"],
        "properties": {
            "logged_out": {"const": true},
            "provider": {"type": "string", "minLength": 1}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthRegionResult {
    #[serde(
        serialize_with = "serialize_region",
        deserialize_with = "deserialize_region"
    )]
    pub region: KimiRegion,
}

pub static OAUTH_REGION_RESULT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["region"],
        "properties": {
            "region": {"enum": ["mainland-cn", "global"]}
        }
    })
});

fn deserialize_iso_date_time<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    crate::_base::utils::iso_date_time::iso_date_time(&value).map_err(serde::de::Error::custom)
}

fn deserialize_optional_iso_date_time<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            crate::_base::utils::iso_date_time::iso_date_time(&value)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn serialize_region<S>(region: &KimiRegion, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(region.as_str())
}

fn deserialize_region<'de, D>(deserializer: D) -> Result<KimiRegion, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "mainland-cn" => Ok(KimiRegion::MainlandCn),
        "global" => Ok(KimiRegion::Global),
        _ => Err(serde::de::Error::custom("invalid Kimi region")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefreshChange {
    pub provider_id: String,
    pub provider_name: String,
    pub added: u64,
    pub removed: u64,
}

static PROVIDER_REFRESH_CHANGE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["provider_id", "provider_name", "added", "removed"],
        "properties": {
            "provider_id": {"type": "string", "minLength": 1},
            "provider_name": {"type": "string", "minLength": 1},
            "added": {"type": "integer", "minimum": 0},
            "removed": {"type": "integer", "minimum": 0}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefreshFailure {
    pub provider: String,
    pub reason: String,
}

static PROVIDER_REFRESH_FAILURE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["provider", "reason"],
        "properties": {
            "provider": {"type": "string", "minLength": 1},
            "reason": {"type": "string", "minLength": 1}
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshOAuthProviderModelsResponse {
    pub changed: Vec<ProviderRefreshChange>,
    pub unchanged: Vec<String>,
    pub failed: Vec<ProviderRefreshFailure>,
}

pub static REFRESH_OAUTH_PROVIDER_MODELS_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["changed", "unchanged", "failed"],
        "properties": {
            "changed": {"type": "array", "items": PROVIDER_REFRESH_CHANGE_SCHEMA.clone()},
            "unchanged": {"type": "array", "items": {"type": "string", "minLength": 1}},
            "failed": {"type": "array", "items": PROVIDER_REFRESH_FAILURE_SCHEMA.clone()}
        }
    })
});
