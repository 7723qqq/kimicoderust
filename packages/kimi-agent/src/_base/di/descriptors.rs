use std::any::Any;
use std::sync::Arc;

pub type DynamicValue = Arc<dyn Any + Send + Sync>;
pub type Constructor<T> = Arc<dyn Fn(Vec<DynamicValue>) -> T + Send + Sync>;

pub struct SyncDescriptor<T = DynamicValue> {
    pub ctor: Constructor<T>,
    pub static_arguments: Vec<DynamicValue>,
}

impl<T> SyncDescriptor<T> {
    pub fn new(ctor: Constructor<T>, static_arguments: Vec<DynamicValue>) -> Self {
        Self {
            ctor,
            static_arguments,
        }
    }
}

impl<T> Clone for SyncDescriptor<T> {
    fn clone(&self) -> Self {
        Self {
            ctor: self.ctor.clone(),
            static_arguments: self.static_arguments.clone(),
        }
    }
}

pub struct SyncDescriptor0<T> {
    pub ctor: Arc<dyn Fn() -> T + Send + Sync>,
}

impl<T> SyncDescriptor0<T> {
    pub fn new(ctor: Arc<dyn Fn() -> T + Send + Sync>) -> Self {
        Self { ctor }
    }
}

impl<T> Clone for SyncDescriptor0<T> {
    fn clone(&self) -> Self {
        Self {
            ctor: self.ctor.clone(),
        }
    }
}
