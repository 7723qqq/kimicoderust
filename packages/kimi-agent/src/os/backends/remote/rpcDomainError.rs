use std::collections::HashSet;

use serde_json::Value;

use crate::remote::protocol::errors::RpcError;

pub struct RpcDomainError {
    pub domain_code: &'static str,
    pub details: Value,
}

pub fn rpc_domain_error(
    error: &RpcError,
    codes: &HashSet<&'static str>,
) -> Option<RpcDomainError> {
    let data = error.data.as_ref()?;
    let domain_code = data.get("domainCode")?.as_str()?;
    let domain_code = codes.get(domain_code)?;
    Some(RpcDomainError {
        domain_code,
        details: data.clone(),
    })
}
