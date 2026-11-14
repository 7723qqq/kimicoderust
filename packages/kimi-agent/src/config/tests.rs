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
fn task_max_running_tasks_prefers_task_then_background() {
    let both = KimiConfig::from_str(
        "[task]\nmax_running_tasks = 5\n[background]\nmax_running_tasks = 2\n",
    )
    .unwrap();
    assert_eq!(both.configured_max_running_tasks(), Some(5));

    let legacy = KimiConfig::from_str("[background]\nmax_running_tasks = 3\n").unwrap();
    assert_eq!(legacy.configured_max_running_tasks(), Some(3));

    let unset = KimiConfig::from_str("").unwrap();
    assert_eq!(unset.configured_max_running_tasks(), None);

    let non_positive = KimiConfig::from_str("[task]\nmax_running_tasks = 0\n").unwrap();
    assert_eq!(non_positive.configured_max_running_tasks(), None);
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

#[test]
fn antigravity_resolves_explicitly_without_credentials() {
    let config = KimiConfig::from_str(
        r#"
default_model = "ag"

[providers.google-cli]
type = "antigravity"
binaryPath = "/opt/agy"

[providers.google-cli.antigravity]
spawnTimeoutMs = 1200
firstFrameTimeoutMs = 3400
idleTimeoutMs = 5600
totalTimeoutMs = 7800

[models.ag]
provider = "google-cli"
model = "gemini-3.7-flash"
"#,
    )
    .unwrap();
    let native = config.extract_native_llm(Some("ag")).unwrap();
    assert_eq!(native.protocol, "antigravity");
    assert_eq!(native.provider_type, "antigravity");
    assert!(native.base_url.is_empty());
    assert!(native.api_key.is_empty());
    assert_eq!(native.binary_path.as_deref(), Some("/opt/agy"));
    let limits = native.antigravity.unwrap();
    assert_eq!(limits.spawn_ms, 1_200);
    assert_eq!(limits.first_frame_ms, 3_400);
    assert_eq!(limits.idle_ms, 5_600);
    assert_eq!(limits.total_ms, 7_800);

    let plain = KimiConfig::from_str(
        r#"
default_model = "m"

[providers.p]
type = "google-genai"

[models.m]
provider = "p"
model = "gemini-3.7-flash"
"#,
    )
    .unwrap();
    assert!(plain.extract_native_llm(Some("m")).is_none());
}

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
    let _env = crate::test_support::EnvScope::unset(&["ANTHROPIC_BASE_URL"]);

    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "anthropic");
    assert_eq!(native.base_url, "https://api.anthropic.com/v1");

    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", "https://ant-proxy.example.com") };
    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://ant-proxy.example.com/v1");

    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", "   ") };
    let native = endpoint_less_config("anthropic")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.base_url, "https://api.anthropic.com/v1");

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
}

#[test]
fn openai_endpoint_falls_back_to_env_then_default() {
    let _env = crate::test_support::EnvScope::unset(&["OPENAI_BASE_URL"]);

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

    unsafe { std::env::set_var("OPENAI_BASE_URL", "   ") };
    let native = endpoint_less_config("openai_responses")
        .extract_native_llm(None)
        .unwrap();
    assert_eq!(native.protocol, "openai_responses");
    assert_eq!(native.base_url, "https://api.openai.com/v1");
}

#[test]
fn kimi_endpoint_falls_back_to_env_then_default() {
    let _env = crate::test_support::EnvScope::unset(&["KIMI_BASE_URL"]);

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
}

#[test]
fn google_genai_endpoint_prefers_the_env_over_the_host_root() {
    let _env = crate::test_support::EnvScope::unset(&["GOOGLE_GEMINI_BASE_URL"]);

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
}

#[test]
fn undeclared_provider_type_without_an_endpoint_does_not_resolve() {
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
    let flat = KimiConfig::from_str("yolo = true\n").unwrap();
    assert_eq!(flat.build_policy_snapshot(None).mode, PermissionMode::Yolo);

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

    let pool = config
        .extract_secondary_model_pool(Some("thinky"))
        .unwrap()
        .unwrap();
    assert_eq!(pool.caller_model_alias.as_deref(), Some("thinky"));
}

fn pool_effort_config(model_extra: &str, tail: &str) -> KimiConfig {
    KimiConfig::from_str(&format!(
        r#"
default_model = "kimi-k2"

[providers.kimi]
type = "openai"
api_key = "sk-kimi-key"
base_url = "https://api.moonshot.cn/v1"

[models.fast]
provider = "kimi"
model = "kimi-k2-fast"
capabilities = ["thinking"]
{model_extra}

[secondary_model]
default_model = "fast"
{tail}
"#
    ))
    .unwrap()
}

fn pool_fast_effort(config: &KimiConfig) -> Option<String> {
    let pool = config.extract_secondary_model_pool(None).unwrap().unwrap();
    pool.models
        .iter()
        .find(|entry| entry.alias == "fast")
        .expect("the pool carries the fast entry")
        .llm
        .reasoning_effort
        .clone()
}

#[test]
fn test_secondary_model_pool_entry_keeps_its_declared_effort() {
    let config = pool_effort_config(
        "default_effort = \"high\"\nsupport_efforts = [\"low\", \"high\"]",
        "",
    );
    assert_eq!(pool_fast_effort(&config).as_deref(), Some("high"));
}

#[test]
fn test_secondary_model_pool_section_effort_beats_the_declared_one() {
    let config = pool_effort_config(
        "default_effort = \"high\"\nsupport_efforts = [\"low\", \"high\"]",
        "default_effort = \"low\"",
    );
    assert_eq!(pool_fast_effort(&config).as_deref(), Some("low"));
}

#[test]
fn test_secondary_model_pool_declared_effort_needs_declared_support() {
    let unsupported = pool_effort_config(
        "default_effort = \"max\"\nsupport_efforts = [\"low\", \"high\"]",
        "",
    );
    assert_eq!(pool_fast_effort(&unsupported), None);

    let no_list = pool_effort_config("default_effort = \"high\"", "");
    assert_eq!(pool_fast_effort(&no_list), None);
}

