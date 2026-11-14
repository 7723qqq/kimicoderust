pub const EBM_BLIND_MUTATION_THRESHOLD: u32 = 3;

pub const EBM_REMINDER: &str = "You have made 3 or more changes since the last time a test or check was run, and none of your recent commands could have verified them. Run a verification command (tests, typecheck, linter) before making further changes, so failures are caught while they are still attributable.";

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

use std::collections::HashSet;

use serde_json::json;

#[derive(Debug, Clone, Default)]
pub struct ToolReceiptPaths {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolReceipt {
    pub tool_name: String,
    pub command: Option<String>,
    pub paths: Option<ToolReceiptPaths>,
    pub success: bool,
    pub is_error: Option<bool>,
    pub write_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSample {
    pub exploration: u32,
    pub verification: u32,
    pub objective: u32,
    pub regression: u32,
    pub churn: u32,
    pub discriminating: u32,
    pub debt_age: u32,
    pub blind_mutations: u32,
}

pub const EMPTY_SAMPLE: ProgressSample = ProgressSample {
    exploration: 0,
    verification: 0,
    objective: 0,
    regression: 0,
    churn: 0,
    discriminating: 0,
    debt_age: 0,
    blind_mutations: 0,
};

#[derive(Clone, Default)]
pub struct TrackerState {
    pub round: u32,
    pub read_paths: HashSet<String>,
    pub commands: HashSet<String>,
    pub failures: HashSet<String>,
    pub actions: HashSet<String>,
    pub verify_seen: HashSet<String>,
    pub verify_pass: HashSet<String>,
    pub mutated_bases: HashSet<String>,
    pub debt: bool,
    pub debt_age: u32,
    pub blind: u32,
}

pub static EMPTY_TRACKER: std::sync::LazyLock<TrackerState> =
    std::sync::LazyLock::new(TrackerState::default);

#[derive(Default)]
pub struct ProgressTracker {
    state: TrackerState,
}

impl ProgressTracker {
    pub fn score_round(&mut self, receipts: &[ToolReceipt]) -> ProgressSample {
        let mut sample = EMPTY_SAMPLE;
        let mut next = self.state.clone();
        next.round += 1;
        for receipt in receipts {
            next = score_receipt(receipt, &mut sample, next);
        }
        if sample.discriminating > 0 {
            next.debt = false;
            next.debt_age = 0;
            next.blind = 0;
        } else {
            if sample.churn > 0 {
                next.debt = true;
                next.blind += sample.churn;
            }
            if next.debt {
                next.debt_age += 1;
            }
        }
        sample.debt_age = next.debt_age;
        sample.blind_mutations = next.blind;
        self.state = next;
        sample
    }

    pub fn blind_mutations(&self) -> u32 {
        self.state.blind
    }

    pub fn debt_age(&self) -> u32 {
        self.state.debt_age
    }
}

fn score_receipt(
    receipt: &ToolReceipt,
    sample: &mut ProgressSample,
    mut state: TrackerState,
) -> TrackerState {
    let command = receipt.command.as_deref().map(str::trim);
    if let Some(command) = command.filter(|command| !command.is_empty()) {
        return score_command(receipt, command, sample, state);
    }
    if receipt.success && is_write(receipt) {
        sample.churn += 1;
        return note_mutated_paths(state, receipt);
    }
    if receipt.success && is_delegation(receipt) {
        sample.exploration += 1;
        return state;
    }
    if receipt.success && receipt.is_error != Some(true) && is_new_read(receipt, &state) {
        sample.exploration += 1;
        if let Some(paths) = &receipt.paths {
            state.read_paths.extend(paths.read.iter().cloned());
        }
        return state;
    }
    if receipt.success && is_read_only(receipt) {
        return state;
    }
    if receipt.success {
        return note_action(state, receipt, sample);
    }
    state
}

fn score_command(
    receipt: &ToolReceipt,
    command: &str,
    sample: &mut ProgressSample,
    mut state: TrackerState,
) -> TrackerState {
    if receipt.success && is_write(receipt) {
        sample.churn += 1;
        state = note_mutated_paths(state, receipt);
    }
    let verify = is_verification_command(command);
    if verify || command_exercises_mutation(command, &state.mutated_bases) {
        sample.discriminating += 1;
    }
    if verify {
        sample.verification += 1;
        let seen = state.verify_seen.contains(command);
        let was_pass = state.verify_pass.contains(command);
        state.verify_seen.insert(command.to_string());
        if receipt.success {
            state.verify_pass.insert(command.to_string());
        }
        if seen && receipt.success && !was_pass {
            sample.objective += 1;
        }
        if seen && !receipt.success && was_pass {
            sample.regression += 1;
        }
    }
    if receipt.success {
        if !verify && !state.commands.contains(command) {
            sample.exploration += 1;
        }
        state.commands.insert(command.to_string());
        return state;
    }
    if state.failures.insert(command.to_string()) {
        sample.exploration += 1;
    }
    state
}

fn note_mutated_paths(mut state: TrackerState, receipt: &ToolReceipt) -> TrackerState {
    if let Some(paths) = &receipt.paths {
        for path in &paths.write {
            let base = path.replace('\\', "/");
            let base = base.rsplit('/').next().unwrap_or_default();
            if base.encode_utf16().count() >= 3 {
                state.mutated_bases.insert(base.to_string());
            }
        }
    }
    state
}

fn note_action(
    mut state: TrackerState,
    receipt: &ToolReceipt,
    sample: &mut ProgressSample,
) -> TrackerState {
    let paths = receipt.paths.as_ref().map(|paths| {
        json!({
            "read": paths.read,
            "write": paths.write,
        })
    });
    let signature = format!(
        "{}\0{}",
        receipt.tool_name,
        serde_json::to_string(&paths.unwrap_or_else(|| json!({}))).unwrap()
    );
    if state.actions.insert(signature) {
        sample.exploration += 1;
    }
    state
}

fn command_exercises_mutation(command: &str, mutated_bases: &HashSet<String>) -> bool {
    mutated_bases.iter().any(|base| command.contains(base))
}

fn is_write(receipt: &ToolReceipt) -> bool {
    receipt
        .paths
        .as_ref()
        .is_some_and(|paths| !paths.write.is_empty())
}

fn is_delegation(receipt: &ToolReceipt) -> bool {
    matches!(receipt.tool_name.as_str(), "Agent" | "AgentSwarm")
}

fn is_read_only(receipt: &ToolReceipt) -> bool {
    receipt.paths.as_ref().is_some_and(|paths| {
        !paths.read.is_empty() && paths.write.is_empty()
    })
}

fn is_new_read(receipt: &ToolReceipt, state: &TrackerState) -> bool {
    receipt.paths.as_ref().is_some_and(|paths| {
        !paths.read.is_empty()
            && paths
                .read
                .iter()
                .any(|path| !state.read_paths.contains(path))
    })
}

