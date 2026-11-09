//! Skill discovery and catalog scanning for Kimi Agent.
//!
//! Scans workspace-local (`.agents/skills`, `.kimi-code/skills`), user-global
//! (`~/.kimi-code/skills`, `~/.agents/skills`), and builtin product skills with
//! precedence: Project > User > Builtin.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-compatible skill descriptor returned by `/api/v1/workspaces/:id/skills`
/// and `/api/v1/sessions/:id/skills`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDescriptor {
    pub name: String,
    pub description: String,
    pub source: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_model_invocation: bool,
    /// UI-scope whitelist (v2 `SkillScope`: `tui` | `web`, #3843). `None`
    /// keeps the skill visible everywhere; a scope list restricts it to
    /// clients whose own mode appears in the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// A sub-skill of a `has-sub-skill: true` parent (v2 `SkillMetadata
    /// .isSubSkill`). `None` on a normal skill; clients use it to expose the
    /// child under its dotted parent name (`/parent.child`) and the engine's
    /// prompt keeps sub-skills out of the model's catalog — the parent skill is
    /// what the model sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_sub_skill: Option<bool>,
    /// Frontmatter `type` (v2 `SkillMetadata.type`): `prompt` | `inline` |
    /// `flow` | `reference`. `None` when the skill declares none, which
    /// [`crate::tools::skill::is_user_activatable_skill_type`] treats as
    /// activatable. Serialized so a host can apply the same gate instead of
    /// declaring a `type` field it never receives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<String>,
    /// Frontmatter `when-to-use` / `when_to_use` / `whenToUse` (v2
    /// `METADATA_ALIASES`, `parser.ts:46-51`). The model listing renders it as
    /// a `When to use:` line under the description (v2 `formatModelSkill`,
    /// `registry.ts:230-232`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
}

/// Skill scope values (v2 `SkillScope`, #3843): `tui` | `web`. `None` keeps
/// the skill visible everywhere.
pub type SkillScopes = Option<Vec<String>>;

/// Parse `scopes` from a frontmatter value (v2 `SkillScope`, #3843).
///
/// Accepts the array YAML produces (including a block list), the bracketed
/// inline list, and a bare comma-separated form. Unknown tokens are dropped;
/// an empty result stays `None`.
fn parse_scopes_value(value: Option<&Value>) -> SkillScopes {
    let scopes: Vec<String> = match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|token| token.trim().to_lowercase())
            .filter(|token| matches!(token.as_str(), "tui" | "web"))
            .collect(),
        Some(Value::String(raw)) => raw
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|token| {
                token
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_lowercase()
            })
            .filter(|token| matches!(token.as_str(), "tui" | "web"))
            .collect(),
        _ => Vec::new(),
    };
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

/// Full frontmatter parse: catalog fields plus the `Skill` tool's metadata
/// (`type`, declared `arguments`).
#[derive(Debug, Clone)]
pub struct ParsedSkillMeta {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
    pub scopes: SkillScopes,
    /// Frontmatter `type` (v2 `SkillMetadata.type`): `prompt` | `inline` |
    /// `flow` | `reference`. `None` defaults to `inline` at render time.
    pub skill_type: Option<String>,
    /// Frontmatter `when-to-use` / `when_to_use` / `whenToUse`.
    pub when_to_use: Option<String>,
    /// Named arguments declared via frontmatter `arguments` — a bare string,
    /// an inline `[a, b]` list, or a block `- a` list — filtered to valid
    /// names (non-empty, not all digits).
    pub argument_names: Vec<String>,
    /// Frontmatter `has-sub-skill` / `hasSubSkill` (v2 `hasSubSkillEnabled`):
    /// the parent opts its directory's children in as sub-skills.
    pub has_sub_skill: bool,
}

/// Parse metadata from Markdown content with optional YAML frontmatter.
pub fn parse_skill_metadata(content: &str, fallback_name: &str) -> (String, String, bool) {
    let meta = parse_skill_frontmatter(content, fallback_name);
    (meta.name, meta.description, meta.disable_model_invocation)
}

/// Parse the full frontmatter (see [`ParsedSkillMeta`]).
///
/// Uses the shared YAML frontmatter reader (v2 `parseFrontmatter`), so folded
/// scalars, block lists, inline comments and nested maps are real YAML rather
/// than the line-by-line reading this replaced.
pub fn parse_skill_frontmatter(content: &str, fallback_name: &str) -> ParsedSkillMeta {
    let mut meta = ParsedSkillMeta {
        name: fallback_name.to_string(),
        description: String::new(),
        disable_model_invocation: false,
        scopes: None,
        skill_type: None,
        when_to_use: None,
        argument_names: Vec::new(),
        has_sub_skill: false,
    };

    // A frontmatter block that cannot be read is not fatal here: the catalog
    // scans whatever skills a directory holds, and one malformed file must not
    // hide the rest. v2 throws (`SkillParseError`); the fork degrades to the
    // body-derived description instead, which is what it did before too.
    //
    // The description is derived from the **body**, never from the raw text:
    // scanning the whole document would pick a frontmatter line such as
    // `name: s` as the skill's description, which is what happened when this
    // path fell back to the un-split text.
    let Ok(parsed) = crate::frontmatter::parse_frontmatter(content) else {
        let body = body_of(content);
        return ParsedSkillMeta {
            description: first_prose_line(&body),
            ..meta
        };
    };
    let data = parsed.data;

    if let Some(name) = non_empty_string(data.get("name")) {
        meta.name = name;
    }
    if let Some(description) = non_empty_string(data.get("description")) {
        meta.description = description;
    }
    if let Some(kind) = non_empty_string(data.get("type")) {
        meta.skill_type = Some(kind);
    }
    // v2 `METADATA_ALIASES` accepts all three spellings of `whenToUse`.
    for key in ["whenToUse", "when-to-use", "when_to_use"] {
        if let Some(when_to_use) = non_empty_string(data.get(key)) {
            meta.when_to_use = Some(when_to_use);
            break;
        }
    }
    // v2 `METADATA_ALIASES` accepts both spellings of each of these two keys.
    for key in ["disable-model-invocation", "disable_model_invocation"] {
        if data.get(key).and_then(Value::as_bool) == Some(true) {
            meta.disable_model_invocation = true;
        }
    }
    for key in ["has-sub-skill", "hasSubSkill"] {
        if data.get(key).and_then(Value::as_bool) == Some(true) {
            meta.has_sub_skill = true;
        }
    }
    meta.scopes = parse_scopes_value(data.get("scopes"));
    meta.argument_names = parse_argument_names_value(data.get("arguments"));

    if meta.description.is_empty() {
        meta.description = first_prose_line(&parsed.body);
    }
    meta
}

/// v2 `isInlineSkillType` (`catalog/types.ts:92-94`): the types the model
/// listing advertises and the `Skill` tool accepts.
pub fn is_inline_skill_type(skill_type: Option<&str>) -> bool {
    matches!(skill_type, None | Some("prompt") | Some("inline"))
}

/// v2 `isUserActivatableSkillType` (`catalog/types.ts:96-98`): the inline types
/// plus `flow`, which only the user's `/skill` path may start.
pub fn is_user_activatable_skill_type(skill_type: Option<&str>) -> bool {
    is_inline_skill_type(skill_type) || skill_type == Some("flow")
}

/// v2 `isSupportedSkillType` (`catalog/types.ts:100-102`): anything outside
/// this set makes the skill file invalid, so discovery skips it.
pub fn is_supported_skill_type(skill_type: Option<&str>) -> bool {
    is_user_activatable_skill_type(skill_type) || skill_type == Some("reference")
}

/// v2 `parseSkillText` (`catalog/parser.ts:53-106`) as discovery applies it:
/// a directory `SKILL.md` must open with `---`, the frontmatter must be valid
/// YAML, `type` must be supported, and a directory skill must declare `name`
/// and `description`.
///
/// Returns the reason to skip the file. [`parse_skill_frontmatter`] stays
/// lenient for the callers that only want best-effort metadata; discovery is
/// where v2 rejects, so the model listing and the `Skill` tool never see a
/// half-parsed skill.
fn parse_skill_for_discovery(
    content: &str,
    fallback_name: &str,
    require_frontmatter: bool,
) -> Result<ParsedSkillMeta, String> {
    if require_frontmatter && content.lines().next().unwrap_or_default().trim() != "---" {
        return Err("Missing frontmatter".to_string());
    }
    let parsed = crate::frontmatter::parse_frontmatter(content)
        .map_err(|error| format!("Invalid frontmatter: {error}"))?;
    let meta = parse_skill_frontmatter(content, fallback_name);
    if !is_supported_skill_type(meta.skill_type.as_deref()) {
        return Err(format!(
            "unsupported skill type \"{}\"",
            meta.skill_type.as_deref().unwrap_or_default()
        ));
    }
    if require_frontmatter {
        // v2 `parser.ts:86-91`: both fields are required on a directory skill.
        if non_empty_string(parsed.data.get("name")).is_none() {
            return Err("Missing required frontmatter field \"name\"".to_string());
        }
        if non_empty_string(parsed.data.get("description")).is_none() {
            return Err("Missing required frontmatter field \"description\"".to_string());
        }
    }
    Ok(meta)
}

