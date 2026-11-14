use super::*;
use crate::tools::tower::types::{TowerAgentKind, TowerCompleteResult, TowerInboxSent};

fn completion_input(mission: &TowerMission, report: &str) -> TowerSendInput {
    TowerSendInput {
        to: TOWER_NAME.into(),
        subject: match mission.kind {
            TowerMissionKind::Build => "review-request".into(),
            TowerMissionKind::Survey => "survey-summary".into(),
            TowerMissionKind::Artifact => "acceptance-request".into(),
        },
        body: report.trim().into(),
        scope: Some(mission.id.clone()),
        action: Some("complete".into()),
        consent_ref: None,
    }
}

fn recovered_completion_input(report: &str, key_artifacts: &[String]) -> TowerSendInput {
    let body = if key_artifacts.is_empty() {
        report.trim().to_string()
    } else {
        format!(
            "Key artifacts:\n{}\n\n{}",
            key_artifacts
                .iter()
                .map(|path| format!("- {path}"))
                .collect::<Vec<_>>()
                .join("\n"),
            report.trim()
        )
    };
    TowerSendInput {
        to: TOWER_NAME.into(),
        subject: "completion-report-state-lost".into(),
        body,
        scope: None,
        action: Some("complete".into()),
        consent_ref: None,
    }
}

fn find_message(
    items: &[TowerInboxItem],
    caller_name: &str,
    input: &TowerSendInput,
) -> Option<TowerInboxItem> {
    items
        .iter()
        .find(|item| {
            item.from == caller_name
                && item.to == input.to.trim()
                && item.subject == input.subject
                && item.scope == input.scope
                && item.action == input.action
                && item.body == input.body.trim()
        })
        .cloned()
}

fn sent_from_existing(existing: TowerInboxItem, mission_id: Option<String>) -> TowerInboxSent {
    let delivery = crate::tools::tower::types::TowerInboxDelivery {
        from: existing.from.clone(),
        to: existing.to.clone(),
        subject: existing.subject.clone(),
        message_id: existing.message_id.clone(),
        file: existing.file.clone(),
        sent_at: existing.sent_at.clone(),
        mission_id: mission_id.clone(),
    };
    TowerInboxSent {
        item: existing,
        delivery,
        mission_id,
        created: false,
        activity_log_error: None,
    }
}

fn empty_result(
    mission: Option<TowerMission>,
    recovered_at: Option<String>,
) -> TowerCompleteResult {
    TowerCompleteResult {
        mission,
        recovered_at,
        message: None,
        notification_created: false,
        notification_error: None,
        activity_log_error: None,
    }
}

impl TowerStore {
    pub async fn complete(
        &self,
        caller_name: &str,
        report: &str,
        key_artifacts: &[String],
    ) -> Result<TowerCompleteResult, String> {
        if report.trim().is_empty() {
            return Err("TowerComplete report must not be empty".into());
        }
        let key_artifacts = normalize_artifact_paths(key_artifacts)?;
        self.complete_resolved(caller_name, report.trim(), &key_artifacts, None)
            .await
    }

    pub async fn complete_agent(
        &self,
        caller_agent_id: &str,
        report: &str,
        key_artifacts: &[String],
    ) -> Result<TowerCompleteResult, String> {
        if report.trim().is_empty() {
            return Err("TowerComplete report must not be empty".into());
        }
        let key_artifacts = normalize_artifact_paths(key_artifacts)?;
        let (_, caller, recovered_at, _) = self.resolve_agent_operation(caller_agent_id).await?;
        self.complete_resolved(&caller, report.trim(), &key_artifacts, recovered_at)
            .await
    }

