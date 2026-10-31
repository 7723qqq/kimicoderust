use super::*;

#[test]
fn test_load_marketplace_has_official_entries() {
    let list = load_marketplace(None);
    assert_eq!(list.len(), 3);

    let ds = list
        .iter()
        .find(|p| p.id == "kimi-datasource")
        .expect("kimi-datasource");
    assert_eq!(ds.tier, "official");
    assert_eq!(ds.display_name, "Kimi Datasource");
    assert_eq!(ds.version.as_deref(), Some("3.4.0"));
    assert_eq!(ds.source, "./official/kimi-datasource");
    assert_eq!(ds.keywords.as_ref().unwrap(), &vec!["data", "mcp"]);

    let wb = list
        .iter()
        .find(|p| p.id == "kimi-webbridge")
        .expect("kimi-webbridge");
    assert_eq!(wb.tier, "official");
    assert_eq!(wb.display_name, "Kimi WebBridge");
    assert_eq!(wb.version.as_deref(), Some("1.11.3"));
    assert_eq!(wb.source, "./official/kimi-webbridge");

    let sp = list
        .iter()
        .find(|p| p.id == "superpowers")
        .expect("superpowers");
    assert_eq!(sp.tier, "curated");
    assert_eq!(sp.display_name, "Superpowers");
    assert_eq!(
        sp.homepage.as_deref(),
        Some("https://github.com/obra/superpowers")
    );
}

#[test]
fn test_load_marketplace_from_custom_repo_root() {
    let temp_dir = tempfile::tempdir().unwrap();
    let plugins_dir = temp_dir.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let custom_catalog = serde_json::json!({
        "version": "1",
        "plugins": [
            {
                "id": "custom-tool",
                "tier": "community",
                "displayName": "Custom Tool",
                "version": "2.0.0",
                "description": "A custom tool",
                "source": "https://example.test/custom"
            }
        ]
    });
    std::fs::write(
        plugins_dir.join("marketplace.json"),
        custom_catalog.to_string(),
    )
    .unwrap();

    let entries = load_marketplace(Some(temp_dir.path()));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "custom-tool");
    assert_eq!(entries[0].tier, "community");
    assert_eq!(entries[0].display_name, "Custom Tool");
    assert_eq!(entries[0].version.as_deref(), Some("2.0.0"));
}

/// v2 #3963 `enabled_plugins`: the sorted, comma-joined enabled set —
/// an empty string when the set is known empty.
#[test]
fn test_enabled_plugin_ids_reports_the_sorted_enabled_set() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store.clone());

    assert_eq!(pm.enabled_plugin_ids(), "", "no plugins: known empty set");

    assert!(pm.set_plugin_enabled("kimi-webbridge", true).unwrap());
    assert_eq!(pm.enabled_plugin_ids(), "kimi-webbridge");

    assert!(pm.set_plugin_enabled("superpowers", true).unwrap());
    assert_eq!(
        pm.enabled_plugin_ids(),
        "kimi-webbridge,superpowers",
        "ids are sorted, not insertion-ordered"
    );

    assert!(pm.set_plugin_enabled("kimi-webbridge", false).unwrap());
    assert_eq!(pm.enabled_plugin_ids(), "superpowers");
}

#[test]
fn test_plugin_manager_enable_disable_and_remove() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store.clone());

    assert!(pm.list_plugins().is_empty());

    // 1. A catalogued plugin can be enabled, and it is recorded with the
    //    catalog's version — not a fabricated literal.
    assert!(pm.set_plugin_enabled("kimi-webbridge", true).unwrap());
    let list1 = pm.list_plugins();
    assert_eq!(list1.len(), 1);
    assert_eq!(list1[0].id, "kimi-webbridge");
    assert_eq!(list1[0].name, "Kimi WebBridge");
    assert!(list1[0].enabled);
    assert_eq!(list1[0].version, "1.11.3");

    // 2. An id that is neither installed nor in the catalog is rejected and
    //    must NOT be recorded (it used to get a fake `version: "1.0.0"`).
    assert!(!pm.set_plugin_enabled("custom-plugin-x", true).unwrap());
    assert_eq!(pm.list_plugins().len(), 1);
    assert!(pm.list_plugins().iter().all(|p| p.id != "custom-plugin-x"));

    // 3. `install_plugin`: unknown → None; catalogued → recorded info.
    assert!(pm.install_plugin("no-such-plugin").unwrap().is_none());
    let installed = pm.install_plugin("superpowers").unwrap().unwrap();
    assert_eq!(installed.version, None);
    let ids: Vec<String> = pm.list_plugins().into_iter().map(|p| p.id).collect();
    assert!(ids.contains(&"superpowers".to_string()));

    // 4. Disable
    assert!(pm.set_plugin_enabled("kimi-webbridge", false).unwrap());
    let list2 = pm.list_plugins();
    let wb = list2.iter().find(|p| p.id == "kimi-webbridge").unwrap();
    assert!(!wb.enabled);

    // 5. Marketplace merge status
    let market = pm.list_marketplace();
    let wb_market = market.iter().find(|p| p.id == "kimi-webbridge").unwrap();
    assert_eq!(wb_market.installed.as_ref().map(|i| i.enabled), Some(false));

    // 6. Persistence across a fresh PluginManager instance on the same store
    let pm_fresh = PluginManager::new(store);
    assert_eq!(pm_fresh.list_plugins().len(), 2);

    // 7. Remove plugin
    assert!(pm_fresh.remove_plugin("kimi-webbridge").unwrap());
    let list3 = pm_fresh.list_plugins();
    assert_eq!(list3.len(), 1);
    assert_eq!(list3[0].id, "superpowers");

    // Remove non-existent plugin returns false
    assert!(!pm_fresh.remove_plugin("non-existent").unwrap());
}

