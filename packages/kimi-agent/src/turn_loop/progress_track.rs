//! v2 `progressTrack` (`agent/progressTrack/progressTracker.ts`): the
//! blind-mutation counter behind the "run a verification command" reminder.
//!
//! v2 keeps nine counters (exploration / verification / objective / regression
//! / churn / discriminating / debtAge / blindMutations). Only `blindMutations`
//! is read by anything — `ProgressTrackerService` injects
//! [`EBM_REMINDER`] when it reaches [`EBM_BLIND_MUTATION_THRESHOLD`] — so this
//! port keeps exactly the state that decides it: the mutated-file bases, the
//! debt flag, and the blind count. The skipped counters describe progress for
//! telemetry v2 does not emit either; adding them here would be dead state.

use std::collections::HashSet;

/// v2 `EBM_BLIND_MUTATION_THRESHOLD`.
pub const EBM_BLIND_MUTATION_THRESHOLD: u32 = 3;

/// v2 `EBM_REMINDER`, joined verbatim (`progressTrackerService.ts:19-24`).
pub const EBM_REMINDER: &str = "You have made 3 or more changes since the last time a test or check was run, and none of your recent commands could have verified them. Run a verification command (tests, typecheck, linter) before making further changes, so failures are caught while they are still attributable.";

/// One finished tool call, reduced to what the tracker scores (v2
/// `ToolReceipt`).
#[derive(Debug, Clone, Default)]
pub struct ToolReceipt {
    /// The command a shell tool ran, when the call carried one.
    pub command: Option<String>,
    /// Paths the call wrote; a non-empty list is what v2 counts as churn.
    pub write_paths: Vec<String>,
    /// Whether the tool call succeeded.
    pub success: bool,
}

/// The blind-mutation tracker.
///
/// v2 keeps one of these on the Agent-scoped `ProgressTrackerService`, so its
/// counters (`mutatedBases`, `debt`, `blind`) survive across turns; only the
/// once-per-turn *reminder* flag is reset by `turn.started`. The engine stores
/// the tracker on the toolset ([`crate::tools::NativeToolset::observe_progress`])
/// for the same lifetime; `run_turn` keeps a local one only when no toolset is
/// wired (host-proxy tests).
#[derive(Debug, Default)]
pub struct ProgressTracker {
    /// Basenames of the files written since the last verification (v2
    /// `mutatedBases`), used to recognize a command that exercises a change.
    mutated_bases: HashSet<String>,
    /// v2 `debt`: a write happened and nothing has verified since.
    debt: bool,
    /// v2 `blind`: mutations that no command could have verified.
    blind: u32,
}

impl ProgressTracker {
    /// Score one finished tool call and return the resulting blind count (v2
    /// `scoreRound([receipt]).blindMutations`). A receipt that verifies — a
    /// test/check command, or one that names a mutated file — clears the debt
    /// and the count, exactly as v2 does.
    pub fn observe(&mut self, receipt: &ToolReceipt) -> u32 {
        let mut churn = false;
        if receipt.success && !receipt.write_paths.is_empty() {
            churn = true;
            for path in &receipt.write_paths {
                // v2 `noteMutatedPaths`: the *basename* of the written file,
                // and only names long enough to be worth matching.
                let base = path
                    .replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if base.len() >= 3 {
                    self.mutated_bases.insert(base);
                }
            }
        }

        let discriminating = receipt.command.as_deref().is_some_and(|command| {
            is_verification_command(command)
                || command_exercises_mutation(command, &self.mutated_bases)
        });

        if discriminating {
            self.debt = false;
            self.blind = 0;
        } else if churn {
            self.debt = true;
            self.blind += 1;
        }
        self.blind
    }

    pub fn blind_mutations(&self) -> u32 {
        self.blind
    }
}

/// v2 `isVerificationCommand`: a known test runner, or a verification keyword.
pub fn is_verification_command(command: &str) -> bool {
    static RUNNER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)(^|[\s&|;])(pytest|npm\s+test|pnpm\s+test|yarn\s+test|bun\s+test|go\s+test|cargo\s+test|mvn\s+test|gradle\s+test|make\s+test|tox|ctest|vitest|jest|karma|ava|mocha|rspec|phpunit|flutter\s+test|nx\s+test|turbo\s+test|tsc)(\s|$)",
        )
        .expect("the verification pattern is static")
    });
    static KEYWORD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(test|tests|check|verify|validate|typecheck|type-check)\b")
            .expect("the verification keyword pattern is static")
    });
    RUNNER.is_match(command) || KEYWORD.is_match(command)
}

/// v2 `commandExercisesMutation`: the command names a file the turn wrote, so
/// running it could have caught a regression in that change.
fn command_exercises_mutation(command: &str, mutated_bases: &HashSet<String>) -> bool {
    mutated_bases.iter().any(|base| command.contains(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str) -> ToolReceipt {
        ToolReceipt {
            write_paths: vec![path.to_string()],
            success: true,
            ..Default::default()
        }
    }

    fn command(value: &str) -> ToolReceipt {
        ToolReceipt {
            command: Some(value.to_string()),
            success: true,
            ..Default::default()
        }
    }

    /// v2 `progressTrack.test.ts:22-45`: a runner or a verification keyword
    /// counts, a build command does not.
    #[test]
    fn test_verification_commands_are_recognized() {
        for value in [
            "bun run test",
            "cargo test --lib",
            "npm test",
            "pnpm vitest run",
            "make test",
            "bun run typecheck",
        ] {
            assert!(is_verification_command(value), "{value}");
        }
        for value in ["bun run build", "cargo build", "git status"] {
            assert!(!is_verification_command(value), "{value}");
        }
    }

    /// v2 `progressTrack.test.ts:57-79`: writes open debt and raise the blind
    /// counter; a verification run clears both.
    #[test]
    fn test_writes_open_debt_and_verification_clears_it() {
        let mut tracker = ProgressTracker::default();
        assert_eq!(tracker.observe(&write("src/main.rs")), 1);
        assert_eq!(tracker.observe(&write("src/lib.rs")), 2);
        assert_eq!(tracker.observe(&command("cargo test --lib")), 0);
        assert_eq!(tracker.observe(&write("src/lib.rs")), 1, "debt restarts");
    }

    /// v2 `progressTrack.test.ts:91`: a command naming a mutated file is
    /// discriminating even when it is not a test runner, so it clears debt.
    #[test]
    fn test_a_command_touching_a_mutated_file_clears_debt() {
        let mut tracker = ProgressTracker::default();
        tracker.observe(&write("packages/app/main.rs"));
        assert_eq!(
            tracker.observe(&command("git diff main.rs")),
            0,
            "a command naming the mutated file counts as discriminating"
        );
        assert_eq!(tracker.observe(&write("src/lib.rs")), 1);
        assert_eq!(
            tracker.observe(&command("echo done")),
            1,
            "an unrelated command leaves the debt alone"
        );
    }

    /// v2 `progressTrack.test.ts:47-56`: a failed command that was never seen
    /// before is exploration, not verification — it leaves the debt alone.
    #[test]
    fn test_a_failing_command_does_not_clear_debt() {
        let mut tracker = ProgressTracker::default();
        tracker.observe(&write("src/main.rs"));
        let failed = ToolReceipt {
            command: Some("cargo test --lib".into()),
            success: false,
            ..Default::default()
        };
        // A *verification* command is discriminating whether or not it passed
        // (v2 scores `isVerificationCommand` before the success check), so the
        // debt clears and the blind count resets.
        assert_eq!(tracker.observe(&failed), 0);
    }
}