    async fn complete_resolved(
        &self,
        caller_name: &str,
        report: &str,
        key_artifacts: &[String],
        recovered_at: Option<String>,
    ) -> Result<TowerCompleteResult, String> {
        let prepare_store = self.clone();
        let prepare_caller = caller_name.to_string();
        let prepare_report = report.to_string();
        let prepare_artifacts = key_artifacts.to_vec();
        let prepare_recovered = recovered_at.clone();
        let commit_store = self.clone();
        let commit_caller = caller_name.to_string();
        let commit_report = report.to_string();
        let commit_artifacts = key_artifacts.to_vec();
        self.run_prepared(
            move || {
                let store = prepare_store.clone();
                let caller = prepare_caller.clone();
                let report = prepare_report.clone();
                let key_artifacts = prepare_artifacts.clone();
                let recovered_at = prepare_recovered.clone();
                async move {
                    let snapshot = store.load_snapshot().await?;
                    let inbox_files = store.protocol_file_names(INBOX_DIR, "").await?;
                    let inbox_items = store.read_inbox(TOWER_NAME, usize::MAX).await?;
                    let raw: serde_json::Value = serde_json::from_str(&snapshot.raw)
                        .map_err(|error| format!("corrupted tower state: {error}"))?;
                    store
                        .complete_from_state(
                            &caller,
                            &report,
                            &key_artifacts,
                            recovered_at,
                            snapshot.state.clone(),
                            MissionUpdateOptions {
                                validate_only: true,
                                prepared_inbox: Some((raw.clone(), inbox_items.clone())),
                                ..Default::default()
                            },
                        )
                        .await?;
                    Ok((snapshot, inbox_files, raw, inbox_items))
                }
            },
            move |(snapshot, inbox_files, raw, inbox_items)| {
                let store = commit_store.clone();
                let caller = commit_caller.clone();
                let report = commit_report.clone();
                let key_artifacts = commit_artifacts.clone();
                let recovered_at = recovered_at.clone();
                async move {
                    let lock_store = store.clone();
                    lock_store
                        .with_completion_lock(move || async move {
                            let state = store
                                .assert_preparation_current(
                                    &snapshot,
                                    Some(inbox_files.as_slice()),
                                    None,
                                )
                                .await?;
                            store
                                .complete_from_state(
                                    &caller,
                                    &report,
                                    &key_artifacts,
                                    recovered_at,
                                    state,
                                    MissionUpdateOptions {
                                        completion_prepared: true,
                                        prepared_inbox: Some((raw, inbox_items)),
                                        ..Default::default()
                                    },
                                )
                                .await
                        })
                        .await
                }
            },
        )
        .await
    }