#[test]
fn parse_plugin_command_reads_frontmatter_and_falls_back() {
    let path = Path::new("/plugins/demo/commands/review.md");

    let with_frontmatter = parse_plugin_command(
        "---\nname: code-review\ndescription: Review the diff\n---\n\nReview $ARGUMENTS\n",
        path,
        "demo",
        None,
    );
    assert_eq!(with_frontmatter.name, "code-review");
    assert_eq!(with_frontmatter.description, "Review the diff");
    assert_eq!(with_frontmatter.body, "Review $ARGUMENTS");
    assert_eq!(with_frontmatter.plugin_id, "demo");

    // No frontmatter: the file stem names it, the first body line describes it.
    let bare = parse_plugin_command("Do the thing\n\nmore\n", path, "demo", None);
    assert_eq!(bare.name, "review");
    assert_eq!(bare.description, "Do the thing");
    assert_eq!(bare.body, "Do the thing\n\nmore");

    // An explicit manifest name wins over the file stem.
    let named = parse_plugin_command("body\n", path, "demo", Some("explicit"));
    assert_eq!(named.name, "explicit");
    assert_eq!(named.description, "body");
}

#[test]
fn resolve_plugin_root_skips_remote_and_missing_sources() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    std::fs::create_dir_all(marketplace.join("official/demo")).unwrap();

    assert_eq!(
        resolve_plugin_root("./official/demo", Some(marketplace)),
        Some(marketplace.join("official/demo"))
    );
    assert_eq!(
        resolve_plugin_root("https://example.test/demo", Some(marketplace)),
        None
    );
    assert_eq!(
        resolve_plugin_root("./official/missing", Some(marketplace)),
        None
    );
    assert_eq!(resolve_plugin_root("./official/demo", None), None);
}

/// The panel renders an install date and the pre-resolution source, and the
/// engine sent neither. Both are facts only the install record knows, so
/// they are recorded once at install and preserved afterwards — a re-install
/// must not silently restate when the plugin arrived.
#[test]
fn an_install_records_when_and_from_where_and_keeps_both() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(root.join("kimi.plugin.json"), r#"{"name":"demo"}"#).unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));

    let installed = pm.install_plugin("demo").unwrap().expect("catalogued");
    let first_install = installed.installed_at.clone().expect("an install is dated");
    assert!(
        chrono::DateTime::parse_from_rfc3339(&first_install).is_ok(),
        "{first_install} is not RFC 3339"
    );
    assert_eq!(
        installed.original_source.as_deref(),
        Some("./official/demo"),
        "the catalog source, before it was resolved to a root"
    );

    // A second install is not a new arrival.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let again = pm
        .install_plugin("demo")
        .unwrap()
        .expect("still catalogued");
    assert_eq!(again.installed_at.as_deref(), Some(first_install.as_str()));
    assert_eq!(again.original_source.as_deref(), Some("./official/demo"));

    // And they reach the host, which is the point of recording them.
    let info = pm.plugin_info("demo").expect("info");
    assert_eq!(info.installed_at.as_deref(), Some(first_install.as_str()));
    assert_eq!(info.original_source.as_deref(), Some("./official/demo"));
}

/// A record written before these fields existed still loads, and reports
/// them as absent rather than inventing a date. The panel then omits the
/// lines, which is the truth: the moment was never written down.
#[test]
fn an_older_install_record_loads_without_the_new_fields() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(root.join("kimi.plugin.json"), r#"{"name":"demo"}"#).unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));
    // The shape a fork from before these fields wrote: enabled, version, root.
    let legacy = InstalledPluginInfo {
        enabled: true,
        version: Some("1.0.0".into()),
        root: None,
        installed_at: None,
        original_source: None,
        updated_at: None,
    };
    assert!(
        serde_json::from_value::<InstalledPluginInfo>(serde_json::json!({
            "enabled": true,
            "version": "1.0.0"
        }))
        .is_ok(),
        "a record without the new fields must still deserialize"
    );

    let mut installed = pm.load_installed_map();
    installed.insert("demo".to_string(), legacy);
    pm.save_installed_map(&installed).unwrap();
    let info = pm.plugin_info("demo").expect("info");
    assert_eq!(info.installed_at, None);
    assert_eq!(info.original_source, None);
    assert!(info.installed_at.is_none(), "no invented date");
}

