use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct AbortScope {
    pub signal: CancellationToken,
}

impl AbortScope {
    pub fn abort(&self) {
        self.signal.cancel();
    }
}

pub fn create_abort_scope() -> AbortScope {
    AbortScope {
        signal: CancellationToken::new(),
    }
}

pub fn with_abort(parent: &CancellationToken) -> AbortScope {
    AbortScope {
        signal: parent.child_token(),
    }
}
