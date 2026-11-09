mod execute;
mod http;
mod schema;
mod specs;

use serde_json::Value;

use execute::{
    arg_string, build_body, build_query, build_url, resolve_base_url, resolve_token, validate_args,
};
use http::{format_result, github_request};
use schema::schema_for;
use specs::{GitHubToolSpec, SPECS};

use super::err_result;
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

#[cfg(test)]
use execute::{base64_encode, encode_path_component, encode_uri_component};
#[cfg(test)]
use serde_json::json;

#[cfg(test)]
pub(crate) static GITHUB_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub const GITHUB_NO_TOKEN_ERROR: &str = "No GitHub token configured. Set token in the [github] section of config.toml, or set the GITHUB_TOKEN (or GH_TOKEN) environment variable.";

const GITHUB_DEFAULT_BASE_URL: &str = "https://api.github.com";

pub fn github_tool_defs() -> Vec<ToolInfo> {
    SPECS
        .iter()
        .map(|spec| ToolInfo {
            name: spec.name.into(),
            description: spec.description.into(),
            input_schema: schema_for(spec),
        })
        .collect()
}

pub fn is_github_tool(tool_name: &str) -> bool {
    SPECS
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case(tool_name))
}

static MUTATING_TOOLS: &[&str] = &[
    "GitHubCreateOrUpdateFile",
    "GitHubCreateTree",
    "GitHubCreateCommit",
    "GitHubUpdateRef",
    "GitHubCreateBranch",
    "GitHubCreateIssue",
    "GitHubUpdateIssue",
    "GitHubAddIssueComment",
    "GitHubCreatePR",
    "GitHubUpdatePR",
    "GitHubMergePR",
    "GitHubCreatePRReview",
];

pub fn is_mutating_tool(tool_name: &str) -> bool {
    MUTATING_TOOLS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(tool_name))
}

pub fn is_readonly_tool(tool_name: &str) -> bool {
    is_github_tool(tool_name) && !is_mutating_tool(tool_name)
}

pub fn rule_subject(tool_name: &str, args: &Value) -> Option<String> {
    let spec = SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?;
    if spec.name.eq_ignore_ascii_case("GitHubGetMe") {
        return Some("me".to_string());
    }
    if spec.fields.iter().any(|field| field.name == "q") {
        return args.get("q").and_then(Value::as_str).map(str::to_string);
    }
    match (arg_string(args, "owner"), arg_string(args, "repo")) {
        (Some(owner), Some(repo)) => Some(format!("{owner}/{repo}")),
        _ => None,
    }
}

pub fn has_token(credentials: Option<&GitHubCredentials>) -> bool {
    resolve_token(credentials).is_some()
}

pub async fn execute_github_tool(
    tool_name: &str,
    args: &Value,
    credentials: Option<&GitHubCredentials>,
) -> Option<ExecutableToolResult> {
    let spec = SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?;
    Some(execute_spec(spec, args, credentials).await)
}

async fn execute_spec(
    spec: &GitHubToolSpec,
    args: &Value,
    credentials: Option<&GitHubCredentials>,
) -> ExecutableToolResult {
    let token = resolve_token(credentials);
    let base_url = resolve_base_url(credentials);
    execute_with_token(spec, args, token.as_deref(), Some(base_url.as_str())).await
}

async fn execute_with_token(
    spec: &GitHubToolSpec,
    args: &Value,
    token: Option<&str>,
    base_url: Option<&str>,
) -> ExecutableToolResult {
    if let Some(issues) = validate_args(spec, args) {
        return err_result(format!("Invalid arguments for {}: {issues}", spec.name));
    }
    let Some(token) = token else {
        return err_result(GITHUB_NO_TOKEN_ERROR.to_string());
    };
    let url = build_url(spec, args, base_url.unwrap_or(GITHUB_DEFAULT_BASE_URL));
    let query = build_query(spec, args);
    let body = build_body(spec, args);
    let response = github_request(spec, url, query, body, token).await;
    format_result(response)
}

#[derive(Clone, Debug, Default)]
pub struct GitHubCredentials {
    pub token: Option<String>,
    pub base_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_NAMES: [&str; 34] = [
        "GitHubGetRepo",
        "GitHubListBranches",
        "GitHubListCommits",
        "GitHubGetCommit",
        "GitHubGetFileContents",
        "GitHubCreateOrUpdateFile",
        "GitHubGetRef",
        "GitHubCreateTree",
        "GitHubCreateCommit",
        "GitHubUpdateRef",
        "GitHubCreateBranch",
        "GitHubListIssues",
        "GitHubGetIssue",
        "GitHubCreateIssue",
        "GitHubUpdateIssue",
        "GitHubAddIssueComment",
        "GitHubListIssueComments",
        "GitHubListPRs",
        "GitHubGetPR",
        "GitHubGetPRDiff",
        "GitHubGetPRFiles",
        "GitHubCreatePR",
        "GitHubUpdatePR",
        "GitHubMergePR",
        "GitHubCreatePRReview",
        "GitHubListPRReviewComments",
        "GitHubSearchCode",
        "GitHubSearchRepos",
        "GitHubSearchIssues",
        "GitHubListWorkflowRuns",
        "GitHubGetWorkflowRun",
        "GitHubListReleases",
        "GitHubGetLatestRelease",
        "GitHubGetMe",
    ];

