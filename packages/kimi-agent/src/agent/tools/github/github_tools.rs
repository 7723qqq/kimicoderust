use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::config_section::{resolve_github_base_url, resolve_github_token};
use super::github::{GitHubToolSpec, IGitHubTool};
use super::github_request::{GitHubRequestDeps, GitHubRequestOptions, github_request};

use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::config::KimiConfig;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::ToolOutputAccumulator;
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::ToolAccesses;
use crate::wire::record::WireAsyncError;

type PathResolver = Arc<dyn Fn(&Value) -> String + Send + Sync>;
type QueryResolver = Arc<dyn Fn(&Value) -> Vec<(String, Value)> + Send + Sync>;
type BodyResolver = Arc<dyn Fn(&Value) -> Vec<(String, Value)> + Send + Sync>;
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
            query,
            body,
            accept: spec.accept.clone(),
            token: resolve_github_token(&self.deps.config).map(str::to_string),
            base_url: resolve_github_base_url(&self.deps.config).map(str::to_string),
            max_bytes: spec.max_bytes,
        };
        let request = github_request(
            spec.method.clone(),
            (spec.path)(&args),
            options,
            GitHubRequestDeps {
                client: self.deps.client.clone(),
                signal: None,
            },
        );
        let response = tokio::select! {
            biased;
            () = context.signal.cancelled() => {
                context.signal.throw_if_aborted()?;
                unreachable!("cancelled signal must be aborted");
            }
            result = request => result.map_err(|error| Box::new(error) as WireAsyncError)?,
        };
        if !response.ok {
            let detail_limit = if response.status >= 500 { 2000 } else { 400 };
            let detail = if response.body.is_empty() {
                String::new()
            } else {
                format!("\n{}", utf16_prefix(&response.body, detail_limit))
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
        let rate = response.rate_remaining.map(rate_notice).unwrap_or_default();
        let shaped = spec.shape.as_ref().and_then(|shape| shape(&response.body, response.next_page, &args)).unwrap_or(response.body);
        let body = if shaped.is_empty() { "(empty response)".to_string() } else { shaped };
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

fn rate_notice(remaining: i64) -> String {
    let warning = if remaining < 100 { " — the GitHub API budget is nearly exhausted" } else { "" };
    format!("\n\n(GitHub rate limit remaining: {remaining}{warning})")
}

impl IGitHubTool for GitHubToolBase {}

impl ExecutableTool for GitHubToolBase {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(if self.spec.mutating == Some(true) {
            ToolSideEffect::Write
        } else {
            ToolSideEffect::Read
        })
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(if self.spec.mutating == Some(true) {
            RepoMutation::Possible
        } else {
            RepoMutation::None
        })
    }

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
                    ("maxChars", integer("Maximum characters of decoded text to return (default 40000).", Some(1), Some(40_000)), false),
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
                let mut body = fields(args, &[("message", "message")]);
                body.push((
                    "content".to_string(),
                    Value::String(
                        base64::engine::general_purpose::STANDARD
                            .encode(args["content"].as_str().unwrap_or_default().as_bytes()),
                    ),
                ));
                body.extend(fields(args, &[("branch", "branch"), ("sha", "sha")]));
                body
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
                let mut body = vec![(
                    "ref".to_string(),
                    Value::String(format!("refs/heads/{}", args["branch"].as_str().unwrap_or_default())),
                )];
                body.extend(fields(args, &[("sha", "sha")]));
                body
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
        with_max_bytes(
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
            crate::agent::tools::github::github_request::GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES,
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
        with_query(
            repo_spec(
                "GitHubGetRepoTree",
                "List a repository tree recursively (paths, types, sizes, SHAs) without cloning it.",
                "GET",
                "/repos/{owner}/{repo}/git/trees/{treeSha}",
                vec![
                    ("treeSha", non_empty("Tree SHA, branch name, or tag to list. Use GitHubGetRef to resolve a branch."), true),
                    ("recursive", boolean("Descend into subdirectories (default true)."), false),
                ],
            ),
            Arc::new(|args| {
                if args.get("recursive").and_then(Value::as_bool) != Some(false) {
                    vec![("recursive".to_string(), Value::String("1".to_string()))]
                } else {
                    Vec::new()
                }
            }),
        ),
        with_query(
            repo_spec(
                "GitHubListPRCommits",
                "List the commits in a pull request.",
                "GET",
                "/repos/{owner}/{repo}/pulls/{pullNumber}/commits",
                vec![("pullNumber", integer("Pull request number.", None, None), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            repo_spec(
                "GitHubGetCheckRuns",
                "List CI check runs for a commit ref (status and conclusion per check).",
                "GET",
                "/repos/{owner}/{repo}/commits/{ref}/check-runs",
                vec![("ref", non_empty("Commit SHA, branch, or tag."), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
        ),
        with_query(
            repo_spec(
                "GitHubListWorkflowRunJobs",
                "List the jobs of a GitHub Actions workflow run.",
                "GET",
                "/repos/{owner}/{repo}/actions/runs/{runId}/jobs",
                vec![("runId", integer("Workflow run id.", None, None), true), per_page(), page()],
            ),
            query(&[("per_page", "perPage"), ("page", "page")]),
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
        max_bytes: None,
        shape: match name {
            "GitHubListBranches" | "GitHubListCommits" | "GitHubGetCommit"
            | "GitHubListIssues" | "GitHubGetIssue" | "GitHubListIssueComments"
            | "GitHubListPRs" | "GitHubGetPR" | "GitHubGetPRFiles"
            | "GitHubListPRReviewComments" | "GitHubSearchCode" | "GitHubSearchRepos"
            | "GitHubSearchIssues" | "GitHubListPRCommits" | "GitHubGetCheckRuns"
            | "GitHubListWorkflowRunJobs" | "GitHubListWorkflowRuns" | "GitHubGetWorkflowRun"
            | "GitHubListReleases" | "GitHubGetLatestRelease" | "GitHubGetFileContents"
            | "GitHubGetRepoTree" => {
                let name = name.to_string();
                Some(Arc::new(move |body, next_page, args| super::github_shape::shape_response(&name, body, next_page, args)))
            }
            _ => None,
        },
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

fn with_max_bytes(mut spec: GitHubToolSpec, max_bytes: usize) -> GitHubToolSpec {
    spec.max_bytes = Some(max_bytes);
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
    Arc::new(move |args| fields(args, mappings))
}

fn body(mappings: &'static [(&'static str, &'static str)]) -> BodyResolver {
    Arc::new(move |args| fields(args, mappings))
}

fn fields(args: &Value, mappings: &[(&str, &str)]) -> Vec<(String, Value)> {
    mappings
        .iter()
        .filter_map(|(target, source)| {
            args.get(*source).map(|value| ((*target).to_string(), value.clone()))
        })
        .collect()
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
    let schema = json!({
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
        ("mode", enumeration(&["100644", "100755", "120000", "160000", "040000"], "Entry mode; defaults to 100644 (regular file)."), false),
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
        // v2's zod error.message is `JSON.stringify({issues}, null, 2)`
        // (zod errors.js:13) — the pretty-printed issues wrapped in an
        // `{"issues": …}` object, not a bare array.
        let wrapped = json!({ "issues": issues });
        Err(serde_json::to_string_pretty(&wrapped).unwrap_or_default())
    }
}

fn parse_value(
    schema: &Value,
    input: Value,
    path: &mut Vec<Value>,
    issues: &mut Vec<Value>,
) -> Value {
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or_default();
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
        let expected = if kind == "integer" { "int" } else { kind };
        issues.push(invalid_type_issue(path, expected, received_type(&input)));
        return input;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(&input)
    {
        issues.push(json!({
            "code": "invalid_value",
            "values": values,
            "path": path,
            "message": format!("Invalid option: expected one of {values:?}")
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
                    "origin": "array",
                    "inclusive": true,
                    "path": path,
                    "message": format!("Too small: expected array to have >={minimum} items")
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
                    "origin": "string",
                    "inclusive": true,
                    "path": path,
                    "message": format!("Too small: expected string to have >={minimum} characters")
                }));
            }
            input
        }
        "integer" | "number" => {
            let value = input.as_f64().expect("number type was checked");
            let origin = if kind == "integer" { "int" } else { "number" };
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
                && value < minimum
            {
                issues.push(json!({
                    "code": "too_small",
                    "minimum": minimum,
                    "origin": origin,
                    "inclusive": true,
                    "path": path,
                    "message": format!("Too small: expected {origin} to be >={minimum}")
                }));
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
                && value > maximum
            {
                issues.push(json!({
                    "code": "too_big",
                    "maximum": maximum,
                    "origin": origin,
                    "inclusive": true,
                    "path": path,
                    "message": format!("Too big: expected {origin} to be <={maximum}")
                }));
            }
            if kind == "integer" && value.abs() <= 9_007_199_254_740_991.0 && value.trunc() == value {
                Value::from(value as i64)
            } else {
                input
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_v2_safe_integral_float_arguments() {
        let file = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetFileContents").unwrap();
        let parsed = parse_args(&file.schema, json!({"owner":"example","repo":"project","path":"x","maxChars":3.0})).unwrap();
        assert_eq!(parsed["maxChars"], json!(3));
    }

    #[test]
    fn all_specs_match_the_v2_generated_contract() {
        let fixture: Value = serde_json::from_str(include_str!("../../../../tests/fixtures/github-specs.json")).unwrap();
        let rows = fixture["rows"].as_array().unwrap();
        assert_eq!(GITHUB_SPECS.len(), rows.len());
        for (spec, expected) in GITHUB_SPECS.iter().zip(rows) {
            let args = parse_args(&spec.schema, expected["args"].clone()).unwrap();
            let query = spec.query.as_ref().map(|query| query(&args));
            let body = spec.body.as_ref().map(|body| body(&args));
            let actual = json!({
                "name":spec.name,"args":args,"description":spec.description,"schema":spec.schema,
                "method":spec.method,"path":(spec.path)(&args),
                "query":query.clone().map(|pairs| pairs.into_iter().collect::<Map<String, Value>>()),
                "queryPairs":query,
                "body":body.clone().map(|pairs| pairs.into_iter().collect::<Map<String, Value>>()),
                "bodyText":body.as_ref().map(|pairs| super::super::github_request::js_json_stringify_pairs(pairs)),
                "accept":spec.accept,"maxBytes":spec.max_bytes,"mutating":spec.mutating==Some(true),
                "hasShape":spec.shape.is_some(),"subject":(spec.subject)(&args),
            });
            assert_eq!(&actual, expected, "{}", expected["name"]);
        }
    }

    #[tokio::test]
    async fn file_content_limiting_accepts_json_integral_floats() {
        use crate::agent::tool_executor::tool_executor::ExecutableToolOutput;

        let spec = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetFileContents").unwrap().clone();
        let response = execute_response(
            spec,
            200,
            r#"{"path":"x","encoding":"base64","content":"aGVsbG8="}"#,
            "",
            json!({"owner":"example","repo":"project","path":"x","maxChars":3.0}),
        ).await;
        assert_eq!(response.output, ExecutableToolOutput::Text("{\"path\":\"x\"}\n---\nhel\n[...truncated 2 characters]".into()));
    }

    #[test]
    fn query_resolvers_keep_reference_parameter_order() {
        let spec = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubListIssues").unwrap();
        let args = parse_args(&spec.schema, json!({"owner":"example","repo":"project","state":"open","labels":"bug,docs","perPage":10,"page":2})).unwrap();
        assert_eq!((spec.query.as_ref().unwrap())(&args), vec![
            ("state".into(), json!("open")),
            ("labels".into(), json!("bug,docs")),
            ("per_page".into(), json!(10)),
            ("page".into(), json!(2)),
        ]);
    }

    #[tokio::test]
    async fn execution_projects_results_and_keeps_pagination_in_the_header() {
        use crate::agent::tool_executor::tool_executor::ExecutableToolOutput;
        let spec = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubListIssues").unwrap().clone();
        let response = execute_response(spec, 200, r#"[{"number":7,"title":" issue ","body":"hidden"}]"#, "Link: <https://example.test/next>; rel=\"next\"\r\nX-RateLimit-Remaining: 99\r\n", json!({"owner":"example","repo":"project"})).await;
        assert_eq!(response.output, ExecutableToolOutput::Text("{\"items\":1,\"has_more\":true}\n{\"number\":7,\"title\":\"issue\"}\n\n(GitHub rate limit remaining: 99 — the GitHub API budget is nearly exhausted)".into()));
    }

    #[tokio::test]
    async fn execution_preserves_unshaped_responses_and_v2_error_detail_limits() {
        use crate::agent::tool_executor::tool_executor::ExecutableToolOutput;
        let spec = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetRepo").unwrap().clone();
        let args = json!({"owner":"example","repo":"project"});
        let response = execute_response(spec.clone(), 200, "raw", "Link: <https://example.test/next>; rel=\"next\"\r\n", args.clone()).await;
        assert_eq!(response.output, ExecutableToolOutput::Text("raw".into()));
        for (status, limit) in [(404, 400), (500, 2000)] {
            let response = execute_response(spec.clone(), status, &"x".repeat(3000), "", args.clone()).await;
            assert_eq!(response.is_error, Some(true));
            assert_eq!(response.output, ExecutableToolOutput::Text(format!("GitHub API error {status} (status {status})\n{}", "x".repeat(limit))));
        }
    }

    #[tokio::test]
    async fn cancellation_closes_the_request_and_preserves_the_reason() {
        use crate::_base::utils::abort::{AbortController, UserCancellationError, user_cancellation_reason};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n").await.unwrap();
            ready.send(()).unwrap();
            let mut byte = [0];
            match tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte)).await.unwrap() {
                Ok(0) | Err(_) => {},
                other => panic!("cancelled request remained open: {other:?}"),
            }
        });
        let mut config = KimiConfig::default();
        config.github.token = Some("YOUR_API_KEY".into());
        config.github.base_url = Some(base_url);
        let spec = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetRepo").unwrap().clone();
        let tool = GitHubToolBase::new(spec, GitHubToolDeps { config:Arc::new(config), client:Some(reqwest::Client::builder().no_proxy().build().unwrap()) });
        let ToolExecution::Runnable(run) = tool.resolve_execution(json!({"owner":"example","repo":"project"})) else { panic!("valid arguments must resolve"); };
        let controller = AbortController::new();
        let context = ExecutableToolContext {turn_id:0,tool_call_id:String::new(),trace:None,metadata:None,signal:controller.signal(),steer_signal:None,on_update:None,on_foreground_task_start:None};
        let executing = tokio::spawn((run.execute)(context));
        waiting.await.unwrap();
        controller.abort(Some(Arc::new(user_cancellation_reason())));
        let error = executing.await.unwrap().unwrap_err();
        assert!(error.is::<UserCancellationError>());
        server.await.unwrap();
    }

    #[test]
    fn additional_specs_preserve_v2_defaults_and_queries() {
        let tree = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetRepoTree").unwrap();
        assert_eq!((tree.query.as_ref().unwrap())(&json!({})), vec![("recursive".to_string(), json!("1"))]);
        assert!((tree.query.as_ref().unwrap())(&json!({"recursive":false})).is_empty());
        for name in ["GitHubListPRCommits", "GitHubGetCheckRuns", "GitHubListWorkflowRunJobs"] {
            assert!(GITHUB_SPECS.iter().any(|spec| spec.name == name), "{name}");
        }
        let file = GITHUB_SPECS.iter().find(|spec| spec.name == "GitHubGetFileContents").unwrap();
        assert!(parse_args(&file.schema, json!({"owner":"example","repo":"project","path":"x","maxChars":0})).is_err());
        assert_eq!(parse_args(&file.schema, json!({"owner":"example","repo":"project","path":"x","maxChars":1})).unwrap()["maxChars"], 1);
    }

    async fn execute_response(spec: GitHubToolSpec, status: u16, body: &str, headers: &str, args: Value) -> ExecutableToolResult {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!("HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let mut config = KimiConfig::default();
        config.github.token = Some("YOUR_API_KEY".into());
        config.github.base_url = Some(base_url);
        let tool = GitHubToolBase::new(spec, GitHubToolDeps { config: Arc::new(config), client: Some(reqwest::Client::builder().no_proxy().build().unwrap()) });
        let ToolExecution::Runnable(run) = tool.resolve_execution(args) else { panic!("valid arguments must resolve"); };
        let context = ExecutableToolContext { turn_id:0, tool_call_id:String::new(), trace:None, metadata:None, signal:crate::_base::utils::abort::AbortController::new().signal(), steer_signal:None, on_update:None, on_foreground_task_start:None };
        let result = (run.execute)(context).await.unwrap();
        server.await.unwrap();
        result
    }

    #[test]
    fn mutation_and_readonly_spec_tables_cover_every_github_tool_once() {
        assert!(!GITHUB_MUTATING_TOOL_NAMES.is_empty());
        assert!(!GITHUB_READONLY_TOOL_NAMES.is_empty());
        for spec in GITHUB_SPECS.iter() {
            assert_ne!(
                GITHUB_MUTATING_TOOL_NAMES.contains(&spec.name),
                GITHUB_READONLY_TOOL_NAMES.contains(&spec.name),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn only_the_pull_request_diff_has_the_smaller_response_limit() {
        for spec in GITHUB_SPECS.iter() {
            if spec.name == "GitHubGetPRDiff" {
                assert_eq!(
                    spec.max_bytes,
                    Some(crate::agent::tools::github::github_request::GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES)
                );
            } else {
                assert_eq!(spec.max_bytes, None, "{}", spec.name);
            }
        }
    }
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
