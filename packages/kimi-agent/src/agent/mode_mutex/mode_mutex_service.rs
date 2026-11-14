use crate::rpc::types::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExclusiveReviewMode {
    HelloWorld,
    Plan,
    Spec,
    Swarm,
    Tower,
}

impl ExclusiveReviewMode {
    pub const ALL: [Self; 5] = [
        Self::HelloWorld,
        Self::Plan,
        Self::Spec,
        Self::Swarm,
        Self::Tower,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::HelloWorld => "hello-world",
            Self::Plan => "plan",
            Self::Spec => "spec",
            Self::Swarm => "swarm",
            Self::Tower => "tower",
        }
    }

    pub fn conflicts(self) -> &'static [Self] {
        match self {
            Self::HelloWorld => &[Self::Spec, Self::Swarm, Self::Tower],
            Self::Plan => &[Self::Spec, Self::Tower],
            Self::Spec => &[Self::HelloWorld, Self::Plan, Self::Swarm, Self::Tower],
            Self::Swarm => &[Self::HelloWorld, Self::Spec, Self::Tower],
            Self::Tower => &[Self::HelloWorld, Self::Plan, Self::Spec, Self::Swarm],
        }
    }
}

pub const MODE_MUTEX_EXIT_REASON: &str = "mode-mutex";

pub fn is_allowed_pair(a: ExclusiveReviewMode, b: ExclusiveReviewMode) -> bool {
    matches!(
        (a, b),
        (ExclusiveReviewMode::Plan, ExclusiveReviewMode::Swarm)
            | (ExclusiveReviewMode::Plan, ExclusiveReviewMode::HelloWorld)
            | (ExclusiveReviewMode::Swarm, ExclusiveReviewMode::Plan)
            | (ExclusiveReviewMode::HelloWorld, ExclusiveReviewMode::Plan)
    )
}

pub trait ExclusiveModeControl: Send + Sync {
    fn is_active(&self, mode: ExclusiveReviewMode) -> bool;

    fn enter<'a>(&'a self, mode: ExclusiveReviewMode) -> BoxFuture<'a, Result<(), String>>;

    fn exit<'a>(
        &'a self,
        mode: ExclusiveReviewMode,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>>;
}

pub fn active_mode(control: &dyn ExclusiveModeControl) -> Option<ExclusiveReviewMode> {
    [
        ExclusiveReviewMode::Plan,
        ExclusiveReviewMode::Spec,
        ExclusiveReviewMode::Swarm,
        ExclusiveReviewMode::HelloWorld,
        ExclusiveReviewMode::Tower,
    ]
    .into_iter()
    .find(|mode| control.is_active(*mode))
}

async fn enter(
    control: &dyn ExclusiveModeControl,
    target: ExclusiveReviewMode,
) -> Result<(), String> {
    if control.is_active(target) {
        return Ok(());
    }
    control.enter(target).await
}

pub async fn leave(
    control: &dyn ExclusiveModeControl,
    target: ExclusiveReviewMode,
) -> Result<(), String> {
    if !control.is_active(target) {
        return Ok(());
    }
    control.exit(target, None).await
}

pub async fn switch_to(
    control: &dyn ExclusiveModeControl,
    target: ExclusiveReviewMode,
) -> Result<(), String> {
    let previous = active_mode(control);
    if previous == Some(target) {
        return Ok(());
    }

    if let Some(previous_mode) = previous
        && !is_allowed_pair(previous_mode, target)
    {
        leave(control, previous_mode).await?;
    }

    match enter(control, target).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(previous_mode) = previous {
                let _ = enter(control, previous_mode).await;
            }
            Err(error)
        }
    }
}

