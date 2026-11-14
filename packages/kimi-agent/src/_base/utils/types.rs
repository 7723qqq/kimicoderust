use futures_util::future::BoxFuture;

pub type Promisify<T> = BoxFuture<'static, T>;
pub type PromisifyMethods<T> = T;

pub enum Promisable<T> {
    Value(T),
    Future(BoxFuture<'static, T>),
}

pub type PromisableMethods<T> = T;