#[test]
fn test_secondary_model_pool_declared_effort_is_dropped_when_thinking_is_disabled() {
    let config = pool_effort_config(
        "default_effort = \"high\"\nsupport_efforts = [\"low\", \"high\"]",
        "[thinking]\nenabled = false",
    );
    assert_eq!(pool_fast_effort(&config), None);
}

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

    assert_eq!(kimi.llm.reasoning_effort.as_deref(), Some("high"));

    let thinky = racers.iter().find(|racer| racer.name == "thinky").unwrap();
    assert_eq!(thinky.llm.protocol, "anthropic");

    assert!(thinky.llm.reasoning_effort.is_none());
    assert!(thinky.llm.thinking_budget.is_none());
}

#[test]
fn test_extract_multi_llm_absent_and_inert_cases() {
    let bare = KimiConfig::from_str(POOL_CONFIG).unwrap();
    assert!(bare.extract_multi_llm(None).unwrap().is_none());

    let empty = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = []
"#
    ))
    .unwrap();
    assert!(empty.extract_multi_llm(None).unwrap().is_none());

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
    let lone = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[agent]
multi_llm = ["kimi-k2"]
"#
    ))
    .unwrap();
    let error = lone.extract_multi_llm(None).unwrap_err();
    assert!(error.contains("at least two"), "unexpected error: {error}");

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

    let table_without_default = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
model = "fast"

[secondary_model.models]
"fast" = ""
"#
    ))
    .unwrap();
    let error = table_without_default
        .extract_secondary_model_pool(None)
        .unwrap_err();
    assert!(
        error.contains("default_model is required when [secondary_model.models] is configured"),
        "{error}"
    );

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
    let pool = force_with_table
        .extract_secondary_model_pool(None)
        .unwrap()
        .expect("a leftover force key no longer kills the pool");
    assert_eq!(pool.models.len(), 1);

    let force_without_default = KimiConfig::from_str(&format!(
        r#"{POOL_CONFIG}
[secondary_model]
force = true
"#
    ))
    .unwrap();
    assert!(
        force_without_default
            .extract_secondary_model_pool(None)
            .unwrap()
            .is_none(),
        "force alone is not a pool"
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
    let _env = crate::test_support::EnvScope::unset(&keys);

    let explicit = KimiConfig::from_str(
        r#"
[loop_control]
max_attempts_per_step = 3
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_max_attempts_per_step(), Some(3));

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
}

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

    let camel = KimiConfig::from_str(
        r#"
[loop_control]
compactionMaxAttempts = 4
"#,
    )
    .unwrap();
    assert_eq!(camel.resolve_compaction_max_attempts(), Some(4));

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

    let no_list = config("xhigh", "capabilities = [\"thinking\"]\n");
    assert!(no_list.extract_secondary_model_pool(None).is_ok());

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

    let plain = config("xhigh", "capabilities = [\"tools\"]\n");
    let error = plain.extract_secondary_model_pool(None).unwrap_err();
    assert!(error.contains("does not support thinking"), "{error}");

    let plain_off = config("off", "capabilities = [\"tools\"]\n");
    assert!(plain_off.extract_secondary_model_pool(None).is_ok());

    let always = config(
        "off",
        "capabilities = [\"thinking\", \"always_thinking\"]\n",
    );
    let error = always.extract_secondary_model_pool(None).unwrap_err();
    assert!(
        error.contains("cannot disable thinking") && error.contains("\"fast\""),
        "{error}"
    );

    let adaptive = config("high", "adaptive_thinking = true\n");
    assert!(adaptive.extract_secondary_model_pool(None).is_ok());

    let padded = config(
        "  HIGH  ",
        "capabilities = [\"thinking\"]\nsupport_efforts = [\"low\", \"high\"]\n",
    );
    assert!(padded.extract_secondary_model_pool(None).is_ok());

    let unset = KimiConfig::from_str(&format!(
        r#"{EFFORT_CONFIG}
[secondary_model]
default_model = "fast"
"#
    ))
    .unwrap();
    assert!(unset.extract_secondary_model_pool(None).is_ok());
}

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

    assert!(KimiConfig::malformed_model_entries("default_model = \"k2\"\n").is_empty());
    assert!(KimiConfig::malformed_model_entries("not = = toml").is_empty());
}

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

    assert!(
        KimiConfig::from_str("[models.typo]\nprovider = \"kimi\"\n")
            .unwrap()
            .config_warnings
            .is_empty()
    );
}

#[test]
fn from_file_reports_invalid_environment_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    std::fs::write(&path, "[environments.dev]\ntype = \"local\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert_eq!(
        config.config_warnings.len(),
        1,
        "{:?}",
        config.config_warnings
    );
    assert!(
        config.config_warnings[0].contains("'environments'"),
        "{}",
        config.config_warnings[0]
    );
    assert!(
        config.config_warnings[0].contains("built in"),
        "{}",
        config.config_warnings[0]
    );

    assert_eq!(
        config.environments.entries["dev"].kind.as_deref(),
        Some("local")
    );

    std::fs::write(&path, "[environments.dev]\ntype = \"podman\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("podman")),
        "{:?}",
        config.config_warnings
    );

    std::fs::write(&path, "[environments]\ndefault = \"missing\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("missing")),
        "{:?}",
        config.config_warnings
    );

    std::fs::write(&path, "[environments.local]\ntype = \"ssh\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("reserved")),
        "{:?}",
        config.config_warnings
    );

    std::fs::write(&path, "[environments.\" dev\"]\ntype = \"ssh\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("whitespace")),
        "{:?}",
        config.config_warnings
    );

    std::fs::write(&path, "[environments.dev]\nhost = \"example.test\"\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("has no type")),
        "{:?}",
        config.config_warnings
    );

    std::fs::write(&path, "[environments]\ndev = 3\n").unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("Ignored invalid config section 'environments'")),
        "{:?}",
        config.config_warnings
    );
    assert!(config.environments.entries.is_empty());

    std::fs::write(
        &path,
        "[environments.dev]\ntype = \"ssh\"\nhost = \"example.test\"\n",
    )
    .unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config.config_warnings.is_empty(),
        "{:?}",
        config.config_warnings
    );
}

#[test]
fn from_file_reports_unknown_and_invalid_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[loop_control]
max_steps_per_turn = 7
max_steps_per_run = 5

[not_a_section]
value = 1

