use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::state::state_registry::IStateRegistry;

pub trait IAppStateService: IStateRegistry {}

pub static I_APP_STATE_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("appStateService"));