/// The document's body: everything after the closing fence, or the whole text
/// when there is no frontmatter. Used to recover a body even when the YAML
/// between the fences failed to parse.
fn body_of(content: &str) -> String {
    crate::frontmatter::parse_frontmatter(content)
        .map(|parsed| parsed.body)
        .unwrap_or_else(|_| {
            // The YAML is unparseable but the fences may still be findable.
            let lines: Vec<&str> = content.lines().collect();
            match lines
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, line)| line.trim() == "---")
            {
                Some((idx, _)) => lines[idx + 1..].join("\n"),
                None => content.to_string(),
            }
        })
}

/// v2 `descriptionFromBody` (`catalog/parser.ts:143-150`): the first line of
/// prose, truncated to 240 characters.
///
/// **One deliberate divergence.** v2 takes the first *non-empty* line; this
/// also skips headings, fences and block quotes, because a skill body almost
/// always opens with `##` or a code fence and v2's version would hand that
/// markup to the model as the description. The line-length rule below is v2's
/// verbatim. The skip list is a superset of v2's filter, so any body where v2
/// and this agree on the first prose line also agree on the result.
fn first_prose_line(body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("---")
                && !line.starts_with("```")
                && !line.starts_with('>')
        })
        .unwrap_or_default();
    // v2's cap: 240 characters, and the 240th is replaced by an ellipsis so the
    // result is never longer than 240. Applied here too — a long description
    // would otherwise reach the prompt unbounded.
    if first.chars().count() > 240 {
        let truncated: String = first.chars().take(239).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

/// v2 `nonEmptyString`: a string is used only when it is not blank.
fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_string)
}

fn is_valid_argument_name(name: &str) -> bool {
    !name.is_empty() && !name.bytes().all(|b| b.is_ascii_digit())
}

/// Parse frontmatter `arguments` (v2 `skillArgumentNames`).
///
/// Accepts a real YAML array — including a block list, which the previous
/// line-based reader had to special-case — plus the bare whitespace string and
/// the inline `[a, b]` list. Entries that are not valid names (blank or all
/// digits) are dropped, matching v2's `isValidName`.
fn parse_argument_names_value(value: Option<&Value>) -> Vec<String> {
    let raw = match value {
        Some(Value::Array(items)) => {
            return items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| is_valid_argument_name(item))
                .map(str::to_string)
                .collect();
        }
        Some(Value::String(raw)) => raw.trim(),
        _ => return Vec::new(),
    };
    if let Some(inner) = raw.strip_prefix('[') {
        inner
            .trim_end_matches(']')
            .split(',')
            .map(|item| item.trim().trim_matches('"').trim_matches('\''))
            .filter(|item| is_valid_argument_name(item))
            .map(str::to_string)
            .collect()
    } else {
        raw.split_whitespace()
            .filter(|item| is_valid_argument_name(item))
            .map(str::to_string)
            .collect()
    }
}

fn scan_directory(
    dir: &Path,
    source: &str,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
) {
    scan_directory_for_plugin(dir, source, None, out, seen);
}

/// [`scan_directory`] for a plugin-contributed root: every descriptor's name
/// is qualified as `<plugin_id>:<bare>` (v2 `registerPluginSkill`), so plugin
/// skills never collide with — or shadow — the bare names of other scopes.
fn scan_plugin_directory(
    dir: &Path,
    plugin_id: &str,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
) {
    // A plugin root reaches the wire as `extra` — v2 tags plugin roots the
    // same way (`manager.pluginSkillRoots`, `manager.ts:314`), and the
    // plugin's identity rides the prompt prefix and now the qualified name,
    // not the descriptor's `source`.
    scan_directory_for_plugin(dir, "extra", Some(plugin_id), out, seen);
}

/// v2 `MAX_SKILL_SCAN_DEPTH` (`fileSkillDiscovery.ts:12`): a `SKILL.md` nested
/// deeper than this is not discovered.
const MAX_SKILL_SCAN_DEPTH: usize = 8;

/// v2 `isSkillScanExcludedEntry` (`fileSkillDiscovery.ts:14-16`): the walk
/// never descends into `node_modules` or a dot directory. A `SKILL.md` inside
/// such a directory *is* still registered when the directory sits in a scanned
/// directory, because upstream collects `directorySkills` before applying this
/// filter.
fn is_scan_excluded_entry(name: &str) -> bool {
    name == "node_modules" || name.starts_with('.')
}

/// The `SKILL.md` (or the fork's lowercase `skill.md`) inside `dir`.
fn skill_file_in(dir: &Path) -> Option<PathBuf> {
    let upper = dir.join("SKILL.md");
    if upper.is_file() {
        return Some(upper);
    }
    let lower = dir.join("skill.md");
    lower.is_file().then_some(lower)
}

/// v2 `walkSkillDir` (`fileSkillDiscovery.ts:38-141`): recursive discovery over
/// a skill root.
///
/// Directories holding a `SKILL.md` are registered at whatever depth they sit
/// (≤ [`MAX_SKILL_SCAN_DEPTH`]); a directory skill that did not opt into
/// `has-sub-skill` hides its children; a `has-sub-skill: true` parent names its
/// descendants `<parent>.<child>` (v2 `allowedSubSkillBundles`); bare `<name>.md`
/// files are parsed only at the root of the scan.
fn scan_directory_for_plugin(
    dir: &Path,
    source: &str,
    plugin_id: Option<&str>,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
) {
    walk_skill_dir(dir, source, plugin_id, true, 0, None, out, seen);
}

#[allow(clippy::too_many_arguments)]
fn walk_skill_dir(
    dir: &Path,
    source: &str,
    plugin_id: Option<&str>,
    is_top_level: bool,
    depth: usize,
    sub_skill_parent: Option<&str>,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_SKILL_SCAN_DEPTH {
        return;
    }
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    // v2 `directorySkills`: every direct child directory holding a `SKILL.md`,
    // collected before the scan-exclusion filter so a dot-directory skill is
    // still registered.
    let mut directory_skills: Vec<(String, PathBuf)> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in &entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(skill_file) = skill_file_in(&entry.path()) {
            directory_skills.push((name.clone(), skill_file));
        }
        if is_scan_excluded_entry(&name) {
            continue;
        }
        subdirs.push(entry.path());
    }

    // Register this level's directory skills, remembering the ones whose
    // children are sub-skills (v2 `allowedSubSkillBundles`, keyed by directory
    // entry name but holding the *qualified* skill name).
    let mut allowed_sub_skill_parents: HashMap<String, String> = HashMap::new();
    let mut directory_skill_names: HashSet<String> = HashSet::new();
    for (name, skill_file) in &directory_skills {
        directory_skill_names.insert(name.clone());
        let Ok(content) = std::fs::read_to_string(skill_file) else {
            continue;
        };
        let meta = push_descriptor(
            &content,
            name,
            &skill_file.to_string_lossy(),
            source,
            true,
            out,
            seen,
            NameQualifiers {
                sub_skill_parent,
                plugin_id,
            },
        );
        if let Some(meta) = meta.filter(|meta| meta.has_sub_skill) {
            allowed_sub_skill_parents.insert(name.clone(), meta.name);
        }
    }

    if is_top_level {
        // v2 registers a plugin root's own `SKILL.md` (its directory name is
        // the skill name). Roots whose manifest opts into `root-skill-only`
        // scanning still walk their children here; that manifest flag is
        // carried by the plugin layer (B10-12), not by the scan root.
        if plugin_id.is_some()
            && let Some(skill_file) = skill_file_in(dir)
            && let Ok(content) = std::fs::read_to_string(&skill_file)
        {
            let root_name = dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            push_descriptor(
                &content,
                &root_name,
                &skill_file.to_string_lossy(),
                source,
                true,
                out,
                seen,
                NameQualifiers {
                    sub_skill_parent,
                    plugin_id,
                },
            );
        }
        // Bare `<name>.md` files are a root-level convenience (v2 parses them
        // only when `isTopLevel`): a directory skill of the same name wins.
        for entry in &entries {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !file_name.ends_with(".md") || file_name == "SKILL.md" {
                continue;
            }
            let base_name = file_name.trim_end_matches(".md");
            if directory_skill_names.contains(base_name) {
                continue;
            }
            if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            push_descriptor(
                &content,
                base_name,
                &entry.path().to_string_lossy(),
                source,
                false,
                out,
                seen,
                NameQualifiers {
                    sub_skill_parent,
                    plugin_id,
                },
            );
        }
    }

    for subdir in subdirs {
        let dir_name = subdir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        // A directory skill that did not opt into sub-skills hides its
        // children (v2 `fileSkillDiscovery.ts:131`).
        let child_parent = match allowed_sub_skill_parents.get(&dir_name) {
            Some(parent) => Some(parent.as_str()),
            None if directory_skill_names.contains(&dir_name) => continue,
            None => sub_skill_parent,
        };
        walk_skill_dir(
            &subdir,
            source,
            plugin_id,
            false,
            depth + 1,
            child_parent,
            out,
            seen,
        );
    }
}