[[hooks]]
event = "PreToolUse"
command = "echo hi"
timeout = 0
"#,
    )
    .unwrap();

    let config = KimiConfig::from_file(&path).unwrap();
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("Ignored invalid config section 'not_a_section'")),
        "{:?}",
        config.config_warnings
    );
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("Ignored invalid config section 'hooks'")),
        "the out-of-range timeout must reject the section: {:?}",
        config.config_warnings
    );

    assert_eq!(config.loop_control.max_steps_per_turn, Some(7));
    assert!(config.hooks.is_empty());

    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("'max_steps_per_run' is deprecated")),
        "{:?}",
        config.config_warnings
    );
}

#[test]
fn test_resolve_max_steps_per_turn() {
    let key = "KIMI_LOOP_MAX_STEPS_PER_TURN";
    let _env = crate::test_support::EnvScope::unset(&[key]);

    let explicit = KimiConfig::from_str(
        r#"
[loop_control]
max_steps_per_turn = 25
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_max_steps_per_turn(), Some(25));

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

    assert!(
        KimiConfig::from_str(SAMPLE_CONFIG)
            .unwrap()
            .resolve_bing_api_service()
            .is_none()
    );
}

#[test]
fn test_resolve_web_services_env_overlay() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_WEB_SEARCH_BASE_URL",
        "KIMI_WEB_SEARCH_API_KEY",
        "KIMI_WEB_FETCH_BASE_URL",
        "KIMI_WEB_FETCH_API_KEY",
    ]);

    let config = KimiConfig::from_str(SERVICES_CONFIG).unwrap();
    let search = config.resolve_web_search_service().unwrap();
    assert_eq!(search.base_url, "https://search.example.test/v1");
    assert_eq!(search.api_key.as_deref(), Some("sk-search"));
    assert_eq!(search.custom_headers.len(), 1);

    unsafe { std::env::set_var("KIMI_WEB_SEARCH_API_KEY", "sk-env") };
    let search = config.resolve_web_search_service().unwrap();
    assert_eq!(search.api_key.as_deref(), Some("sk-env"));
    assert_eq!(search.custom_headers.len(), 1);

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

    assert_eq!(
        config
            .resolve_web_fetch_service()
            .unwrap()
            .api_key
            .as_deref(),
        Some("sk-fetch")
    );

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
}

#[test]
fn test_resolve_subagent_and_swarm_timeouts() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_SUBAGENT_TIMEOUT_MS",
        "KIMI_CODE_SWARM_TIMEOUT_MS",
    ]);

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

    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "9000") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(9000));
    assert_eq!(config.resolve_swarm_timeout_ms(), Some(7000));
    unsafe { std::env::set_var("KIMI_CODE_SWARM_TIMEOUT_MS", "8000") };
    assert_eq!(config.resolve_swarm_timeout_ms(), Some(8000));
    unsafe { std::env::remove_var("KIMI_CODE_SWARM_TIMEOUT_MS") };

    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "0") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(0));
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "-1") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(5000));
    unsafe { std::env::set_var("KIMI_SUBAGENT_TIMEOUT_MS", "soon") };
    assert_eq!(config.resolve_subagent_timeout_ms(), Some(5000));

    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(absent.resolve_subagent_timeout_ms(), None);
    assert_eq!(absent.resolve_swarm_timeout_ms(), None);
}

#[test]
fn provider_custom_headers_reach_the_resolved_llm() {
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

    assert_eq!(native.base_url, "https://gateway.example.com/v1");

    assert_eq!(native.protocol, "anthropic");
    assert!(native.beta_api);

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

    assert_eq!(config.resolve_effort(None).as_deref(), Some("high"));

    let bare = KimiConfig::from_str("default_model = \"m\"\n").unwrap();
    assert!(bare.build_policy_snapshot(None).tools_filter.is_none());
    assert_eq!(bare.resolve_effort(None), None);
}

#[test]
fn permission_dangerous_command_guard_accepts_both_spellings() {
    let snake = KimiConfig::from_str("[permission]\ndangerous_command_guard = false\n").unwrap();
    assert_eq!(
        snake.permission.and_then(|p| p.dangerous_command_guard),
        Some(false),
    );
    let camel = KimiConfig::from_str("[permission]\ndangerousCommandGuard = false\n").unwrap();
    assert_eq!(
        camel.permission.and_then(|p| p.dangerous_command_guard),
        Some(false),
    );
}

#[test]
fn workspace_require_trust_defaults_on_and_accepts_both_spellings() {
    let bare = KimiConfig::from_str("default_model = \"m\"\n").unwrap();
    assert!(bare.resolve_workspace_require_trust());

    let on = KimiConfig::from_str("[workspace]\nrequire_trust = true\n").unwrap();
    assert!(on.resolve_workspace_require_trust());

    let off = KimiConfig::from_str("[workspace]\nrequire_trust = false\n").unwrap();
    assert!(!off.resolve_workspace_require_trust());

    let camel = KimiConfig::from_str("[workspace]\nrequireTrust = false\n").unwrap();
    assert!(!camel.resolve_workspace_require_trust());
}

#[test]
fn thinking_off_sends_the_model_off_effort() {
    let config = KimiConfig::from_str("[thinking]\nenabled = false\n").unwrap();
    assert_eq!(config.resolve_effort(Some("none")).as_deref(), Some("none"));

    assert_eq!(config.resolve_effort(None).as_deref(), Some("off"));

    let on = KimiConfig::from_str("[thinking]\nenabled = true\neffort = \"low\"\n").unwrap();
    assert_eq!(on.resolve_effort(Some("none")).as_deref(), Some("low"));
}

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
fn session_runtime_scoped_rules_stay_out_of_the_user_configured_stages() {
    let config = KimiConfig::from_str(
        r#"
[[permission.rules]]
decision = "deny"
pattern = "Write(*.env)"
scope = "session-runtime"

[[permission.rules]]
decision = "allow"
pattern = "Read"
"#,
    )
    .unwrap();
    let snapshot = config.build_policy_snapshot(None);
    assert!(snapshot.deny_rules.is_empty());
    assert!(snapshot.rule_reasons.is_empty());
    assert_eq!(snapshot.allow_rules, vec!["Read".to_string()]);

    let verdict = crate::permission::PermissionEngine::new(snapshot)
        .evaluate("Write", &serde_json::json!({ "path": "prod.env" }));
    assert_ne!(verdict.policy_name, "UserConfiguredDeny");
}

