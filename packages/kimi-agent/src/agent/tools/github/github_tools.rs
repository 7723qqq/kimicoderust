use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::config_section::{resolve_github_base_url, resolve_github_token};
use super::github::{GitHubToolSpec, IGitHubTool};
use super::github_request::{GitHubRequestDeps, GitHubRequestOptions, github_request};
use crate::_base::utils::abort::{abortable, abort_error, is_user_cancellation, user_cancellation_reason};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::config::KimiConfig;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::ToolOutputAccumulator;
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::ToolAccesses;
use crate::wire::record::WireAsyncError;

type PathResolver = Arc<dyn Fn(&Value) -> String + Send + Sync>;
type QueryResolver = Arc<dyn Fn(&Value) -> Map<String, Value> + Send + Sync>;
type BodyResolver = Arc<dyn Fn(&Value) -> Value + Send + Sync>;
type SubjectResolver = Arc<dyn Fn(&Value) -> String + Send + Sync>;

#[derive(Clone)]
pub struct GitHubToolDeps {
    pub config: Arc<KimiConfig>,
    pub client: Option<reqwest::Client>,
}

#[derive(Clone)]
pub struct GitHubToolBase {
    spec: GitHubToolSpec,
    deps: GitHubToolDeps,
}

impl GitHubToolBase {
    pub fn new(spec: GitHubToolSpec, deps: GitHubToolDeps) -> Self {
        Self { spec, deps }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &str {
        &self.spec.description
    }

    pub fn parameters(&self) -> Map<String, Value> {
        self.spec
            .schema
            .as_object()
            .cloned()
            .expect("GitHub input schema is an object")
    }

    pub fn resolve_execution(&self, raw_args: Value) -> ToolExecution {
        let args = match parse_args(&self.spec.schema, raw_args) {
            Ok(args) => args,
            Err(error) => {
                return ToolExecution::Error(ExecutableToolResult::error(format!(
                    "Invalid arguments for {}: {error}",
                    self.spec.name
                )));
            }
        };
        let subject = (self.spec.subject)(&args);
        let rule_subject = subject.clone();
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::none()),
            display: None,
            description: Some(self.spec.name.clone()),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(&self.spec.name, &subject),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_subject)
            })),
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { this.execute(this.spec.clone(), args, context).await })
            }),
        })
    }

    async fn execute(
        &self,
        spec: GitHubToolSpec,
        args: Value,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let query = spec.query.as_ref().map(|query| query(&args));
        let body = spec.body.as_ref().map(|body| body(&args));
        let options = GitHubRequestOptions {
            query: query.map(|query| query.into_iter().collect()),
            body,
            accept: spec.accept.clone(),
            token: resolve_github_token(&self.deps.config).map(str::to_string),
            base_url: resolve_github_base_url(&self.deps.config).map(str::to_string),
        };
        let request = github_request(
            &spec.method,
            &(spec.path)(&args),
            options,
            GitHubRequestDeps {
                client: self.deps.client.clone(),
                signal: None,
            },
        );
        let response = match abortable(request, context.signal).await {
            Ok(result) => result.map_err(|error| Box::new(error) as WireAsyncError)?,
            Err(error) if is_user_cancellation(error.as_ref()) => {
                return Err(Box::new(user_cancellation_reason()) as WireAsyncError);
            }
            Err(_) => return Err(Box::new(abort_error()) as WireAsyncError),
        };
        if !response.ok {
            let detail = if response.body.is_empty() {
                String::new()
            } else {
                format!("\n{}", utf16_prefix(&response.body, 4000))
            };
            let status = if response.status > 0 {
                format!(" (status {})", response.status)
            } else {
                String::new()
            };
            return Ok(ExecutableToolResult::error(format!(
                "{}{status}{detail}",
                response
                    .error
                    .unwrap_or_else(|| "GitHub request failed".to_string())
            )));
        }
        let rate = response
            .rate_remaining
            .map(|remaining| format!("\n\n(GitHub rate limit remaining: {remaining})"))
            .unwrap_or_default();
        let body = if response.body.is_empty() {
            "(empty response)".to_string()
        } else {
            response.body
        };
        let mut builder = ToolOutputAccumulator::new();
        builder.write(&format!("{body}{rate}"));
        let result = builder.ok("", None);
        Ok(ExecutableToolResult {
            output: result.content.into(),
            is_error: Some(result.is_error),
            note: result.brief,
            spill: result.spill,
            ..Default::default()
        })
    }
}