/// v2's `resolveInstallSource` / `parseGithubUrl` (`app/plugin/source.ts:19-92`),
/// case for case. The `source` value is the whole basis of the host's
/// provenance label and trust badge, so a misclassification is a wrong badge
/// rather than a cosmetic difference.
#[test]
fn an_install_source_is_classified_the_way_v2_classifies_it() {
    let github = |source: &str| classify_plugin_source(source).github.expect("github");

    // A bare repository URL: no ref named, so v2's "no explicit pin"
    // spelling. v2 would have resolved the latest release tag over the
    // network here; `explicitGithubRef` drops a `HEAD` branch before
    // comparing, so every consumer reads it the same way.
    let bare = github("https://github.com/octocat/hello");
    assert_eq!(bare.owner, "octocat");
    assert_eq!(bare.repo, "hello");
    assert_eq!(bare.ref_.kind, PluginGithubRefKind::Branch);
    assert_eq!(bare.ref_.value, "HEAD");

    // `.git` is stripped from the repo, `www.` accepted from the host.
    let suffix = github("https://www.github.com/octocat/hello.git");
    assert_eq!(suffix.repo, "hello");

    // `tree/<ref>`: hex is a commit, anything else a branch.
    let branch = github("https://github.com/octocat/hello/tree/main");
    assert_eq!(branch.ref_.kind, PluginGithubRefKind::Branch);
    assert_eq!(branch.ref_.value, "main");
    let sha = github("https://github.com/octocat/hello/tree/a1b2c3d4e5f6");
    assert_eq!(sha.ref_.kind, PluginGithubRefKind::Sha);
    // 6 hex characters is not a SHA — too short to be one.
    assert_eq!(
        github("https://github.com/octocat/hello/tree/a1b2c3")
            .ref_
            .kind,
        PluginGithubRefKind::Branch
    );

    let tag = github("https://github.com/octocat/hello/releases/tag/v2.1.0");
    assert_eq!(tag.ref_.kind, PluginGithubRefKind::Tag);
    assert_eq!(tag.ref_.value, "v2.1.0");

    let commit = github("https://github.com/octocat/hello/commit/deadbeef");
    assert_eq!(commit.ref_.kind, PluginGithubRefKind::Sha);
    assert_eq!(commit.ref_.value, "deadbeef");

    // A ref with an escaped slash keeps it: v2 joins the decoded segments.
    let slashy = github("https://github.com/octocat/hello/tree/feature%2Fx");
    assert_eq!(slashy.ref_.value, "feature/x");

    // Not a github source: another host, a non-https scheme, a bare host, a
    // trailing path that names no ref, or a non-URL.
    for source in [
        "https://example.test/octocat/hello",
        "http://github.com/octocat/hello",
        "https://github.com/octocat",
        "https://github.com/octocat/hello/blob/main/README.md",
        "./official/demo",
        "/abs/path/to/plugin",
    ] {
        let resolved = classify_plugin_source(source);
        assert!(
            resolved.github.is_none(),
            "{source} must not read as a github source"
        );
    }
    assert_eq!(
        classify_plugin_source("https://example.test/p.zip").kind,
        PluginSourceKind::ZipUrl
    );
    assert_eq!(
        classify_plugin_source("./official/demo").kind,
        PluginSourceKind::LocalPath
    );
    // Surrounding whitespace is trimmed, as v2's `source.trim()` does.
    assert_eq!(
        classify_plugin_source("  https://example.test/p.zip  ").kind,
        PluginSourceKind::ZipUrl
    );
}

/// The wire has to carry the *vocabulary*, because every host-side reader
/// compares against it: the provenance label, the trust badge, the official
/// install check and the update notifier's identity key all branch on
/// `source === 'github' | 'zip-url'`. A raw URL matched none of them, so the
/// badge was always "third-party" and the update notifier never fired.
#[test]
fn the_wire_carries_the_source_vocabulary_and_the_github_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/remote");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"remote","tier":"community","displayName":"Remote","description":"From GitHub","source":"https://github.com/example/remote/tree/main"}]}"#,
        )
        .unwrap();
    std::fs::write(root.join("kimi.plugin.json"), r#"{"name":"remote"}"#).unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));
    pm.install_plugin("remote").unwrap().expect("catalogued");

    let info = pm.plugin_info("remote").expect("info");
    assert_eq!(info.source, PluginSourceKind::Github);
    let github = info.github.as_ref().expect("github metadata");
    assert_eq!(github.owner, "example");
    assert_eq!(github.repo, "remote");
    assert_eq!(github.ref_.kind, PluginGithubRefKind::Branch);
    assert_eq!(github.ref_.value, "main");
    // The pre-resolution spelling is kept, which is what the host pairs with
    // the vocabulary to render `github <owner>/<repo>@<ref>`.
    assert_eq!(
        info.original_source.as_deref(),
        Some("https://github.com/example/remote/tree/main")
    );

    // The list projection carries both too: the list view's label and badge
    // are computed from a `PluginSummary`, not from the detail view.
    let summary = pm
        .list_plugins()
        .into_iter()
        .find(|plugin| plugin.id == "remote")
        .expect("summary");
    assert_eq!(summary.source, PluginSourceKind::Github);
    assert!(summary.github.is_some());
    assert!(summary.original_source.is_some());

    let json = serde_json::to_value(&summary).unwrap();
    assert_eq!(json["source"], "github");
    assert_eq!(json["github"]["ref"]["kind"], "branch");
    assert_eq!(json["github"]["ref"]["value"], "main");
}

/// v2 stamps `updatedAt` on every install and keeps `installedAt` from the
/// first (`manager.ts:140-141`), so a re-install is what an update looks
/// like here — the panel only shows the line when the two differ.
#[test]
fn a_reinstall_keeps_the_install_time_and_advances_the_update_time() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(root.join("kimi.plugin.json"), r#"{"name":"demo"}"#).unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));

    let first = pm.install_plugin("demo").unwrap().expect("catalogued");
    let installed_at = first.installed_at.clone().expect("dated");
    let first_updated = first.updated_at.clone().expect("dated");

    std::thread::sleep(std::time::Duration::from_millis(5));
    let again = pm
        .install_plugin("demo")
        .unwrap()
        .expect("still catalogued");
    assert_eq!(again.installed_at.as_deref(), Some(installed_at.as_str()));
    assert_ne!(
        again.updated_at.as_deref(),
        Some(first_updated.as_str()),
        "a re-install advances updatedAt"
    );
    assert!(chrono::DateTime::parse_from_rfc3339(again.updated_at.as_ref().unwrap()).is_ok());
}