#[test]
fn experimental_flags_and_default_mode_parse() {
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
fn default_permission_mode_rejects_unknown_values() {
    assert!(KimiConfig::from_str("default_permission_mode = \"plan\"").is_err());
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

    let section = crate::prompt::skills_renderer::generate_skills_section_with_extra(None, &extra);
    assert!(section.contains("my-extra-skill"), "{section}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_all_available_skills_resolves_with_documented_default() {
    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert!(absent.resolve_merge_all_available_skills());

    let off = KimiConfig::from_str("merge_all_available_skills = false\n").unwrap();
    assert!(!off.resolve_merge_all_available_skills());

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
        crate::prompt::SystemPromptBuilder::build_default_with_skill_config(dir, Vec::new(), true)
            .unwrap();
    assert!(on.contains("merged-a"), "{on}");
    assert!(on.contains("merged-b"), "{on}");

    let off =
        crate::prompt::SystemPromptBuilder::build_default_with_skill_config(dir, Vec::new(), false)
            .unwrap();
    assert!(off.contains("merged-a"), "{off}");
    assert!(
        !off.contains("merged-b"),
        "merge_all_available_skills=false must not scan the second project dir"
    );
}

#[test]
fn test_resolve_image_limits() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_IMAGE_READ_BYTE_BUDGET",
        "KIMI_IMAGE_MAX_EDGE_PX",
    ]);

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

    let absent = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(absent.resolve_image_read_byte_budget(), None);
    assert_eq!(absent.resolve_image_max_edge_px(), None);
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

    let bare = KimiConfig::from_str(SAMPLE_CONFIG).unwrap();
    assert_eq!(bare.extract_native_llm(None).unwrap().capabilities, None);
}

#[test]
fn test_resolve_thinking_keep() {
    let key = "KIMI_MODEL_THINKING_KEEP";
    let _env = crate::test_support::EnvScope::unset(&[key]);

    let explicit = KimiConfig::from_str(
        r#"
[thinking]
keep = "all"
"#,
    )
    .unwrap();
    assert_eq!(explicit.resolve_thinking_keep().as_deref(), Some("all"));

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

    let disabled = KimiConfig::from_str(
        r#"
[thinking]
enabled = false
keep = "all"
"#,
    )
    .unwrap();
    assert_eq!(disabled.resolve_thinking_keep(), None);

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

    let padded = KimiConfig::from_str("[thinking]\nkeep = \"  OFF  \"\n").unwrap();
    assert_eq!(padded.resolve_thinking_keep(), None);

    let partial = KimiConfig::from_str("[thinking]\nkeep = \"none-of-the-above\"\n").unwrap();
    assert_eq!(
        partial.resolve_thinking_keep().as_deref(),
        Some("none-of-the-above")
    );
    let all = KimiConfig::from_str("[thinking]\nkeep = \"all\"\n").unwrap();
    assert_eq!(all.resolve_thinking_keep().as_deref(), Some("all"));

    unsafe { std::env::set_var(key, "all") };
    assert_eq!(off.resolve_thinking_keep().as_deref(), Some("all"));
    unsafe { std::env::set_var(key, "none") };
    assert_eq!(explicit.resolve_thinking_keep(), None);

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
}

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

    assert_eq!(native.max_input_size, Some(262144));
    assert_eq!(native.off_effort.as_deref(), Some("none"));
}

#[test]
fn a_declared_alias_resolves_to_its_entry() {
    let config = alias_config("aliases = [\"kimi-latest\"]\n");

    assert_eq!(
        config.extract_native_llm(Some("alias")).unwrap().model,
        "kimi-k2-0711"
    );

    let via_alias = config.extract_native_llm(Some("kimi-latest")).unwrap();
    assert_eq!(via_alias.model, "kimi-k2-0711");
    assert_eq!(via_alias.api_key, "sk-provider-key");

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

#[test]
fn mcp_section_parses_the_global_deferred_default() {
    let camel = KimiConfig::from_str("[mcp]\ndefaultDeferred = true\n").unwrap();
    assert_eq!(camel.mcp.default_deferred, Some(true));

    let snake = KimiConfig::from_str("[mcp]\ndefault_deferred = false\n").unwrap();
    assert_eq!(snake.mcp.default_deferred, Some(false));

    let absent = KimiConfig::from_str("[mcp]\nstartupTimeoutMs = 1000\n").unwrap();
    assert_eq!(absent.mcp.default_deferred, None);
}

#[test]
fn model_request_knobs_reach_the_native_transport() {
    let config = KimiConfig::from_str(
        r#"
[providers.openai]
type = "openai"
api_key = "sk-test"
base_url = "https://api.openai.com/v1"

[models.custom]
provider = "openai"
model = "gpt-x"
responseFormat = { type = "json_schema", jsonSchema = { name = "out", schema = { type = "object" }, strict = true } }
sampling = { temperature = 0.2, topP = 0.9 }
determinism = { seed = 7 }
extraParams = { openai = { logit_bias = { "1" = 2 } } }

[models.bare]
provider = "openai"
model = "gpt-y"
"#,
    )
    .unwrap();

    let resolved = config.extract_native_llm(Some("custom")).unwrap();
    let llm = config.native_llm_config(&resolved);
    assert_eq!(
        llm.response_format,
        Some(crate::rpc::types::NativeResponseFormat::JsonSchema {
            json_schema: crate::rpc::types::NativeJsonSchema {
                name: "out".into(),
                schema: serde_json::json!({ "type": "object" }),
                strict: Some(true),
                description: None,
            },
        })
    );
    assert_eq!(
        llm.sampling,
        Some(crate::rpc::types::NativeSampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
        })
    );
    assert_eq!(
        llm.determinism,
        Some(crate::rpc::types::NativeDeterminism { seed: Some(7) })
    );
    assert_eq!(
        llm.extra_params
            .as_ref()
            .and_then(|params| params.openai.get("logit_bias")),
        Some(&serde_json::json!({ "1": 2 }))
    );

    let resolved = config.extract_native_llm(Some("bare")).unwrap();
    let llm = config.native_llm_config(&resolved);
    assert_eq!(llm.response_format, None);
    assert_eq!(llm.sampling, None);
    assert_eq!(llm.determinism, None);
    assert_eq!(llm.extra_params, None);
}

