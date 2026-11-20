use base64::Engine as _;
use indexmap::IndexMap;
use serde_json::{Map, Value};

#[derive(Default)]
struct Projection(IndexMap<String, String>);

impl Projection {
    fn insert(&mut self, key: String, value: Value) {
        self.0.insert(key, super::github_request::js_json_stringify(&value));
    }

    fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }
}

pub fn shape_response(name: &str, body: &str, next_page: bool, args: &Value) -> Option<String> {
    match name {
        "GitHubListBranches" => shape_array(body, project_branch, None, false),
        "GitHubListCommits" => shape_array(body, project_commit, None, false),
        "GitHubListPRCommits" => shape_array(body, project_commit, None, next_page),
        "GitHubGetCommit" => {
            let value: Value = serde_json::from_str(body).ok()?;
            Some(project_commit_detail(value.as_object()?))
        },
        "GitHubListIssues" => shape_array(body, project_issue, None, next_page),
        "GitHubGetIssue" => shape_object(body, project_issue_detail),
        "GitHubListIssueComments" | "GitHubListPRReviewComments" => {
            shape_array(body, project_comment, None, next_page)
        }
        "GitHubListPRs" => shape_array(body, project_pull, None, next_page),
        "GitHubGetPR" => shape_object(body, project_pull_detail),
        "GitHubGetPRFiles" => shape_array(body, project_pull_file, None, next_page),
        "GitHubSearchCode" => shape_search(body, project_code_search, next_page),
        "GitHubSearchRepos" => shape_search(body, project_repo_search, next_page),
        "GitHubSearchIssues" => shape_search(body, project_issue_search, next_page),
        "GitHubGetCheckRuns" => shape_wrapped_list(body, "check_runs", project_check_run, next_page),
        "GitHubListWorkflowRunJobs" => shape_wrapped_list(body, "jobs", project_workflow_job, next_page),
        "GitHubListWorkflowRuns" => shape_array(body, project_workflow_run, None, next_page),
        "GitHubGetWorkflowRun" => shape_object(body, project_workflow_run),
        "GitHubListReleases" => shape_array(body, project_release, None, next_page),
        "GitHubGetLatestRelease" => shape_object(body, project_release),
        "GitHubGetFileContents" => project_file_contents(body, args["maxChars"].as_u64().unwrap_or(40_000) as usize),
        "GitHubGetRepoTree" => project_tree_listing(body),
        _ => None,
    }
}

fn shape_object(body: &str, project: fn(&Map<String, Value>) -> Projection) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    Some(serialize(&project(object)))
}

fn shape_array(
    body: &str,
    project: fn(&Map<String, Value>) -> Projection,
    total: Option<Value>,
    next_page: bool,
) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let items = value.as_array()?;
    Some(project_items(items, project, total, next_page))
}

fn shape_search(
    body: &str,
    project: fn(&Map<String, Value>) -> Projection,
    next_page: bool,
) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    let items = object.get("items")?.as_array()?;
    let total = object.get("total_count").filter(|value| value.is_number()).cloned();
    Some(project_items(items, project, total, next_page))
}

fn shape_wrapped_list(
    body: &str,
    key: &str,
    project: fn(&Map<String, Value>) -> Projection,
    next_page: bool,
) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    let items = object.get(key)?.as_array()?;
    let total = object.get("total_count").filter(|value| value.is_number()).cloned();
    Some(project_items(items, project, total, next_page))
}

fn project_items(
    items: &[Value],
    project: fn(&Map<String, Value>) -> Projection,
    total: Option<Value>,
    next_page: bool,
) -> String {
    let mut header = Projection::default();
    header.insert("items".into(), Value::from(items.len()));
    if let Some(total) = total {
        header.insert("total".into(), Value::from(total));
    }
    if next_page {
        header.insert("has_more".into(), Value::Bool(true));
    }
    let mut lines = vec![serialize(&header)];
    for item in items {
        if let Some(object) = item.as_object() {
            lines.push(serialize(&project(object)));
        }
    }
    lines.join("\n")
}