/// A declared root that does not exist must be reported as v2 reports it — a
/// `warn` for "not a directory" — never as an escape.
///
/// This regressed: the first containment check canonicalized the *target* and
/// fell back to the path as written when that failed, while the root was
/// canonicalized. On Windows those two disagree whenever the root is spelled
/// with an 8.3 short name (`C:\Users\ADMINI~1\...`), so every missing
/// directory came back as "resolves outside the plugin" — an error, on the
/// most ordinary manifest there is.
#[test]
fn a_missing_declared_root_is_a_warning_and_not_an_escape() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    std::fs::create_dir_all(&root).unwrap();
    // Neither the root's skills dir nor its agents dir exists.
    std::fs::write(
        root.join("kimi.plugin.json"),
        r#"{"name":"hollow","skills":"./skills/","agents":"./agents/"}"#,
    )
    .unwrap();

    let parsed = parse_plugin_manifest(&root);
    assert!(
        parsed.diagnostics.iter().all(|d| d.severity == "warn"),
        "no error expected, got {:?}",
        parsed.diagnostics
    );
    assert_eq!(parsed.diagnostics.len(), 2, "{:?}", parsed.diagnostics);
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.message.contains("not a directory")),
        "{:?}",
        parsed.diagnostics
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("outside the plugin")),
        "{:?}",
        parsed.diagnostics
    );
}

/// v2's state rule (`manager.ts:596-602`): an `error` diagnostic or an
/// unreadable manifest makes the record an error, a **warning does not**.
/// The fork used to answer `ok` for a plugin whose manifest the same read had
/// just rejected — and `hasErrors` contradicted it at the same time.
///
/// `state` is badge-only in the fork: nothing gates a plugin's contributions
/// on it, so this changes what the panel says and nothing else. That is also
/// why the "an error-state plugin contributes nothing" half of v2's rule is
/// *not* adopted here — see the ledger.
#[test]
fn the_state_follows_the_diagnostics_and_a_warning_does_not_flip_it() {
    let temp = tempfile::tempdir().unwrap();

    let install = |name: &str, manifest: &str, skills_dir: bool| {
        let marketplace = temp.path().join(name);
        let root = marketplace.join("official").join(name);
        std::fs::create_dir_all(&root).unwrap();
        if skills_dir {
            std::fs::create_dir_all(root.join("skills")).unwrap();
        }
        std::fs::write(
                marketplace.join("marketplace.json"),
                format!(
                    r#"{{"version":"1","plugins":[{{"id":"{name}","tier":"official","displayName":"{name}","description":"d","source":"./official/{name}"}}]}}"#
                ),
            )
            .unwrap();
        std::fs::write(root.join("kimi.plugin.json"), manifest).unwrap();
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));
        pm.install_plugin(name).unwrap().expect("catalogued");
        let info = pm.plugin_info(name).expect("info");
        (pm, info)
    };

    // Clean: ok, no diagnostics.
    let (_, clean) = install("clean", r#"{"name":"clean","skills":"./skills/"}"#, true);
    assert_eq!(clean.state, "ok");
    assert!(!clean.has_errors);
    assert!(clean.diagnostics.is_empty());

    // A warning — here a declared skill root that is not a directory — leaves
    // the state alone, exactly as v2 does.
    let (_, warned) = install("warned", r#"{"name":"warned","skills":"./skills/"}"#, false);
    assert_eq!(
        warned.state, "ok",
        "a warning is not an error: {:?}",
        warned.diagnostics
    );
    assert!(!warned.has_errors);
    assert!(
        warned
            .diagnostics
            .iter()
            .any(|d| d.severity == "warn" && d.message.contains("not a directory")),
        "{:?}",
        warned.diagnostics
    );

    // An error — a path that tries to leave the plugin — flips it. The skill
    // root was already dropped when the path was resolved, so the badge now
    // matches what the plugin actually does.
    let (_, escaping) = install(
        "escaping",
        r#"{"name":"escaping","skills":"../elsewhere/"}"#,
        true,
    );
    assert_eq!(escaping.state, "error");
    assert!(escaping.has_errors);
    assert!(
        escaping.diagnostics.iter().any(|d| d.severity == "error"),
        "{:?}",
        escaping.diagnostics
    );

    // No manifest at all is an error too — and used to report `ok`.
    let (pm, _) = install("manifestless", r#"{"name":"manifestless"}"#, true);
    std::fs::remove_file(
        temp.path()
            .join("manifestless")
            .join("official")
            .join("manifestless")
            .join("kimi.plugin.json"),
    )
    .unwrap();
    let info = pm.plugin_info("manifestless").expect("info");
    assert_eq!(info.state, "error");
    assert!(info.has_errors);
    assert!(
        info.diagnostics
            .iter()
            .any(|d| d.severity == "error" && d.message.contains("No manifest at")),
        "{:?}",
        info.diagnostics
    );
}

#[test]
fn plugin_commands_and_info_read_the_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    std::fs::create_dir_all(root.join("commands")).unwrap();
    // Declared below, so it has to exist for the skill count to be 1.
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(
        root.join("kimi.plugin.json"),
        r#"{
              "name": "demo",
              "version": "1.0.0",
              "skills": "./skills/",
              "mcpServers": { "data": { "command": "node", "args": ["server.mjs"] } },
              "commands": [{ "path": "./commands/review.md" }]
            }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("commands/review.md"),
        "---\ndescription: Review the diff\n---\n\nReview it\n",
    )
    .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));

    // Nothing is installed yet, so nothing is contributed.
    assert!(pm.enabled_commands().is_empty());

    pm.install_plugin("demo").unwrap().expect("catalogued");
    let commands = pm.enabled_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].plugin_id, "demo");
    assert_eq!(commands[0].name, "review");
    assert_eq!(commands[0].description, "Review the diff");
    assert_eq!(commands[0].body, "Review it");

    let info = pm.plugin_info("demo").expect("installed");
    assert_eq!(info.command_count, 1);
    assert_eq!(info.mcp_server_count, 1);
    assert_eq!(info.skill_count, 1);
    assert_eq!(info.state, "ok");
    assert_eq!(info.mcp_servers[0].name, "data");
    assert_eq!(info.mcp_servers[0].transport, "stdio");
    assert!(info.mcp_servers[0].enabled);

    // Disabling one MCP server is recorded and reflected; an unknown name
    // is refused rather than silently recorded.
    assert!(pm.set_mcp_server_enabled("demo", "data", false).unwrap());
    let info = pm.plugin_info("demo").expect("installed");
    assert!(!info.mcp_servers[0].enabled);
    assert!(!pm.set_mcp_server_enabled("demo", "nope", false).unwrap());

    // Disabling the plugin drops its commands.
    pm.set_plugin_enabled("demo", false).unwrap();
    assert!(pm.enabled_commands().is_empty());

    // Reload reports the diff against the snapshot taken at construction.
    let summary = pm.reload();
    assert_eq!(summary.added, vec!["demo".to_string()]);
    assert!(summary.removed.is_empty());
    assert!(summary.errors.is_empty());

    // A second reload has nothing new to report.
    let summary = pm.reload();
    assert!(summary.added.is_empty());
    assert!(summary.removed.is_empty());
    assert!(summary.errors.is_empty());
}