#[test]
fn adaptive_anthropic_native_llm_carries_the_effort_not_a_budget() {
    let config = KimiConfig::from_str(
        r#"
[providers.anthropic]
type = "anthropic"
api_key = "sk-ant"
base_url = "https://api.anthropic.com/v1"

[models.adaptive]
provider = "anthropic"
model = "claude-adaptive"
adaptive_thinking = true
support_efforts = ["low", "medium", "high"]
default_effort = "high"
"#,
    )
    .unwrap();
    let resolved = config.extract_native_llm(Some("adaptive")).unwrap();
    let llm = super::native_llm_config(resolved, Some("medium"), None, None);
    assert_eq!(llm.thinking_budget, None, "adaptive mode sends no budget");
    assert_eq!(llm.reasoning_effort.as_deref(), Some("medium"));

    let resolved = config.extract_native_llm(Some("adaptive")).unwrap();
    let off = super::native_llm_config(resolved, Some("off"), None, None);
    assert_eq!(
        off.reasoning_effort.as_deref(),
        Some("off"),
        "the encoder needs the explicit disable"
    );
    assert_eq!(off.thinking_budget, None);

    let config = KimiConfig::from_str(
        r#"
[providers.anthropic]
type = "anthropic"
api_key = "sk-ant"
base_url = "https://api.anthropic.com/v1"

[models.plain]
provider = "anthropic"
model = "claude-sonnet"
"#,
    )
    .unwrap();
    let resolved = config.extract_native_llm(Some("plain")).unwrap();
    let plain = super::native_llm_config(resolved, Some("high"), None, None);
    assert_eq!(plain.thinking_budget, Some(32_000));
    assert_eq!(plain.reasoning_effort, None);
}

fn token_counting_env() -> std::sync::MutexGuard<'static, ()> {
    let guard = crate::session::token_counting::tests::env_lock();
    unsafe { std::env::remove_var("KIMI_TOKEN_COUNTING_STRATEGY") };
    guard
}

#[test]
fn token_counting_strategy_defaults_to_the_historical_behaviour() {
    use crate::session::token_counting::TokenCountingStrategy;
    let _env = token_counting_env();
    for source in ["", "[tokenCounting]\n"] {
        let config = KimiConfig::from_str(source).unwrap();
        assert_eq!(
            config.resolve_token_counting_strategy(),
            TokenCountingStrategy::MeasuredAndEstimated,
            "{source:?}"
        );
        assert!(config.token_counting_warnings().is_empty());
    }
}

#[test]
fn token_counting_strategy_reads_the_section_and_reports_invalid_values() {
    use crate::session::token_counting::TokenCountingStrategy;
    let _env = token_counting_env();
    let measured = KimiConfig::from_str("[tokenCounting]\nstrategy = \"measured\"\n").unwrap();
    assert_eq!(
        measured.resolve_token_counting_strategy(),
        TokenCountingStrategy::Measured
    );
    let estimated = KimiConfig::from_str("[token_counting]\nstrategy = \"estimated\"\n").unwrap();
    assert_eq!(
        estimated.resolve_token_counting_strategy(),
        TokenCountingStrategy::Estimated
    );

    let invalid = KimiConfig::from_str("[tokenCounting]\nstrategy = \"both\"\n").unwrap();
    assert_eq!(
        invalid.resolve_token_counting_strategy(),
        TokenCountingStrategy::MeasuredAndEstimated,
        "an invalid value must not be silently accepted, but the gauge keeps the default"
    );
    let warnings = invalid.token_counting_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("'both'"), "{warnings:?}");
    assert!(warnings[0].contains("measured+estimated"), "{warnings:?}");
}

#[test]
fn token_counting_strategy_env_overrides_and_falls_through() {
    use crate::session::token_counting::TokenCountingStrategy;
    let _env = token_counting_env();
    const ENV: &str = "KIMI_TOKEN_COUNTING_STRATEGY";
    let file = KimiConfig::from_str("[tokenCounting]\nstrategy = \"measured\"\n").unwrap();

    unsafe { std::env::set_var(ENV, " ESTIMATED ") };
    assert_eq!(
        file.resolve_token_counting_strategy(),
        TokenCountingStrategy::Estimated
    );

    unsafe { std::env::set_var(ENV, "bogus") };
    assert_eq!(
        file.resolve_token_counting_strategy(),
        TokenCountingStrategy::Measured,
        "an invalid env value must not override a valid file value"
    );

    let empty = KimiConfig::from_str("").unwrap();
    assert_eq!(
        empty.resolve_token_counting_strategy(),
        TokenCountingStrategy::MeasuredAndEstimated
    );

    unsafe { std::env::remove_var(ENV) };

    assert_eq!(
        file.resolve_token_counting_strategy(),
        TokenCountingStrategy::Measured
    );
}