/// v2 `registerPluginSkill` (`3f0857eb7d` #4188): a plugin skill's catalog
/// name is the qualified `<plugin_id>:<bare>` — never the bare name, so two
/// plugins can ship the same bare name without the dedupe set dropping one,
/// and a plugin skill can no longer shadow a project/user/extra skill under
/// that bare name. `bare` strips a pre-existing `<plugin_id>:` prefix, so
/// re-registering an already-qualified skill stays idempotent (v2's
/// `lark:lark:lark-doc` case).
fn qualify_plugin_skill_name(plugin_id: &str, name: &str) -> String {
    let prefix = format!("{plugin_id}:");
    let bare = name.strip_prefix(&prefix).unwrap_or(name);
    format!("{prefix}{bare}")
}

/// The two optional name qualifications [`push_descriptor`] applies, in
/// order: the `has-sub-skill` parent first (`<parent>.<child>`), then the
/// plugin prefix (`<plugin_id>:<bare>`, idempotent — the prefix is stripped
/// first, so a plugin's sub-skill chain qualifies exactly once).
#[derive(Debug, Clone, Copy, Default)]
struct NameQualifiers<'a> {
    sub_skill_parent: Option<&'a str>,
    plugin_id: Option<&'a str>,
}

/// Parse one skill file and append its descriptor under the given source,
/// honoring the case-insensitive dedupe set. `sub_skill_parent` names the
/// `has-sub-skill: true` parent whose child this is: the descriptor then takes
/// the qualified `<parent>.<child>` name and is marked as a sub-skill (v2
/// `parseAndRegister`'s `subSkillParentName` branch). `plugin_id` qualifies
/// the name as `<plugin_id>:<bare>` (v2 `registerPluginSkill`); applied after
/// the sub-skill qualification, and idempotent because the prefix is stripped
/// first, so a plugin's `<parent>.<child>` chain qualifies exactly once.
///
/// Returns the metadata of the descriptor it appended, so the caller can act
/// on `has_sub_skill`; `None` when the file is invalid or the name was already
/// taken.
fn push_descriptor(
    content: &str,
    fallback_name: &str,
    path: &str,
    source: &str,
    require_frontmatter: bool,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
    qualifiers: NameQualifiers<'_>,
) -> Option<ParsedSkillMeta> {
    let mut meta = match parse_skill_for_discovery(content, fallback_name, require_frontmatter) {
        Ok(meta) => meta,
        Err(reason) => {
            // v2 `parseAndRegister` (`fileSkillDiscovery.ts:191-204`): a skill
            // that cannot be parsed is skipped with a warning, never
            // registered — registering it would put a half-parsed entry in both
            // the model listing and the `Skill` tool.
            tracing::warn!(path, reason = %reason, "Skipping invalid skill");
            return None;
        }
    };
    if let Some(parent) = qualifiers.sub_skill_parent {
        meta.name = qualify_sub_skill_name(parent, &meta.name);
    }
    if let Some(plugin_id) = qualifiers.plugin_id {
        meta.name = qualify_plugin_skill_name(plugin_id, &meta.name);
    }
    if !seen.insert(meta.name.to_lowercase()) {
        return None;
    }
    out.push(SkillDescriptor {
        name: meta.name.clone(),
        description: meta.description.clone(),
        source: source.to_string(),
        path: path.replace('\\', "/"),
        disable_model_invocation: meta.disable_model_invocation,
        scopes: meta.scopes.clone(),
        is_sub_skill: qualifiers.sub_skill_parent.map(|_| true),
        skill_type: meta.skill_type.clone(),
        when_to_use: meta.when_to_use.clone(),
    });
    Some(meta)
}

/// v2 `qualifySubSkillName`: `<parent>.<child>`, leaving a name that is already
/// qualified — or a child named after its parent — untouched.
fn qualify_sub_skill_name(parent: &str, child: &str) -> String {
    if child == parent || child.starts_with(&format!("{parent}.")) {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

/// The roots and policy one catalog scan uses, owned so a session can serve the
/// *engine's* catalog to a host later (v2 serves the catalog from the engine:
/// the builtin product skills, the dotted sub-skill commands and
/// `extra_skill_dirs` exist only there).
#[derive(Debug, Clone, Default)]
pub struct SkillScanRoots {
    pub root: Option<PathBuf>,
    pub extra_dirs: Vec<PathBuf>,
    /// Skill roots contributed by enabled plugins, each with the plugin it came
    /// from (v2 `manager.pluginSkillRoots`, `app/plugin/manager.ts:307-321`).
    /// Scanned after `extra_dirs` and before the builtins, because v2 ranks
    /// `plugin` (5) between `extra` (10) and `builtin` (0).
    pub plugin_dirs: Vec<PluginSkillDir>,
    pub merge_all_available_skills: bool,
}

/// One plugin-contributed skill root plus the plugin's own instructions, which
/// v2 prefixes onto every skill that root provides
/// (`registry.renderSkillPrompt`, `catalog/registry.ts:71-79`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkillDir {
    pub dir: PathBuf,
    pub plugin_id: String,
    pub instructions: Option<String>,
    /// The skill this plugin activates when a session starts (v2
    /// `manifest.sessionStart.skill`). Repeated on every root the plugin
    /// contributes; the session-start renderer keys on the plugin id, not the
    /// root, so only one block per plugin is emitted.
    pub session_start: Option<String>,
}

impl SkillScanRoots {
    /// The catalog this session advertises: project scope, the user scope, the
    /// extra roots, the plugin roots (registered under qualified
    /// `<plugin_id>:<skill>` names, v2 `registerPluginSkill`), then the builtins.
    pub fn catalog(&self) -> Vec<SkillDescriptor> {
        scan_all_skills_with_extra_merge_and_plugins(
            self.root.as_deref(),
            &self.extra_dirs,
            &self.plugin_dirs,
            self.merge_all_available_skills,
        )
    }

    /// The non-brand roots in the order the scan walks them. The declared extra
    /// roots come before the plugin roots because v2 ranks `extra` (10) above
    /// `plugin` (5) — `SKILL_SOURCE_PRIORITY`, where the higher number wins.
    pub fn roots_in_precedence_order(&self) -> Vec<PathBuf> {
        roots_in_precedence_order(&self.extra_dirs, &self.plugin_dirs)
    }

    /// Every directory [`Self::catalog`] will read, in scan order. Absent roots
    /// are included (the scan tolerates them); this list exists so a change
    /// detector can ask which of them failed before trusting an empty catalog.
    pub fn scanned_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for group in [project_skill_dirs(self.root.as_deref()), user_skill_dirs()] {
            if self.merge_all_available_skills {
                roots.extend(group);
            } else if let Some(first) = group.into_iter().find(|dir| dir.is_dir()) {
                roots.push(first);
            }
        }
        roots.extend(self.extra_dirs.iter().cloned());
        roots.extend(self.plugin_dirs.iter().map(|entry| entry.dir.clone()));
        roots
    }

    /// [`Self::catalog`] with a root-level read failure kept distinct from an
    /// empty catalog. A configured root that exists but cannot be listed makes
    /// the whole scan `Unreadable`: a change detector must never read that as
    /// "every skill was deleted" and send a false reminder.
    pub fn catalog_outcome(&self) -> crate::primitives::read::ReadOutcome<Vec<SkillDescriptor>> {
        for root in self.scanned_roots() {
            match std::fs::read_dir(&root) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return crate::primitives::read::ReadOutcome::Unreadable(
                        crate::primitives::read::UnreadableCause::new(
                            root.display().to_string(),
                            error,
                        ),
                    );
                }
            }
        }
        crate::primitives::read::ReadOutcome::Found(self.catalog())
    }

    /// The plugin a skill root came from, if any — the counterpart of v2's
    /// `SkillRoot.plugin`, which the renderer needs for the instruction prefix.
    pub fn plugin_for(&self, dir: &Path) -> Option<&PluginSkillDir> {
        self.plugin_dirs.iter().find(|p| p.dir == dir)
    }
}

