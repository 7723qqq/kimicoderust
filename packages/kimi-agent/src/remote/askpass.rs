use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const SSH_ASKPASS_PORT_ENV: &str = "KIMI_CODE_SSH_ASKPASS_PORT";
pub const SSH_ASKPASS_TOKEN_ENV: &str = "KIMI_CODE_SSH_ASKPASS_TOKEN";

fn parse_js_number(input: &str) -> f64 {
    let value = input.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
                | '\n'
                | '\r'
        )
    });
    if value.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = value.strip_prefix(prefix) {
            return u128::from_str_radix(digits, radix)
                .map(|number| number as f64)
                .unwrap_or(f64::NAN);
        }
    }
    value.parse::<f64>().unwrap_or(f64::NAN)
}

fn utf16_len_for_limit(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.encode_utf16().count(),
        Err(error) if error.error_len().is_none() => std::str::from_utf8(&bytes[..error.valid_up_to()])
            .unwrap()
            .encode_utf16()
            .count(),
        Err(_) => String::from_utf8_lossy(bytes).encode_utf16().count(),
    }
}

pub fn is_ssh_askpass_environment(env: &HashMap<String, Option<String>>) -> bool {
    env.get(SSH_ASKPASS_PORT_ENV).is_some_and(Option::is_some)
        && env.get(SSH_ASKPASS_TOKEN_ENV).is_some_and(Option::is_some)
}

pub async fn run_ssh_askpass(env: &HashMap<String, Option<String>>) -> i32 {
    let port = env
        .get(SSH_ASKPASS_PORT_ENV)
        .and_then(|value| value.as_deref())
        .map(parse_js_number)
        .unwrap_or(f64::NAN);
    let token = env.get(SSH_ASKPASS_TOKEN_ENV).and_then(|value| value.as_deref());
    if !port.is_finite()
        || port.fract() != 0.0
        || port <= 0.0
        || port > 65535.0
        || token.is_none_or(|token| token.is_empty())
    {
        return 1;
    }
    let token = token.unwrap();
    let mut socket = match tokio::time::timeout(
        Duration::from_millis(10_000),
        tokio::net::TcpStream::connect(("127.0.0.1", port as u16)),
    )
    .await
    {
        Ok(Ok(socket)) => socket,
        _ => return 1,
    };
    if !matches!(
        tokio::time::timeout(
            Duration::from_millis(10_000),
            socket.write_all(format!("{}\n", json!({ "token": token })).as_bytes()),
        )
        .await,
        Ok(Ok(()))
    ) {
        return 1;
    }
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match tokio::time::timeout(Duration::from_millis(10_000), socket.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(read)) => {
                response.extend_from_slice(&buffer[..read]);
                if utf16_len_for_limit(&response) > 64 * 1024 {
                    return 1;
                }
            }
            _ => return 1,
        }
    }
    let response = String::from_utf8_lossy(&response);
    let result = match serde_json::from_str::<serde_json::Value>(&response) {
        Ok(result) => result,
        Err(_) => return 1,
    };
    let Some(value) = result.get("value").and_then(serde_json::Value::as_str) else {
        return 1;
    };
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(format!("{value}\n").as_bytes()).await;
    let _ = stdout.flush().await;
    0
}