#[test]
fn read_section_resolves_both_spellings_and_rejects_non_positive() {
    let empty = KimiConfig::from_str("").unwrap();
    assert_eq!(empty.resolve_read_limits(), (None, None));

    let snake =
        KimiConfig::from_str("[read]\ndefault_max_chars = 1200\nmax_chars = 4000\n").unwrap();
    assert_eq!(snake.resolve_read_limits(), (Some(1200), Some(4000)));

    let camel = KimiConfig::from_str("[read]\ndefaultMaxChars = 1200\nmaxChars = 4000\n").unwrap();
    assert_eq!(camel.resolve_read_limits(), (Some(1200), Some(4000)));

    let zero = KimiConfig::from_str("[read]\nmax_chars = 0\n").unwrap();
    assert_eq!(
        zero.resolve_read_limits(),
        (None, None),
        "a zero ceiling must not clamp every read to nothing"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[read]\nmax_chars = 0\n").unwrap();
    let loaded = KimiConfig::from_file(&path).unwrap();
    assert!(
        loaded
            .config_warnings
            .iter()
            .any(|warning| warning.contains("[read].max_chars")),
        "{:?}",
        loaded.config_warnings
    );
}

#[test]
fn llm_request_byte_budget_prefers_env_then_file_then_default() {
    let _env = crate::test_support::EnvScope::unset(&["KIMI_LLM_REQUEST_BYTE_BUDGET"]);

    assert_eq!(
        KimiConfig::from_str("")
            .unwrap()
            .resolve_llm_request_byte_budget(),
        DEFAULT_LLM_REQUEST_BYTE_BUDGET
    );

    let file = KimiConfig::from_str("[llm_requester]\nrequest_byte_budget = 4096\n").unwrap();
    assert_eq!(file.resolve_llm_request_byte_budget(), 4096);

    let camel = KimiConfig::from_str("[llmRequester]\nrequestByteBudget = 8192\n").unwrap();
    assert_eq!(
        camel.resolve_llm_request_byte_budget(),
        8192,
        "the camelCase alias must not fall through to the default"
    );

    unsafe { std::env::set_var("KIMI_LLM_REQUEST_BYTE_BUDGET", "2048") };
    assert_eq!(file.resolve_llm_request_byte_budget(), 2048);

    for invalid in ["0", "", "  ", "abc"] {
        unsafe { std::env::set_var("KIMI_LLM_REQUEST_BYTE_BUDGET", invalid) };
        assert_eq!(
            file.resolve_llm_request_byte_budget(),
            4096,
            "{invalid:?} must not override the file value"
        );
    }
}

#[test]
fn sandbox_mode_resolves_and_reports_unknown_spellings() {
    assert_eq!(
        KimiConfig::from_str("").unwrap().resolve_sandbox_mode(),
        None,
        "unset keeps the sandbox off"
    );
    for (value, expected) in [
        ("off", crate::tools::sandbox::SandboxMode::Off),
        ("read-only", crate::tools::sandbox::SandboxMode::ReadOnly),
        (
            "workspace-write",
            crate::tools::sandbox::SandboxMode::WorkspaceWrite,
        ),
    ] {
        let config = KimiConfig::from_str(&format!("[sandbox]\nmode = \"{value}\"\n")).unwrap();
        assert_eq!(config.resolve_sandbox_mode(), Some(expected), "{value}");
    }

    let typo = KimiConfig::from_str("[sandbox]\nmode = \"readonlyy\"\n").unwrap();
    assert_eq!(typo.resolve_sandbox_mode(), None);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[sandbox]\nmode = \"readonlyy\"\n").unwrap();
    let loaded = KimiConfig::from_file(&path).unwrap();
    assert!(
        loaded
            .config_warnings
            .iter()
            .any(|warning| warning.contains("[sandbox].mode")),
        "{:?}",
        loaded.config_warnings
    );
}

#[test]
fn builtin_product_skills_env_overrides_file() {
    let _env = crate::test_support::EnvScope::unset(&["KIMI_CODE_BUILTIN_PRODUCT_SKILLS"]);

    assert!(
        KimiConfig::from_str("")
            .unwrap()
            .resolve_builtin_product_skills()
    );

    let off = KimiConfig::from_str("builtin_product_skills = false\n").unwrap();
    assert!(!off.resolve_builtin_product_skills());

    let camel = KimiConfig::from_str("builtinProductSkills = false\n").unwrap();
    assert!(!camel.resolve_builtin_product_skills());

    unsafe { std::env::set_var("KIMI_CODE_BUILTIN_PRODUCT_SKILLS", "1") };
    assert!(
        off.resolve_builtin_product_skills(),
        "a truthy env switch must re-enable product skills"
    );

    unsafe { std::env::set_var("KIMI_CODE_BUILTIN_PRODUCT_SKILLS", "0") };
    assert!(
        !KimiConfig::from_str("")
            .unwrap()
            .resolve_builtin_product_skills(),
        "a falsy env switch must disable them even with no file value"
    );

    unsafe { std::env::set_var("KIMI_CODE_BUILTIN_PRODUCT_SKILLS", "banana") };
    assert!(
        KimiConfig::from_str("")
            .unwrap()
            .resolve_builtin_product_skills(),
        "an unrecognized env value is not a switch"
    );
}

#[test]
fn thinking_forced_effort_outranks_the_chosen_effort() {
    let _env = crate::test_support::EnvScope::unset(&["KIMI_MODEL_THINKING_EFFORT"]);

    let file =
        KimiConfig::from_str("[thinking]\neffort = \"low\"\nforced_effort = \"high\"\n").unwrap();
    assert_eq!(file.resolve_effort(None).as_deref(), Some("high"));

    let camel =
        KimiConfig::from_str("[thinking]\neffort = \"low\"\nforcedEffort = \"high\"\n").unwrap();
    assert_eq!(camel.resolve_effort(None).as_deref(), Some("high"));

    unsafe { std::env::set_var("KIMI_MODEL_THINKING_EFFORT", "max") };
    assert_eq!(
        file.resolve_effort(None).as_deref(),
        Some("max"),
        "the env-forced effort must beat the file"
    );
    for invalid in ["", "  "] {
        unsafe { std::env::set_var("KIMI_MODEL_THINKING_EFFORT", invalid) };
        assert_eq!(
            file.resolve_effort(None).as_deref(),
            Some("high"),
            "{invalid:?} must not override the file"
        );
    }

    unsafe { std::env::set_var("KIMI_MODEL_THINKING_EFFORT", "off") };
    assert_eq!(file.resolve_effort(None).as_deref(), Some("off"));

    let disabled = KimiConfig::from_str(
        "[thinking]\nenabled = false\neffort = \"low\"\nforced_effort = \"high\"\n",
    )
    .unwrap();
    assert_eq!(
        disabled.resolve_effort(Some("none")).as_deref(),
        Some("none"),
        "disabled thinking bypasses the effort entirely"
    );
}

