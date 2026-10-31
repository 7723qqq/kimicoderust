use super::*;
use std::str::FromStr;

const SAMPLE_CONFIG: &str = r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[providers.anthropic]
type = "anthropic"
api_key = "sk-ant-key"
base_url = "https://api.anthropic.com"

[models.kimi-k2]
provider = "kimi"
model = "kimi-k2-0711"
system_prompt = "You are Kimi."

[agent]
engine = "rust"
native_tools = true
yolo = false

[permission]
mode = "manual"

[[permission.rules]]
decision = "deny"
pattern = "Write(secret.txt)"

[[permission.rules]]
decision = "allow"
pattern = "Read(*)"
"#;

#[test]
fn provider_env_binding_survives_config_roundtrip() {
    let config = KimiConfig::from_str(
        "[providers.example]\ntype = \"openai\"\napi_key_env = \"EXAMPLE_API_KEY\"\n",
    )
    .unwrap();
    let provider = &config.providers["example"];
    let value = serde_json::to_value(provider).unwrap();
    assert_eq!(value["api_key_env"], "EXAMPLE_API_KEY");
    assert!(provider.api_key.is_none());
    let encoded = toml::to_string(provider).unwrap();
    let restored: ProviderConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap()["api_key_env"],
        "EXAMPLE_API_KEY"
    );
}

#[test]
fn test_parse_sample_config() {
    let config = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(config.default_model.as_deref(), Some("kimi-k2"));
    assert_eq!(config.agent.native_tools, Some(true));

    let native_llm = config.extract_native_llm(None).unwrap();
    assert_eq!(native_llm.protocol, "openai");
    assert_eq!(native_llm.provider_type, "openai");
    assert_eq!(native_llm.base_url, "https://api.moonshot.cn/v1");
    assert_eq!(native_llm.api_key, "sk-kimi-key");
    assert_eq!(native_llm.model, "kimi-k2-0711");

    let policy = config.build_policy_snapshot(None);
    assert_eq!(policy.mode, PermissionMode::Manual);
    assert_eq!(policy.deny_rules, vec!["Write(secret.txt)"]);
    assert_eq!(policy.allow_rules, vec!["Read(*)"]);
}

#[test]
fn test_extract_anthropic_llm() {
    let config = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    let native_llm = config.extract_native_llm(Some("anthropic")).unwrap();
    assert_eq!(native_llm.protocol, "anthropic");
    assert_eq!(native_llm.provider_type, "anthropic");
    assert_eq!(native_llm.base_url, "https://api.anthropic.com/v1");
    assert_eq!(native_llm.api_key, "sk-ant-key");
}

/// v2 `ModelRecord.upgrade`: the model-retirement metadata the TUI's
/// migration prompt reads. It is catalog data, not transport config, so it
/// only has to parse and round-trip.
#[test]
fn test_model_upgrade_metadata_parses() {
    let config = KimiConfig::from_str(
        r#"
[models.retiring]
provider = "acme"
model = "old-model"

[models.retiring.upgrade]
model = "new-model"
migration_markdown = "Switch to new-model."
retirement_at = "2026-12-01"
"#,
    )
    .unwrap();
    let upgrade = config.models["retiring"].upgrade.as_ref().unwrap();
    assert_eq!(upgrade.model, "new-model");
    assert_eq!(upgrade.migration_markdown, "Switch to new-model.");
    assert_eq!(upgrade.retirement_at.as_deref(), Some("2026-12-01"));
}