#[test]
fn enabled_plugins_contribute_skill_dirs_and_mcp_configs() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    // The declared skill root has to exist: v2 drops a path that is not a
    // directory (with a `warn`), where the fork used to count the entry and
    // let the later scan find nothing.
    std::fs::create_dir_all(root.join("skills/review")).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(
            root.join("kimi.plugin.json"),
            r#"{
              "name": "demo",
              "skills": "./skills/",
              "skillInstructions": "  Always answer in the house voice.  ",
              "mcpServers": {
                "data": { "command": "node", "args": ["server.mjs"], "env": { "TOKEN": "x" }, "cwd": "./" },
                "remote": { "url": "https://example.test/mcp" }
              }
            }"#,
        )
        .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_marketplace_dir(Some(marketplace.to_path_buf()));

    // Nothing installed: nothing contributed.
    assert!(pm.plugin_skill_dirs().is_empty());
    assert!(pm.plugin_mcp_configs().is_empty());

    pm.install_plugin("demo").unwrap().expect("catalogued");
    let skill_dirs = pm.plugin_skill_dirs();
    assert_eq!(skill_dirs.len(), 1);
    // Resolved, so the answer is the real path rather than the manifest's
    // `./skills/` joined onto however the root happened to be spelled.
    assert_eq!(
        skill_dirs[0].dir,
        without_verbatim_prefix(&std::fs::canonicalize(root.join("skills")).unwrap())
    );
    assert_eq!(skill_dirs[0].plugin_id, "demo");
    // Trimmed on the way in: the renderer wraps the text in
    // `<plugin-instructions>` and an indented body would only widen the gap.
    assert_eq!(
        skill_dirs[0].instructions.as_deref(),
        Some("Always answer in the house voice.")
    );

    let configs = pm.plugin_mcp_configs();
    assert_eq!(configs.len(), 2);
    let data = configs
        .iter()
        .find(|config| config.name == "demo__data")
        .expect("data");
    assert_eq!(data.transport, "stdio");
    assert_eq!(data.command.as_deref(), Some("node"));
    assert_eq!(data.args, vec!["server.mjs"]);
    assert_eq!(data.env.get("TOKEN").map(String::as_str), Some("x"));
    // A manifest `cwd` is anchored to the plugin root, not the process cwd.
    let root_posix = root.to_string_lossy().replace('\\', "/");
    assert!(
        data.cwd
            .as_deref()
            .is_some_and(|cwd| cwd.starts_with(&root_posix)),
        "cwd {:?} must sit under {root_posix}",
        data.cwd
    );
    let remote = configs
        .iter()
        .find(|config| config.name == "demo__remote")
        .expect("remote");
    assert_eq!(remote.transport, "http");
    assert_eq!(remote.url.as_deref(), Some("https://example.test/mcp"));

    // Disabling one server drops it from the contributed set.
    assert!(pm.set_mcp_server_enabled("demo", "data", false).unwrap());
    let configs = pm.plugin_mcp_configs();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].name, "demo__remote");

    // Disabling the plugin drops everything.
    pm.set_plugin_enabled("demo", false).unwrap();
    assert!(pm.plugin_skill_dirs().is_empty());
    assert!(pm.plugin_mcp_configs().is_empty());
}

