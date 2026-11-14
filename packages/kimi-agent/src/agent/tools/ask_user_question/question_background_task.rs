use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::_base::utils::abort::{AbortSignal, is_abort_error};
use crate::agent::task::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskKind, AgentTaskSettlement,
    AgentTaskSettlementStatus, AgentTaskSink, QuestionTaskInfo,
};
use crate::agent::tool_executor::tool_executor::{ExecutableToolOutput, ExecutableToolResult};
use crate::wire::record::WireAsyncError;

pub type QuestionRun = Arc<
    dyn Fn(AbortSignal) -> BoxFuture<'static, Result<ExecutableToolResult, WireAsyncError>>
        + Send
        + Sync,
>;

pub struct QuestionBackgroundTask {
    run: QuestionRun,
    description: String,
    question_count: usize,
    tool_call_id: Option<String>,
}

impl QuestionBackgroundTask {
    pub fn new(
        run: QuestionRun,
        description: String,
        question_count: usize,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            run,
            description,
            question_count,
            tool_call_id,
        }
    }
}

#[async_trait]
impl AgentTask for QuestionBackgroundTask {
    fn id_prefix(&self) -> &str {
        "question"
    }

    fn kind(&self) -> AgentTaskKind {
        AgentTaskKind::Question
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn start(&self, sink: AgentTaskSink) -> Result<(), WireAsyncError> {
        match (self.run)(sink.signal.clone()).await {
            Ok(result) => {
                let output = match result.output {
                    ExecutableToolOutput::Text(text) => text,
                    ExecutableToolOutput::Parts(parts) => {
                        serde_json::to_string(&parts).unwrap_or_default()
                    }
                };
                if result.is_error == Some(true) {
                    sink.settle(AgentTaskSettlement {
                        status: AgentTaskSettlementStatus::Failed,
                        stop_reason: Some(output),
                    })
                    .await;
                    return Ok(());
                }
                sink.append_output(output);
                sink.settle(AgentTaskSettlement {
                    status: AgentTaskSettlementStatus::Completed,
                    stop_reason: None,
                })
                .await;
            }
            Err(error) => {
                if sink.signal.aborted() && is_abort_error(error.as_ref()) {
                    sink.settle(AgentTaskSettlement {
                        status: AgentTaskSettlementStatus::Killed,
                        stop_reason: None,
                    })
                    .await;
                    return Ok(());
                }
                sink.settle(AgentTaskSettlement {
                    status: AgentTaskSettlementStatus::Failed,
                    stop_reason: Some(error.to_string()),
                })
                .await;
            }
        }
        Ok(())
    }

    fn to_info(&self, base: AgentTaskInfoBase) -> AgentTaskInfo {
        AgentTaskInfo::Question(QuestionTaskInfo {
            base,
            question_count: self.question_count,
            tool_call_id: self.tool_call_id.clone(),
        })
    }
}