/// The provider family travels with the resolved transport: protocol alone
/// cannot tell Kimi (thinking object) from any other OpenAI-compatible
/// vendor (`reasoning_effort`).
#[test]
fn test_extract_native_llm_carries_the_provider_type() {
    let config = KimiConfig::from_str(
        r#"
[providers.moonshot]
type = "kimi"
api_key = "sk-x"
base_url = "https://api.moonshot.ai/v1"

[models.moonshot-kimi]
provider = "moonshot"
model = "kimi-k3"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(Some("moonshot-kimi")).unwrap();
    assert_eq!(native.provider_type, "kimi");
    assert_eq!(native.protocol, "openai");
}

/// A one-provider config whose provider carries a credential but no
/// endpoint, so the provider type's env/default chain is what resolves.
fn endpoint_less_config(provider_type: &str) -> KimiConfig {
    KimiConfig::from_str(&format!(
        r#"
default_model = "m"

[providers.p]
type = "{provider_type}"
api_key = "sk-x"

[models.m]
provider = "p"
model = "some-model"
"#
    ))
    .unwrap()
}

#[test]
fn anthropic_endpoint_falls_back_to_env_then_default() {
    let keys = ["ANTHROPIC_BASE_URL"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    // No declared endpoint and no env: the SDK default, carried by the
    // engine because it assembles the URL itself.
    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "anthropic");
    assert_eq!(native.base_url, "https://api.anthropic.com/v1");

    // The env endpoint wins over the default and rides the same
    // normalization a declared `base_url` does.
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", "https://ant-proxy.example.com") };
    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://ant-proxy.example.com/v1");

    // A blank env value counts as unset (v2 `read`), so the default stands.
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", "   ") };
    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://api.anthropic.com/v1");

    // A declared endpoint outranks the env variable.
    let declared = KimiConfig::from_str(
        r#"
default_model = "m"

[providers.p]
type = "anthropic"
base_url = "https://declared.example.com"
api_key = "sk-x"

[models.m]
provider = "p"
model = "some-model"
"#,
    )
    .unwrap();
    let native = declared.extract_native_llm(None).unwrap();
    assert_eq!(native.base_url, "https://declared.example.com/v1");

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn openai_endpoint_falls_back_to_env_then_default() {
    let keys = ["OPENAI_BASE_URL"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let native = endpoint_less_config("openai")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "openai");
    assert_eq!(native.base_url, "https://api.openai.com/v1");

    unsafe { std::env::set_var("OPENAI_BASE_URL", "https://oai-proxy.example.com/v1") };
    let native = endpoint_less_config("openai")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://oai-proxy.example.com/v1");

    // The Responses protocol shares the OpenAI declaration.
    unsafe { std::env::set_var("OPENAI_BASE_URL", "   ") };
    let native = endpoint_less_config("openai_responses")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "openai_responses");
    assert_eq!(native.base_url, "https://api.openai.com/v1");

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn kimi_endpoint_falls_back_to_env_then_default() {
    let keys = ["KIMI_BASE_URL"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let native = endpoint_less_config("kimi")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "openai");
    assert_eq!(native.base_url, "https://api.moonshot.ai/v1");

    unsafe { std::env::set_var("KIMI_BASE_URL", "https://kimi-proxy.example.com") };
    let native = endpoint_less_config("kimi")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://kimi-proxy.example.com/v1");

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn google_genai_endpoint_prefers_the_env_over_the_host_root() {
    let keys = ["GOOGLE_GEMINI_BASE_URL"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let native = endpoint_less_config("google-genai")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "google-genai");
    assert_eq!(
        native.base_url,
        "https://generativelanguage.googleapis.com/v1beta"
    );

    unsafe { std::env::set_var("GOOGLE_GEMINI_BASE_URL", "https://gem-proxy.example.com") };
    let native = endpoint_less_config("google-genai")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://gem-proxy.example.com/v1beta");

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn undeclared_provider_type_without_an_endpoint_does_not_resolve() {
    // A type the engine has no endpoint declaration for keeps the old
    // behavior: without a declared `base_url` the model does not resolve
    // natively and the host proxy takes it.
    assert!(
        endpoint_less_config("deepseek")
            .extract_native_llm(None)
            .is_none()
    );
}

#[test]
fn oauth_bound_provider_resolves_without_a_static_key() {
    let config = KimiConfig::from_str(
        r#"
default_model = "managed"

[providers.managed]
type = "openai"
base_url = "https://api.kimi.com/v1"
oauth = { provider = "kimi" }

[models.managed]
provider = "managed"
model = "kimi-k2-0711"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.api_key, "");
    assert_eq!(native.auth_provider.as_deref(), Some("managed"));
}

#[test]
fn provider_without_key_or_oauth_does_not_resolve() {
    let config = KimiConfig::from_str(
        r#"
default_model = "naked"

[providers.naked]
type = "openai"
base_url = "https://api.example.com/v1"

[models.naked]
provider = "naked"
model = "some-model"
"#,
    )
    .unwrap();
    assert!(config.extract_native_llm(None).is_none());
}

#[test]
fn top_level_yolo_resolves_to_yolo_mode() {
    // v1 flat form.
    let flat = KimiConfig::from_str("yolo = true\n").unwrap();
    assert_eq!(flat.build_policy_snapshot(None).mode, PermissionMode::Yolo);
    // The sectioned form keeps working.
    let sectioned = KimiConfig::from_str("[agent]\nyolo = true\n").unwrap();
    assert_eq!(
        sectioned.build_policy_snapshot(None).mode,
        PermissionMode::Yolo
    );
}

#[test]
fn top_level_plan_mode_and_default_plan_mode_parse() {
    let config = KimiConfig::from_str("plan_mode = true\ndefault_plan_mode = true\n").unwrap();
    assert_eq!(config.plan_mode, Some(true));
    assert_eq!(config.default_plan_mode, Some(true));
    // camelCase aliases (the document schema's spelling).
    let camel = KimiConfig::from_str("planMode = true\ndefaultPlanMode = true\n").unwrap();
    assert_eq!(camel.plan_mode, Some(true));
    assert_eq!(camel.default_plan_mode, Some(true));
}

#[test]
fn model_catalog_and_telemetry_parse() {
    let config = KimiConfig::from_str(
            "telemetry = true\n\n[model_catalog]\nrefresh_interval_ms = 3600000\nrefresh_on_start = true\n",
        )
        .unwrap();
    assert_eq!(config.telemetry, Some(true));
    let catalog = config.model_catalog.expect("model_catalog parsed");
    assert_eq!(catalog.refresh_interval_ms, Some(3_600_000));
    assert_eq!(catalog.refresh_on_start, Some(true));
}

#[test]
fn auto_session_title_parses_and_defaults_to_enabled() {
    // Absent means enabled (upstream #3962: only an explicit false
    // disables); both spellings parse.
    let absent = KimiConfig::from_str("yolo = true\n").unwrap();
    assert_eq!(absent.auto_session_title, None);
    let off = KimiConfig::from_str("auto_session_title = false\n").unwrap();
    assert_eq!(off.auto_session_title, Some(false));
    let camel = KimiConfig::from_str("autoSessionTitle = false\n").unwrap();
    assert_eq!(camel.auto_session_title, Some(false));
}

const POOL_CONFIG: &str = r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[providers.anthropic]
type = "anthropic"
api_key = "sk-ant-key"
base_url = "https://api.anthropic.com"

[models.kimi-k2]
provider = "kimi"
model = "kimi-k2-0711"
capabilities = ["thinking"]

[models.fast]
provider = "kimi"
model = "kimi-k2-fast"
capabilities = ["thinking"]

[models.thinky]
provider = "anthropic"
model = "claude-sonnet"
capabilities = ["thinking"]
"#;

#[test]
fn test_extract_secondary_model_pool() {
    let config = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "fast"
default_effort = "high"

[secondary_model.models]
"kimi-k2" = "Hard problems."
"fast" = "Cheap and quick."
"thinky" = ""
"#
    ))
    .unwrap();
    let pool = config.extract_secondary_model_pool(None).unwrap().unwrap();
    assert!(!pool.force);
    assert_eq!(pool.default_model, "fast");
    assert_eq!(pool.caller_model_alias.as_deref(), Some("kimi-k2"));

    let fast = pool
        .models
        .iter()
        .find(|entry| entry.alias == "fast")
        .unwrap();
    assert_eq!(fast.hint, "Cheap and quick.");
    assert_eq!(fast.llm.model, "kimi-k2-fast");
    assert_eq!(fast.llm.reasoning_effort.as_deref(), Some("high"));

    let thinky = pool
        .models
        .iter()
        .find(|entry| entry.alias == "thinky")
        .unwrap();
    assert_eq!(thinky.llm.protocol, "anthropic");
    assert_eq!(thinky.llm.thinking_budget, Some(32000));

    // The session's own alias (`--model`) wins the `[main model]` marker.
    let pool = config
        .extract_secondary_model_pool(Some("thinky"))
        .unwrap()
        .unwrap();
    assert_eq!(pool.caller_model_alias.as_deref(), Some("thinky"));
}

/// `[agent].multi_llm` resolves each named alias into a native transport.
/// That is what makes the race runnable at all: a racer without one is a
/// host proxy, and every config-reading entry point answers `host/llm_chat`
/// with an error, so such a race can never produce a winner.
#[test]
fn test_extract_multi_llm_resolves_native_racers() {
    let config = KimiConfig::from_str(
        r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[providers.anthropic]
type = "anthropic"
api_key = "sk-ant-key"
base_url = "https://api.anthropic.com"

[models.kimi-k2]
provider = "kimi"
model = "kimi-k2-0711"
default_effort = "high"
capabilities = ["thinking"]

[models.thinky]
provider = "anthropic"
model = "claude-sonnet"
capabilities = ["thinking"]

[agent]
multi_llm = ["kimi-k2", "thinky"]
"#,
    )
    .unwrap();
    let racers = config.extract_multi_llm(None).unwrap().unwrap();
    assert_eq!(racers.len(), 2);

    let kimi = racers.iter().find(|racer| racer.name == "kimi-k2").unwrap();
    assert_eq!(kimi.llm.model, "kimi-k2-0711");
    assert_eq!(kimi.llm.protocol, "openai");
    // The alias's own declared effort rides its transport, not the session's.
    assert_eq!(kimi.llm.reasoning_effort.as_deref(), Some("high"));

    let thinky = racers.iter().find(|racer| racer.name == "thinky").unwrap();
    assert_eq!(thinky.llm.protocol, "anthropic");
    // `thinky` declares none, so it carries none — the racers are different
    // models with their own capabilities and must not inherit each other's.
    assert!(thinky.llm.reasoning_effort.is_none());
    assert!(thinky.llm.thinking_budget.is_none());
}

#[test]
fn test_extract_multi_llm_absent_and_inert_cases() {
    // Absent key: no race, which is the default.
    let bare = KimiConfig::from_str(POOL_CONFIG).unwrap();
    assert!(bare.extract_multi_llm(None).unwrap().is_none());

    // An empty list is inert rather than a one-racer race.
    let empty = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = []
"#
    ))
    .unwrap();
    assert!(empty.extract_multi_llm(None).unwrap().is_none());

    // Blank entries are trimmed away, so `["", "fast"]` is a lone racer.
    let blanks = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = ["", "  "]
"#
    ))
    .unwrap();
    assert!(blanks.extract_multi_llm(None).unwrap().is_none());
}

