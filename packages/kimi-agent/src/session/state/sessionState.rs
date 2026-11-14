use crate::_base::state::state_registry::IStateRegistry;

pub use crate::state::state::{StateDefinition, define_state};

pub trait ISessionStateService: IStateRegistry + Send + Sync {}

impl ISessionStateService for crate::_base::state::state_registry::StateRegistry {}