/// v2's two manifest locations, root first (`app/plugin/manifest.ts:16-17`,
/// `:55-57`): the dir form answers when the root form is absent, and when
/// both exist the root one wins with the other reported as shadowed. The
/// fork read only `kimi.plugin.json`, so a plugin on the dir form
/// contributed nothing at all.
#[test]
fn a_manifest_is_read_from_either_location_and_a_shadowed_one_is_reported() {
    let temp = tempfile::tempdir().unwrap();

    let dir_form = temp.path().join("dir-form");
    std::fs::create_dir_all(dir_form.join(".kimi-plugin")).unwrap();
    std::fs::write(
        dir_form.join(".kimi-plugin").join("plugin.json"),
        r#"{"name":"dirform","skills":"./skills/"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir_form.join("skills")).unwrap();
    let parsed = parse_plugin_manifest(&dir_form);
    assert_eq!(parsed.manifest_kind.as_deref(), Some("kimi-plugin-dir"));
    assert_eq!(
        parsed.manifest_path.as_deref(),
        Some(
            dir_form
                .join(".kimi-plugin")
                .join("plugin.json")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(parsed.shadowed_manifest_path.is_none());
    assert_eq!(
        parsed
            .manifest
            .as_ref()
            .and_then(|m| m.name.clone())
            .as_deref(),
        Some("dirform")
    );

    let both = temp.path().join("both");
    std::fs::create_dir_all(both.join(".kimi-plugin")).unwrap();
    std::fs::write(
        both.join("kimi.plugin.json"),
        r#"{"name":"winner","skills":"./skills/"}"#,
    )
    .unwrap();
    std::fs::write(
        both.join(".kimi-plugin").join("plugin.json"),
        r#"{"name":"loser"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(both.join("skills")).unwrap();
    let parsed = parse_plugin_manifest(&both);
    assert_eq!(parsed.manifest_kind.as_deref(), Some("kimi-plugin-root"));
    assert_eq!(
        parsed
            .manifest
            .as_ref()
            .and_then(|m| m.name.clone())
            .as_deref(),
        Some("winner"),
        "the root manifest wins"
    );
    // Reported rather than silently ignored: an author editing the file that
    // has no effect otherwise gets no signal at all.
    assert_eq!(
        parsed.shadowed_manifest_path.as_deref(),
        Some(
            both.join(".kimi-plugin")
                .join("plugin.json")
                .to_string_lossy()
                .as_ref()
        )
    );

    let none = temp.path().join("none");
    std::fs::create_dir_all(&none).unwrap();
    let parsed = parse_plugin_manifest(&none);
    assert!(parsed.manifest.is_none());
    assert!(parsed.manifest_path.is_none());
    assert_eq!(parsed.diagnostics.len(), 1);
    assert_eq!(parsed.diagnostics[0].severity, "error");
    assert!(
        parsed.diagnostics[0].message.contains("No manifest at"),
        "{}",
        parsed.diagnostics[0].message
    );
}

/// A manifest is third-party content, so a declared path may not lead out of
/// the plugin (v2 `resolveDirListField`'s containment check,
/// `app/plugin/manifest.ts:194-201`). The fork resolved `skills` with
/// `root.join(rel.trim_start_matches("./"))` and additionally accepted an
/// absolute path outright, so a plugin could point the scan at any directory
/// on the machine. Every drop is reported, because a missing skill root is
/// otherwise indistinguishable from a plugin that never declared one.
#[test]
fn a_manifest_path_may_not_leave_the_plugin_and_every_drop_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    let outside = temp.path().join("outside-skills");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(
        root.join("kimi.plugin.json"),
        r#"{
              "name": "escaper",
              "skills": ["./skills/", "./../outside-skills", "/etc", "skills-relative"]
            }"#,
    )
    .unwrap();

    let parsed = parse_plugin_manifest(&root);
    let skills = parsed
        .manifest
        .as_ref()
        .and_then(|m| m.skills.clone())
        .expect("skills");
    assert_eq!(
        skills.len(),
        1,
        "only the in-plugin root survives: {skills:?}"
    );
    let root_real = std::fs::canonicalize(&root).unwrap();
    assert_eq!(
        PathBuf::from(&skills[0]),
        without_verbatim_prefix(&std::fs::canonicalize(root_real.join("skills")).unwrap())
    );

    let messages: Vec<&str> = parsed
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    // `./../outside-skills` passes the `./` rule and is caught by the
    // containment check — the case that actually matters, since it is the
    // one a prefix test would wave through.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.contains("resolves outside the plugin"))
            .count(),
        1,
        "{messages:?}"
    );
    // The other two are refused by the prefix rule before containment is
    // reached; the absolute one used to be accepted outright by the fork.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.contains("must start with \"./\""))
            .count(),
        2,
        "{messages:?}"
    );
}