#[test]
fn test_extract_multi_llm_rejects_a_lone_racer_and_unknown_alias() {
    // One entry is not a race; silently racing a single provider would let
    // `providers` outrank `native_llm` for no benefit.
    let lone = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = ["kimi-k2"]
"#
    ))
    .unwrap();
    let error = lone.extract_multi_llm(None).unwrap_err();
    assert!(error.contains("at least two"), "unexpected error: {error}");

    // An alias that cannot resolve names itself instead of being dropped.
    let unknown = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = ["kimi-k2", "not-a-model"]
"#
    ))
    .unwrap();
    let error = unknown.extract_multi_llm(None).unwrap_err();
    assert!(
        error.contains("not-a-model"),
        "the unresolvable alias must be named: {error}"
    );
}

#[test]
fn test_secondary_model_pool_shapes_and_errors() {
    let lone = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "fast"
"#
    ))
    .unwrap();
    let pool = lone.extract_secondary_model_pool(None).unwrap().unwrap();
    assert_eq!(pool.default_model, "fast");
    assert_eq!(pool.models.len(), 1);
    assert_eq!(pool.models[0].alias, "fast");
    assert_eq!(pool.models[0].hint, "");

    let recipe = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
model = "fast"
"#
    ))
    .unwrap();
    let pool = recipe.extract_secondary_model_pool(None).unwrap().unwrap();
    assert_eq!(pool.default_model, "fast");
    assert_eq!(pool.models.len(), 1);

    // A section with no pool keys (patch-only recipe) stays inert.
    let inert = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_effort = "high"
"#
    ))
    .unwrap();
    assert!(inert.extract_secondary_model_pool(None).unwrap().is_none());
    assert!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .extract_secondary_model_pool(None)
            .unwrap()
            .is_none()
    );

    let unknown_default = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "missing"

[secondary_model.models]
"fast" = ""
"#
    ))
    .unwrap();
    let error = unknown_default
        .extract_secondary_model_pool(None)
        .unwrap_err();
    assert!(
        error.contains("is not a [secondary_model.models] key"),
        "{error}"
    );

    let force_with_table = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "fast"
force = true

[secondary_model.models]
"fast" = ""
"#
    ))
    .unwrap();
    let error = force_with_table
        .extract_secondary_model_pool(None)
        .unwrap_err();
    assert!(error.contains("force cannot be combined"), "{error}");

    let force_without_default = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
force = true
"#
    ))
    .unwrap();
    let error = force_without_default
        .extract_secondary_model_pool(None)
        .unwrap_err();
    assert!(
        error.contains("required when [secondary_model].force is set"),
        "{error}"
    );

    let reserved = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "primary"

[secondary_model.models]
primary = ""
"#
    ))
    .unwrap();
    let error = reserved.extract_secondary_model_pool(None).unwrap_err();
    assert!(error.contains("reserved"), "{error}");

    let unresolvable = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
default_model = "nope"

[secondary_model.models]
"nope" = ""
"#
    ))
    .unwrap();
    let error = unresolvable.extract_secondary_model_pool(None).unwrap_err();
    assert!(error.contains("could not be resolved"), "{error}");
}

#[test]
fn test_resolve_max_attempts_per_step() {
    let keys = [
        "KIMI_LOOP_MAX_ATTEMPTS_PER_STEP",
        "KIMI_LOOP_MAX_RETRIES_PER_STEP",
    ];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    // The env vars outrank the file: clear them first so the config-only
    // precedence below is not poisoned by the developer's shell.
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let explicit = KimiConfig::from_str(
        r#"
[loop_control]
max_attempts_per_step = 3
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_max_attempts_per_step(), Some(3));

    // The deprecated spelling still resolves, but the current key wins.
    let deprecated = KimiConfig::from_str(
        r#"
[loop_control]
max_retries_per_step = 5
"#,
    )
    .unwrap();
    assert_eq!(deprecated.resolve_max_attempts_per_step(), Some(5));

    let both = KimiConfig::from_str(
        r#"
[loop_control]
max_attempts_per_step = 3
max_retries_per_step = 5
"#,
    )
    .unwrap();
    assert_eq!(both.resolve_max_attempts_per_step(), Some(3));

    // An absent (or empty) section keeps the engine default.
    assert_eq!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .resolve_max_attempts_per_step(),
        None
    );
    assert_eq!(
        KimiConfig::from_str("[loop_control]\n")
            .unwrap()
            .resolve_max_attempts_per_step(),
        None
    );

    unsafe {
        std::env::set_var(keys[0], "7");
        std::env::set_var(keys[1], "9");
    }
    assert_eq!(both.resolve_max_attempts_per_step(), Some(7), "env wins");
    unsafe { std::env::remove_var(keys[0]) };
    assert_eq!(
        both.resolve_max_attempts_per_step(),
        Some(9),
        "the deprecated env is the next source"
    );
    unsafe { std::env::remove_var(keys[1]) };
    assert_eq!(both.resolve_max_attempts_per_step(), Some(3), "config next");

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// v2 #3750: `[loop_control].compaction_max_attempts` caps one compaction
/// round's total requests. Upstream binds no env var to the key, so the
/// file is the only source; the schema floor is 1, so a `0` is ignored
/// rather than read as "no attempts at all".
#[test]
fn test_resolve_compaction_max_attempts() {
    let explicit = KimiConfig::from_str(
        r#"
[loop_control]
compaction_max_attempts = 3
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_compaction_max_attempts(), Some(3));

    // The camelCase spelling the TS schema writes round-trips too.
    let camel = KimiConfig::from_str(
        r#"
[loop_control]
compactionMaxAttempts = 4
"#,
    )
    .unwrap();
    assert_eq!(camel.resolve_compaction_max_attempts(), Some(4));

    // Unset, empty, and below the schema floor all keep the engine default.
    for raw in [
        SAMPLE_CONFIG,
        "[loop_control]\n",
        "[loop_control]\ncompaction_max_attempts = 0\n",
    ] {
        assert_eq!(
            KimiConfig::from_str(raw)
                .unwrap()
                .resolve_compaction_max_attempts(),
            None,
            "{raw}"
        );
    }
}