    async fn complete_from_state(
        &self,
        caller_name: &str,
        report: &str,
        key_artifacts: &[String],
        recovered_at: Option<String>,
        state: TowerState,
        options: MissionUpdateOptions,
    ) -> Result<TowerCompleteResult, String> {
        let caller = self.find_agent(&state, caller_name);
        let mission = caller
            .as_ref()
            .and_then(|agent| agent.mission_id.as_deref())
            .and_then(|id| state.missions.iter().find(|mission| mission.id == id))
            .cloned();
        let inbox_items = match options.prepared_inbox.as_ref() {
            Some((_, items)) => items.clone(),
            None => self.read_inbox(TOWER_NAME, usize::MAX).await?,
        };

        let Some(mission) = mission else {
            let worker = caller
                .as_ref()
                .is_some_and(|agent| agent.kind == TowerAgentKind::Worker);
            let mission_id = caller.as_ref().and_then(|agent| agent.mission_id.clone());
            if !(worker && recovered_at.is_some()) {
                if worker && let Some(mission_id) = mission_id {
                    return Err(format!("unknown mission \"{mission_id}\""));
                }
                return Err(format!(
                    "agent \"{caller_name}\" cannot use TowerComplete — only the worker who owns a mission can complete it"
                ));
            }
            let input = recovered_completion_input(report, key_artifacts);
            if let Some(existing) = find_message(&inbox_items, caller_name, &input) {
                let mut result = empty_result(None, recovered_at);
                result.message = Some(sent_from_existing(existing, None));
                return Ok(result);
            }
            if options.validate_only {
                return Ok(empty_result(None, recovered_at));
            }
            let sent = self.send_detailed_locked(caller_name, input, true).await;
            return Ok(match sent {
                Ok(sent) => TowerCompleteResult {
                    mission: None,
                    recovered_at,
                    notification_created: sent.created,
                    activity_log_error: sent.activity_log_error.clone(),
                    message: Some(sent),
                    notification_error: None,
                },
                Err(error) => TowerCompleteResult {
                    mission: None,
                    recovered_at,
                    message: None,
                    notification_created: false,
                    notification_error: Some(error),
                    activity_log_error: None,
                },
            });
        };

        if mission.kind != TowerMissionKind::Artifact && !key_artifacts.is_empty() {
            return Err(
                "TowerComplete key_artifacts is only supported for artifact missions".into(),
            );
        }
        if mission.kind == TowerMissionKind::Artifact && key_artifacts.is_empty() {
            return Err(format!(
                "TowerComplete key_artifacts for artifact mission {} must name at least one committed key deliverable",
                mission.id
            ));
        }

        let mut completion_tip: Option<String> = None;
        let mut completion_changed: Vec<String> = Vec::new();
        if mission.kind == TowerMissionKind::Artifact {
            if !branch_exists(&self.repo_root, &mission.branch).await? {
                if options.completion_prepared {
                    return Err(format!(
                        "artifact mission {} cannot complete — its branch \"{}\" does not exist, so there are no committed deliverables to accept",
                        mission.id, mission.branch
                    ));
                }
            } else {
                let tip = branch_tip(&self.repo_root, &mission.branch).await?;
                let base = self.diff_base(&state.base, &mission).await?;
                completion_changed =
                    diff_name_only(&self.repo_root, &base, &mission.branch).await?;
                if completion_changed.is_empty() {
                    if options.completion_prepared {
                        return Err(format!(
                            "artifact mission {} cannot complete — branch \"{}\" has no changes vs \"{}\"; commit the deliverables there first",
                            mission.id, mission.branch, base
                        ));
                    }
                } else {
                    let changed: HashSet<&str> =
                        completion_changed.iter().map(String::as_str).collect();
                    let uncommitted: Vec<String> = key_artifacts
                        .iter()
                        .filter(|path| !changed.contains(path.as_str()))
                        .cloned()
                        .collect();
                    if !uncommitted.is_empty() {
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must name paths from the committed branch diff; not found: {}",
                            mission.id,
                            uncommitted.join(", ")
                        ));
                    }
                    let out_of_scope: Vec<String> = key_artifacts
                        .iter()
                        .filter(|path| {
                            !mission.scope.iter().any(|pattern| {
                                if let Ok(glob) = globset::Glob::new(pattern) {
                                    glob.compile_matcher().is_match(path)
                                } else {
                                    path.starts_with(pattern.trim_end_matches('*'))
                                }
                            })
                        })
                        .cloned()
                        .collect();
                    if !out_of_scope.is_empty() {
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must stay inside its scope ({}): {}",
                            mission.id,
                            mission.scope.join(", "),
                            out_of_scope.join(", ")
                        ));
                    }
                    let mut missing_at_tip: Vec<String> = Vec::new();
                    for path in key_artifacts {
                        let entry_type = git(
                            &self.repo_root,
                            &["cat-file", "-t", &format!("{tip}:{path}")],
                        )
                        .await
                        .unwrap_or_default();
                        if entry_type.trim() != "blob" {
                            missing_at_tip.push(path.clone());
                        }
                    }
                    if !missing_at_tip.is_empty() {
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must name files present at completion tip {}; missing or not files: {}",
                            mission.id,
                            &tip[..tip.len().min(7)],
                            missing_at_tip.join(", ")
                        ));
                    }
                }
                completion_tip = Some(tip);
            }
        }

        let input = completion_input(&mission, report);
        let completion_report = match completion_tip.as_deref() {
            Some(tip) => {
                artifact_acceptance_report(report, key_artifacts, &completion_changed, tip)
            }
            None => report.to_string(),
        };
        let input = TowerSendInput {
            body: completion_report.clone(),
            ..input
        };
        let same_completion = mission.kind != TowerMissionKind::Artifact
            || (completion_tip.as_deref() == mission.completion_tip.as_deref()
                && same_artifact_paths(mission.key_artifacts.as_ref(), key_artifacts));
        if mission.status == TowerMissionStatus::Completed
            && same_completion
            && let Some(existing) = find_message(&inbox_items, caller_name, &input)
        {
            let mut result = empty_result(Some(mission.clone()), recovered_at);
            result.message = Some(sent_from_existing(existing, Some(mission.id)));
            return Ok(result);
        }

        let validate_only = options.validate_only;
        let mission = self
            .update_mission_in_state(
                caller_name,
                &mission.id,
                TowerMissionPatch {
                    status: Some(TowerMissionStatus::Completed),
                    ..Default::default()
                },
                state,
                MissionUpdateOptions {
                    completion_tip,
                    completion_key_artifacts: Some(key_artifacts.to_vec()),
                    ..options
                },
            )
            .await?;
        if validate_only {
            return Ok(empty_result(Some(mission), recovered_at));
        }
        match self.send_detailed_locked(caller_name, input, true).await {
            Ok(sent) => Ok(TowerCompleteResult {
                mission: Some(mission),
                recovered_at,
                notification_created: sent.created,
                activity_log_error: sent.activity_log_error.clone(),
                message: Some(sent),
                notification_error: None,
            }),
            Err(error) => Ok(TowerCompleteResult {
                mission: Some(mission),
                recovered_at,
                message: None,
                notification_created: false,
                notification_error: Some(error),
                activity_log_error: None,
            }),
        }
    }
}