/// A sibling directory whose name merely starts with the plugin's is not
/// inside it (`isWithin` is component-wise, `manifest.ts:552-555`), so
/// `../plugin-evil` cannot pass as `plugin`.
#[test]
fn containment_is_component_wise_not_a_prefix_match() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    let sibling = temp.path().join("plugin-evil");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("kimi.plugin.json"),
        r#"{"name":"c","skills":"../plugin-evil/"}"#,
    )
    .unwrap();

    let parsed = parse_plugin_manifest(&root);
    assert!(
        parsed
            .manifest
            .as_ref()
            .and_then(|m| m.skills.as_ref())
            .is_none_or(Vec::is_empty),
        "a sibling sharing the name prefix is outside: {:?}",
        parsed.manifest.as_ref().and_then(|m| m.skills.clone())
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|d| d.message.contains("must start with")),
        "{:?}",
        parsed.diagnostics
    );
}

/// The panel's manifest-derived lines read these fields, and the engine sent
/// none of them: `keywords` and `sessionStart` were not even parsed, and
/// `skills` reached the host as whatever shape the JSON had — a single
/// string, which the panel then iterated character by character.
#[test]
fn the_manifest_reaches_the_host_in_the_shape_the_panel_reads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(
        root.join("kimi.plugin.json"),
        r#"{
              "name": "brandpack",
              "version": "1.2.0",
              "description": "House style",
              "keywords": ["brand", "voice"],
              "author": "Moonshot AI",
              "license": "MIT",
              "homepage": "https://example.test/brandpack",
              "skills": "./skills/",
              "sessionStart": { "skill": "brandpack.onboard" },
              "skillInstructions": "Answer in the house voice.",
              "interface": {
                "displayName": "Brand Pack",
                "shortDescription": "Keeps the house voice.",
                "developerName": "Example",
                "websiteURL": "https://example.test"
              }
            }"#,
    )
    .unwrap();

    let parsed = parse_plugin_manifest(&root);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let manifest = parsed.manifest.expect("manifest");

    // A bare string author is v2's `{ name }` shorthand (`readAuthor`), which
    // is the form the bundled plugins use.
    assert_eq!(
        manifest.author.as_ref().and_then(|a| a.name.as_deref()),
        Some("Moonshot AI")
    );
    assert_eq!(
        manifest.keywords.as_deref(),
        Some(["brand".to_string(), "voice".to_string()].as_slice())
    );
    assert_eq!(
        manifest.session_start.as_ref().map(|s| s.skill.as_str()),
        Some("brandpack.onboard")
    );
    assert_eq!(manifest.license.as_deref(), Some("MIT"));
    assert_eq!(
        manifest.homepage.as_deref(),
        Some("https://example.test/brandpack")
    );
    assert_eq!(
        manifest
            .interface
            .as_ref()
            .and_then(|i| i.short_description.as_deref()),
        Some("Keeps the house voice.")
    );
    // A list, always: the panel iterates this, and a bare string would have
    // produced one line per character.
    let skills = manifest.skills.as_deref().expect("skills list");
    assert_eq!(skills.len(), 1);
    assert!(skills[0].ends_with("skills"), "{}", skills[0]);

    // And the serialized wire is what the host parses.
    let json = serde_json::to_value(&manifest).unwrap();
    assert_eq!(json["sessionStart"]["skill"], "brandpack.onboard");
    assert_eq!(json["skillInstructions"], "Answer in the house voice.");
    assert!(json["skills"].is_array());
    assert_eq!(json["interface"]["developerName"], "Example");
}

#[test]
fn a_node_runner_rewrites_node_plugin_servers_onto_the_host_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path();
    let root = marketplace.join("official/demo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","description":"A demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(
            root.join("kimi.plugin.json"),
            r#"{
              "name": "demo",
              "mcpServers": {
                "data": { "command": "node", "args": ["./bin/server.mjs", "--port", "7"], "env": { "TOKEN": "x" } },
                "native": { "command": "./bin/daemon", "args": ["serve"] },
                "remote": { "url": "https://example.test/mcp" }
              }
            }"#,
        )
        .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store)
        .with_marketplace_dir(Some(marketplace.to_path_buf()))
        .with_node_runner(Some(temp.path().join("kimi")));
    pm.install_plugin("demo").unwrap().expect("catalogued");

    let configs = pm.plugin_mcp_configs();
    let data = configs
        .iter()
        .find(|config| config.name == "demo__data")
        .expect("data");
    // `node` + its entry arg are folded into the host's hidden sub-command;
    // the rest of the argument vector and the env pass through unchanged,
    // and the plugin root rides KIMI_PLUGIN_ROOT.
    assert_eq!(
        data.command.as_deref(),
        Some(temp.path().join("kimi").to_string_lossy().as_ref())
    );
    assert_eq!(
        data.args,
        vec!["__plugin_run_node", "./bin/server.mjs", "--port", "7"]
    );
    assert_eq!(
        data.env.get("KIMI_PLUGIN_ROOT").map(String::as_str),
        Some(root.to_string_lossy().as_ref())
    );
    assert_eq!(data.env.get("TOKEN").map(String::as_str), Some("x"));
    // A non-node command is left un-rewritten (its `./` path still
    // resolves against the plugin root, as always).
    let native = configs
        .iter()
        .find(|config| config.name == "demo__native")
        .expect("native");
    assert_eq!(
        native.command.as_deref(),
        Some(resolve_dir_against(&root, "./bin/daemon").as_str())
    );
    assert_eq!(native.args, vec!["serve"]);
    assert!(!native.env.contains_key("KIMI_PLUGIN_ROOT"));
}

