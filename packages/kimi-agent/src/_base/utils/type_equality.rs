pub type Flatten<T> = T;

pub trait Equal<A, B> {}

impl<T> Equal<T, T> for T {}

pub trait AssertExact<B>: Equal<B, B> {}

impl<T> AssertExact<T> for T {}
