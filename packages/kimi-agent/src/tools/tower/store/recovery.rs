use super::*;
use crate::tools::tower::paths::TOWER_ROOT;
use crate::tools::tower::types::TowerAgentKind;

impl TowerStore {
    pub(crate) async fn state_was_lost(&self) -> bool {
        if fs::read_to_string(self.abs(STATE_FILE)).await.is_ok() {
            return false;
        }
        fs::metadata(self.abs(TOWER_ROOT)).await.is_ok()
    }

    pub(crate) async fn recovered_at(&self) -> Result<Option<String>, String> {
        Ok(self.load_state_value().await.ok().and_then(|raw| {
            raw.get("recoveredAt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        }))
    }

    pub(super) async fn load_or_recover_locked(
        &self,
    ) -> Result<(TowerState, Option<String>), String> {
        match self.load().await {
            Ok(state) => {
                let recovered_at = self.load_state_value().await.ok().and_then(|raw| {
                    raw.get("recoveredAt")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
                Ok((state, recovered_at))
            }
            Err(_) if self.state_was_lost().await => self.recover_minimal_state_locked().await,
            Err(error) => Err(error),
        }
    }

    async fn recover_minimal_state_locked(&self) -> Result<(TowerState, Option<String>), String> {
        // Another caller may have recovered while this one was observing the
        // missing file. The outer state lock makes this second load decisive.
        if let Ok(state) = self.load().await {
            let recovered_at = self.load_state_value().await.ok().and_then(|raw| {
                raw.get("recoveredAt")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
            return Ok((state, recovered_at));
        }
        if !self.state_was_lost().await {
            return Err("tower is not initialized in this repository — run TowerInit first".into());
        }
        let base = current_branch(&self.repo_root).await?;
        for dir in &[INBOX_DIR, FINDINGS_DIR, REVIEWS_DIR, MISSIONS_DIR, LOG_DIR] {
            fs::create_dir_all(self.abs(dir))
                .await
                .map_err(|e| e.to_string())?;
        }
        let recovered_at = chrono::Utc::now().to_rfc3339();
        let state = TowerState {
            version: 1,
            base,
            mode: "branch".into(),
            created_at: recovered_at.clone(),
            session_id: None,
            roster: TowerRoster::default(),
            missions: Vec::new(),
        };
        self.save(&state).await?;
        let mut raw = self.load_state_value().await?;
        raw["recoveredAt"] = serde_json::Value::String(recovered_at.clone());
        self.save_state_value(&raw).await?;
        self.append_log(TOWER_NAME, "state.recovered", &[], Some(STATE_FILE))
            .await?;
        Ok((state, Some(recovered_at)))
    }

    pub(super) async fn resolve_agent_operation(
        &self,
        caller_agent_id: &str,
    ) -> Result<(TowerState, String, Option<String>), String> {
        match self.load().await {
            Ok(state) => {
                let recovered_at = self.recovered_at().await?;
                match self.resolve_caller_name(&state, caller_agent_id) {
                    Ok(caller) => return Ok((state, caller, recovered_at)),
                    Err(error) if recovered_at.is_none() => return Err(error),
                    Err(_) => {}
                }
            }
            Err(_) if self.state_was_lost().await => {}
            Err(error) => return Err(error),
        }
        let caller_agent_id = caller_agent_id.to_string();
        self.with_state_lock(move || async move {
            let (mut state, recovered_at) = self.load_or_recover_locked().await?;
            let caller = self
                .resolve_recovered_caller_locked(
                    &mut state,
                    &caller_agent_id,
                    recovered_at.as_deref(),
                )
                .await?;
            Ok((state, caller, recovered_at))
        })
        .await
    }

    pub(super) async fn resolve_recovered_caller_locked(
        &self,
        state: &mut TowerState,
        caller_agent_id: &str,
        recovered_at: Option<&str>,
    ) -> Result<String, String> {
        if let Ok(caller) = self.resolve_caller_name(state, caller_agent_id) {
            return Ok(caller);
        }
        let Some(recovered_at) = recovered_at else {
            return self.resolve_caller_name(state, caller_agent_id);
        };
        if caller_agent_id == "main" {
            return Ok(TOWER_NAME.into());
        }
        let caller = caller_agent_id.to_string();
        state.roster.agents.push(TowerRosterEntry {
            name: caller.clone(),
            agent_id: caller_agent_id.to_string(),
            session_id: state.session_id.clone(),
            kind: TowerAgentKind::Worker,
            mission_id: None,
            review_target: None,
            review_mission_id: None,
            worktree: None,
            branch: None,
            spawned_at: recovered_at.to_string(),
            died_at: None,
            death_status: None,
            death_reason: None,
            status: None,
        });
        self.save(state).await?;
        self.append_log(
            TOWER_NAME,
            "roster.placeholder",
            &[("name", &caller), ("agent", caller_agent_id)],
            None,
        )
        .await?;
        Ok(caller)
    }
}
