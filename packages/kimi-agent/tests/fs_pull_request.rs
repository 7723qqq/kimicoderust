use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use kimi_agent::server::fs_routes::{
    handle_git_status, handle_git_status_batch, handle_pull_request, handle_pull_request_batch,
};
use serde_json::{Value, json};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct PathEnvGuard(Option<std::ffi::OsString>);

impl PathEnvGuard {
    fn prepend(dir: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        let paths = std::iter::once(dir.to_path_buf()).chain(
            previous
                .as_deref()
                .into_iter()
                .flat_map(std::env::split_paths),
        );
        let value = std::env::join_paths(paths).unwrap();
        unsafe { std::env::set_var("PATH", value) };
        Self(previous)
    }
}

impl Drop for PathEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

struct FakeGh {
    dir: tempfile::TempDir,
    log: PathBuf,
    _path: PathEnvGuard,
    _env_lock: MutexGuard<'static, ()>,
}

impl FakeGh {
    fn new() -> Self {
        let env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        std::fs::write(dir.path().join("succeed"), expected_gh_output()).unwrap();
        #[cfg(windows)]
        {
            let script = format!(
                concat!(
                    "@echo off\r\n",
                    ">>\"{}\" echo %GH_NO_UPDATE_NOTIFIER%,%GH_PROMPT_DISABLED%,%*\r\n",
                    "if exist \"%~dp0succeed\" (\r\n",
                    "type \"%~dp0succeed\"\r\n",
                    "exit /b 0\r\n",
                    ")\r\n",
                    "exit /b 1\r\n"
                ),
                log.display()
            );
            std::fs::write(dir.path().join("gh.cmd"), script).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s,%s,%s\\n' \"$GH_NO_UPDATE_NOTIFIER\" \"$GH_PROMPT_DISABLED\" \"$*\" >> '{1}'\n",
                    "if [ -f '{0}/succeed' ]; then\n",
                    "cat '{0}/succeed'\n",
                    "exit 0\n",
                    "fi\n",
                    "exit 1\n"
                ),
                dir.path().display(),
                log.display()
            );
            let path = dir.path().join("gh");
            std::fs::write(&path, script).unwrap();
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
        let path = PathEnvGuard::prepend(dir.path());
        Self {
            dir,
            log,
            _path: path,
            _env_lock: env_lock,
        }
    }

    fn set_output(&self, output: Option<&str>) {
        let marker = self.dir.path().join("succeed");
        if let Some(output) = output {
            std::fs::write(marker, output).unwrap();
        } else {
            std::fs::remove_file(marker).unwrap();
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn expected_gh_output() -> &'static str {
    r#"{"number":13,"url":"https://github.com/acme/repo/pull/13","state":"OPEN","isDraft":true}"#
}

fn expected_pull_request() -> Value {
    json!({
        "number": 13,
        "state": "draft",
        "url": "https://github.com/acme/repo/pull/13"
    })
}

fn git_repo_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed", args);
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    std::fs::create_dir(root.path().join("pkg")).unwrap();
    std::fs::write(root.path().join("pkg/committed.txt"), "v1\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "init"]);
    root
}

fn response_json(response: kimi_agent::server::router::HttpResponse) -> Value {
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).unwrap()
}

#[test]
fn git_status_queries_pull_request_and_false_skips_gh() {
    let root = git_repo_fixture();
    let gh = FakeGh::new();
    let json = response_json(handle_git_status(
        root.path(),
        &json!({ "include_pull_request": false }),
    ));
    assert!(json["pullRequest"].is_null());
    assert!(gh.calls().is_empty(), "false must not spawn gh");

    let json = response_json(handle_git_status(root.path(), &json!({})));
    assert_eq!(json["pullRequest"], expected_pull_request());
    let calls = gh.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].replace('"', ""),
        "1,1,pr view --json number,url,state,isDraft",
        "the default query uses the upstream argv and noninteractive env"
    );
}

#[test]
fn pull_request_null_after_query_is_distinct_from_skipped_query() {
    let root = git_repo_fixture();
    let gh = FakeGh::new();
    gh.set_output(None);

    let json = response_json(handle_git_status(root.path(), &json!({})));
    assert!(json["pullRequest"].is_null());
    assert_eq!(
        gh.calls().len(),
        1,
        "a failed gh lookup was still attempted"
    );

    let json = response_json(handle_git_status(
        root.path(),
        &json!({ "include_pull_request": false }),
    ));
    assert!(json["pullRequest"].is_null());
    assert_eq!(gh.calls().len(), 1, "the skipped null performs no query");
}

#[test]
fn pull_request_routes_query_and_share_the_cwd_cache() {
    let root = git_repo_fixture();
    let gh = FakeGh::new();

    for _ in 0..2 {
        let json = response_json(handle_pull_request(root.path(), &json!({})));
        assert_eq!(json, expected_pull_request());
    }
    assert_eq!(
        gh.calls().len(),
        1,
        "the second root query hits the TTL cache"
    );

    let json = response_json(handle_pull_request_batch(
        root.path(),
        &json!({ "roots": ["pkg", "pkg", "missing-dir"] }),
    ));
    assert_eq!(json["results"]["pkg"], expected_pull_request());
    assert!(json["errors"]["missing-dir"].is_object());
    assert_eq!(gh.calls().len(), 2, "a scoped cwd has its own cache entry");

    let json = response_json(handle_git_status_batch(
        root.path(),
        &json!({ "roots": ["pkg"], "include_pull_request": false }),
    ));
    assert!(json["results"]["pkg"]["pullRequest"].is_null());
    assert_eq!(gh.calls().len(), 2, "batch false must not spawn gh");

    let json = response_json(handle_git_status_batch(
        root.path(),
        &json!({ "roots": ["pkg"] }),
    ));
    assert_eq!(
        json["results"]["pkg"]["pullRequest"],
        expected_pull_request()
    );
    assert_eq!(
        gh.calls().len(),
        2,
        "status and direct queries share the cache"
    );
}

#[test]
fn pull_request_output_is_normalized_and_validated() {
    let root = git_repo_fixture();
    let gh = FakeGh::new();
    let cases = [
        (
            r#"{"number":12,"url":"https://example.com/pull/12","state":"OPEN","isDraft":false}"#,
            json!({"number":12,"state":"open","url":"https://example.com/pull/12"}),
        ),
        (
            r#"{"number":11,"url":"https://example.com/pull/11","state":"MERGED","isDraft":true}"#,
            json!({"number":11,"state":"merged","url":"https://example.com/pull/11"}),
        ),
        (
            r#"{"number":10,"url":"https://example.com/pull/10","state":"CLOSED"}"#,
            json!({"number":10,"state":"closed","url":"https://example.com/pull/10"}),
        ),
        (
            r#"{"number":0,"url":"https://example.com/pull/0","state":"OPEN"}"#,
            Value::Null,
        ),
        (
            r#"{"number":9,"url":"file:///tmp/pull/9","state":"OPEN"}"#,
            Value::Null,
        ),
        (
            r#"{"number":8,"url":"https://example.com/pull/8","state":"UNKNOWN"}"#,
            Value::Null,
        ),
        ("not json", Value::Null),
    ];

    for (index, (output, expected)) in cases.into_iter().enumerate() {
        let scoped = format!("case-{index}");
        std::fs::create_dir(root.path().join(&scoped)).unwrap();
        gh.set_output(Some(output));
        let json = response_json(handle_git_status(root.path(), &json!({ "root": scoped })));
        assert_eq!(json["pullRequest"], expected, "case {index}");
    }
}
