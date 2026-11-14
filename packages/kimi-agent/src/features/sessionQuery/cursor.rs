use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionSearchCursor {
    pub offset: usize,
}

impl SessionSearchCursor {
    pub fn new(offset: usize) -> Self {
        Self { offset }
    }
}
