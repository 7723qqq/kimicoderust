use std::collections::HashMap;

pub trait IHostRequestHeaders: Send + Sync {
    fn headers(&self) -> HashMap<String, String>;
    fn third_party_headers(&self) -> HashMap<String, String>;
    fn identity_slug(&self) -> Option<&str> {
        None
    }
}