impl IGitHubTool for GitHubToolBase {}

impl ExecutableTool for GitHubToolBase {
    fn name(&self) -> &str {
        GitHubToolBase::name(self)
    }

    fn description(&self) -> &str {
        GitHubToolBase::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        GitHubToolBase::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = GitHubToolBase::resolve_execution(self, input);
        Box::pin(async move { Ok(execution) })
    }
}

pub fn make_github_tool_ctor(
    spec: GitHubToolSpec,
) -> impl Fn(Arc<KimiConfig>) -> GitHubToolBase + Send + Sync {
    move |config| GitHubToolBase::new(spec.clone(), GitHubToolDeps { config, client: None })
}

pub static GITHUB_SPECS: LazyLock<Vec<GitHubToolSpec>> = LazyLock::new(build_github_specs);
pub static GITHUB_READONLY_TOOL_NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    GITHUB_SPECS
        .iter()
        .filter(|spec| spec.mutating != Some(true))
        .map(|spec| spec.name.clone())
        .collect()
});
pub static GITHUB_MUTATING_TOOL_NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    GITHUB_SPECS
        .iter()
        .filter(|spec| spec.mutating == Some(true))
        .map(|spec| spec.name.clone())
        .collect()
});

fn build_github_specs() -> Vec<GitHubToolSpec> {
    vec![
        repo_spec(
            "GitHubGetRepo",
            "Get metadata for a repository (description, default branch, stars, visibility).",
            "GET",
            "/repos/{owner}/{repo}",
            vec![],
        ),
        with_query(
            repo_spec(
                "GitHubListBranches",
                "List branches in a repository.",
                "GET",
                "/repos/{owner}/{repo}/branches",
                vec![per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            repo_spec(
                "GitHubListCommits",
                "List commits on a repository, optionally filtered by branch/sha or path.",
                "GET",
                "/repos/{owner}/{repo}/commits",
                vec![
                    ("sha", string("Branch name or commit SHA to start from."), false),
                    ("path", string("Only commits touching this file path."), false),
                    per_page(),
                    page(),
                ],
            ),
            query(&[("sha", "sha"), ("path", "path"), ("per_page", "perPage"), ("page", "page")]),
        ),
        repo_spec(
            "GitHubGetCommit",
            "Get a single commit, including its diff stats and changed files.",
            "GET",
            "/repos/{owner}/{repo}/commits/{ref}",
            vec![("ref", non_empty("Commit SHA, branch, or tag."), true)],
        ),
        with_query(
            repo_spec(
                "GitHubGetFileContents",
                "Get a file or directory listing. File content is returned base64-encoded in the `content` field.",
                "GET",
                "/repos/{owner}/{repo}/contents/{path}",
                vec![
                    ("path", non_empty("Path to the file or directory in the repo."), true),
                    ("ref", string("Branch, tag, or commit SHA (defaults to the default branch)."), false),
                ],
            ),
            query(&[("ref", "ref")]),
        ),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreateOrUpdateFile",
                "Create or update a file. Provide plain-text `content` (encoded to base64 automatically). Pass `sha` when updating an existing file.",
                "PUT",
                "/repos/{owner}/{repo}/contents/{path}",
                vec![
                    ("path", non_empty("Path to the file in the repo."), true),
                    ("message", non_empty("Commit message."), true),
                    ("content", string("Plain (UTF-8) file content."), true),
                    ("branch", string("Target branch (defaults to the default branch)."), false),
                    ("sha", string("Blob SHA of the file being replaced (required when updating)."), false),
                ],
            ),
            Arc::new(|args| {
                let mut body = object_fields(args, &["message", "branch", "sha"]);
                body.insert(
                    "content".to_string(),
                    Value::String(
                        base64::engine::general_purpose::STANDARD
                            .encode(args["content"].as_str().unwrap_or_default().as_bytes()),
                    ),
                );
                Value::Object(body)
            }),
        )),
        repo_spec(
            "GitHubGetRef",
            "Get a Git reference (branch or tag head). Returns the current SHA for ref names like `heads/main`.",
            "GET",
            "/repos/{owner}/{repo}/git/ref/{ref}",
            vec![("ref", non_empty("Ref name relative to `refs/`, e.g. `heads/main` or `tags/v1`."), true)],
        ),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreateTree",
                "Create a Git tree from file entries. Pair with GitHubCreateCommit and GitHubUpdateRef to push commits via the API without a local clone.",
                "POST",
                "/repos/{owner}/{repo}/git/trees",
                vec![
                    ("baseTree", string("SHA of an existing tree to apply the entries on top of."), false),
                    ("tree", tree_entries(), true),
                ],
            ),
            body(&[("base_tree", "baseTree"), ("tree", "tree")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreateCommit",
                "Create a commit object for a tree SHA with parent commit SHAs.",
                "POST",
                "/repos/{owner}/{repo}/git/commits",
                vec![
                    ("message", non_empty("Commit message."), true),
                    ("tree", non_empty("Git tree SHA the commit points to."), true),
                    ("parents", string_array("Parent commit SHAs (one for a normal commit).", true), true),
                ],
            ),
            body(&[("message", "message"), ("tree", "tree"), ("parents", "parents")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubUpdateRef",
                "Point a Git reference at a new commit SHA — the final step of pushing commits via the API.",
                "PATCH",
                "/repos/{owner}/{repo}/git/refs/{ref}",
                vec![
                    ("ref", non_empty("Ref name relative to `refs/`, e.g. `heads/main`."), true),
                    ("sha", non_empty("Commit SHA the ref should point to."), true),
                    ("force", boolean("Overwrite the ref even when not a fast-forward."), false),
                ],
            ),
            body(&[("sha", "sha"), ("force", "force")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreateBranch",
                "Create a new branch pointing at an existing commit SHA.",
                "POST",
                "/repos/{owner}/{repo}/git/refs",
                vec![
                    ("branch", non_empty("New branch name."), true),
                    ("sha", non_empty("Commit SHA the new branch points to."), true),
                ],
            ),
            Arc::new(|args| {
                let mut body = object_fields(args, &["sha"]);
                body.insert(
                    "ref".to_string(),
                    Value::String(format!("refs/heads/{}", args["branch"].as_str().unwrap_or_default())),
                );
                Value::Object(body)
            }),
        )),
        with_query(
            repo_spec(
                "GitHubListIssues",
                "List issues in a repository (excludes pull requests unless combined with search).",
                "GET",
                "/repos/{owner}/{repo}/issues",
                vec![
                    ("state", enumeration(&["open", "closed", "all"], "Issue state filter."), false),
                    ("labels", string("Comma-separated label names."), false),
                    per_page(),
                    page(),
                ],
            ),
            query(&[("state", "state"), ("labels", "labels"), ("per_page", "perPage"), ("page", "page")]),
        ),
        repo_spec(
            "GitHubGetIssue",
            "Get a single issue by number.",
            "GET",
            "/repos/{owner}/{repo}/issues/{issueNumber}",
            vec![("issueNumber", integer("Issue number.", None, None), true)],
        ),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreateIssue",
                "Create a new issue.",
                "POST",
                "/repos/{owner}/{repo}/issues",
                vec![
                    ("title", non_empty("Issue title."), true),
                    ("body", string("Issue body (Markdown)."), false),
                    ("labels", string_array("Label names to apply.", false), false),
                    ("assignees", string_array("User logins to assign.", false), false),
                ],
            ),
            body(&[("title", "title"), ("body", "body"), ("labels", "labels"), ("assignees", "assignees")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubUpdateIssue",
                "Update an issue (title, body, state, labels, assignees).",
                "PATCH",
                "/repos/{owner}/{repo}/issues/{issueNumber}",
                vec![
                    ("issueNumber", integer("Issue number.", None, None), true),
                    ("title", string(""), false),
                    ("body", string(""), false),
                    ("state", enumeration(&["open", "closed"], ""), false),
                    ("labels", string_array("", false), false),
                    ("assignees", string_array("", false), false),
                ],
            ),
            body(&[("title", "title"), ("body", "body"), ("state", "state"), ("labels", "labels"), ("assignees", "assignees")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubAddIssueComment",
                "Add a comment to an issue or pull request.",
                "POST",
                "/repos/{owner}/{repo}/issues/{issueNumber}/comments",
                vec![
                    ("issueNumber", integer("Issue or PR number.", None, None), true),
                    ("body", non_empty("Comment body (Markdown)."), true),
                ],
            ),
            body(&[("body", "body")]),
        )),
        with_query(
            repo_spec(
                "GitHubListIssueComments",
                "List comments on an issue or pull request.",
                "GET",
                "/repos/{owner}/{repo}/issues/{issueNumber}/comments",
                vec![("issueNumber", integer("Issue or PR number.", None, None), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            repo_spec(
                "GitHubListPRs",
                "List pull requests in a repository.",
                "GET",
                "/repos/{owner}/{repo}/pulls",
                vec![
                    ("state", enumeration(&["open", "closed", "all"], ""), false),
                    ("head", string("Filter by head branch (user:ref)."), false),
                    ("base", string("Filter by base branch name."), false),
                    per_page(),
                    page(),
                ],
            ),
            query(&[("state", "state"), ("head", "head"), ("base", "base"), ("per_page", "perPage"), ("page", "page")]),
        ),
        repo_spec(
            "GitHubGetPR",
            "Get a single pull request by number.",
            "GET",
            "/repos/{owner}/{repo}/pulls/{pullNumber}",
            vec![("pullNumber", integer("Pull request number.", None, None), true)],
        ),
        with_accept(
            repo_spec(
                "GitHubGetPRDiff",
                "Get the unified diff for a pull request.",
                "GET",
                "/repos/{owner}/{repo}/pulls/{pullNumber}",
                vec![("pullNumber", integer("Pull request number.", None, None), true)],
            ),
            "application/vnd.github.diff",
        ),
        with_query(
            repo_spec(
                "GitHubGetPRFiles",
                "List the files changed in a pull request.",
                "GET",
                "/repos/{owner}/{repo}/pulls/{pullNumber}/files",
                vec![("pullNumber", integer("Pull request number.", None, None), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreatePR",
                "Open a new pull request.",
                "POST",
                "/repos/{owner}/{repo}/pulls",
                vec![
                    ("title", non_empty("PR title."), true),
                    ("head", non_empty("Source branch (or user:branch for cross-repo)."), true),
                    ("base", non_empty("Target branch to merge into."), true),
                    ("body", string("PR description (Markdown)."), false),
                    ("draft", boolean("Open as a draft PR."), false),
                ],
            ),
            body(&[("title", "title"), ("head", "head"), ("base", "base"), ("body", "body"), ("draft", "draft")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubUpdatePR",
                "Update a pull request (title, body, state, base branch).",
                "PATCH",
                "/repos/{owner}/{repo}/pulls/{pullNumber}",
                vec![
                    ("pullNumber", integer("Pull request number.", None, None), true),
                    ("title", string(""), false),
                    ("body", string(""), false),
                    ("state", enumeration(&["open", "closed"], ""), false),
                    ("base", string("New base branch."), false),
                ],
            ),
            body(&[("title", "title"), ("body", "body"), ("state", "state"), ("base", "base")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubMergePR",
                "Merge a pull request.",
                "PUT",
                "/repos/{owner}/{repo}/pulls/{pullNumber}/merge",
                vec![
                    ("pullNumber", integer("Pull request number.", None, None), true),
                    ("commitTitle", string("Title for the merge commit."), false),
                    ("mergeMethod", enumeration(&["merge", "squash", "rebase"], "Merge strategy."), false),
                ],
            ),
            body(&[("commit_title", "commitTitle"), ("merge_method", "mergeMethod")]),
        )),
        with_mutating(with_body(
            repo_spec(
                "GitHubCreatePRReview",
                "Submit a review on a pull request (APPROVE, REQUEST_CHANGES, or COMMENT).",
                "POST",
                "/repos/{owner}/{repo}/pulls/{pullNumber}/reviews",
                vec![
                    ("pullNumber", integer("Pull request number.", None, None), true),
                    ("event", enumeration(&["APPROVE", "REQUEST_CHANGES", "COMMENT"], "Review action."), true),
                    ("body", string("Review summary comment."), false),
                ],
            ),
            body(&[("event", "event"), ("body", "body")]),
        )),
        with_query(
            repo_spec(
                "GitHubListPRReviewComments",
                "List review comments (inline code comments) on a pull request.",
                "GET",
                "/repos/{owner}/{repo}/pulls/{pullNumber}/comments",
                vec![("pullNumber", integer("Pull request number.", None, None), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            search_spec(
                "GitHubSearchCode",
                "Search code across GitHub. Use qualifiers like `repo:owner/name`, `path:`, `language:`.",
                "/search/code",
                vec![per_page(), page()],
            ),
            query(&[("q", "q"), ("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            search_spec(
                "GitHubSearchRepos",
                "Search repositories. Supports qualifiers like `language:`, `stars:>100`, `user:`.",
                "/search/repositories",
                vec![("sort", enumeration(&["stars", "forks", "updated"], ""), false), per_page(), page()],
            ),
            query(&[("q", "q"), ("sort", "sort"), ("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            search_spec(
                "GitHubSearchIssues",
                "Search issues and pull requests. Supports qualifiers like `repo:`, `is:pr`, `author:`, `state:`.",
                "/search/issues",
                vec![("sort", enumeration(&["comments", "created", "updated"], ""), false), per_page(), page()],
            ),
            query(&[("q", "q"), ("sort", "sort"), ("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            repo_spec(
                "GitHubListWorkflowRuns",
                "List GitHub Actions workflow runs for a repository.",
                "GET",
                "/repos/{owner}/{repo}/actions/runs",
                vec![
                    ("branch", string("Filter by branch."), false),
                    ("status", string("Filter by status/conclusion (e.g. success, failure, in_progress)."), false),
                    per_page(),
                    page(),
                ],
            ),
            query(&[("branch", "branch"), ("status", "status"), ("per_page", "perPage"), ("page", "page")]),
        ),
        repo_spec(
            "GitHubGetWorkflowRun",
            "Get a single GitHub Actions workflow run.",
            "GET",
            "/repos/{owner}/{repo}/actions/runs/{runId}",
            vec![("runId", integer("Workflow run id.", None, None), true)],
        ),
        with_query(
            repo_spec(
                "GitHubListReleases",
                "List releases for a repository.",
                "GET",
                "/repos/{owner}/{repo}/releases",
                vec![per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        repo_spec(
            "GitHubGetLatestRelease",
            "Get the latest published release for a repository.",
            "GET",
            "/repos/{owner}/{repo}/releases/latest",
            vec![],
        ),
        base_spec(
            "GitHubGetMe",
            "Get the authenticated user (verifies the configured token).",
            "GET",
            "/user",
            object_schema(vec![]),
            Arc::new(|_| "me".to_string()),
        ),
    ]
}

fn repo_spec(
    name: &'static str,
    description: &'static str,
    method: &'static str,
    path: &'static str,
    mut fields: Vec<(&'static str, Value, bool)>,
) -> GitHubToolSpec {
    let mut all = vec![owner(), repo()];
    all.append(&mut fields);
    base_spec(
        name,
        description,
        method,
        path,
        object_schema(all),
        Arc::new(|args| {
            format!(
                "{}/{}",
                args["owner"].as_str().unwrap_or_default(),
                args["repo"].as_str().unwrap_or_default()
            )
        }),
    )
}

fn search_spec(
    name: &'static str,
    description: &'static str,
    path: &'static str,
    mut fields: Vec<(&'static str, Value, bool)>,
) -> GitHubToolSpec {
    let mut all = vec![("q", non_empty("Search query."), true)];
    all.append(&mut fields);
    base_spec(
        name,
        description,
        "GET",
        path,
        object_schema(all),
        Arc::new(|args| args["q"].as_str().unwrap_or_default().to_string()),
    )
}

fn base_spec(
    name: &str,
    description: &str,
    method: &str,
    path: &'static str,
    schema: Value,
    subject: SubjectResolver,
) -> GitHubToolSpec {
    GitHubToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema: to_input_json_schema(schema),
        method: method.to_string(),
        path: path_resolver(path),
        query: None,
        body: None,
        accept: None,
        mutating: None,
        subject,
    }
}

fn with_query(mut spec: GitHubToolSpec, resolver: QueryResolver) -> GitHubToolSpec {
    spec.query = Some(resolver);
    spec
}

fn with_body(mut spec: GitHubToolSpec, resolver: BodyResolver) -> GitHubToolSpec {
    spec.body = Some(resolver);
    spec
}

fn with_accept(mut spec: GitHubToolSpec, accept: &str) -> GitHubToolSpec {
    spec.accept = Some(accept.to_string());
    spec
}

fn with_mutating(mut spec: GitHubToolSpec) -> GitHubToolSpec {
    spec.mutating = Some(true);
    spec
}

fn path_resolver(template: &'static str) -> PathResolver {
    Arc::new(move |args| {
        let mut path = String::new();
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            path.push_str(&rest[..start]);
            let end = rest[start..]
                .find('}')
                .map(|offset| start + offset)
                .expect("GitHub path placeholder is terminated");
            let key = &rest[start + 1..end];
            path.push_str(&scalar_string(args.get(key)));
            rest = &rest[end + 1..];
        }
        path.push_str(rest);
        path
    })
}

fn query(mappings: &'static [(&'static str, &'static str)]) -> QueryResolver {
    Arc::new(move |args| {
        let mut values = Map::new();
        for (target, source) in mappings {
            if let Some(value) = args.get(*source) {
                values.insert((*target).to_string(), value.clone());
            }
        }
        values
    })
}

fn body(mappings: &'static [(&'static str, &'static str)]) -> BodyResolver {
    Arc::new(move |args| {
        let mut values = Map::new();
        for (target, source) in mappings {
            if let Some(value) = args.get(*source) {
                values.insert((*target).to_string(), value.clone());
            }
        }
        Value::Object(values)
    })
}

fn object_fields(args: &Value, fields: &[&str]) -> Map<String, Value> {
    let mut values = Map::new();
    for field in fields {
        if let Some(value) = args.get(*field) {
            values.insert((*field).to_string(), value.clone());
        }
    }
    values
}

fn scalar_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn owner() -> (&'static str, Value, bool) {
    ("owner", non_empty("Repository owner (user or organization login)."), true)
}

fn repo() -> (&'static str, Value, bool) {
    ("repo", non_empty("Repository name."), true)
}

fn per_page() -> (&'static str, Value, bool) {
    ("perPage", integer("Results per page (1–100).", Some(1), Some(100)), false)
}

fn page() -> (&'static str, Value, bool) {
    ("page", integer("Page number (1-based).", Some(1), None), false)
}

fn string(description: &str) -> Value {
    described(json!({ "type": "string" }), description)
}

fn non_empty(description: &str) -> Value {
    described(json!({ "type": "string", "minLength": 1 }), description)
}

fn integer(description: &str, minimum: Option<i64>, maximum: Option<i64>) -> Value {
    let mut schema = json!({
        "type": "integer",
        "minimum": minimum.unwrap_or(-9007199254740991),
        "maximum": maximum.unwrap_or(9007199254740991)
    });
    described(schema, description)
}

fn boolean(description: &str) -> Value {
    described(json!({ "type": "boolean" }), description)
}

fn enumeration(values: &'static [&'static str], description: &str) -> Value {
    described(json!({ "type": "string", "enum": values }), description)
}

fn string_array(description: &str, non_empty: bool) -> Value {
    let mut schema = json!({ "type": "array", "items": { "type": "string" } });
    if non_empty {
        schema["minItems"] = json!(1);
    }
    described(schema, description)
}

fn tree_entries() -> Value {
    let entry = object_schema(vec![
        ("path", non_empty("Path relative to the repository root."), true),
        ("mode", enumeration(&["100644", "100755", "040000", "160000", "120000"], "Entry mode; defaults to 100644 (regular file)."), false),
        ("type", enumeration(&["blob", "tree", "commit"], "Entry type; defaults to blob."), false),
        ("content", string("Plain UTF-8 file content (creates a new blob)."), false),
        ("sha", string("SHA of an existing blob/tree to point the entry at."), false),
    ]);
    json!({
        "type": "array",
        "items": entry,
        "minItems": 1,
        "description": "Tree entries to write."
    })
}

fn described(mut schema: Value, description: &str) -> Value {
    if !description.is_empty() {
        schema["description"] = json!(description);
    }
    schema
}

fn object_schema(fields: Vec<(&'static str, Value, bool)>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        properties.insert(name.to_string(), schema);
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false
    });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    schema
}

fn parse_args(schema: &Value, input: Value) -> Result<Value, String> {
    let mut issues = Vec::new();
    let parsed = parse_value(schema, input, &mut Vec::new(), &mut issues);
    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(serde_json::to_string_pretty(&issues).unwrap_or_default())
    }
}

fn parse_value(
    schema: &Value,
    input: Value,
    path: &mut Vec<Value>,
    issues: &mut Vec<Value>,
) -> Value {
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or_default();
    let expected = if kind == "integer" { "number" } else { kind };
    let type_valid = match kind {
        "object" => input.is_object(),
        "array" => input.is_array(),
        "string" => input.is_string(),
        "integer" => input.as_f64().is_some_and(|value| value.is_finite() && value.fract() == 0.0),
        "number" => input.as_f64().is_some_and(|value| value.is_finite()),
        "boolean" => input.is_boolean(),
        _ => true,
    };
    if !type_valid {
        issues.push(invalid_type_issue(path, expected, received_type(&input)));
        return input;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(&input)
    {
        issues.push(json!({
            "code": "invalid_enum_value",
            "options": values,
            "path": path,
            "message": format!("Invalid enum value. Expected one of {values:?}")
        }));
    }
    match kind {
        "object" => {
            let input = input.as_object().expect("object type was checked");
            let properties = schema.get("properties").and_then(Value::as_object);
            let required = schema.get("required").and_then(Value::as_array);
            let mut output = Map::new();
            if let Some(properties) = properties {
                for (name, property_schema) in properties {
                    path.push(Value::String(name.clone()));
                    match input.get(name) {
                        Some(value) => {
                            let value = parse_value(property_schema, value.clone(), path, issues);
                            output.insert(name.clone(), value);
                        }
                        None if required.is_some_and(|required| {
                            required.iter().any(|value| value.as_str() == Some(name.as_str()))
                        }) => {
                            issues.push(invalid_type_issue(
                                path,
                                property_schema
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .map(|kind| if kind == "integer" { "number" } else { kind })
                                    .unwrap_or("unknown"),
                                "undefined",
                            ));
                        }
                        None => {}
                    }
                    path.pop();
                }
            }
            Value::Object(output)
        }
        "array" => {
            let values = input.as_array().expect("array type was checked");
            if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
                && values.len() < minimum as usize
            {
                issues.push(json!({
                    "code": "too_small",
                    "minimum": minimum,
                    "type": "array",
                    "inclusive": true,
                    "path": path,
                    "message": format!("Array must contain at least {minimum} element(s)")
                }));
            }
            let mut output = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                path.push(json!(index));
                if let Some(item_schema) = schema.get("items") {
                    output.push(parse_value(item_schema, value.clone(), path, issues));
                } else {
                    output.push(value.clone());
                }
                path.pop();
            }
            Value::Array(output)
        }
        "string" => {
            let value = input.as_str().expect("string type was checked");
            if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
                && value.encode_utf16().count() < minimum as usize
            {
                issues.push(json!({
                    "code": "too_small",
                    "minimum": minimum,
                    "type": "string",
                    "inclusive": true,
                    "path": path,
                    "message": format!("String must contain at least {minimum} character(s)")
                }));
            }
            input
        }
        "integer" | "number" => {
            let value = input.as_f64().expect("number type was checked");
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
                && value < minimum
            {
                issues.push(json!({
                    "code": "too_small",
                    "minimum": minimum,
                    "type": "number",
                    "inclusive": true,
                    "path": path,
                    "message": format!("Number must be greater than or equal to {minimum}")
                }));
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
                && value > maximum
            {
                issues.push(json!({
                    "code": "too_big",
                    "maximum": maximum,
                    "type": "number",
                    "inclusive": true,
                    "path": path,
                    "message": format!("Number must be less than or equal to {maximum}")
                }));
            }
            input
        }
        _ => input,
    }
}

fn invalid_type_issue(path: &[Value], expected: &str, received: &str) -> Value {
    json!({
        "code": "invalid_type",
        "expected": expected,
        "received": received,
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}")
    })
}

fn received_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn utf16_prefix(value: &str, max_units: usize) -> &str {
    if value.encode_utf16().count() <= max_units {
        return value;
    }
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    &value[..end]
}