#[test]
fn task_section_overrides_background_and_env_overrides_task() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S",
        "KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT",
        "KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S",
        "KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE",
        "KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS",
    ]);

    let merged = KimiConfig::from_str(
        r#"
[background]
bash_task_timeout_s = 120
kill_grace_period_ms = 1000
keep_alive_on_exit = true
bash_auto_background_on_timeout = false
max_settled_tasks = 5

[task]
bash_task_timeout_s = 240
kill_grace_period_ms = 2000
keep_alive_on_exit = false
bash_auto_background_on_timeout = true
max_settled_tasks = 7
print_wait_ceiling_s = 30
print_background_mode = "drain"
print_max_turns = 4
"#,
    )
    .unwrap();

    assert_eq!(merged.resolve_bash_task_timeout_s(), Some(240));
    assert_eq!(merged.resolve_kill_grace_period_ms(), Some(2000));
    assert_eq!(merged.resolve_keep_alive_on_exit(), Some(false));
    assert_eq!(merged.resolve_bash_auto_background_on_timeout(), Some(true));
    assert_eq!(merged.resolve_max_settled_tasks(), Some(7));
    assert_eq!(merged.resolve_print_wait_ceiling_s(), Some(30));
    assert_eq!(merged.resolve_print_background_mode(), "drain");
    assert_eq!(merged.resolve_print_max_turns(), Some(4));

    let limits = merged.background_limits();
    assert_eq!(limits.bash_task_timeout_s, Some(240));
    assert_eq!(limits.kill_grace_period_ms, Some(2000));
    assert_eq!(limits.keep_alive_on_exit, Some(false));
    assert_eq!(limits.bash_auto_background_on_timeout, Some(true));

    unsafe {
        std::env::set_var("KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S", "999");
        std::env::set_var("KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT", "1");
        std::env::set_var("KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S", "60");
        std::env::set_var("KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS", "9");
    }
    assert_eq!(merged.resolve_bash_task_timeout_s(), Some(999));
    assert_eq!(merged.resolve_keep_alive_on_exit(), Some(true));
    assert_eq!(merged.resolve_print_wait_ceiling_s(), Some(60));
    assert_eq!(merged.resolve_print_max_turns(), Some(9));

    unsafe { std::env::set_var("KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE", "bogus") };
    assert_eq!(
        merged.resolve_print_background_mode(),
        "drain",
        "keep_alive_on_exit is on, so the default is drain"
    );
    unsafe { std::env::set_var("KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE", "exit") };
    assert_eq!(merged.resolve_print_background_mode(), "exit");

    unsafe {
        std::env::remove_var("KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S");
        std::env::remove_var("KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT");
        std::env::remove_var("KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S");
        std::env::remove_var("KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE");
        std::env::remove_var("KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS");
    }
    let defaults = KimiConfig::from_str("").unwrap();
    assert_eq!(defaults.resolve_print_background_mode(), "steer");
}

#[test]
fn legacy_permission_lists_and_tool_match_shorthand_resolve() {
    let config = KimiConfig::from_str(
        r#"
[permission]
deny = ["Write(secret.txt)", { tool = "Bash", match = "rm -rf *" }, { tool = "WebFetch" }]
allow = "Read(*)"
ask = ["Edit(*)"]

[[permission.rules]]
decision = "deny"
tool = "Bash"
match = "curl *"
"#,
    )
    .unwrap();
    let snapshot = config.build_policy_snapshot(None);
    assert_eq!(
        snapshot.deny_rules,
        vec![
            "Bash(curl *)",
            "Write(secret.txt)",
            "Bash(rm -rf *)",
            "WebFetch"
        ]
    );
    assert_eq!(snapshot.allow_rules, vec!["Read(*)"]);
    assert_eq!(snapshot.ask_rules, vec!["Edit(*)"]);
}

#[test]
fn operation_log_retention_parses_safe_bounds() {
    let config = KimiConfig::from_str(
        r#"
[operationLog]
maxOperations = 12
maxAgeMs = 0
maxTotalBytes = 4096
"#,
    )
    .unwrap();
    assert_eq!(config.operation_log.max_operations, Some(12));
    assert_eq!(config.operation_log.max_age_ms, Some(0));
    assert_eq!(config.operation_log.max_total_bytes, Some(4096));
    assert!(
        KimiConfig::from_str("[operationLog]\nmaxOperations = 0\n").is_err(),
        "maxOperations must be positive"
    );
    assert!(
        KimiConfig::from_str("[operationLog]\nmaxAgeMs = -1\n").is_err(),
        "maxAgeMs must be non-negative"
    );
}

#[test]
fn model_overrides_prefer_env_and_drop_out_of_range_values() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_MODEL_TEMPERATURE",
        "KIMI_MODEL_TOP_P",
        "KIMI_MODEL_MAX_COMPLETION_TOKENS",
        "KIMI_MODEL_MAX_TOKENS",
        "KIMI_MODEL_THINKING_KEEP",
    ]);

    let defaults = KimiConfig::from_str("").unwrap().resolve_model_overrides();
    assert_eq!(defaults, ResolvedModelOverrides::default());

    let file = KimiConfig::from_str(
        r#"
[model_overrides]
temperature = 0.3
top_p = 0.9
thinking_keep = "all"
max_completion_tokens = 2048
"#,
    )
    .unwrap();
    assert_eq!(
        file.resolve_model_overrides(),
        ResolvedModelOverrides {
            temperature: Some(0.3),
            top_p: Some(0.9),
            thinking_keep: Some("all".into()),
            max_completion_tokens: Some(2048),
        }
    );

    unsafe {
        std::env::set_var("KIMI_MODEL_TEMPERATURE", "0.7");
        std::env::set_var("KIMI_MODEL_TOP_P", "0.5");
        std::env::set_var("KIMI_MODEL_MAX_COMPLETION_TOKENS", "4096");
    }
    let env = file.resolve_model_overrides();
    assert_eq!(env.temperature, Some(0.7));
    assert_eq!(env.top_p, Some(0.5));
    assert_eq!(env.max_completion_tokens, Some(4096));

    let bad = KimiConfig::from_str(
        "[model_overrides]\ntemperature = 5.0\ntop_p = -1.0\nmax_completion_tokens = 0\n",
    )
    .unwrap();
    unsafe {
        std::env::remove_var("KIMI_MODEL_TEMPERATURE");
        std::env::remove_var("KIMI_MODEL_TOP_P");
        std::env::remove_var("KIMI_MODEL_MAX_COMPLETION_TOKENS");
    }
    assert_eq!(
        bad.resolve_model_overrides().temperature,
        Some(5.0),
        "v2 does not bound temperature, so it is passed through"
    );
    assert_eq!(bad.resolve_model_overrides().top_p, Some(-1.0));
    assert_eq!(
        bad.resolve_model_overrides().max_completion_tokens,
        None,
        "maxCompletionTokens is positive-only"
    );

    unsafe { std::env::set_var("KIMI_MODEL_TEMPERATURE", "NaN") };
    assert_eq!(
        file.resolve_model_overrides().temperature,
        Some(0.3),
        "a non-finite env value falls back to the file"
    );
}

#[test]
fn mcp_environment_binding_is_reported_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[mcp_servers.remote]
command = "tool"
environment_id = "devbox"