/// [`SkillScanRoots::roots_in_precedence_order`] as a free function, so a
/// `SkillScan` (the `Skill` tool's and the renderer) applies the same order
/// rather than re-deriving it.
pub fn roots_in_precedence_order(
    extra_dirs: &[PathBuf],
    plugin_dirs: &[PluginSkillDir],
) -> Vec<PathBuf> {
    extra_dirs
        .iter()
        .cloned()
        .chain(plugin_dirs.iter().map(|entry| entry.dir.clone()))
        .collect()
}

/// A builtin product skill: its catalog descriptor plus the embedded body
/// the `Skill` tool loads (v2 ships the same content through `*.md?raw`).
pub struct BuiltinSkillDef {
    pub descriptor: SkillDescriptor,
    pub body: &'static str,
    /// v2 `productSpecific`: hidden when `builtinProductSkills` is disabled
    /// (`builtin.ts:30-32`).
    pub product_specific: bool,
}

const CHECK_KIMI_CODE_DOCS_BODY: &str = include_str!("builtin/check-kimi-code-docs.md");
const MCP_CONFIG_BODY: &str = include_str!("builtin/mcp-config.md");
const UPDATE_CONFIG_BODY: &str = include_str!("builtin/update-config.md");
const WRITE_GOAL_BODY: &str = include_str!("builtin/write-goal.md");
const CUSTOM_THEME_BODY: &str = include_str!("builtin/custom-theme.md");
const IMPORT_FROM_CC_CODEX_BODY: &str = include_str!("builtin/import-from-cc-codex.md");
const SUB_SKILL_BODY: &str = include_str!("builtin/sub-skill/SKILL.md");
const SUB_SKILL_REVIEW_BODY: &str = include_str!("builtin/sub-skill/review/SKILL.md");
const SUB_SKILL_CONSOLIDATE_BODY: &str = include_str!("builtin/sub-skill/consolidate/SKILL.md");

/// Every builtin product skill, derived from its embedded `SKILL.md`: the
/// descriptor's name/description and the body the tool loads stay one source
/// (v2's `parseSkillText`, then the path/dir override).
///
/// This is the unfiltered list; [`builtin_skills`] applies v2's
/// `productSpecific` gate.
pub fn builtin_skill_defs() -> Vec<BuiltinSkillDef> {
    vec![
        // v2 `MCP_CONFIG_SKILL` (`builtin/mcp-config.ts:15-25`): an inline
        // product skill the model must not start on its own.
        builtin_def("mcp-config", MCP_CONFIG_BODY, None, true, true),
        builtin_def(
            "check-kimi-code-docs",
            CHECK_KIMI_CODE_DOCS_BODY,
            None,
            false,
            true,
        ),
        builtin_def("update-config", UPDATE_CONFIG_BODY, None, false, true),
        builtin_def("write-goal", WRITE_GOAL_BODY, None, false, false),
        // v2's `custom-theme` wrapper override: user-only (model invocation
        // disabled) and TUI-scoped.
        builtin_def(
            "custom-theme",
            CUSTOM_THEME_BODY,
            Some(vec!["tui".into()]),
            true,
            true,
        ),
        // v2 `IMPORT_FROM_CC_CODEX_SKILL`: a user-only product skill (it
        // migrates local Claude Code / Codex assets, so the model must not
        // start it on its own).
        builtin_def(
            "import-from-cc-codex",
            IMPORT_FROM_CC_CODEX_BODY,
            None,
            true,
            true,
        ),
        // v2's `sub-skill` bundle: the parent container plus its two
        // user-invocable children, whose names are qualified by the parent.
        sub_skill_def("sub-skill", "builtin://sub-skill", SUB_SKILL_BODY),
        sub_skill_def(
            "sub-skill.review",
            "builtin://sub-skill/review",
            SUB_SKILL_REVIEW_BODY,
        ),
        sub_skill_def(
            "sub-skill.consolidate",
            "builtin://sub-skill/consolidate",
            SUB_SKILL_CONSOLIDATE_BODY,
        ),
    ]
}

fn builtin_def(
    name: &str,
    body: &'static str,
    scopes: Option<Vec<String>>,
    disable_model_invocation: bool,
    product_specific: bool,
) -> BuiltinSkillDef {
    let parsed = parse_skill_frontmatter(body, name);
    BuiltinSkillDef {
        descriptor: SkillDescriptor {
            name: parsed.name,
            description: parsed.description,
            source: "builtin".into(),
            path: format!("builtin://{name}"),
            disable_model_invocation,
            scopes: if scopes.is_some() {
                scopes
            } else {
                parsed.scopes
            },
            is_sub_skill: None,
            skill_type: parsed.skill_type,
            when_to_use: parsed.when_to_use,
        },
        body,
        product_specific,
    }
}

/// One member of the `sub-skill` bundle (v2 `makeBuiltin`): the descriptor name
/// is the qualified one — the body carries only the child's own name — and
/// every member is user-invocable, so the prompt never advertises it. The two
/// children additionally carry `is_sub_skill`, which is what clients use to
/// expose them as dotted commands (`/sub-skill.review`).
fn sub_skill_def(name: &str, pseudo_path: &str, body: &'static str) -> BuiltinSkillDef {
    let parsed = parse_skill_frontmatter(body, name);
    BuiltinSkillDef {
        descriptor: SkillDescriptor {
            name: name.to_string(),
            description: parsed.description,
            source: "builtin".into(),
            path: pseudo_path.to_string(),
            disable_model_invocation: true,
            scopes: parsed.scopes,
            is_sub_skill: (name != "sub-skill").then_some(true),
            skill_type: parsed.skill_type,
            when_to_use: parsed.when_to_use,
        },
        body,
        product_specific: false,
    }
}

/// Builtin descriptors (see [`builtin_skill_defs`]), filtered the way v2's
/// `visibleBuiltinSkills` (`builtin/builtin.ts:25-37`) does:
///
/// - `productSpecific` skills are hidden when `builtinProductSkills` is `false`
///   (`KIMI_CODE_BUILTIN_PRODUCT_SKILLS`, documented default `true`);
/// - an `experimentalFlag` would gate the rest, but no builtin currently
///   declares one, so that filter is vacuous today.
///
/// Runtime contributions (`registerBuiltinSkill`) are a host-side API v2 takes
/// from TS plugin packages; the fork's equivalent is a plugin skill root, which
/// the scan adds under the `plugin` rank instead.
pub fn builtin_skills() -> Vec<SkillDescriptor> {
    let product_skills = crate::config::KimiConfig::resolved_builtin_product_skills();
    builtin_skill_defs()
        .into_iter()
        .filter(|def| product_skills || !def.product_specific)
        .map(|def| def.descriptor)
        .collect()
}

/// Scan all skills available to the given workspace root, merged with user-level
/// and builtin skills following Project > User > Builtin priority.
pub fn scan_all_skills(workspace_root: Option<&Path>) -> Vec<SkillDescriptor> {
    scan_all_skills_with_extra(workspace_root, &[])
}

/// [`scan_all_skills`] plus the user's `extra_skill_dirs` (schema): those
/// are scanned as project-level skills, after the conventional project
/// directories and before the user/home ones.
///
/// This is the "merge every available directory" default; callers that read
/// `merge_all_available_skills` from a config file use
/// [`scan_all_skills_with_extra_and_merge`] instead.
pub fn scan_all_skills_with_extra(
    workspace_root: Option<&Path>,
    extra_dirs: &[PathBuf],
) -> Vec<SkillDescriptor> {
    scan_all_skills_with_extra_and_merge(workspace_root, extra_dirs, true)
}

/// [`scan_all_skills_with_extra`] with the `merge_all_available_skills`
/// switch (schema; documented default `true`).
///
/// A scope group scans every directory it declares only while merging is on.
/// With merging off the group contributes just its first existing directory
/// (v2 `pushBrandGroup`'s `pushFirstExisting` fallback), so the flag selects
/// one scope's skills instead of layering `.agents/skills` and
/// `.kimi-code/skills` together. `extra_skill_dirs` and the builtins are
/// configured roots rather than brand groups, so the flag never drops them.
///
/// The switch selects one directory where this scan used to layer two: v2
/// gates only its single-element brand groups (`[.kimi-code/skills]`, and
/// `[skills]` under the brand home), so its `pushFirstExisting` fallback
/// resolves to the very directory merging keeps and the key is inert there.
/// The fork folds each scope's brand and generic directory into one group,
/// which is what gives the documented "from all available directories"
/// reading something to switch between.
pub fn scan_all_skills_with_extra_and_merge(
    workspace_root: Option<&Path>,
    extra_dirs: &[PathBuf],
    merge_all_available_skills: bool,
) -> Vec<SkillDescriptor> {
    scan_all_skills_with_extra_merge_and_plugins(
        workspace_root,
        extra_dirs,
        &[],
        merge_all_available_skills,
    )
}