/// v2 #3785 `assertValidSubagentDefaultEffort`: a pool-wide
/// `default_effort` must be one every pool model can run, so a typo fails
/// at startup instead of silently degrading the subagent's thinking.
#[test]
fn test_secondary_model_default_effort_validation() {
    const EFFORT_CONFIG: &str = r#"
[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[models.fast]
provider = "kimi"
model = "kimi-k2-fast"
"#;
    let config = |effort: &str, model_extra: &str| {
        KimiConfig::from_str(&format!(
            r#"{EFFORT_CONFIG}{model_extra}
[secondary_model]
default_model = "fast"
default_effort = "{effort}"

[secondary_model.models]
"fast" = ""
"#
        ))
        .unwrap()
    };

    // A thinking model that declares no effort list accepts any effort.
    let no_list = config("xhigh", "capabilities = [\"thinking\"]\n");
    assert!(no_list.extract_secondary_model_pool(None).is_ok());

    // A declared list bounds it, and the error names the model and the
    // supported set.
    let bounded = config(
        "xhigh",
        "capabilities = [\"thinking\"]\nsupport_efforts = [\"low\", \"high\", \"max\"]\n",
    );
    let error = bounded.extract_secondary_model_pool(None).unwrap_err();
    assert!(
        error.contains("[secondary_model].default_effort \"xhigh\"")
            && error.contains("\"fast\"")
            && error.contains("low, high, max"),
        "{error}"
    );

    // A model without thinking support rejects any concrete effort...
    let plain = config("xhigh", "capabilities = [\"tools\"]\n");
    let error = plain.extract_secondary_model_pool(None).unwrap_err();
    assert!(error.contains("does not support thinking"), "{error}");

    // ...but `off` is always acceptable for it.
    let plain_off = config("off", "capabilities = [\"tools\"]\n");
    assert!(plain_off.extract_secondary_model_pool(None).is_ok());

    // `off` cannot silence a model that always reasons.
    let always = config(
        "off",
        "capabilities = [\"thinking\", \"always_thinking\"]\n",
    );
    let error = always.extract_secondary_model_pool(None).unwrap_err();
    assert!(
        error.contains("cannot disable thinking") && error.contains("\"fast\""),
        "{error}"
    );

    // `adaptive_thinking` alone counts as thinking support.
    let adaptive = config("high", "adaptive_thinking = true\n");
    assert!(adaptive.extract_secondary_model_pool(None).is_ok());

    // The effort is normalized before comparison, so casing and padding
    // do not turn a supported effort into a failure.
    let padded = config(
        "  HIGH  ",
        "capabilities = [\"thinking\"]\nsupport_efforts = [\"low\", \"high\"]\n",
    );
    assert!(padded.extract_secondary_model_pool(None).is_ok());

    // An unset effort skips the check entirely.
    let unset = KimiConfig::from_str(&format!(
        r#"{EFFORT_CONFIG}
[secondary_model]
default_model = "fast"
"#
    ))
    .unwrap();
    assert!(unset.extract_secondary_model_pool(None).is_ok());
}

/// v2 #3681 `collectMalformedModelEntries`: a `[models]` entry without a
/// `model` field cannot resolve, and the usual cause is an unquoted dotted
/// alias — the warning spells out the quoted table name to write instead.
#[test]
fn test_malformed_model_entries_warn() {
    let warnings = KimiConfig::malformed_model_entries(
        r#"
[models.good]
provider = "kimi"
model = "kimi-k2"

[models."dotted.alias"]
provider = "kimi"
model = "kimi-k2"

[models.typo]
provider = "kimi"
max_context_size = 1000

[models.unquoted.dotted]
provider = "kimi"
model = "kimi-k2"
"#,
    );
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].contains("[models] entry 'typo' is missing the 'model' field")
            && warnings[0].ends_with('.'),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[1].contains("[models] entry 'unquoted' is missing the 'model' field")
            && warnings[1].contains(r#"[models."unquoted.dotted"]"#),
        "{}",
        warnings[1]
    );

    // A config with no `[models]` table, or one that is not TOML at all,
    // produces nothing rather than a second error.
    assert!(KimiConfig::malformed_model_entries("default_model = \"k2\"\n").is_empty());
    assert!(KimiConfig::malformed_model_entries("not = = toml").is_empty());
}

/// The same warnings the load path logs are staged on the config, which is
/// what gives the server something to broadcast as `event.config.warning`
/// (and the v3 `config.warning` entity) once a lane exists.
#[test]
fn from_file_stages_the_malformed_model_entry_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[models.good]
provider = "kimi"
model = "kimi-k2"

[models.typo]
provider = "kimi"
max_context_size = 1000
"#,
    )
    .unwrap();

    let config = KimiConfig::from_file(&path).unwrap();
    assert_eq!(
        config.config_warnings.len(),
        1,
        "{:?}",
        config.config_warnings
    );
    assert!(
        config.config_warnings[0].contains("[models] entry 'typo' is missing"),
        "{}",
        config.config_warnings[0]
    );

    // A file whose `[models]` entries all resolve stages nothing, so the
    // server has no empty warning entity to broadcast.
    std::fs::write(
        &path,
        "[models.good]\nprovider = \"kimi\"\nmodel = \"kimi-k2\"\n",
    )
    .unwrap();
    assert!(
        KimiConfig::from_file(&path)
            .unwrap()
            .config_warnings
            .is_empty()
    );

    // Parsing alone cannot see the raw TOML an entry's shape lives in, so
    // the staged list stays empty on that path.
    assert!(
        KimiConfig::from_str("[models.typo]\nprovider = \"kimi\"\n")
            .unwrap()
            .config_warnings
            .is_empty()
    );
}

