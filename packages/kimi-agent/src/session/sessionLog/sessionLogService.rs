use crate::_base::state::state_registry::IStateRegistryExt;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use serde_json::Value;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::file_log::{FileLogWriter, FileLogWriterOptions, create_file_log_writer};
use crate::_base::log::log::{I_LOG_SERVICE, ILogService, ILogger, LogLevel, LogPayload};
use crate::_base::log::log_config::{ILogOptions, resolve_session_log_path};
use crate::_base::log::log_service::{BoundLogger, LogLevelState, track_log_close};
use crate::app::scopes::LifecycleScope;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::state::session_state::ISessionStateService;
use crate::state::state::{StateDefinition, define_state};
use crate::wire::record::WireAsyncError;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;

pub static SESSION_LOG_ROOT_LEVEL_KEY: LazyLock<StateDefinition<LogLevelState>> =
    LazyLock::new(|| {
        define_state(
            "sessionLog.rootLevel",
            Arc::new(|| LogLevelState {
                level: LogLevel::Info,
            }),
        )
    });

fn seed_root_level(states: &Arc<dyn ISessionStateService>, level: LogLevel) -> LogLevelState {
    states.contribute_state(&SESSION_LOG_ROOT_LEVEL_KEY);
    states.set(&SESSION_LOG_ROOT_LEVEL_KEY, LogLevelState { level });
    states.get(&SESSION_LOG_ROOT_LEVEL_KEY)
}

pub struct SessionLogService {
    states: Arc<dyn ISessionStateService>,
    sink: Arc<FileLogWriter>,
    logger: BoundLogger,
}

impl SessionLogService {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        options: Arc<ILogOptions>,
        session: Arc<dyn ISessionContext>,
    ) -> Self {
        let sink = create_file_log_writer(FileLogWriterOptions {
            path: resolve_session_log_path(&session.session_dir()),
            max_bytes: options.session_max_bytes(),
            files: options.session_files(),
            omit_context_keys: vec!["sessionId".to_string()],
        });
        let level = seed_root_level(&states, options.level());
        let logger = BoundLogger::new(
            sink.clone(),
            level,
            HashMap::from([("sessionId".to_string(), Value::String(session.session_id()))]),
        );
        Self {
            states,
            sink,
            logger,
        }
    }

    pub fn level(&self) -> LogLevel {
        self.states.get(&SESSION_LOG_ROOT_LEVEL_KEY).level
    }

    pub fn set_level(&self, level: LogLevel) {
        self.states
            .set(&SESSION_LOG_ROOT_LEVEL_KEY, LogLevelState { level });
    }

    pub async fn flush(&self) -> bool {
        self.sink.flush().await
    }

    pub async fn close(&self) -> Result<(), WireAsyncError> {
        self.sink.close().await
    }

    pub fn dispose(&self) {
        self.sink.flush_sync();
        track_log_close(self.sink.close());
        self.logger.dispose();
    }
}

impl ILogger for SessionLogService {
    fn error(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.error(message, payload)
    }

    fn warn(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.warn(message, payload)
    }

    fn info(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.info(message, payload)
    }

    fn debug(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.debug(message, payload)
    }

    fn child(&self, ctx: HashMap<String, Value>) -> Arc<dyn crate::_base::log::log::ILogger> {
        self.logger.child(ctx)
    }
}

impl ILogService for SessionLogService {
    fn level(&self) -> LogLevel {
        SessionLogService::level(self)
    }

    fn set_level(&self, level: LogLevel) {
        SessionLogService::set_level(self, level)
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        let sink = self.sink.clone();
        async move {
            sink.flush().await;
        }
        .boxed()
    }
}