fn project_issue(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_num(&mut out, "number", item.get("number"));
    put_text(&mut out, "title", item.get("title"), Some(200));
    put_text(&mut out, "state", item.get("state"), None);
    put_text(&mut out, "user", user_login(item), None);
    put_labels(&mut out, item.get("labels"));
    put_num(&mut out, "comments", item.get("comments"));
    put_text(&mut out, "updated_at", item.get("updated_at"), None);
    if item.contains_key("pull_request") {
        out.insert("is_pr".into(), Value::Bool(true));
    }
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_issue_detail(item: &Map<String, Value>) -> Projection {
    let mut out = project_issue(item);
    put_text(&mut out, "body", item.get("body"), Some(4000));
    out
}

fn project_issue_search(item: &Map<String, Value>) -> Projection {
    let mut out = project_issue(item);
    if let Some(repo) = item.get("repository_url").and_then(Value::as_str).map(js_trim).filter(|repo| !repo.is_empty()) {
        out.insert("repo".into(), Value::String(repo.replacen("https://api.github.com/repos/", "", 1)));
    }
    out
}

fn project_pull(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_num(&mut out, "number", item.get("number"));
    put_text(&mut out, "title", item.get("title"), Some(200));
    put_text(&mut out, "state", item.get("state"), None);
    if item.get("draft").and_then(Value::as_bool) == Some(true) {
        out.insert("draft".into(), Value::Bool(true));
    }
    put_text(&mut out, "user", user_login(item), None);
    put_text(&mut out, "head", item.get("head").and_then(|value| value.get("ref")), None);
    put_text(&mut out, "base", item.get("base").and_then(|value| value.get("ref")), None);
    put_num(&mut out, "comments", item.get("comments"));
    put_text(&mut out, "updated_at", item.get("updated_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_pull_detail(item: &Map<String, Value>) -> Projection {
    let mut out = project_pull(item);
    put_text(&mut out, "body", item.get("body"), Some(4000));
    if item.get("merged").and_then(Value::as_bool) == Some(true) {
        out.insert("merged".into(), Value::Bool(true));
    }
    put_num(&mut out, "additions", item.get("additions"));
    put_num(&mut out, "deletions", item.get("deletions"));
    put_num(&mut out, "changed_files", item.get("changed_files"));
    put_num(&mut out, "commits", item.get("commits"));
    out
}

fn project_branch(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "name", item.get("name"), None);
    put_text(&mut out, "sha", item.get("commit").and_then(|value| value.get("sha")), None);
    if item.get("protected").and_then(Value::as_bool) == Some(true) {
        out.insert("protected".into(), Value::Bool(true));
    }
    out
}

fn project_commit(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    let commit = item.get("commit");
    let author = commit.and_then(|value| value.get("author"));
    put_text(&mut out, "sha", item.get("sha"), None);
    put_text(&mut out, "author", author.and_then(|value| value.get("name")), None);
    put_text(&mut out, "date", author.and_then(|value| value.get("date")), None);
    if let Some(message) = commit.and_then(|value| value.get("message")).and_then(Value::as_str).map(js_trim).filter(|message| !message.is_empty()) {
        out.insert("message".into(), Value::String(message.split('\n').next().unwrap_or_default().to_string()));
    }
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_commit_detail(item: &Map<String, Value>) -> String {
    let mut out = Projection::default();
    let commit = item.get("commit");
    put_text(&mut out, "sha", item.get("sha"), None);
    put_text(&mut out, "message", commit.and_then(|value| value.get("message")), Some(2000));
    put_text(&mut out, "author", commit.and_then(|value| value.get("author")).and_then(|value| value.get("name")), None);
    let mut fields = out.0.iter().map(|(key, value)| format!("{}:{}", serde_json::to_string(key).unwrap(), value)).collect::<Vec<_>>();
    if let Some(stats) = item.get("stats").and_then(Value::as_object) {
        let mut projected = Projection::default();
        put_num(&mut projected, "additions", stats.get("additions"));
        put_num(&mut projected, "deletions", stats.get("deletions"));
        put_num(&mut projected, "total", stats.get("total"));
        fields.push(format!("\"stats\":{}", serialize(&projected)));
    }
    let files = item.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let empty = Map::new();
    let changed = files.iter().take(100).map(|value| serialize(&project_pull_file(value.as_object().unwrap_or(&empty)))).collect::<Vec<_>>();
    fields.push(format!("\"files\":{}", changed.len()));
    fields.push(format!("\"changed\":[{}]", changed.join(",")));
    if files.len() > 100 {
        fields.push(format!("\"more_files\":{}", files.len() - 100));
    }
    format!("{{{}}}", fields.join(","))
}

fn project_comment(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_num(&mut out, "id", item.get("id"));
    put_text(&mut out, "user", user_login(item), None);
    put_text(&mut out, "created_at", item.get("created_at"), None);
    put_text(&mut out, "path", item.get("path"), None);
    put_text(&mut out, "body", item.get("body"), Some(300));
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_pull_file(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "filename", item.get("filename"), None);
    put_text(&mut out, "status", item.get("status"), None);
    put_num(&mut out, "additions", item.get("additions"));
    put_num(&mut out, "deletions", item.get("deletions"));
    put_num(&mut out, "changes", item.get("changes"));
    out
}

fn project_release(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "tag", item.get("tag_name"), None);
    put_text(&mut out, "name", item.get("name"), Some(120));
    if item.get("draft").and_then(Value::as_bool) == Some(true) {
        out.insert("draft".into(), Value::Bool(true));
    }
    if item.get("prerelease").and_then(Value::as_bool) == Some(true) {
        out.insert("prerelease".into(), Value::Bool(true));
    }
    put_text(&mut out, "published_at", item.get("published_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_workflow_run(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_num(&mut out, "id", item.get("id"));
    put_text(&mut out, "name", item.get("name"), Some(120));
    put_text(&mut out, "status", item.get("status"), None);
    put_text(&mut out, "conclusion", item.get("conclusion"), None);
    put_text(&mut out, "branch", item.get("head_branch"), None);
    put_num(&mut out, "run_number", item.get("run_number"));
    put_text(&mut out, "created_at", item.get("created_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_workflow_job(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_num(&mut out, "id", item.get("id"));
    put_text(&mut out, "name", item.get("name"), Some(120));
    put_text(&mut out, "status", item.get("status"), None);
    put_text(&mut out, "conclusion", item.get("conclusion"), None);
    put_text(&mut out, "started_at", item.get("started_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_check_run(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "name", item.get("name"), Some(120));
    put_text(&mut out, "status", item.get("status"), None);
    put_text(&mut out, "conclusion", item.get("conclusion"), None);
    put_text(&mut out, "started_at", item.get("started_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_code_search(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "path", item.get("path"), None);
    put_text(&mut out, "repo", item.get("repository").and_then(|value| value.get("full_name")), None);
    put_text(&mut out, "fragment", item.get("text_matches").and_then(Value::as_array).and_then(|items| items.first()).and_then(|value| value.get("fragment")), Some(200));
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_repo_search(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "full_name", item.get("full_name"), None);
    put_text(&mut out, "description", item.get("description"), Some(160));
    put_num(&mut out, "stars", item.get("stargazers_count"));
    put_text(&mut out, "language", item.get("language"), None);
    put_text(&mut out, "updated_at", item.get("updated_at"), None);
    put_text(&mut out, "url", item.get("html_url"), None);
    out
}

fn project_file_contents(body: &str, max_chars: usize) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    if let Some(items) = value.as_array() {
        return Some(project_items(items, project_file_entry, None, false));
    }
    let item = value.as_object()?;
    let mut header = Projection::default();
    put_text(&mut header, "path", item.get("path"), None);
    if !header.contains_key("path") {
        header.insert("path".into(), Value::String(String::new()));
    }
    put_text(&mut header, "sha", item.get("sha"), None);
    put_num(&mut header, "size", item.get("size"));
    put_text(&mut header, "type", item.get("type"), None);
    put_text(&mut header, "download_url", item.get("download_url"), None);
    if item.get("encoding").and_then(Value::as_str) != Some("base64")
        || item.get("content").and_then(Value::as_str).is_none()
    {
        return Some(serialize(&header));
    }
    let content = item.get("content").and_then(Value::as_str).unwrap();
    let mut compact = content.chars().take_while(|ch| *ch != '=').filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '-' | '_')).collect::<String>().replace('-', "+").replace('_', "/");
    if compact.len() % 4 == 1 {
        compact.pop();
    }
    let decoder = base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let decoded = match decoder.decode(compact) {
        Ok(bytes) => bytes,
        Err(_) => {
            header.insert("encoding".into(), Value::String("base64".into()));
            header.insert("decoded".into(), Value::Bool(false));
            return Some(serialize(&header));
        }
    };
    if decoded.iter().take(8192).any(|byte| *byte == 0) {
        header.insert("binary".into(), Value::Bool(true));
        header.insert("bytes".into(), Value::from(decoded.len()));
        header.insert("note".into(), Value::String("binary file, contents not shown".into()));
        return Some(serialize(&header));
    }
    let decoded = String::from_utf8_lossy(decoded.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&decoded));
    let cap = max_chars.clamp(1, 40_000);
    let units = decoded.encode_utf16().collect::<Vec<_>>();
    let content = if units.len() > cap {
        format!("{}\n[...truncated {} characters]", String::from_utf16_lossy(&units[..cap]), units.len() - cap)
    } else {
        decoded.into_owned()
    };
    Some(format!("{}\n---\n{content}", serialize(&header)))
}

fn project_tree_listing(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    let tree = object.get("tree")?.as_array()?;
    let shown = tree.len().min(3000);
    let mut header = Projection::default();
    header.insert("entries".into(), Value::from(tree.len()));
    header.insert("shown".into(), Value::from(shown));
    if object.get("truncated").and_then(Value::as_bool) == Some(true) || tree.len() > shown {
        header.insert("truncated".into(), Value::Bool(true));
    }
    put_text(&mut header, "sha", object.get("sha"), None);
    let mut lines = vec![serialize(&header)];
    for item in tree.iter().take(shown).filter_map(Value::as_object) {
        lines.push(serialize(&project_tree_entry(item)));
    }
    Some(lines.join("\n"))
}

fn project_file_entry(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "type", item.get("type"), None);
    put_text(&mut out, "name", item.get("name"), None);
    put_num(&mut out, "size", item.get("size"));
    put_text(&mut out, "sha", item.get("sha"), None);
    out
}

fn project_tree_entry(item: &Map<String, Value>) -> Projection {
    let mut out = Projection::default();
    put_text(&mut out, "path", item.get("path"), None);
    put_text(&mut out, "type", item.get("type"), None);
    if item.get("type").and_then(Value::as_str).map(js_trim) == Some("blob") {
        put_num(&mut out, "size", item.get("size"));
    }
    out
}

fn put_labels(out: &mut Projection, value: Option<&Value>) {
    let Some(items) = value.and_then(Value::as_array) else { return; };
    let labels = items.iter().filter_map(|item| {
        if let Some(label) = item.as_str() {
            return Some(Value::String(label.to_string()));
        }
        let label = js_trim(item.get("name").and_then(Value::as_str)?);
        (!label.is_empty()).then(|| Value::String(label.to_string()))
    }).take(12).collect::<Vec<_>>();
    if !labels.is_empty() { out.insert("labels".into(), Value::Array(labels)); }
}

fn user_login(item: &Map<String, Value>) -> Option<&Value> {
    item.get("user").and_then(|user| user.get("login"))
        .filter(|value| value.as_str().is_some_and(|text| !js_trim(text).is_empty()))
        .or_else(|| item.get("login"))
}

fn put_num(out: &mut Projection, key: &str, value: Option<&Value>) {
    if let Some(Value::Number(number)) = value { out.insert(key.into(), Value::Number(number.clone())); }
}

fn put_text(out: &mut Projection, key: &str, value: Option<&Value>, limit: Option<usize>) {
    let Some(value) = value.and_then(Value::as_str) else { return; };
    let value = js_trim(value);
    if value.is_empty() { return; }
    let units = value.encode_utf16().collect::<Vec<_>>();
    let json = if let Some(limit) = limit.filter(|limit| units.len() > *limit) {
        let mut clipped = units[..limit].to_vec();
        clipped.push('…' as u16);
        json_utf16(&clipped)
    } else {
        serde_json::to_string(value).unwrap()
    };
    out.0.insert(key.into(), json);
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch| matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

fn json_utf16(units: &[u16]) -> String {
    let mut json = String::from("\"");
    for ch in char::decode_utf16(units.iter().copied()) {
        match ch {
            Ok(ch) => {
                let encoded = serde_json::to_string(&ch.to_string()).unwrap();
                json.push_str(&encoded[1..encoded.len() - 1]);
            }
            Err(error) => json.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    json.push('"');
    json
}

fn serialize(value: &Projection) -> String {
    let fields = value.0.iter().map(|(key, value)| format!("{}:{value}", serde_json::to_string(key).unwrap())).collect::<Vec<_>>();
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projections_match_v2_generated_cases() {
        let fixture: Value = serde_json::from_str(include_str!("../../../../tests/fixtures/github-shape.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let actual = shape_response(case["name"].as_str().unwrap(), case["body"].as_str().unwrap(), case["next"].as_bool().unwrap(), &case["args"]);
            assert_eq!(actual.as_deref(), case["expected"].as_str(), "{}", case["name"]);
        }
    }

    #[test]
    fn invalid_single_object_shape_falls_back() {
        assert_eq!(shape_response("GitHubGetIssue", "null", false, &json!({})), None);
    }
}
