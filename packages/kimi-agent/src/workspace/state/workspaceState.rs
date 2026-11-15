use crate::_base::state::state_registry::{IStateRegistry, IStateRegistryExt};

pub trait IWorkspaceStateService: IStateRegistry {}

impl IStateRegistryExt for dyn IWorkspaceStateService + '_ {}