#[test]
fn a_remote_install_lands_in_the_managed_root() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let marketplace = temp.path().join("marketplace");
    std::fs::create_dir_all(&marketplace).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"curated","displayName":"Demo","description":"A demo","source":"https://github.com/example/demo"}]}"#,
        )
        .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store)
        .with_marketplace_dir(Some(marketplace))
        .with_home_dir(Some(home.clone()));

    // The managed root is where a remote install would land, and it wins
    // over the catalog source once it holds a manifest.
    let managed = pm.managed_root("demo").expect("home is set");
    assert_eq!(managed, home.join("plugins").join("demo"));
    assert!(pm.plugin_root("demo").is_none());

    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("kimi.plugin.json"), r#"{"name":"demo"}"#).unwrap();
    assert_eq!(pm.plugin_root("demo"), Some(managed.clone()));

    // A local path that is not catalogued installs under its manifest name,
    // never the path string, and stays resolvable to the directory the user
    // pointed at.
    let local = temp.path().join("local-plugin");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("kimi.plugin.json"),
        r#"{"name":"local","version":"2.1.0"}"#,
    )
    .unwrap();
    let installed = pm
        .install_plugin_from(local.to_str().unwrap())
        .unwrap()
        .expect("a local manifest installs");
    assert_eq!(
        installed.0, "local",
        "keyed by the manifest name, not the path"
    );
    assert_eq!(installed.1.version.as_deref(), Some("2.1.0"));
    assert_eq!(pm.plugin_root("local"), Some(local.clone()));
    assert!(pm.install_plugin_from("no-such-plugin").unwrap().is_none());
}

/// A catalogued plugin whose `source` is a URL is remote even when it is
/// installed by its catalog id: the install has to fetch the archive, not
/// just record a row. The loopback source is refused by the SSRF guard, so
/// the refusal proves the download path was entered.
#[test]
fn a_catalogued_url_source_downloads_when_installed_by_id() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path().join("marketplace");
    std::fs::create_dir_all(&marketplace).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"plugins":[{"id":"remote-demo","displayName":"Remote Demo","source":"http://127.0.0.1:9/plugin.zip"}]}"#,
        )
        .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store)
        .with_marketplace_dir(Some(marketplace))
        .with_home_dir(Some(temp.path().join("home")));

    let error = pm
        .install_plugin_from("remote-demo")
        .expect_err("a refused download is not a successful install");
    assert!(error.contains("private"), "unexpected error: {error}");
    assert!(
        pm.list_plugins().is_empty(),
        "a refused download must not record an install"
    );
}

#[test]
fn an_uncatalogued_local_path_never_installs_as_a_path_shaped_id() {
    let temp = tempfile::tempdir().unwrap();
    let marketplace = temp.path().join("marketplace");
    std::fs::create_dir_all(&marketplace).unwrap();
    std::fs::write(marketplace.join("marketplace.json"), r#"{"plugins":[]}"#).unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store)
        .with_marketplace_dir(Some(marketplace))
        .with_home_dir(Some(temp.path().join("home")));

    let local = temp.path().join("standalone");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("kimi.plugin.json"), r#"{"name":"standalone"}"#).unwrap();

    let (id, _) = pm
        .install_plugin_from(local.to_str().unwrap())
        .unwrap()
        .expect("the manifest identifies it");
    assert_eq!(id, "standalone");
    // The id is a real key: every later call resolves through it.
    let ids: Vec<String> = pm.list_plugins().into_iter().map(|p| p.id).collect();
    assert_eq!(ids, vec!["standalone".to_string()]);
    assert_eq!(pm.plugin_root(&id), Some(local));

    // A directory with no manifest is not installable, so a bogus path can
    // never record an install.
    let bare = temp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert!(
        pm.install_plugin_from(bare.to_str().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn removing_a_plugin_also_drops_its_managed_copy() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let marketplace = temp.path().join("marketplace");
    std::fs::create_dir_all(marketplace.join("official/demo")).unwrap();
    std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","tier":"official","displayName":"Demo","source":"./official/demo"}]}"#,
        )
        .unwrap();
    std::fs::write(
        marketplace.join("official/demo/kimi.plugin.json"),
        r#"{"name":"demo"}"#,
    )
    .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store)
        .with_marketplace_dir(Some(marketplace))
        .with_home_dir(Some(home.clone()));

    // A remote install is the case that lands a copy under `<home>/plugins`.
    let managed = home.join("plugins").join("demo");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("kimi.plugin.json"), r#"{"name":"demo"}"#).unwrap();
    pm.install_plugin_from("demo")
        .unwrap()
        .expect("the catalog identifies it");
    assert_eq!(pm.plugin_root("demo"), Some(managed.clone()));

    assert!(pm.remove_plugin("demo").unwrap());
    assert!(
        !managed.exists(),
        "the managed copy is gone, so a removed plugin cannot keep contributing"
    );
    // The catalog still declares it, but nothing is installed.
    assert!(pm.list_plugins().is_empty());
}

#[test]
fn managed_roots_refuse_ids_that_are_not_a_single_path_segment() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let pm = PluginManager::new(store).with_home_dir(Some(temp.path().to_path_buf()));

    assert!(pm.managed_root("demo").is_some());
    for id in ["..", ".", "", "a/b", "a\\b", "C:\\Windows", "../escape"] {
        assert!(
            pm.managed_root(id).is_none(),
            "{id:?} must not resolve to a managed directory"
        );
    }
}
