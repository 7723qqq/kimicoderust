#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSource {
    #[default]
    Builtin,
    User,
    Mcp,
}

impl ToolSource {
    fn of_name(name: &str) -> Self {
        if is_mcp_tool_name(name) {
            Self::Mcp
        } else {
            Self::Builtin
        }
    }
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolActivationPolicy {
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
}

fn glob_match(pattern: &str, name: &str) -> bool {
    match globset::GlobBuilder::new(pattern)
        .case_insensitive(false)
        .build()
    {
        Ok(glob) => glob.compile_matcher().is_match(name),
        Err(error) => {
            tracing::error!(
                pattern,
                %error,
                "tool policy glob could not be compiled; no tool may match this pattern"
            );
            false
        }
    }
}

struct PolicyRef<'a> {
    tools: Option<&'a [String]>,
    disallowed_tools: Option<&'a [String]>,
}

impl<'a> From<&'a ToolActivationPolicy> for PolicyRef<'a> {
    fn from(policy: &'a ToolActivationPolicy) -> Self {
        Self {
            tools: policy.tools.as_deref(),
            disallowed_tools: policy.disallowed_tools.as_deref(),
        }
    }
}

pub fn is_tool_active(policy: &ToolActivationPolicy, name: &str, source: ToolSource) -> bool {
    is_tool_active_ref(policy.into(), name, source)
}

fn is_tool_active_ref(policy: PolicyRef<'_>, name: &str, source: ToolSource) -> bool {
    if let Some(tools) = policy.tools {
        let allowed = if source == ToolSource::Mcp {
            tools
                .iter()
                .filter(|pattern| is_mcp_tool_name(pattern.as_str()))
                .any(|pattern| glob_match(pattern, name))
        } else {
            tools.iter().any(|pattern| pattern.as_str() == name)
        };
        if !allowed {
            return false;
        }
    }
    let Some(disallowed_tools) = policy.disallowed_tools else {
        return true;
    };
    if source != ToolSource::Mcp {
        return !disallowed_tools
            .iter()
            .any(|pattern| pattern.as_str() == name);
    }
    !disallowed_tools
        .iter()
        .filter(|pattern| is_mcp_tool_name(pattern.as_str()))
        .any(|pattern| glob_match(pattern, name))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GlobalToolsPolicy {
    #[serde(default)]
    pub enabled: Option<Vec<String>>,
    #[serde(default)]
    pub disabled: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolPolicyLayers {
    #[serde(default)]
    pub workspace_disabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub profile: ToolActivationPolicy,
    #[serde(default)]
    pub global: Option<GlobalToolsPolicy>,
    #[serde(default)]
    pub session_disabled_tools: Option<Vec<String>>,
}

pub fn is_tool_active_composed(layers: &ToolPolicyLayers, name: &str, source: ToolSource) -> bool {
    let global = layers.global.as_ref();
    let global_layer = PolicyRef {
        tools: global
            .and_then(|policy| policy.enabled.as_deref())
            .filter(|enabled| !enabled.is_empty()),
        disallowed_tools: global.and_then(|policy| policy.disabled.as_deref()),
    };
    let workspace_layer = PolicyRef {
        tools: None,
        disallowed_tools: layers.workspace_disabled_tools.as_deref(),
    };
    let session_layer = PolicyRef {
        tools: None,
        disallowed_tools: layers.session_disabled_tools.as_deref(),
    };
    is_tool_active_ref(workspace_layer, name, source)
        && is_tool_active_ref((&layers.profile).into(), name, source)
        && is_tool_active_ref(global_layer, name, source)
        && is_tool_active_ref(session_layer, name, source)
}

pub fn resolve_active_tool_names(policy: &ToolActivationPolicy) -> Option<Vec<String>> {
    let tools = policy.tools.as_ref()?;
    Some(
        tools
            .iter()
            .filter(|pattern| {
                is_tool_active(
                    policy,
                    pattern.as_str(),
                    ToolSource::of_name(pattern.as_str()),
                )
            })
            .cloned()
            .collect(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InactiveToolPatternKind {
    WildcardNotMcp,
    IncompleteMcpName,
    UnknownTool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactiveToolPattern {
    pub pattern: String,
    pub kind: InactiveToolPatternKind,
}

fn has_glob_magic(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}'))
}

pub fn literal_tool_names(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|pattern| {
            let pattern = pattern.as_str();
            !is_mcp_tool_name(pattern) && !has_glob_magic(pattern)
        })
        .cloned()
        .collect()
}

pub fn find_inactive_tool_patterns(
    patterns: &[String],
    is_known_tool_name: Option<&dyn Fn(&str) -> bool>,
) -> Vec<InactiveToolPattern> {
    let mut issues = Vec::new();
    for pattern in patterns {
        if is_mcp_tool_name(pattern) {
            if !has_glob_magic(pattern) && !pattern["mcp__".len()..].contains("__") {
                issues.push(InactiveToolPattern {
                    pattern: pattern.clone(),
                    kind: InactiveToolPatternKind::IncompleteMcpName,
                });
            }
            continue;
        }
        if has_glob_magic(pattern) {
            issues.push(InactiveToolPattern {
                pattern: pattern.clone(),
                kind: InactiveToolPatternKind::WildcardNotMcp,
            });
            continue;
        }
        if let Some(is_known_tool_name) = is_known_tool_name
            && !is_known_tool_name(pattern)
        {
            issues.push(InactiveToolPattern {
                pattern: pattern.clone(),
                kind: InactiveToolPatternKind::UnknownTool,
            });
        }
    }
    issues
}