/// v2 `findProjectRoot` (`skillRoots.ts:100-105`): the nearest ancestor of
/// `workspace_root` holding `.git`, falling back to `workspace_root` itself
/// when there is none.
fn resolve_project_root(workspace_root: &Path) -> PathBuf {
    for ancestor in workspace_root.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    workspace_root.to_path_buf()
}

/// The project-scope skill roots for `workspace_root`, in scan order. Shared by
/// the catalog scan and [`SkillScanRoots::scanned_roots`] so the change
/// detector probes exactly the directories the scan reads.
fn project_skill_dirs(workspace_root: Option<&Path>) -> Vec<PathBuf> {
    workspace_root
        .map(resolve_project_root)
        .map(|root| {
            vec![
                root.join(".agents").join("skills"),
                root.join(".kimi-code").join("skills"),
            ]
        })
        .unwrap_or_default()
}

/// The user-scope skill roots, in scan order (same home resolution as the scan).
fn user_skill_dirs() -> Vec<PathBuf> {
    let home_path = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    let home = PathBuf::from(home_path);
    vec![
        home.join(".kimi-code").join("skills"),
        home.join(".agents").join("skills"),
    ]
}

/// [`scan_all_skills_with_extra_and_merge`] plus plugin-contributed roots
/// (v2 `manager.pluginSkillRoots`): scanned after the declared extra roots and
/// before the builtins (`extra` 10 > `plugin` 5 > `builtin` 0), each
/// registered under its qualified `<plugin_id>:<skill>` name (v2
/// `registerPluginSkill`, `3f0857eb7d` #4188).
pub fn scan_all_skills_with_extra_merge_and_plugins(
    workspace_root: Option<&Path>,
    extra_dirs: &[PathBuf],
    plugin_dirs: &[PluginSkillDir],
    merge_all_available_skills: bool,
) -> Vec<SkillDescriptor> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    // 1. Project skills. v2 `findProjectRoot` (`skillRoots.ts:100-105`) walks
    // up from the work dir to the nearest `.git`, so a session started in a
    // repository subdirectory still sees the repository-level skills.
    scan_scope_group(
        &project_skill_dirs(workspace_root),
        "project",
        merge_all_available_skills,
        &mut out,
        &mut seen,
    );

    // 2. User skills
    scan_scope_group(
        &user_skill_dirs(),
        "user",
        merge_all_available_skills,
        &mut out,
        &mut seen,
    );

    // 3. `extra_skill_dirs`: additional scan roots the user declared. Scanned
    // after the user scope because v2 ranks `user` (20) above `extra` (10) —
    // see `SKILL_SOURCE_PRIORITY` / `workspaceSkillCatalogService.ts:140`,
    // where the higher number wins. The fork used to scan them second, so an
    // extra dir shadowed a same-named user skill.
    for dir in extra_dirs {
        scan_directory(dir, "extra", &mut out, &mut seen);
    }

    // 4. Plugin roots (v2 rank 5): qualified `<plugin_id>:<skill>` names, so a
    // plugin skill never shadows — or collides with — the bare names above.
    for plugin in plugin_dirs {
        scan_plugin_directory(&plugin.dir, &plugin.plugin_id, &mut out, &mut seen);
    }

    // 5. Builtin skills — v2's lowest rank (0), so anything above shadows them.
    for builtin in builtin_skills() {
        let key = builtin.name.to_lowercase();
        if seen.insert(key) {
            out.push(builtin);
        }
    }

    out
}