[mcp_servers.local]
command = "tool"
environment_id = "local"
"#,
    )
    .unwrap();
    let config = KimiConfig::from_file(&path).unwrap();
    assert_eq!(
        config.mcp_servers["remote"].environment_id.as_deref(),
        Some("devbox")
    );
    assert!(
        config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("[mcp_servers.remote].environment_id")),
        "{:?}",
        config.config_warnings
    );
    assert!(
        !config
            .config_warnings
            .iter()
            .any(|warning| warning.contains("[mcp_servers.local]")),
        "{:?}",
        config.config_warnings
    );
}

#[test]
fn provider_and_service_auxiliary_fields_parse() {
    let config = KimiConfig::from_str(
        r#"
[providers.gateway]
type = "openai"
api_key = "sk-x"
modelSource = "oauth-catalog"
env = { GATEWAY_TENANT = "team-a" }

[services.moonshot_search]
base_url = "https://search.example"
oauth = { account = "me" }
"#,
    )
    .unwrap();
    let provider = &config.providers["gateway"];
    assert_eq!(provider.model_source.as_deref(), Some("oauth-catalog"));
    assert_eq!(
        provider
            .env
            .as_ref()
            .and_then(|env| env.get("GATEWAY_TENANT")),
        Some(&"team-a".to_string())
    );
    assert!(
        config
            .services
            .moonshot_search
            .as_ref()
            .unwrap()
            .oauth
            .is_some()
    );
}

#[test]
fn env_model_overlay_builds_the_reserved_provider_and_alias() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_MODEL_NAME",
        "KIMI_MODEL_API_KEY",
        "KIMI_MODEL_PROVIDER_TYPE",
        "KIMI_MODEL_BASE_URL",
        "KIMI_MODEL_MAX_CONTEXT_SIZE",
        "KIMI_MODEL_MAX_OUTPUT_SIZE",
        "KIMI_MODEL_CAPABILITIES",
        "KIMI_MODEL_ADAPTIVE_THINKING",
        "KIMI_MODEL_DISPLAY_NAME",
        "KIMI_MODEL_REASONING_KEY",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "").unwrap();

    let plain = KimiConfig::load_or_default(&path);
    assert!(plain.models.get(ENV_MODEL_ALIAS_KEY).is_none());
    assert!(plain.providers.get(ENV_MODEL_PROVIDER_KEY).is_none());

    unsafe {
        std::env::set_var("KIMI_MODEL_NAME", "env-model");
        std::env::set_var("KIMI_MODEL_API_KEY", "sk-env");
        std::env::set_var("KIMI_MODEL_PROVIDER_TYPE", "kimi");
    }
    let config = KimiConfig::load_or_default(&path);
    assert_eq!(config.default_model.as_deref(), Some(ENV_MODEL_ALIAS_KEY));
    let alias = &config.models[ENV_MODEL_ALIAS_KEY];
    assert_eq!(alias.provider.as_deref(), Some(ENV_MODEL_PROVIDER_KEY));
    assert_eq!(alias.model.as_deref(), Some("env-model"));
    assert_eq!(alias.max_context_size, Some(262_144));
    assert_eq!(
        alias.capabilities.as_deref(),
        Some(["image_in".to_string(), "thinking".to_string()].as_slice())
    );

    let native = config.extract_native_llm(None).unwrap();
    assert_eq!(native.model, "env-model");
    assert_eq!(native.api_key, "sk-env");
    assert_eq!(native.provider_type, "kimi");
    assert_eq!(native.base_url, "https://api.moonshot.ai/v1");

    unsafe { std::env::remove_var("KIMI_MODEL_PROVIDER_TYPE") };
    let defaulted = KimiConfig::load_or_default(&path);
    assert_eq!(
        defaulted.providers[ENV_MODEL_PROVIDER_KEY]
            .provider_type
            .as_deref(),
        Some("kimi")
    );
}

#[test]
fn env_model_overlay_rejects_malformed_values() {
    let _env = crate::test_support::EnvScope::unset(&[
        "KIMI_MODEL_NAME",
        "KIMI_MODEL_MAX_CONTEXT_SIZE",
        "KIMI_MODEL_MAX_OUTPUT_SIZE",
        "KIMI_MODEL_ADAPTIVE_THINKING",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "default_model = \"file-model\"\n").unwrap();
    unsafe { std::env::set_var("KIMI_MODEL_NAME", "env-model") };

    for (key, value) in [
        ("KIMI_MODEL_MAX_CONTEXT_SIZE", "abc"),
        ("KIMI_MODEL_MAX_CONTEXT_SIZE", "0"),
        ("KIMI_MODEL_MAX_OUTPUT_SIZE", "-1"),
    ] {
        unsafe { std::env::set_var(key, value) };
        let failed = KimiConfig::load_or_default(&path);
        assert!(
            failed
                .config_warnings
                .iter()
                .any(|warning| warning.contains(key)),
            "{key}={value}: {:?}",
            failed.config_warnings
        );
        assert_eq!(
            failed.default_model, None,
            "a failed overlay must not leave the file config applied"
        );
        unsafe { std::env::remove_var(key) };
    }

    unsafe { std::env::set_var("KIMI_MODEL_ADAPTIVE_THINKING", "maybe") };
    let failed = KimiConfig::load_or_default(&path);
    assert!(
        failed
            .config_warnings
            .iter()
            .any(|warning| warning.contains("KIMI_MODEL_ADAPTIVE_THINKING")),
        "{:?}",
        failed.config_warnings
    );
}

#[test]
fn max_ralph_iterations_parses_and_reports_below_floor() {
    let config = KimiConfig::from_str("[loop_control]\nmax_ralph_iterations = 3\n").unwrap();
    assert_eq!(config.loop_control.max_ralph_iterations, Some(3));

    let camel = KimiConfig::from_str("[loop_control]\nmaxRalphIterations = 5\n").unwrap();
    assert_eq!(camel.loop_control.max_ralph_iterations, Some(5));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[loop_control]\nmax_ralph_iterations = -2\n").unwrap();
    let loaded = KimiConfig::from_file(&path).unwrap();
    assert!(
        loaded
            .config_warnings
            .iter()
            .any(|warning| warning.contains("maxRalphIterations")),
        "{:?}",
        loaded.config_warnings
    );
}