#[test]
fn test_resolve_max_steps_per_turn() {
    let key = "KIMI_LOOP_MAX_STEPS_PER_TURN";
    let saved = std::env::var(key).ok();
    unsafe { std::env::remove_var(key) };

    let explicit = KimiConfig::from_str(
        r#"
[loop_control]
max_steps_per_turn = 25
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_max_steps_per_turn(), Some(25));

    // The deprecated rename still resolves, but the current key wins.
    let deprecated = KimiConfig::from_str(
        r#"
[loop_control]
max_steps_per_run = 30
"#,
    )
    .unwrap();
    assert_eq!(deprecated.resolve_max_steps_per_turn(), Some(30));

    let both = KimiConfig::from_str(
        r#"
[loop_control]
max_steps_per_turn = 25
max_steps_per_run = 30
"#,
    )
    .unwrap();
    assert_eq!(both.resolve_max_steps_per_turn(), Some(25));

    // `0` means unlimited, exactly like an unset value.
    let unlimited = KimiConfig::from_str(
        r#"
[loop_control]
max_steps_per_turn = 0
"#,
    )
    .unwrap();
    assert_eq!(unlimited.resolve_max_steps_per_turn(), None);

    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(absent.resolve_max_steps_per_turn(), None);

    unsafe { std::env::set_var(key, "12") };
    assert_eq!(both.resolve_max_steps_per_turn(), Some(12), "env wins");
    unsafe { std::env::set_var(key, "0") };
    assert_eq!(
        both.resolve_max_steps_per_turn(),
        None,
        "an env 0 means unlimited and outranks the config"
    );

    unsafe {
        match saved {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

const SERVICES_CONFIG: &str = r#"
[services.moonshot_search]
base_url = "https://search.example.test/v1"
api_key = "sk-search"
custom_headers = { "X-Trace" = "t1" }

[services.moonshotFetch]
baseUrl = "https://fetch.example.test/v1"
apiKey = "sk-fetch"

[services.bing_api]
base_url = "https://api.bing.microsoft.com/v7.0/search"
api_key = "sk-bing"
"#;

#[test]
fn test_parse_services_config() {
    // Snake_case is the file contract; the camelCase section + keys parse
    // too (the host schema accepts both spellings).
    let config = KimiConfig::from_str(SERVICES_CONFIG).unwrap();
    let search = config.services.moonshot_search.as_ref().unwrap();
    assert_eq!(
        search.base_url.as_deref(),
        Some("https://search.example.test/v1")
    );
    assert_eq!(search.api_key.as_deref(), Some("sk-search"));
    assert_eq!(
        search
            .custom_headers
            .as_ref()
            .unwrap()
            .get("X-Trace")
            .map(String::as_str),
        Some("t1")
    );
    let fetch = config.services.moonshot_fetch.as_ref().unwrap();
    assert_eq!(
        fetch.base_url.as_deref(),
        Some("https://fetch.example.test/v1")
    );
    assert_eq!(fetch.api_key.as_deref(), Some("sk-fetch"));
    let bing = config.services.bing_api.as_ref().unwrap();
    assert_eq!(
        bing.base_url.as_deref(),
        Some("https://api.bing.microsoft.com/v7.0/search")
    );
    assert_eq!(bing.api_key.as_deref(), Some("sk-bing"));
    // An absent section stays inert.
    assert!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .services
            .moonshot_search
            .is_none()
    );
}

#[test]
fn test_resolve_bing_api_service() {
    let config = KimiConfig::from_str(SERVICES_CONFIG).unwrap();
    let bing = config.resolve_bing_api_service().unwrap();
    assert_eq!(bing.base_url, "https://api.bing.microsoft.com/v7.0/search");
    assert_eq!(bing.api_key.as_deref(), Some("sk-bing"));

    // Env overlay: an env base URL is a credential boundary (mirrors the
    // moonshot search resolution).
    unsafe {
        std::env::set_var("KIMI_BING_BASE_URL", "https://env.example.test/bing");
        std::env::set_var("KIMI_BING_API_KEY", "sk-env-bing");
    }
    let bing = config.resolve_bing_api_service().unwrap();
    assert_eq!(bing.base_url, "https://env.example.test/bing");
    assert_eq!(bing.api_key.as_deref(), Some("sk-env-bing"));
    unsafe {
        std::env::remove_var("KIMI_BING_BASE_URL");
        std::env::remove_var("KIMI_BING_API_KEY");
    }

    // An absent section resolves to None.
    assert!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .resolve_bing_api_service()
            .is_none()
    );
}

#[test]
fn test_resolve_web_services_env_overlay() {
    let keys = [
        "KIMI_WEB_SEARCH_BASE_URL",
        "KIMI_WEB_SEARCH_API_KEY",
        "KIMI_WEB_FETCH_BASE_URL",
        "KIMI_WEB_FETCH_API_KEY",
    ];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    // The env vars outrank the file: clear them first so the config-only
    // precedence below is not poisoned by the developer's shell.
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let config = KimiConfig::from_str(SERVICES_CONFIG).unwrap();
    let search = config.resolve_web_search_service().unwrap();
    assert_eq!(search.base_url, "https://search.example.test/v1");
    assert_eq!(search.api_key.as_deref(), Some("sk-search"));
    assert_eq!(search.custom_headers.len(), 1);

    // An env api key outranks the persisted one; config headers stay.
    unsafe { std::env::set_var("KIMI_WEB_SEARCH_API_KEY", "sk-env") };
    let search = config.resolve_web_search_service().unwrap();
    assert_eq!(search.api_key.as_deref(), Some("sk-env"));
    assert_eq!(search.custom_headers.len(), 1);

    // An env base URL is a credential boundary: the persisted key and
    // headers never cross into it, and a blank env key is unset rather
    // than the config key.
    unsafe {
        std::env::set_var(
            "KIMI_WEB_SEARCH_BASE_URL",
            "https://env.example.test/search",
        );
        std::env::set_var("KIMI_WEB_SEARCH_API_KEY", "  ");
    }
    let search = config.resolve_web_search_service().unwrap();
    assert_eq!(search.base_url, "https://env.example.test/search");
    assert_eq!(search.api_key, None);
    assert!(search.custom_headers.is_empty());

    // The fetch entry resolves independently of the search env overlay.
    assert_eq!(
        config
            .resolve_web_fetch_service()
            .unwrap()
            .api_key
            .as_deref(),
        Some("sk-fetch")
    );

    // A blank base URL (and an absent entry) resolve to None.
    unsafe { std::env::remove_var("KIMI_WEB_SEARCH_BASE_URL") };
    let blank = KimiConfig::from_str(
        r#"
[services.moonshot_search]
base_url = "   "
api_key = "sk-search"
"#,
    )
    .unwrap();
    assert!(blank.resolve_web_search_service().is_none());
    assert!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .resolve_web_search_service()
            .is_none()
    );

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn test_resolve_subagent_and_swarm_timeouts() {
    let keys = ["KIMI_SUBAGENT_TIMEOUT_MS", "KIMI_CODE_SWARM_TIMEOUT_MS"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let config = KimiConfig::from_str(
        r#"
[subagent]
timeout_ms = 5000

[swarm]
timeoutMs = 7000
"#,
    )
    .unwrap();
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(5000));
    assert_eq!(config.resolve_swarm_timeout_ms(), Some(7000));

    // The env overlay outranks the file and each knob is independent.
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "9000") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(9000));
    assert_eq!(config.resolve_swarm_timeout_ms(), Some(7000));
    unsafe { std::env::set_var("KIMI_CODE_SWARM_TIMEOUT_MS", "8000") };
    assert_eq!(config.resolve_swarm_timeout_ms(), Some(8000));
    unsafe { std::env::remove_var("KIMI_CODE_SWARM_TIMEOUT_MS") };

    // `0` passes through — the tools read it as the engine default,
    // matching the host-driven paths; an invalid env value falls back to
    // the file.
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "0") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(0));
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "-1") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(5000));
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "soon") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(5000));

    // Absent sections resolve to None (the engine keeps its 2h default).
    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(absent.resolve_subagent_timeout_ms(), None);
    assert_eq!(absent.resolve_swarm_timeout_ms(), None);

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn provider_custom_headers_reach_the_resolved_llm() {
    // Self-hosted gateways carry their auth/routing in custom headers;
    // without this the deployment 401s on every request.
    let config = KimiConfig::from_str(
        r#"
default_model = "gw"

[providers.gw]
type = "openai"
base_url = "https://gw.internal/v1"
api_key = "k"
customHeaders = { "X-Gateway-Key" = "abc", "X-Tenant" = "t1" }

[models.gw]
provider = "gw"
model = "gpt"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(
        native
            .custom_headers
            .get("X-Gateway-Key")
            .map(String::as_str),
        Some("abc")
    );
    assert_eq!(
        native.custom_headers.get("X-Tenant").map(String::as_str),
        Some("t1")
    );

    // Snake_case is the file contract; both spellings must parse.
    let snake = KimiConfig::from_str(
        r#"
default_model = "gw"

[providers.gw]
type = "openai"
base_url = "https://gw.internal/v1"
api_key = "k"
custom_headers = { "X-Gateway-Key" = "abc" }

[models.gw]
provider = "gw"
model = "gpt"
"#,
    )
    .unwrap();
    assert_eq!(
        snake
            .extract_native_llm(None)
            .unwrap()
            .custom_headers
            .get("X-Gateway-Key")
            .map(String::as_str),
        Some("abc")
    );
}

#[test]
fn alias_base_url_and_protocol_override_the_provider() {
    let config = KimiConfig::from_str(
        r#"
default_model = "kimi"

[providers.moonshot]
type = "openai"
base_url = "https://api.moonshot.ai/v1"
api_key = "k"

[models.kimi]
provider = "moonshot"
model = "kimi-k2"
baseUrl = "https://gateway.example.com/v1"
protocol = "anthropic"
betaApi = true
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    // A declared alias endpoint wins over the provider default.
    assert_eq!(native.base_url, "https://gateway.example.com/v1");
    // The alias's wire protocol wins over the provider type.
    assert_eq!(native.protocol, "anthropic");
    assert!(native.beta_api);

    // Without an alias override the provider decides, and the default is
    // Chat Completions.
    let plain = KimiConfig::from_str(
        r#"
default_model = "kimi"

[providers.moonshot]
type = "openai"
base_url = "https://api.moonshot.ai/v1"
api_key = "k"

[models.kimi]
provider = "moonshot"
model = "kimi-k2"
"#,
    )
    .unwrap();
    let native = plain.extract_native_llm(None).unwrap();
    assert_eq!(native.base_url, "https://api.moonshot.ai/v1");
    assert_eq!(native.protocol, "openai");
    assert!(!native.beta_api);
}

#[test]
fn vertexai_provider_type_resolves_the_google_protocol() {
    // The model catalog maps `vertexai` to `google-genai`; the endpoint
    // resolver must not answer differently, or a Vertex-shaped URL gets
    // an OpenAI body. The endpoint declaration table still excludes it
    // (no Vertex wire protocol, no env fallback) — only an explicitly
    // declared `base_url` resolves.
    let config = KimiConfig::from_str(
        r#"
default_model = "v"

[providers.vertex]
type = "vertexai"
base_url = "https://us-central1-aiplatform.googleapis.com"
api_key = "k"

[models.v]
provider = "vertex"
model = "gemini-2.5-pro"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.protocol, "google-genai");
    assert_eq!(
        native.base_url,
        "https://us-central1-aiplatform.googleapis.com/v1beta"
    );

    // Without a declared endpoint it still does not resolve: Vertex has
    // no default host (it is regional) and no env fallback here.
    let bare = KimiConfig::from_str(
        r#"
default_model = "v"

[providers.vertex]
type = "vertexai"
api_key = "k"

[models.v]
provider = "vertex"
model = "gemini-2.5-pro"
"#,
    )
    .unwrap();
    assert!(bare.extract_native_llm(None).is_none());
}

#[test]
fn tools_switch_and_effort_reach_the_engine() {
    let config = KimiConfig::from_str(
        r#"
[tools]
enabled = ["Read", "Bash", "mcp__github__*"]
disabled = ["Write"]

[thinking]
effort = "high"
"#,
    )
    .unwrap();
    let snapshot = config.build_policy_snapshot(None);
    let filter = snapshot
        .tools_filter
        .as_ref()
        .expect("the [tools] switch must reach the snapshot");
    assert!(filter.allows("Read"));
    assert!(filter.allows("mcp__github__search"), "MCP names are globs");
    assert!(!filter.allows("Write"));
    // Thinking on: the chosen effort goes on the wire.
    assert_eq!(config.resolve_effort(None).as_deref(), Some("high"));

    // No switch at all leaves the filter unset (every tool survives).
    let bare = KimiConfig::from_str("default_model = \"m\"\n").unwrap();
    assert!(bare.build_policy_snapshot(None).tools_filter.is_none());
    assert_eq!(bare.resolve_effort(None), None);
}

#[test]
fn thinking_off_sends_the_model_off_effort() {
    // Models whose default is to reason need the "off" effort sent
    // explicitly instead of an omitted field (schema `offEffort`).
    let config = KimiConfig::from_str("[thinking]\nenabled = false\n").unwrap();
    assert_eq!(config.resolve_effort(Some("none")).as_deref(), Some("none"));
    // No declared off-effort: the host-level off token still travels, so a
    // Kimi transport can encode its protocol-level `thinking: disabled`.
    assert_eq!(config.resolve_effort(None).as_deref(), Some("off"));

    let on = KimiConfig::from_str("[thinking]\nenabled = true\neffort = \"low\"\n").unwrap();
    assert_eq!(on.resolve_effort(Some("none")).as_deref(), Some("low"));
}

/// v2 `LoopControlSchema`: the two compaction knobs the engine accepts for
/// config parity but does not read yet. Out-of-range values are discarded
/// (not clamped), which resolves to the engine default instead of
/// triggering every step.
#[test]
fn loop_control_compaction_knobs_parse_and_ignore_out_of_range() {
    let empty = KimiConfig::from_str("").unwrap();
    assert_eq!(empty.resolve_compaction_trigger_ratio(), None);
    assert_eq!(empty.resolve_reserved_context_size(), None);

    let config = KimiConfig::from_str(
        "[loop_control]
compaction_trigger_ratio = 0.7
reserved_context_size = 8000
",
    )
    .unwrap();
    assert_eq!(config.resolve_compaction_trigger_ratio(), Some(0.7));
    assert_eq!(config.resolve_reserved_context_size(), Some(8000));

    // Schema range is 0.5..=0.99; outside it is ignored, not clamped.
    for raw in ["0.1", "1.5"] {
        let bad = KimiConfig::from_str(&format!(
            "[loop_control]
compaction_trigger_ratio = {raw}
"
        ))
        .unwrap();
        assert_eq!(
            bad.resolve_compaction_trigger_ratio(),
            None,
            "{raw} is outside the v2 schema range"
        );
    }

    // camelCase aliases accepted, as elsewhere in this file.
    let aliased = KimiConfig::from_str(
        "[loop_control]
compactionTriggerRatio = 0.9
",
    )
    .unwrap();
    assert_eq!(aliased.resolve_compaction_trigger_ratio(), Some(0.9));
}

#[test]
fn cron_section_switches_read_and_default_off() {
    // v2 `DEFAULT_CRON_CONFIG`: every switch defaults to false.
    let empty = KimiConfig::from_str("").unwrap();
    assert!(!empty.cron_disabled());
    assert!(!empty.cron_no_jitter());
    assert!(!empty.cron_no_stale());

    let config = KimiConfig::from_str(
        "[cron]
disabled = true
no_jitter = true
no_stale = true
",
    )
    .unwrap();
    assert!(config.cron_disabled());
    assert!(config.cron_no_jitter());
    assert!(config.cron_no_stale());
}

/// The camelCase spelling the v2 config section writes (`noJitter` /
/// `noStale`) has to deserialize too: `[cron]` is parsed without
/// `deny_unknown_fields`, so a missing alias drops the key silently —
/// the switch reads as `false` with no error anywhere.
#[test]
fn cron_section_accepts_camel_case_keys() {
    let camel = KimiConfig::from_str(
        "[cron]
noJitter = true
noStale = true
",
    )
    .unwrap();
    assert!(camel.cron_no_jitter());
    assert!(camel.cron_no_stale());
    // Asserted on the field too: the resolvers read env vars first, so
    // only the parsed value proves the alias landed.
    assert_eq!(camel.cron.no_jitter, Some(true));
    assert_eq!(camel.cron.no_stale, Some(true));
}

#[test]
fn permission_rule_reasons_reach_the_denial() {
    let config = KimiConfig::from_str(
        r#"
[[permission.rules]]
decision = "deny"
pattern = "Write(*.env)"
reason = "secrets must never be written by the agent"
scope = "project"
"#,
    )
    .unwrap();
    let snapshot = config.build_policy_snapshot(None);
    assert_eq!(
        snapshot
            .rule_reasons
            .get("Write(*.env)")
            .map(String::as_str),
        Some("secrets must never be written by the agent")
    );
    // The engine echoes it in the verdict.
    let verdict = crate::permission::PermissionEngine::new(snapshot)
        .evaluate("Write", &serde_json::json!({ "path": "prod.env" }));
    assert!(
        verdict
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("secrets must never be written by the agent"),
        "{:?}",
        verdict.reason
    );
}

#[test]
fn experimental_flags_and_default_mode_parse() {
    // `[experimental]` is a free boolean/string map: clients probe it to
    // gate UI features, so the engine must carry it verbatim.
    let config = KimiConfig::from_str(
        r#"
default_permission_mode = "auto"

[experimental]
"secondary-model" = true
flavor = "canary"
"#,
    )
    .unwrap();
    assert_eq!(
        config.experimental.get("secondary-model"),
        Some(&ExperimentalValue::Bool(true))
    );
    assert_eq!(
        config.experimental.get("flavor"),
        Some(&ExperimentalValue::String("canary".into()))
    );
    // `default_permission_mode` is the mode when `[permission].mode` is
    // unset; `[permission].mode` still wins over it.
    assert_eq!(
        config.build_policy_snapshot(None).mode,
        crate::permission::PermissionMode::Auto
    );
    let explicit = KimiConfig::from_str(
        r#"
default_permission_mode = "auto"

[permission]
mode = "manual"
"#,
    )
    .unwrap();
    assert_eq!(
        explicit.build_policy_snapshot(None).mode,
        crate::permission::PermissionMode::Manual
    );
}

#[test]
fn extra_skill_dirs_are_scanned() {
    let dir = std::env::temp_dir().join(format!("kimi-skills-{}", std::process::id()));
    let skill = dir.join("my-extra-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: my-extra-skill\ndescription: From an extra dir\n---\n\nBody.\n",
    )
    .unwrap();

    let config = KimiConfig::from_str(&format!(
        "extra_skill_dirs = [\"{}\"]\n",
        dir.to_string_lossy().replace('\\', "/")
    ))
    .unwrap();
    let extra = config.extra_skill_dirs_paths();
    assert_eq!(extra.len(), 1);

    let found = crate::skills::scan_all_skills_with_extra(None, &extra)
        .into_iter()
        .any(|skill| skill.name == "my-extra-skill");
    assert!(found, "extra_skill_dirs must feed the skill listing");

    // And the prompt's skills section lists it, so the model can call it.
    let section = crate::prompt::skills_renderer::generate_skills_section_with_extra(None, &extra);
    assert!(section.contains("my-extra-skill"), "{section}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_all_available_skills_resolves_with_documented_default() {
    // Unset keeps the documented default (`true`), so a file that never
    // mentions the key behaves exactly as before.
    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert!(absent.resolve_merge_all_available_skills());

    let off = KimiConfig::from_str("merge_all_available_skills = false\n").unwrap();
    assert!(!off.resolve_merge_all_available_skills());

    // The camelCase alias the schema and the web client write is accepted.
    let camel = KimiConfig::from_str("mergeAllAvailableSkills = false\n").unwrap();
    assert!(!camel.resolve_merge_all_available_skills());
}

#[test]
fn merge_flag_reaches_the_prompt_skills_section() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();

    std::fs::create_dir_all(dir.join(".agents").join("skills").join("a")).unwrap();
    std::fs::write(
        dir.join(".agents")
            .join("skills")
            .join("a")
            .join("SKILL.md"),
        "---\nname: merged-a\ndescription: A\n---\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".kimi-code").join("skills").join("b")).unwrap();
    std::fs::write(
        dir.join(".kimi-code")
            .join("skills")
            .join("b")
            .join("SKILL.md"),
        "---\nname: merged-b\ndescription: B\n---\n",
    )
    .unwrap();

    let on =
        crate::prompt::SystemPromptBuilder::build_default_with_skill_config(dir, Vec::new(), true);
    assert!(on.contains("merged-a"), "{on}");
    assert!(on.contains("merged-b"), "{on}");

    let off =
        crate::prompt::SystemPromptBuilder::build_default_with_skill_config(dir, Vec::new(), false);
    assert!(off.contains("merged-a"), "{off}");
    assert!(
        !off.contains("merged-b"),
        "merge_all_available_skills=false must not scan the second project dir"
    );
}

#[test]
fn test_resolve_image_limits() {
    let keys = ["KIMI_IMAGE_READ_BYTE_BUDGET", "KIMI_IMAGE_MAX_EDGE_PX"];
    let saved: Vec<Option<String>> = keys.iter().map(|key| std::env::var(key).ok()).collect();
    unsafe {
        for key in keys {
            std::env::remove_var(key);
        }
    }

    let config = KimiConfig::from_str(
        r#"
[image]
read_byte_budget = 131072
maxEdgePx = 800
"#,
    )
    .unwrap();
    assert_eq!(config.resolve_image_read_byte_budget(), Some(131072));
    assert_eq!(config.resolve_image_max_edge_px(), Some(800));

    // Env wins; zero and invalid values fall back to the file.
    unsafe { std::env::set_var("KIMI_IMAGE_READ_BYTE_BUDGET", "65536") };
    assert_eq!(config.resolve_image_read_byte_budget(), Some(65536));
    unsafe { std::env::set_var("KIMI_IMAGE_READ_BYTE_BUDGET", "0") };
    assert_eq!(config.resolve_image_read_byte_budget(), Some(131072));
    unsafe { std::env::set_var("KIMI_IMAGE_READ_BYTE_BUDGET", "nope") };
    assert_eq!(config.resolve_image_read_byte_budget(), Some(131072));
    unsafe { std::env::remove_var("KIMI_IMAGE_READ_BYTE_BUDGET") };
    unsafe { std::env::set_var("KIMI_IMAGE_MAX_EDGE_PX", "400") };
    assert_eq!(config.resolve_image_max_edge_px(), Some(400));
    unsafe { std::env::remove_var("KIMI_IMAGE_MAX_EDGE_PX") };

    // An absent section keeps the engine defaults (None).
    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(absent.resolve_image_read_byte_budget(), None);
    assert_eq!(absent.resolve_image_max_edge_px(), None);

    unsafe {
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn test_extract_native_llm_capabilities() {
    let config = KimiConfig::from_str(
        r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[models.kimi-k2]
provider = "kimi"
model = "kimi-k2-0711"
capabilities = ["thinking", "image_in"]
"#,
    )
    .unwrap();
    let llm = config.extract_native_llm(None).unwrap();
    assert_eq!(
        llm.capabilities.as_deref(),
        Some(&["thinking".to_string(), "image_in".to_string()][..])
    );

    // No declared capabilities stays unknown (None) — the lenient default.
    let bare = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(bare.extract_native_llm(None).unwrap().capabilities, None);
}

#[test]
fn test_resolve_thinking_keep() {
    let key = "KIMI_MODEL_THINKING_KEEP";
    let saved = std::env::var(key).ok();
    unsafe { std::env::remove_var(key) };

    let explicit = KimiConfig::from_str(
        r#"
[thinking]
keep = "all"
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_thinking_keep().as_deref(), Some("all"));

    // Off values disable the passthrough; unset stays opt-in (no keep).
    let off = KimiConfig::from_str(
        r#"
[thinking]
keep = "off"
"#,
    )
    .unwrap();
    assert_eq!(off.resolve_thinking_keep(), None);
    assert_eq!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .resolve_thinking_keep(),
        None
    );

    // `enabled = false` gates the passthrough off.
    let disabled = KimiConfig::from_str(
        r#"
[thinking]
enabled = false
keep = "all"
"#,
    )
    .unwrap();
    assert_eq!(disabled.resolve_thinking_keep(), None);

    // Every off value disables the passthrough, case- and
    // whitespace-insensitively. The same six spellings are duplicated in
    // `resolveThinkingKeep` (node-sdk/src/native/native-llm-resolver.ts),
    // so this table is the Rust half of a cross-language contract: a
    // spelling added on one side and not the other would silently change
    // what a user can turn off.
    for off_value in ["false", "0", "no", "off", "none", "null"] {
        for spelling in [off_value.to_string(), off_value.to_uppercase()] {
            let config =
                KimiConfig::from_str(&format!("[thinking]\nkeep = \"{spelling}\"\n")).unwrap();
            assert_eq!(
                config.resolve_thinking_keep(),
                None,
                "{spelling:?} must disable the passthrough"
            );
        }
    }
    // Padded values trim to the same off value.
    let padded = KimiConfig::from_str("[thinking]\nkeep = \"  OFF  \"\n").unwrap();
    assert_eq!(padded.resolve_thinking_keep(), None);
    // A value that merely contains an off word is not one, and a bare
    // "all" is the enabling spelling.
    let partial = KimiConfig::from_str("[thinking]\nkeep = \"none-of-the-above\"\n").unwrap();
    assert_eq!(
        partial.resolve_thinking_keep().as_deref(),
        Some("none-of-the-above")
    );
    let all = KimiConfig::from_str("[thinking]\nkeep = \"all\"\n").unwrap();
    assert_eq!(all.resolve_thinking_keep().as_deref(), Some("all"));

    // Env outranks the file, off values included.
    unsafe { std::env::set_var(key, "all") };
    assert_eq!(off.resolve_thinking_keep().as_deref(), Some("all"));
    unsafe { std::env::set_var(key, "none") };
    assert_eq!(explicit.resolve_thinking_keep(), None);

    // Every `[secondary_model]` entry rides the same passthrough.
    unsafe { std::env::remove_var(key) };
    let pool = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[thinking]
keep = "all"

[secondary_model]
default_model = "fast"
"#
    ))
    .unwrap()
    .extract_secondary_model_pool(None)
    .unwrap()
    .unwrap();
    assert_eq!(pool.models[0].llm.thinking_keep.as_deref(), Some("all"));

    unsafe {
        match saved {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

/// A one-provider config with the `[models.alias]` body appended, so each
/// test below states only the fields it is about.
fn alias_config(body: &str) -> KimiConfig {
    KimiConfig::from_str(&format!(
        r#"
default_model = "alias"

[providers.kimi]
type = "openai"
api_key = "sk-provider-key"
base_url = "https://api.moonshot.cn/v1"

[models.alias]
provider = "kimi"
model = "kimi-k2-0711"
{body}
"#
    ))
    .unwrap()
}

#[test]
fn model_name_wins_over_model_for_the_wire_name() {
    let named = alias_config("name = \"kimi-k2-0905\"\n");
    assert_eq!(
        named.extract_native_llm(None).unwrap().model,
        "kimi-k2-0905"
    );
    // Without `name` the entry's `model` is the wire name.
    assert_eq!(
        alias_config("").extract_native_llm(None).unwrap().model,
        "kimi-k2-0711"
    );
}

#[test]
fn provider_id_is_an_alias_for_provider() {
    let config = KimiConfig::from_str(
        r#"
default_model = "alias"

[providers.kimi]
type = "openai"
api_key = "sk-provider-key"
base_url = "https://api.moonshot.cn/v1"

[models.alias]
provider_id = "kimi"
model = "kimi-k2-0711"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.api_key, "sk-provider-key");
    assert_eq!(native.base_url, "https://api.moonshot.cn/v1");
}

#[test]
fn model_overrides_shadow_the_base_catalog_fields() {
    let config = alias_config(
        r#"max_output_size = 4096
max_input_size = 262144
capabilities = ["thinking"]
reasoning_key = "reasoning_content"
off_effort = "none"

[models.alias.overrides]
max_output_size = 8192
capabilities = ["thinking", "image_in"]
reasoning_key = "reasoning"
"#,
    );
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.max_output_size, Some(8192));
    assert_eq!(native.reasoning_key.as_deref(), Some("reasoning"));
    assert_eq!(
        native.capabilities.as_deref(),
        Some(&["thinking".to_string(), "image_in".to_string()][..])
    );
    // A field the override does not mention keeps the base value.
    assert_eq!(native.max_input_size, Some(262144));
    assert_eq!(native.off_effort.as_deref(), Some("none"));
}

#[test]
fn a_declared_alias_resolves_to_its_entry() {
    let config = alias_config("aliases = [\"kimi-latest\"]\n");
    // The table key still resolves directly.
    assert_eq!(
        config.extract_native_llm(Some("alias")).unwrap().model,
        "kimi-k2-0711"
    );
    // A declared alias reaches the same entry.
    let via_alias = config.extract_native_llm(Some("kimi-latest")).unwrap();
    assert_eq!(via_alias.model, "kimi-k2-0711");
    assert_eq!(via_alias.api_key, "sk-provider-key");
    // An undeclared name still resolves to nothing.
    assert!(config.extract_native_llm(Some("nope")).is_none());
}

#[test]
fn model_api_key_wins_over_the_providers() {
    let native = alias_config("api_key = \"sk-model-key\"\n")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.api_key, "sk-model-key");
    assert_eq!(native.auth_provider, None);
}

#[test]
fn model_oauth_wins_over_the_providers_static_key() {
    let native = alias_config("oauth = { provider = \"kimi\" }\n")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.api_key, "");
    assert_eq!(native.auth_provider.as_deref(), Some("kimi"));
}

#[test]
fn model_oauth_resolves_without_a_provider_credential() {
    let config = KimiConfig::from_str(
        r#"
default_model = "alias"

[providers.kimi]
type = "openai"
base_url = "https://api.moonshot.cn/v1"

[models.alias]
provider = "kimi"
model = "kimi-k2-0711"
oauth = { provider = "kimi" }
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.api_key, "");
    assert_eq!(native.auth_provider.as_deref(), Some("kimi"));
}

#[test]
fn secondary_pool_entries_carry_the_model_catalog_fields() {
    let config = KimiConfig::from_str(
        r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[models.kimi-k2]
provider = "kimi"
model = "kimi-k2-0711"

[models.fast]
provider = "kimi"
model = "kimi-k2-fast"
name = "kimi-k2-fast-0905"
capabilities = ["thinking", "image_in"]
system_prompt = "Be terse."
max_input_size = 131072
adaptive_thinking = true
reasoning_key = "reasoning"
off_effort = "none"

[secondary_model]
default_model = "fast"
"#,
    )
    .unwrap();
    let pool = config.extract_secondary_model_pool(None).unwrap().unwrap();
    let fast = &pool.models[0].llm;
    assert_eq!(fast.model, "kimi-k2-fast-0905");
    assert_eq!(fast.max_input_size, Some(131072));
    assert_eq!(fast.adaptive_thinking, Some(true));
    assert_eq!(fast.reasoning_key.as_deref(), Some("reasoning"));
    assert_eq!(fast.off_effort.as_deref(), Some("none"));
    assert_eq!(fast.system_prompt.as_deref(), Some("Be terse."));
    assert_eq!(
        fast.capabilities.as_deref(),
        Some(&["thinking".to_string(), "image_in".to_string()][..])
    );
}

/// v2 #4210: `[mcp] defaultDeferred` is the global disclosure default, and the
/// camelCase spelling is accepted like the two timeout keys.
#[test]
fn mcp_section_parses_the_global_deferred_default() {
    let camel = KimiConfig::from_str("[mcp]\ndefaultDeferred = true\n").unwrap();
    assert_eq!(camel.mcp.default_deferred, Some(true));

    let snake = KimiConfig::from_str("[mcp]\ndefault_deferred = false\n").unwrap();
    assert_eq!(snake.mcp.default_deferred, Some(false));

    let absent = KimiConfig::from_str("[mcp]\nstartupTimeoutMs = 1000\n").unwrap();
    assert_eq!(absent.mcp.default_deferred, None);
}