/// Scan one scope's directories: all of them when merging is on, else only
/// the first that exists, so a scope with no directory still contributes
/// nothing rather than shadowing a lower-precedence one.
fn scan_scope_group(
    dirs: &[PathBuf],
    source: &str,
    merge_all_available_skills: bool,
    out: &mut Vec<SkillDescriptor>,
    seen: &mut HashSet<String>,
) {
    if merge_all_available_skills {
        for dir in dirs {
            scan_directory(dir, source, out, seen);
        }
        return;
    }
    if let Some(first) = dirs.iter().find(|dir| dir.is_dir()) {
        scan_directory(first, source, out, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::read::ReadOutcome;

    /// A root that exists but is not a directory cannot be listed. The change
    /// notifier must see `Unreadable`, not an empty catalog, or it would report
    /// that every skill was deleted.
    #[test]
    fn test_catalog_outcome_separates_an_unreadable_root_from_an_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let readable = SkillScanRoots {
            root: None,
            extra_dirs: vec![dir.path().join("does-not-exist")],
            plugin_dirs: Vec::new(),
            merge_all_available_skills: true,
        };
        assert!(
            matches!(readable.catalog_outcome(), ReadOutcome::Found(_)),
            "an absent root is an empty catalog, not a failure"
        );

        let file_root = dir.path().join("skills-is-a-file");
        std::fs::write(&file_root, "not a directory").unwrap();
        let unreadable = SkillScanRoots {
            root: None,
            extra_dirs: vec![file_root],
            plugin_dirs: Vec::new(),
            merge_all_available_skills: true,
        };
        assert!(
            matches!(unreadable.catalog_outcome(), ReadOutcome::Unreadable(_)),
            "a root that cannot be listed must not read as empty"
        );
    }

    #[test]
    fn test_parse_skill_metadata_frontmatter() {
        let raw = r#"---
name: my-custom-skill
description: Does something awesome.
disable-model-invocation: true
---

# Title
Body text here.
"#;
        let (name, desc, disabled) = parse_skill_metadata(raw, "fallback");
        assert_eq!(name, "my-custom-skill");
        assert_eq!(desc, "Does something awesome.");
        assert!(disabled);
    }

    #[test]
    fn test_parse_skill_metadata_without_frontmatter() {
        let raw = r#"# Skill Title

This is the first paragraph describing the skill.
"#;
        let (name, desc, disabled) = parse_skill_metadata(raw, "my-fallback");
        assert_eq!(name, "my-fallback");
        assert_eq!(desc, "This is the first paragraph describing the skill.");
        assert!(!disabled);
    }

    #[test]
    fn test_scan_directory_and_precedence() {
        let temp_dir = tempfile::tempdir().unwrap();
        let proj_root = temp_dir.path();

        let proj_skills = proj_root.join(".agents").join("skills");
        let skill_a = proj_skills.join("skill-a");
        std::fs::create_dir_all(&skill_a).unwrap();
        std::fs::write(
            skill_a.join("SKILL.md"),
            "---\nname: skill-a\ndescription: Project version of A\n---\n",
        )
        .unwrap();

        let list = scan_all_skills(Some(proj_root));
        let found_a = list.iter().find(|s| s.name == "skill-a");
        assert!(found_a.is_some());
        let a = found_a.unwrap();
        assert_eq!(a.source, "project");
        assert_eq!(a.description, "Project version of A");

        // Builtins should also be included
        assert!(list.iter().any(|s| s.name == "check-kimi-code-docs"));
    }

    /// v2 `walkSkillDir` (`fileSkillDiscovery.ts:38-141`): `SKILL.md` files are
    /// found at any depth ≤ 8, the walk does not descend into `node_modules` /
    /// dot directories (a direct child skill in one still registers), and a
    /// bare `.md` file is parsed only at the root of a scan root.
    #[test]
    fn test_discovery_walks_nested_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let project = temp_dir.path();
        let skills_root = project.join(".kimi-code").join("skills");

        let nested = skills_root.join("group").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\nname: nested-skill\ndescription: Found two levels down.\n---\n",
        )
        .unwrap();

        let hidden = skills_root.join(".hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(
            hidden.join("SKILL.md"),
            "---\nname: hidden-skill\ndescription: Dot directory.\n---\n",
        )
        .unwrap();

        let modules = skills_root.join("node_modules").join("dep");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(
            modules.join("SKILL.md"),
            "---\nname: dependency-skill\ndescription: Must not be scanned.\n---\n",
        )
        .unwrap();

        std::fs::write(
            skills_root.join("loose.md"),
            "Loose skill body without frontmatter.\n",
        )
        .unwrap();

        let list = scan_all_skills(Some(project));
        assert!(list.iter().any(|skill| skill.name == "nested-skill"));
        // A dot directory holding a SKILL.md is registered (upstream collects
        // `directorySkills` before the exclusion filter) ...
        assert!(list.iter().any(|skill| skill.name == "hidden-skill"));
        // ... but `node_modules` is never walked into.
        assert!(!list.iter().any(|skill| skill.name == "dependency-skill"));
        // Root-level bare `.md` files still register.
        assert!(list.iter().any(|skill| skill.name == "loose"));
    }

    /// v2 `findProjectRoot` (`skillRoots.ts:100-105`): the project scope is the
    /// nearest ancestor holding `.git`, not the session's work dir.
    #[test]
    fn test_project_skills_resolve_from_the_git_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = temp_dir.path();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let skill_dir = repo.join(".kimi-code").join("skills").join("repo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: repo-skill\ndescription: Lives at the repo root.\n---\n",
        )
        .unwrap();

        let subdir = repo.join("packages").join("app");
        std::fs::create_dir_all(&subdir).unwrap();

        let list = scan_all_skills(Some(&subdir));
        assert!(list.iter().any(|skill| skill.name == "repo-skill"));
    }

    /// v2 `parseSkillText` + `parseAndRegister` (`catalog/parser.ts:53-106`,
    /// `fileSkillDiscovery.ts:191-204`): a directory `SKILL.md` without
    /// frontmatter, with an unsupported `type`, or without `name`/`description`
    /// is skipped instead of registered with degraded metadata.
    #[test]
    fn test_invalid_directory_skills_are_skipped() {
        let temp_dir = tempfile::tempdir().unwrap();
        let proj_root = temp_dir.path();
        let skills_dir = proj_root.join(".agents").join("skills");

        let write = |name: &str, content: &str| {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), content).unwrap();
        };

        write("missing-frontmatter", "# Just a title\n\nBody.\n");
        write(
            "unsupported-type",
            "---\nname: unsupported-type\ndescription: nope\ntype: banana\n---\nBody.\n",
        );
        write(
            "missing-description",
            "---\nname: missing-description\n---\nBody.\n",
        );
        write(
            "valid-skill",
            "---\nname: valid-skill\ndescription: fine\n---\nBody.\n",
        );

        let list = scan_all_skills(Some(proj_root));
        assert!(list.iter().any(|s| s.name == "valid-skill"));
        for skipped in [
            "missing-frontmatter",
            "unsupported-type",
            "missing-description",
        ] {
            assert!(
                !list.iter().any(|s| s.name == skipped),
                "{skipped} must not be registered"
            );
        }
    }

    /// v2's ranking (`SKILL_SOURCE_PRIORITY`, `features/skill/catalog/skillSource.ts:11-17`,
    /// applied by `workspaceSkillCatalogService.ts:140` where the **higher**
    /// number wins): builtin 0 < plugin 5 < extra 10 < user 20 < workspace 30.
    ///
    /// `user` over `extra` is the half that used to be wrong: the fork scanned
    /// `extra_skill_dirs` second and labelled them `"project"`. The plugin rank
    /// is pinned by [`plugin_roots_rank_below_extra_dirs`].
    #[test]
    fn test_extra_dirs_rank_below_user_and_report_their_own_source() {
        let temp_dir = tempfile::tempdir().unwrap();
        let proj_root = temp_dir.path();
        let extra_root = temp_dir.path().join("extra-root");

        // Same skill name in both an extra dir and the user scope.
        let extra_skill = extra_root.join("shared-skill");
        std::fs::create_dir_all(&extra_skill).unwrap();
        std::fs::write(
            extra_skill.join("SKILL.md"),
            "---\nname: shared-skill\ndescription: From an extra dir\n---\n",
        )
        .unwrap();

        // The user scope is read from the OS home, so point HOME at a temp dir
        // holding the same skill. On Windows `USERPROFILE` wins, so set both.
        let home = temp_dir.path().join("home");
        let user_skill = home.join(".kimi-code").join("skills").join("shared-skill");
        std::fs::create_dir_all(&user_skill).unwrap();
        std::fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: shared-skill\ndescription: From the user scope\n---\n",
        )
        .unwrap();

        // The env scope covers exactly the scan that reads the user scope: it
        // holds the suite's environment lock and restores `HOME`/`USERPROFILE`
        // on drop, so a panicking assertion below cannot leave the whole run
        // with a redirected home.
        let list = {
            let _env = crate::test_support::EnvScope::set(vec![
                ("HOME", Some(home.to_string_lossy().into_owned())),
                ("USERPROFILE", Some(home.to_string_lossy().into_owned())),
            ]);
            scan_all_skills_with_extra(Some(proj_root), &[extra_root])
        };

        let shared = list.iter().find(|s| s.name == "shared-skill").unwrap();
        assert_eq!(
            shared.description, "From the user scope",
            "user (20) outranks extra (10) in v2"
        );
        assert_eq!(shared.source, "user");

        // An extra-dir-only skill keeps its own label instead of claiming to be
        // a workspace skill — the label reaches the model's `<skill-loaded
        // source=...>` now that the engine renders the prompt.
        let extra_only = temp_dir.path().join("extra-only-root");
        std::fs::create_dir_all(extra_only.join("extra-only")).unwrap();
        std::fs::write(
            extra_only.join("extra-only").join("SKILL.md"),
            "---\nname: extra-only\ndescription: Extra\n---\n",
        )
        .unwrap();
        let list = scan_all_skills_with_extra(Some(proj_root), &[extra_only]);
        let found = list.iter().find(|s| s.name == "extra-only").unwrap();
        assert_eq!(found.source, "extra");
    }

    /// The plugin rank: `extra` (10) outranks `plugin` (5), and a plugin root
    /// is scanned even though it is not a declared `extra_skill_dirs` entry.
    /// Before the plugin roots had their own list they were appended to the
    /// extra roots, which happened to order correctly but left the contributing
    /// plugin unattributed — the renderer could not prefix its instructions.
    #[test]
    fn plugin_roots_rank_below_extra_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let extra_root = temp_dir.path().join("extra-root");
        let plugin_root = temp_dir.path().join("plugin").join("skills");

        for (root, description) in [
            (&extra_root, "From an extra dir"),
            (&plugin_root, "From a plugin"),
        ] {
            std::fs::create_dir_all(root.join("clash")).unwrap();
            std::fs::write(
                root.join("clash").join("SKILL.md"),
                format!("---\nname: clash\ndescription: {description}\n---\n"),
            )
            .unwrap();
        }
        // A plugin-only skill, to show the root is scanned at all.
        std::fs::create_dir_all(plugin_root.join("plugin-only")).unwrap();
        std::fs::write(
            plugin_root.join("plugin-only").join("SKILL.md"),
            "---\nname: plugin-only\ndescription: Only the plugin has it\n---\n",
        )
        .unwrap();

        let roots = SkillScanRoots {
            root: None,
            extra_dirs: vec![extra_root],
            plugin_dirs: vec![PluginSkillDir {
                dir: plugin_root,
                plugin_id: "demo".into(),
                instructions: None,
                session_start: None,
            }],
            merge_all_available_skills: true,
        };
        let list = roots.catalog();

        // v2 `registerPluginSkill` (#4188): the extra dir keeps the bare name
        // (`extra` 10 > `plugin` 5), and the plugin's copy registers under its
        // qualified name instead of shadowing or being dropped.
        let clash = list.iter().find(|s| s.name == "clash").unwrap();
        assert_eq!(
            clash.description, "From an extra dir",
            "extra (10) outranks plugin (5) in v2"
        );
        let qualified = list.iter().find(|s| s.name == "demo:clash").unwrap();
        assert_eq!(qualified.description, "From a plugin");
        assert!(list.iter().any(|s| s.name == "demo:plugin-only"));
        assert!(!list.iter().any(|s| s.name == "plugin-only"));
        // A plugin root reaches the wire as `extra` — v2 tags plugin roots the
        // same way (`manager.pluginSkillRoots`, `manager.ts:314`), and the
        // plugin's identity rides the prompt prefix and the qualified name,
        // not the descriptor's `source`.
        assert_eq!(clash.source, "extra");
        assert_eq!(qualified.source, "extra");
    }

    #[test]
    fn same_named_skills_from_different_plugins_stay_visible_under_qualified_names() {
        // v2 `keeps same-named skills from different plugins visible under
        // qualified names only` (registry.test.ts, #4188).
        let temp_dir = tempfile::tempdir().unwrap();
        let plugin_a = temp_dir.path().join("plugin-a");
        let plugin_b = temp_dir.path().join("plugin-b");
        for (root, description) in [(&plugin_a, "from plugin a"), (&plugin_b, "from plugin b")] {
            std::fs::create_dir_all(root.join("lark-doc")).unwrap();
            std::fs::write(
                root.join("lark-doc").join("SKILL.md"),
                format!("---\nname: lark-doc\ndescription: {description}\n---\n"),
            )
            .unwrap();
        }
        let roots = SkillScanRoots {
            root: None,
            extra_dirs: Vec::new(),
            plugin_dirs: vec![
                PluginSkillDir {
                    dir: plugin_a,
                    plugin_id: "plugin-a".into(),
                    instructions: None,
                    session_start: None,
                },
                PluginSkillDir {
                    dir: plugin_b,
                    plugin_id: "plugin-b".into(),
                    instructions: None,
                    session_start: None,
                },
            ],
            merge_all_available_skills: true,
        };
        let list = roots.catalog();

        assert!(list.iter().all(|s| s.name != "lark-doc"));
        assert_eq!(
            list.iter()
                .find(|s| s.name == "plugin-a:lark-doc")
                .unwrap()
                .description,
            "from plugin a"
        );
        assert_eq!(
            list.iter()
                .find(|s| s.name == "plugin-b:lark-doc")
                .unwrap()
                .description,
            "from plugin b"
        );
    }

    #[test]
    fn plugin_skill_does_not_shadow_a_bare_name_from_another_scope() {
        // v2 `does not let plugin skills shadow bare names from other sources`.
        let temp_dir = tempfile::tempdir().unwrap();
        let proj_root = temp_dir.path();
        let plugin_root = proj_root.join("plugin");
        std::fs::create_dir_all(plugin_root.join("review")).unwrap();
        std::fs::write(
            plugin_root.join("review").join("SKILL.md"),
            "---\nname: review\ndescription: from plugin\n---\n",
        )
        .unwrap();
        let project_skill = proj_root.join(".kimi-code").join("skills").join("review");
        std::fs::create_dir_all(&project_skill).unwrap();
        std::fs::write(
            project_skill.join("SKILL.md"),
            "---\nname: review\ndescription: from project\n---\n",
        )
        .unwrap();

        let roots = SkillScanRoots {
            root: Some(proj_root.to_path_buf()),
            extra_dirs: Vec::new(),
            plugin_dirs: vec![PluginSkillDir {
                dir: plugin_root,
                plugin_id: "superpowers".into(),
                instructions: None,
                session_start: None,
            }],
            merge_all_available_skills: true,
        };
        let list = roots.catalog();

        let bare = list.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(bare.description, "from project");
        assert_eq!(bare.source, "project");
        assert_eq!(
            list.iter()
                .find(|s| s.name == "superpowers:review")
                .unwrap()
                .description,
            "from plugin"
        );
    }

    #[test]
    fn re_registering_a_qualified_plugin_skill_stays_idempotent() {
        // v2 `stays idempotent when a merged catalog re-registers listed
        // skills` (`lark:lark:lark-doc` must never appear).
        let temp_dir = tempfile::tempdir().unwrap();
        let plugin_root = temp_dir.path().join("plugin");
        std::fs::create_dir_all(plugin_root.join("lark-doc")).unwrap();
        std::fs::write(
            plugin_root.join("lark-doc").join("SKILL.md"),
            "---\nname: lark:lark-doc\ndescription: from plugin\n---\n",
        )
        .unwrap();

        let roots = SkillScanRoots {
            root: None,
            extra_dirs: Vec::new(),
            plugin_dirs: vec![PluginSkillDir {
                dir: plugin_root,
                plugin_id: "lark".into(),
                instructions: None,
                session_start: None,
            }],
            merge_all_available_skills: true,
        };
        let list = roots.catalog();

        assert!(list.iter().any(|s| s.name == "lark:lark-doc"));
        assert!(list.iter().all(|s| s.name != "lark:lark:lark-doc"));
    }

    #[test]
    fn test_merge_switch_limits_scope_group_to_first_existing_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let proj_root = temp_dir.path();

        let generic = proj_root
            .join(".agents")
            .join("skills")
            .join("generic-skill");
        std::fs::create_dir_all(&generic).unwrap();
        std::fs::write(
            generic.join("SKILL.md"),
            "---\nname: generic-skill\ndescription: From .agents\n---\n",
        )
        .unwrap();
        let brand = proj_root
            .join(".kimi-code")
            .join("skills")
            .join("brand-skill");
        std::fs::create_dir_all(&brand).unwrap();
        std::fs::write(
            brand.join("SKILL.md"),
            "---\nname: brand-skill\ndescription: From .kimi-code\n---\n",
        )
        .unwrap();

        // Merging (the documented default) layers both project directories.
        let merged = scan_all_skills_with_extra_and_merge(Some(proj_root), &[], true);
        assert!(merged.iter().any(|s| s.name == "generic-skill"));
        assert!(merged.iter().any(|s| s.name == "brand-skill"));

        // With merging off the project group keeps only `.agents/skills`, its
        // first existing directory, so `.kimi-code/skills` goes unscanned.
        let selected = scan_all_skills_with_extra_and_merge(Some(proj_root), &[], false);
        assert!(selected.iter().any(|s| s.name == "generic-skill"));
        assert!(
            !selected.iter().any(|s| s.name == "brand-skill"),
            "merging off must not scan the group's second directory"
        );

        // Builtins stay regardless of the switch.
        assert!(selected.iter().any(|s| s.name == "check-kimi-code-docs"));
    }

    /// A body-derived description is capped the way v2 caps it
    /// (`descriptionFromBody`, `catalog/parser.ts:148-149`): past 240
    /// characters the 240th is replaced by an ellipsis, so the result is never
    /// longer than 240. Without the cap an unbounded first line reaches the
    /// prompt as the skill's description.
    #[test]
    fn a_body_derived_description_is_capped_at_240_characters() {
        let long = "x".repeat(300);
        let doc = format!("---\nname: s\n---\n{long}\n");
        let meta = parse_skill_frontmatter(&doc, "s");
        assert_eq!(
            meta.description.chars().count(),
            240,
            "the cap is 240 characters including the ellipsis"
        );
        assert!(
            meta.description.ends_with('…'),
            "the 240th character is replaced by an ellipsis, got {:?}",
            meta.description.chars().last()
        );
        assert!(
            !meta.description.starts_with('x') || meta.description.contains('…'),
            "a capped description is marked, not silently truncated"
        );

        // Exactly at the cap: untouched, because v2 only truncates *past* 240.
        let exact = "y".repeat(240);
        let doc = format!("---\nname: s\n---\n{exact}\n");
        let meta = parse_skill_frontmatter(&doc, "s");
        assert_eq!(meta.description, exact, "240 characters is not truncated");
        assert!(!meta.description.contains('…'));

        // An explicit frontmatter description is the author's, not the
        // fallback, so the cap does not apply to it — v2 caps only
        // `descriptionFromBody`'s output.
        let doc = format!("---\nname: s\ndescription: \"{long}\"\n---\nbody\n");
        let meta = parse_skill_frontmatter(&doc, "s");
        assert_eq!(
            meta.description, long,
            "a declared description is not capped"
        );
    }

    /// The four ways the previous line-by-line reader mis-read frontmatter.
    /// Each case was run against the old parser and produced a wrong value:
    /// a folded scalar came back as the literal `">"`, a block `scopes:` list
    /// was dropped entirely (widening the skill's visibility), an inline
    /// comment was kept in the value, and a nested map left its parent key
    /// empty. Real YAML (v2 `parseFrontmatter` over `js-yaml`) fixes all four.
    #[test]
    fn frontmatter_is_read_as_yaml_not_as_lines() {
        let folded = parse_skill_frontmatter(
            "---\nname: s\ndescription: >\n  a long folded\n  desc\n---\nb",
            "s",
        );
        assert_eq!(folded.description, "a long folded desc");

        let block = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\nscopes:\n  - tui\n  - web\n---\nbody",
            "s",
        );
        assert_eq!(block.scopes, Some(vec!["tui".into(), "web".into()]));

        let commented =
            parse_skill_frontmatter("---\nname: s  # the name\ndescription: d\n---\nbody", "s");
        assert_eq!(commented.name, "s");

        // The nested map keeps its own keys; `meta` is not flattened to "".
        let nested = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\nmeta:\n  a: 1\n  b: 2\n---\nbody",
            "s",
        );
        assert_eq!(nested.name, "s");
        assert_eq!(nested.description, "d");

        let block_args = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\narguments:\n  - alpha\n  - beta\n---\nbody",
            "s",
        );
        assert_eq!(block_args.argument_names, vec!["alpha", "beta"]);
    }

    /// An unquoted value containing `: ` is not valid YAML — the second colon
    /// starts a nested mapping. v2 throws here (`js-yaml` does too), so the
    /// fork degrades to the body-derived description rather than inventing a
    /// value; a quoted one parses normally.
    #[test]
    fn a_malformed_frontmatter_block_degrades_instead_of_inventing_values() {
        let bad = parse_skill_frontmatter(
            "---\nname: s\ndescription: Who they are: name, role.\n---\n\nFirst body line.\n",
            "s",
        );
        assert_eq!(
            bad.description, "First body line.",
            "an unparseable block falls back to the body's first prose line"
        );
        assert_eq!(bad.name, "s", "the fallback name still applies");

        let quoted = parse_skill_frontmatter(
            "---\nname: s\ndescription: \"Who they are: name, role.\"\n---\nbody",
            "s",
        );
        assert_eq!(quoted.description, "Who they are: name, role.");
    }

    #[test]
    fn test_parse_scopes_frontmatter() {
        // Bracketed list, bare list, and single-token forms all parse.
        assert_eq!(
            parse_skill_frontmatter(
                "---\nname: s\ndescription: d\nscopes: [tui]\n---\nbody",
                "s"
            )
            .scopes,
            Some(vec!["tui".into()])
        );
        assert_eq!(
            parse_skill_frontmatter(
                "---\nname: s\ndescription: d\nscopes: [tui, web]\n---\nbody",
                "s"
            )
            .scopes,
            Some(vec!["tui".into(), "web".into()])
        );
        assert_eq!(
            parse_skill_frontmatter("---\nname: s\ndescription: d\nscopes: web\n---\nbody", "s")
                .scopes,
            Some(vec!["web".into()])
        );

        // Unknown tokens are dropped; nothing valid stays None.
        assert_eq!(
            parse_skill_frontmatter(
                "---\nname: s\ndescription: d\nscopes: [cli, ide]\n---\nbody",
                "s"
            )
            .scopes,
            None
        );
    }

    #[test]
    fn test_parse_type_and_arguments_frontmatter() {
        // Bare string arguments map positionally by name.
        let meta = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\ntype: prompt\narguments: target mode\n---\nbody",
            "s",
        );
        assert_eq!(meta.skill_type.as_deref(), Some("prompt"));
        assert_eq!(meta.argument_names, vec!["target", "mode"]);

        // Inline list arguments.
        let meta = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\narguments: [a, b]\n---\nbody",
            "s",
        );
        assert_eq!(meta.argument_names, vec!["a", "b"]);

        // Block list arguments.
        let meta = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\narguments:\n  - a\n  - b\n---\nbody",
            "s",
        );
        assert_eq!(meta.argument_names, vec!["a", "b"]);

        // All-digit and empty names are dropped (they are positional, not named).
        let meta = parse_skill_frontmatter(
            "---\nname: s\ndescription: d\narguments: [\"1\", name, \"\"]\n---\nbody",
            "s",
        );
        assert_eq!(meta.argument_names, vec!["name"]);
    }

    #[test]
    fn test_builtin_sub_skill_bundle_is_user_only_and_qualified() {
        let defs = builtin_skill_defs();

        // The container itself: user-invocable, not itself a sub-skill.
        let parent = defs
            .iter()
            .find(|d| d.descriptor.name == "sub-skill")
            .expect("sub-skill builtin");
        assert!(parent.descriptor.disable_model_invocation);
        assert_eq!(parent.descriptor.is_sub_skill, None);
        assert!(!parent.body.is_empty());

        // Its children carry the qualified name clients expose as a dotted
        // command, and stay user-only.
        for (name, path) in [
            ("sub-skill.review", "builtin://sub-skill/review"),
            ("sub-skill.consolidate", "builtin://sub-skill/consolidate"),
        ] {
            let child = defs.iter().find(|d| d.descriptor.name == name).expect(name);
            assert_eq!(child.descriptor.is_sub_skill, Some(true), "{name}");
            assert!(
                child.descriptor.disable_model_invocation,
                "{name} is user-invocable only"
            );
            assert_eq!(child.descriptor.path, path);
            assert!(!child.body.is_empty(), "{name} needs its body");
        }

        // The Claude Code / Codex importer is a user-only product skill too.
        let importer = defs
            .iter()
            .find(|d| d.descriptor.name == "import-from-cc-codex")
            .expect("import-from-cc-codex builtin");
        assert!(importer.descriptor.disable_model_invocation);
        assert_eq!(importer.descriptor.is_sub_skill, None);
        assert!(!importer.body.is_empty());
    }

    /// v2 `fileSkillDiscovery`'s `allowedSubSkillBundles`: a parent that opts in
    /// with `has-sub-skill: true` has its directory children registered as
    /// `<parent>.<child>`.
    #[test]
    fn test_a_sub_skill_parent_qualifies_its_children() {
        let temp_dir = tempfile::tempdir().unwrap();
        let parent = temp_dir
            .path()
            .join(".agents")
            .join("skills")
            .join("bundle");
        std::fs::create_dir_all(parent.join("child")).unwrap();
        std::fs::write(
            parent.join("SKILL.md"),
            "---\nname: bundle\ndescription: Container.\nhas-sub-skill: true\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            parent.join("child").join("SKILL.md"),
            "---\nname: child\ndescription: A child.\n---\n\nChild body.\n",
        )
        .unwrap();

        let list = scan_all_skills(Some(temp_dir.path()));
        let parent_entry = list.iter().find(|s| s.name == "bundle").expect("parent");
        assert_eq!(parent_entry.is_sub_skill, None);
        let child_entry = list
            .iter()
            .find(|s| s.name == "bundle.child")
            .expect("qualified child");
        assert_eq!(child_entry.is_sub_skill, Some(true));
        assert_eq!(child_entry.description, "A child.");
        assert!(
            child_entry
                .path
                .replace('\\', "/")
                .ends_with("bundle/child/SKILL.md"),
            "{}",
            child_entry.path
        );
    }

    /// Without the opt-in the children are ordinary directories, not skills —
    /// the flag is what pulls them into the catalog.
    #[test]
    fn test_a_parent_without_the_flag_keeps_its_children_unlisted() {
        let temp_dir = tempfile::tempdir().unwrap();
        let parent = temp_dir.path().join(".agents").join("skills").join("plain");
        std::fs::create_dir_all(parent.join("child")).unwrap();
        std::fs::write(
            parent.join("SKILL.md"),
            "---\nname: plain\ndescription: No children.\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            parent.join("child").join("SKILL.md"),
            "---\nname: child\ndescription: Not a sub-skill.\n---\n\nChild body.\n",
        )
        .unwrap();

        let list = scan_all_skills(Some(temp_dir.path()));
        assert!(list.iter().any(|s| s.name == "plain"));
        assert!(
            !list
                .iter()
                .any(|s| s.name == "child" || s.name == "plain.child"),
            "a parent without `has-sub-skill` must not register its children"
        );
    }

    #[test]
    fn test_parse_disable_model_invocation_accepts_both_spellings() {
        // v2 `METADATA_ALIASES` maps both spellings onto the same field, and
        // the docs plus existing skills on disk use the snake_case one.
        for body in [
            "---\nname: s\ndisable-model-invocation: true\n---\nbody",
            "---\nname: s\ndisable_model_invocation: true\n---\nbody",
            "---\nname: s\ndisable_model_invocation: TRUE\n---\nbody",
        ] {
            assert!(
                parse_skill_frontmatter(body, "s").disable_model_invocation,
                "{body}"
            );
        }
        for body in [
            "---\nname: s\n---\nbody",
            "---\nname: s\ndisable_model_invocation: false\n---\nbody",
        ] {
            assert!(
                !parse_skill_frontmatter(body, "s").disable_model_invocation,
                "{body}"
            );
        }
    }

    #[test]
    fn test_parse_has_sub_skill_frontmatter() {
        // Both spellings v2 accepts (`hasSubSkillEnabled`).
        for body in [
            "---\nname: s\nhas-sub-skill: true\n---\nbody",
            "---\nname: s\nhasSubSkill: true\n---\nbody",
            "---\nname: s\nhas-sub-skill: TRUE\n---\nbody",
        ] {
            assert!(parse_skill_frontmatter(body, "s").has_sub_skill, "{body}");
        }
        // Absent or off leaves it unset.
        for body in [
            "---\nname: s\n---\nbody",
            "---\nname: s\nhas-sub-skill: false\n---\nbody",
        ] {
            assert!(!parse_skill_frontmatter(body, "s").has_sub_skill, "{body}");
        }
    }

    #[test]
    fn test_custom_theme_builtin_is_user_only_and_tui_scoped() {
        let defs = builtin_skill_defs();
        let theme = defs
            .iter()
            .find(|d| d.descriptor.name == "custom-theme")
            .expect("custom-theme builtin");
        assert_eq!(theme.descriptor.scopes, Some(vec!["tui".into()]));
        assert!(theme.descriptor.disable_model_invocation);
        // The body the Skill tool loads is present and frontmatter-free at load.
        assert!(!theme.body.is_empty());

        // The unscoped builtins stay model-invocable.
        let docs = defs
            .iter()
            .find(|d| d.descriptor.name == "check-kimi-code-docs")
            .unwrap();
        assert_eq!(docs.descriptor.scopes, None);
        assert!(!docs.descriptor.disable_model_invocation);
    }
}