pub async fn evict_conflicts_of(control: &dyn ExclusiveModeControl, entered: ExclusiveReviewMode) {
    for conflicting in entered.conflicts() {
        if control.is_active(*conflicting) {
            let reason = (*conflicting == ExclusiveReviewMode::Tower)
                .then_some(MODE_MUTEX_EXIT_REASON);
            let _ = control.exit(*conflicting, reason).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ProbeControl {
        active: Mutex<Vec<ExclusiveReviewMode>>,
        calls: Mutex<Vec<String>>,
        fail_enter: Mutex<Option<ExclusiveReviewMode>>,
        fail_exit: Mutex<Option<ExclusiveReviewMode>>,
    }

    impl ProbeControl {
        fn with_active(modes: &[ExclusiveReviewMode]) -> Self {
            let probe = Self::default();
            *probe.active.lock().unwrap() = modes.to_vec();
            probe
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ExclusiveModeControl for ProbeControl {
        fn is_active(&self, mode: ExclusiveReviewMode) -> bool {
            self.active.lock().unwrap().contains(&mode)
        }

        fn enter<'a>(&'a self, mode: ExclusiveReviewMode) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if *self.fail_enter.lock().unwrap() == Some(mode) {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("enter-failed:{}", mode.as_str()));
                    return Err(format!("enter {}", mode.as_str()));
                }
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("enter:{}", mode.as_str()));
                let mut active = self.active.lock().unwrap();
                if !active.contains(&mode) {
                    active.push(mode);
                }
                Ok(())
            })
        }

        fn exit<'a>(
            &'a self,
            mode: ExclusiveReviewMode,
            reason: Option<&'a str>,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if *self.fail_exit.lock().unwrap() == Some(mode) {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("exit-failed:{}", mode.as_str()));
                    return Err(format!("exit {}", mode.as_str()));
                }
                self.calls.lock().unwrap().push(match reason {
                    Some(reason) => format!("exit:{}:{reason}", mode.as_str()),
                    None => format!("exit:{}", mode.as_str()),
                });
                self.active.lock().unwrap().retain(|item| *item != mode);
                Ok(())
            })
        }
    }

    #[test]
    fn conflict_table_matches_v2() {
        assert_eq!(
            ExclusiveReviewMode::HelloWorld.conflicts(),
            &[
                ExclusiveReviewMode::Spec,
                ExclusiveReviewMode::Swarm,
                ExclusiveReviewMode::Tower
            ]
        );
        assert_eq!(
            ExclusiveReviewMode::Plan.conflicts(),
            &[ExclusiveReviewMode::Spec, ExclusiveReviewMode::Tower]
        );
        assert_eq!(
            ExclusiveReviewMode::Spec.conflicts(),
            &[
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Plan,
                ExclusiveReviewMode::Swarm,
                ExclusiveReviewMode::Tower
            ]
        );
        assert_eq!(
            ExclusiveReviewMode::Swarm.conflicts(),
            &[
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Spec,
                ExclusiveReviewMode::Tower
            ]
        );
        assert_eq!(
            ExclusiveReviewMode::Tower.conflicts(),
            &[
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Plan,
                ExclusiveReviewMode::Spec,
                ExclusiveReviewMode::Swarm
            ]
        );
    }

    #[test]
    fn allowed_pairs_are_plan_with_swarm_or_hello_world() {
        for a in ExclusiveReviewMode::ALL {
            for b in ExclusiveReviewMode::ALL {
                let expected = matches!(
                    (a, b),
                    (ExclusiveReviewMode::Plan, ExclusiveReviewMode::Swarm)
                        | (ExclusiveReviewMode::Plan, ExclusiveReviewMode::HelloWorld)
                        | (ExclusiveReviewMode::Swarm, ExclusiveReviewMode::Plan)
                        | (ExclusiveReviewMode::HelloWorld, ExclusiveReviewMode::Plan)
                );
                assert_eq!(is_allowed_pair(a, b), expected, "{a:?} -> {b:?}");
            }
        }
        assert!(!is_allowed_pair(
            ExclusiveReviewMode::Plan,
            ExclusiveReviewMode::Spec
        ));
        assert!(!is_allowed_pair(
            ExclusiveReviewMode::Swarm,
            ExclusiveReviewMode::HelloWorld
        ));
    }

    #[test]
    fn active_mode_uses_v2_priority() {
        assert_eq!(active_mode(&ProbeControl::default()), None);
        assert_eq!(
            active_mode(&ProbeControl::with_active(&[
                ExclusiveReviewMode::Tower,
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Swarm,
                ExclusiveReviewMode::Spec,
                ExclusiveReviewMode::Plan,
            ])),
            Some(ExclusiveReviewMode::Plan)
        );
        assert_eq!(
            active_mode(&ProbeControl::with_active(&[
                ExclusiveReviewMode::Tower,
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Swarm,
                ExclusiveReviewMode::Spec,
            ])),
            Some(ExclusiveReviewMode::Spec)
        );
        assert_eq!(
            active_mode(&ProbeControl::with_active(&[
                ExclusiveReviewMode::Tower,
                ExclusiveReviewMode::HelloWorld,
                ExclusiveReviewMode::Swarm,
            ])),
            Some(ExclusiveReviewMode::Swarm)
        );
        assert_eq!(
            active_mode(&ProbeControl::with_active(&[
                ExclusiveReviewMode::Tower,
                ExclusiveReviewMode::HelloWorld,
            ])),
            Some(ExclusiveReviewMode::HelloWorld)
        );
        assert_eq!(
            active_mode(&ProbeControl::with_active(&[ExclusiveReviewMode::Tower])),
            Some(ExclusiveReviewMode::Tower)
        );
    }

    #[tokio::test]
    async fn switching_to_the_active_mode_is_a_noop() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Spec]);
        switch_to(&control, ExclusiveReviewMode::Spec)
            .await
            .unwrap();
        assert!(control.calls().is_empty());
    }

    #[tokio::test]
    async fn switching_evicts_an_incompatible_previous_mode() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Spec]);
        switch_to(&control, ExclusiveReviewMode::Tower)
            .await
            .unwrap();
        assert_eq!(control.calls(), vec!["exit:spec", "enter:tower"]);
        assert!(control.is_active(ExclusiveReviewMode::Tower));
        assert!(!control.is_active(ExclusiveReviewMode::Spec));
    }

    #[tokio::test]
    async fn switching_keeps_an_allowed_previous_mode() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Plan]);
        switch_to(&control, ExclusiveReviewMode::Swarm)
            .await
            .unwrap();
        assert_eq!(control.calls(), vec!["enter:swarm"]);
        assert!(control.is_active(ExclusiveReviewMode::Plan));
        assert!(control.is_active(ExclusiveReviewMode::Swarm));
    }

    #[tokio::test]
    async fn switching_restores_the_previous_mode_when_the_target_fails() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Spec]);
        *control.fail_enter.lock().unwrap() = Some(ExclusiveReviewMode::Tower);
        let error = switch_to(&control, ExclusiveReviewMode::Tower)
            .await
            .unwrap_err();
        assert_eq!(error, "enter tower");
        assert_eq!(
            control.calls(),
            vec!["exit:spec", "enter-failed:tower", "enter:spec"]
        );
        assert!(control.is_active(ExclusiveReviewMode::Spec));
        assert!(!control.is_active(ExclusiveReviewMode::Tower));
    }

    #[tokio::test]
    async fn leave_is_a_noop_when_the_mode_is_inactive() {
        let control = ProbeControl::default();
        leave(&control, ExclusiveReviewMode::Plan).await.unwrap();
        assert!(control.calls().is_empty());
    }

    #[tokio::test]
    async fn entering_evicts_every_conflict_with_the_mutex_reason() {
        let control = ProbeControl::with_active(&[
            ExclusiveReviewMode::HelloWorld,
            ExclusiveReviewMode::Plan,
            ExclusiveReviewMode::Swarm,
            ExclusiveReviewMode::Tower,
        ]);
        evict_conflicts_of(&control, ExclusiveReviewMode::Spec).await;
        assert_eq!(
            control.calls(),
            vec![
                "exit:hello-world:mode-mutex",
                "exit:plan:mode-mutex",
                "exit:swarm:mode-mutex",
                "exit:tower:mode-mutex"
            ]
        );
    }

    #[tokio::test]
    async fn entering_skips_inactive_conflicts() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Tower]);
        evict_conflicts_of(&control, ExclusiveReviewMode::Plan).await;
        assert_eq!(control.calls(), vec!["exit:tower:mode-mutex"]);
    }

    #[tokio::test]
    async fn eviction_ignores_exit_errors() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Spec]);
        *control.fail_exit.lock().unwrap() = Some(ExclusiveReviewMode::Spec);
        evict_conflicts_of(&control, ExclusiveReviewMode::Tower).await;
        assert_eq!(control.calls(), vec!["exit-failed:spec"]);
    }

    #[tokio::test]
    async fn leave_propagates_exit_errors() {
        let control = ProbeControl::with_active(&[ExclusiveReviewMode::Tower]);
        *control.fail_exit.lock().unwrap() = Some(ExclusiveReviewMode::Tower);
        let error = switch_to(&control, ExclusiveReviewMode::Plan)
            .await
            .unwrap_err();
        assert_eq!(error, "exit tower");
        assert_eq!(control.calls(), vec!["exit-failed:tower"]);
    }
}
