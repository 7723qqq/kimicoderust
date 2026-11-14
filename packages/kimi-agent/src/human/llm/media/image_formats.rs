use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderImagePolicy {
    pub accepted_mimes: HashSet<String>,
    pub inline_byte_budget: u64,
}

pub const DEFAULT_INLINE_IMAGE_BYTE_BUDGET: u64 = 3_932_160;

fn baseline_image_policy() -> ProviderImagePolicy {
    ProviderImagePolicy {
        accepted_mimes: ["image/png", "image/jpeg", "image/gif", "image/webp"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        inline_byte_budget: DEFAULT_INLINE_IMAGE_BYTE_BUDGET,
    }
}

fn kimi_image_policy() -> ProviderImagePolicy {
    let baseline = baseline_image_policy();
    let mut accepted_mimes = baseline.accepted_mimes;
    accepted_mimes.extend(
        ["image/bmp", "image/heic", "image/heif"]
            .into_iter()
            .map(str::to_string),
    );
    ProviderImagePolicy {
        accepted_mimes,
        inline_byte_budget: 5 * 1024 * 1024,
    }
}

pub fn provider_image_policy() -> ProviderImagePolicy {
    baseline_image_policy()
}

pub fn provider_image_policy_for(provider: Option<&str>) -> ProviderImagePolicy {
    if provider == Some("kimi") {
        kimi_image_policy()
    } else {
        baseline_image_policy()
    }
}
