use std::error::Error;
use std::fmt;

use super::graph::Graph;

#[derive(Clone, Debug)]
pub struct CyclicDependencyError {
    pub path: Vec<String>,
    message: String,
}

impl CyclicDependencyError {
    pub fn from_path(path: Vec<String>) -> Self {
        Self {
            message: format!("Cyclic DI dependency detected: {}", path.join(" → ")),
            path,
        }
    }

    pub fn from_graph<T>(graph: &Graph<T>) -> Self {
        let cycle = graph.find_cycle_slow();
        let detail = cycle.clone().unwrap_or_else(|| {
            format!("UNABLE to detect cycle, dumping graph:\n{graph}")
        });
        let path = cycle
            .map(|cycle| cycle.split(" -> ").map(str::to_owned).collect())
            .unwrap_or_default();
        Self {
            message: format!("cyclic dependency between services: {detail}"),
            path,
        }
    }
}

impl fmt::Display for CyclicDependencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CyclicDependencyError {}

#[derive(Clone, Debug)]
pub struct CascadeConflictError {
    pub token: String,
    pub detail: String,
}

impl CascadeConflictError {
    pub fn new(token: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CascadeConflictError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Cascade conflict resolving '{}': {}",
            self.token, self.detail
        )
    }
}

impl Error for CascadeConflictError {}