    fn spec_by_name(name: &str) -> &'static GitHubToolSpec {
        SPECS.iter().find(|spec| spec.name == name).unwrap()
    }

    #[test]
    fn test_all_34_defs_present_with_correct_names() {
        let defs = github_tool_defs();
        assert_eq!(defs.len(), 34);
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, EXPECTED_NAMES);
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object");
            assert_eq!(def.input_schema["additionalProperties"], false);
        }
    }

    #[test]
    fn test_descriptions_match_v2_verbatim() {
        let defs = github_tool_defs();
        let by_name = |name: &str| defs.iter().find(|def| def.name == name).unwrap();
        assert_eq!(
            by_name("GitHubGetRepo").description,
            "Get metadata for a repository (description, default branch, stars, visibility)."
        );
        assert_eq!(
            by_name("GitHubGetFileContents").description,
            "Get a file or directory listing. File content is returned base64-encoded in the `content` field."
        );
        assert_eq!(
            by_name("GitHubCreateOrUpdateFile").description,
            "Create or update a file. Provide plain-text `content` (encoded to base64 automatically). Pass `sha` when updating an existing file."
        );
        assert_eq!(
            by_name("GitHubGetRef").description,
            "Get a Git reference (branch or tag head). Returns the current SHA for ref names like `heads/main`."
        );
        assert_eq!(
            by_name("GitHubGetMe").description,
            "Get the authenticated user (verifies the configured token)."
        );
    }

    #[test]
    fn test_schema_field_alignment() {
        let defs = github_tool_defs();
        let repo = defs.iter().find(|def| def.name == "GitHubGetRepo").unwrap();
        let props = repo.input_schema["properties"].as_object().unwrap();
        assert_eq!(props["owner"]["type"], "string");
        assert_eq!(props["owner"]["minLength"], 1);
        assert_eq!(
            props["owner"]["description"],
            "Repository owner (user or organization login)."
        );
        assert_eq!(repo.input_schema["required"], json!(["owner", "repo"]));

        let branches = defs
            .iter()
            .find(|def| def.name == "GitHubListBranches")
            .unwrap();
        assert_eq!(branches.input_schema["required"], json!(["owner", "repo"]));
        let props = branches.input_schema["properties"].as_object().unwrap();
        assert_eq!(props["perPage"]["type"], "integer");
        assert_eq!(props["perPage"]["minimum"], 1);
        assert_eq!(props["perPage"]["maximum"], 100);
        assert_eq!(props["page"]["type"], "integer");
    }

    #[tokio::test]
    async fn test_no_token_returns_v2_error() {
        let result = execute_with_token(
            spec_by_name("GitHubGetRepo"),
            &json!({ "owner": "octocat", "repo": "hello-world" }),
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, GITHUB_NO_TOKEN_ERROR);
    }

    #[tokio::test]
    async fn test_missing_required_arg_returns_v2_style_error() {
        let result = execute_with_token(
            spec_by_name("GitHubGetRepo"),
            &json!({ "owner": "octocat" }),
            Some("dummy"),
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .starts_with("Invalid arguments for GitHubGetRepo: ")
        );
        assert!(result.content.contains("repo"));

        let result = execute_with_token(
            spec_by_name("GitHubGetRepo"),
            &json!({}),
            Some("dummy"),
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner"));
        assert!(result.content.contains("repo"));
    }

    #[tokio::test]
    async fn test_wrong_arg_type_returns_v2_style_error() {
        let result = execute_with_token(
            spec_by_name("GitHubGetRepo"),
            &json!({ "owner": "octocat", "repo": 42 }),
            Some("dummy"),
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .starts_with("Invalid arguments for GitHubGetRepo: ")
        );
        assert!(result.content.contains("expected string"));
    }

    #[test]
    fn test_unknown_tool_returns_none_and_case_insensitive_dispatch() {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _env_guard = GITHUB_ENV_TEST_LOCK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime")
                    .block_on(async {
                        assert!(
                            execute_github_tool("GitHubNope", &json!({}), None)
                                .await
                                .is_none()
                        );

                        let result = execute_github_tool("githubgetrepo", &json!({}), None)
                            .await
                            .unwrap();
                        assert!(result.is_error);
                        assert!(
                            result
                                .content
                                .starts_with("Invalid arguments for GitHubGetRepo: ")
                        );
                    });
            });
        });
    }

    #[test]
    fn test_path_templates() {
        assert_eq!(spec_by_name("GitHubGetRepo").path, "/repos/{owner}/{repo}");
        assert_eq!(
            spec_by_name("GitHubGetCommit").path,
            "/repos/{owner}/{repo}/commits/{ref}"
        );
        assert_eq!(
            spec_by_name("GitHubGetPRDiff").path,
            "/repos/{owner}/{repo}/pulls/{pullNumber}"
        );
        assert_eq!(spec_by_name("GitHubSearchCode").path, "/search/code");
        assert_eq!(spec_by_name("GitHubGetMe").path, "/user");
        assert_eq!(
            spec_by_name("GitHubGetPRDiff").accept,
            Some("application/vnd.github.diff")
        );
    }

    #[test]
    fn test_query_templates() {
        let commits = spec_by_name("GitHubListCommits");
        let arg_keys: Vec<&str> = commits.query.iter().map(|(_, arg)| *arg).collect();
        assert_eq!(arg_keys, vec!["sha", "path", "perPage", "page"]);
        let query_keys: Vec<&str> = commits.query.iter().map(|(key, _)| *key).collect();
        assert_eq!(query_keys, vec!["sha", "path", "per_page", "page"]);
        let prs = spec_by_name("GitHubListPRs");
        let arg_keys: Vec<&str> = prs.query.iter().map(|(_, arg)| *arg).collect();
        assert_eq!(arg_keys, vec!["state", "head", "base", "perPage", "page"]);
    }

    #[test]
    fn test_body_templates() {
        let create_file = spec_by_name("GitHubCreateOrUpdateFile");
        assert_eq!(create_file.body_base64, &[("content", "content")][..]);
        let create_branch = spec_by_name("GitHubCreateBranch");
        assert_eq!(
            create_branch.body_prefix,
            &[("ref", "branch", "refs/heads/")][..]
        );
        let merge = spec_by_name("GitHubMergePR");
        let body_keys: Vec<&str> = merge.body.iter().map(|(key, _)| *key).collect();
        assert_eq!(body_keys, vec!["commit_title", "merge_method"]);
    }

    #[test]
    fn test_build_url_and_query() {
        let args = json!({ "owner": "octocat", "repo": "hello-world" });
        assert_eq!(
            build_url(
                spec_by_name("GitHubGetRepo"),
                &args,
                GITHUB_DEFAULT_BASE_URL
            ),
            "https://api.github.com/repos/octocat/hello-world"
        );
        let commits = spec_by_name("GitHubListCommits");
        let args = json!({ "owner": "o", "repo": "r", "sha": "main", "perPage": 10 });
        assert_eq!(
            build_query(commits, &args).as_deref(),
            Some("?sha=main&per_page=10")
        );

        let args = json!({ "owner": "o", "repo": "r" });
        assert_eq!(build_query(commits, &args), None);
    }

    #[test]
    fn test_build_body() {
        let create_file = spec_by_name("GitHubCreateOrUpdateFile");
        let args = json!({
            "message": "update",
            "content": "hello",
            "branch": "main",
        });
        let body = build_body(create_file, &args).unwrap();
        assert_eq!(body["message"], "update");
        assert_eq!(body["content"], base64_encode(b"hello"));
        assert_eq!(body["branch"], "main");
        assert!(body.get("sha").is_none());
        let create_branch = spec_by_name("GitHubCreateBranch");
        let args = json!({ "branch": "feature", "sha": "abc123" });
        let body = build_body(create_branch, &args).unwrap();
        assert_eq!(body["ref"], "refs/heads/feature");
        assert_eq!(body["sha"], "abc123");
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn test_encoding_helpers() {
        assert_eq!(encode_uri_component("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(encode_uri_component("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_path_component("a b"), "a%20b");
        assert_eq!(encode_path_component("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_github_token_env() {
        let _env_guard = GITHUB_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved_token = std::env::var("GITHUB_TOKEN").ok();
        let saved_gh = std::env::var("GH_TOKEN").ok();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        assert_eq!(resolve_token(None), None);
        unsafe {
            std::env::set_var("GH_TOKEN", "gh-token");
        }
        assert_eq!(resolve_token(None).as_deref(), Some("gh-token"));
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "github-token");
        }

        assert_eq!(resolve_token(None).as_deref(), Some("github-token"));
        match saved_token {
            Some(value) => unsafe { std::env::set_var("GITHUB_TOKEN", value) },
            None => unsafe { std::env::remove_var("GITHUB_TOKEN") },
        }
        match saved_gh {
            Some(value) => unsafe { std::env::set_var("GH_TOKEN", value) },
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }
    }

    #[test]
    fn test_credentials_config_wins_over_env() {
        let _env_guard = GITHUB_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved_token = std::env::var("GITHUB_TOKEN").ok();
        let saved_gh = std::env::var("GH_TOKEN").ok();
        let saved_url = std::env::var("GITHUB_API_URL").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "env-token");
            std::env::remove_var("GH_TOKEN");
            std::env::set_var("GITHUB_API_URL", "https://env.example.com/");
        }

        let credentials = GitHubCredentials {
            token: Some("config-token".into()),
            base_url: Some("https://config.example.com/".into()),
        };
        assert_eq!(
            resolve_token(Some(&credentials)).as_deref(),
            Some("config-token")
        );
        assert_eq!(
            resolve_base_url(Some(&credentials)),
            "https://config.example.com"
        );

        let blank = GitHubCredentials {
            token: Some("   ".into()),
            base_url: Some("".into()),
        };
        assert_eq!(resolve_token(Some(&blank)).as_deref(), Some("env-token"));
        assert_eq!(resolve_base_url(Some(&blank)), "https://env.example.com");

        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
            std::env::remove_var("GITHUB_API_URL");
        }
        assert_eq!(resolve_token(Some(&blank)), None);
        assert_eq!(resolve_base_url(Some(&blank)), GITHUB_DEFAULT_BASE_URL);
        match saved_token {
            Some(value) => unsafe { std::env::set_var("GITHUB_TOKEN", value) },
            None => unsafe { std::env::remove_var("GITHUB_TOKEN") },
        }
        match saved_gh {
            Some(value) => unsafe { std::env::set_var("GH_TOKEN", value) },
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }
        match saved_url {
            Some(value) => unsafe { std::env::set_var("GITHUB_API_URL", value) },
            None => unsafe { std::env::remove_var("GITHUB_API_URL") },
        }
    }

    #[test]
    fn test_mutating_and_readonly_partition() {
        let readonly: Vec<&str> = SPECS
            .iter()
            .map(|spec| spec.name)
            .filter(|name| is_readonly_tool(name))
            .collect();
        let mutating: Vec<&str> = SPECS
            .iter()
            .map(|spec| spec.name)
            .filter(|name| is_mutating_tool(name))
            .collect();
        assert_eq!(readonly.len() + mutating.len(), 34);
        assert_eq!(mutating.len(), 12);
        for name in [
            "GitHubCreateIssue",
            "GitHubMergePR",
            "GitHubCreatePRReview",
            "GitHubUpdateRef",
        ] {
            assert!(is_mutating_tool(name), "{name} should be mutating");
            assert!(!is_readonly_tool(name));
        }
        for name in [
            "GitHubGetRepo",
            "GitHubGetPRDiff",
            "GitHubSearchCode",
            "GitHubGetMe",
        ] {
            assert!(is_readonly_tool(name), "{name} should be read-only");
        }
        assert!(!is_mutating_tool("GitHubNope"));
    }

    #[test]
    fn test_rule_subject_shapes() {
        let repo_args = json!({ "owner": "octocat", "repo": "hello-world" });
        assert_eq!(
            rule_subject("GitHubGetRepo", &repo_args).as_deref(),
            Some("octocat/hello-world")
        );
        assert_eq!(
            rule_subject("GitHubCreateIssue", &repo_args).as_deref(),
            Some("octocat/hello-world")
        );

        assert_eq!(
            rule_subject("GitHubSearchCode", &json!({ "q": "fmt rust" })).as_deref(),
            Some("fmt rust")
        );

        assert_eq!(
            rule_subject("GitHubGetMe", &json!({})).as_deref(),
            Some("me")
        );

        assert_eq!(rule_subject("GitHubGetRepo", &json!({})), None);
        assert_eq!(rule_subject("GitHubNope", &repo_args), None);
    }

    #[test]
    fn test_has_token_gating() {
        let _env_guard = GITHUB_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved_token = std::env::var("GITHUB_TOKEN").ok();
        let saved_gh = std::env::var("GH_TOKEN").ok();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        assert!(!has_token(None));
        assert!(!has_token(Some(&GitHubCredentials::default())));
        assert!(has_token(Some(&GitHubCredentials {
            token: Some("t".into()),
            base_url: None,
        })));
        unsafe { std::env::set_var("GITHUB_TOKEN", "env-token") };
        assert!(has_token(None));
        match saved_token {
            Some(value) => unsafe { std::env::set_var("GITHUB_TOKEN", value) },
            None => unsafe { std::env::remove_var("GITHUB_TOKEN") },
        }
        match saved_gh {
            Some(value) => unsafe { std::env::set_var("GH_TOKEN", value) },
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }
    }
}
