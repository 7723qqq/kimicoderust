use super::types::{AgentTaskInfo, TaskInfoBase};

pub trait AgentTaskTiming {
    fn started_at(&self) -> f64;
    fn ended_at(&self) -> Option<f64>;
}

impl AgentTaskTiming for AgentTaskInfo {
    fn started_at(&self) -> f64 {
        self.started_at()
    }

    fn ended_at(&self) -> Option<f64> {
        self.ended_at()
    }
}

impl AgentTaskTiming for TaskInfoBase {
    fn started_at(&self) -> f64 {
        self.started_at
    }

    fn ended_at(&self) -> Option<f64> {
        self.ended_at
    }
}

pub fn format_task_wall_time(task: &dyn AgentTaskTiming, now_ms: Option<f64>) -> String {
    let now = now_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as f64);
    let duration_ms = (task.ended_at().unwrap_or(now) - task.started_at()).max(0.0);
    format!("{:.3} seconds", duration_ms / 1000.0)
}
