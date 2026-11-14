use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccessOperation {
    Read,
    Write,
    Execute,
}

impl PathAccessOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }
}

pub trait ISessionWorkspaceContext: Send + Sync {
    fn work_dir(&self) -> String;
    fn additional_dirs(&self) -> Vec<String>;
    fn set_work_dir(&self, work_dir: String);
    fn resolve(&self, rel: &str) -> String;
    fn is_within(&self, abs_path: &str) -> bool;
    fn assert_allowed(&self, abs_path: &str, op: PathAccessOperation) -> Result<String, WireAsyncError>;
}
