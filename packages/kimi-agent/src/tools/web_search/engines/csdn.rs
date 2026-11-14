use std::time::Duration;

use serde_json::Value;

use super::super::WebSearchResultEntry;

const CSDN_SEARCH_URL: &str = "https://so.csdn.net/api/v3/search";
const USER_AGENT: &str = "Apifox/1.0.0 (https://apifox.com)";
const COOKIE: &str = "uuid_tt_dd=10_20283040220-1750745713898-623562; dc_session_id=10_1750745713898.508399; dc_sid=0aa6fae5250c4389fac68320b1cb43b2; waf_captcha_marker=1b4e9099857d7aedf0941f03fa70bfb22ea2153f7fa053b8101ed28dc1504b11; c_pref=default; c_ref=default; fid=20_93458541565-1750745714849-027048; c_first_ref=default; c_first_page=https%3A//so.csdn.net/so/search%3Fq%3Dweb%2520search%2520mcp; c_dsid=11_1750745714849.980720; c_segment=10; c_page_id=default; creative_btn_mp=1; log_Id_view=9; fe_request_id=1750745715289_2973_2073791; dc_tos=syck1f; log_Id_pv=1; log_Id_click=1; uuid_tt_dd=10_20283045860-1751096847125-425142; dc_session_id=10_1751096847125.891975";

pub async fn search(query: &str, limit: usize) -> Result<Vec<WebSearchResultEntry>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("CSDN search request failed: {error}"))?;
    let mut all_results = Vec::new();
    let mut page = 1;
    while all_results.len() < limit {
        let url = search_url(query, page);
        let response = client
            .get(&url)
            .header("Pragma", "no-cache")
            .header("Cookie", COOKIE)
            .header("Accept", "*/*")
            .header("Host", "so.csdn.net")
            .header("Connection", "keep-alive")
            .send()
            .await
            .map_err(|error| format!("CSDN search request to {url} failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "CSDN search request failed: HTTP {}.",
                status.as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("CSDN search request to {url} failed: {error}"))?;
        let page_results = parse_results(&body)?;
        if page_results.is_empty() {
            break;
        }
        all_results.extend(page_results);
        page += 1;
    }
    all_results.truncate(limit);
    Ok(all_results)
}

fn search_url(query: &str, page: usize) -> String {
    let mut url = url::Url::parse(CSDN_SEARCH_URL).expect("valid CSDN URL");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("p", &page.to_string());
    url.into()
}

fn parse_results(body: &str) -> Result<Vec<WebSearchResultEntry>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("CSDN search request returned invalid JSON: {error}"))?;
    let Some(items) = value.get("result_vos").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?;
            let url = item.get("url_location")?.as_str()?;
            Some(WebSearchResultEntry {
                title: title.to_string(),
                url: url.to_string(),
                snippet: item
                    .get("digest")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                site_name: item
                    .get("nickname")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                date: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_items_with_title_and_url() {
        let results = parse_results(
            r#"{"result_vos":[{"title":"Rust","url_location":"https://blog.csdn.net/rust","digest":"safe","nickname":"alice"},{"title":"missing url"}]}"#,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].snippet, "safe");
        assert_eq!(results[0].site_name.as_deref(), Some("alice"));
    }

    #[test]
    fn missing_result_list_is_empty_and_invalid_json_is_an_error() {
        assert!(parse_results("{}").unwrap().is_empty());
        assert!(parse_results("not json").is_err());
    }

    #[test]
    fn url_uses_v2_query_names() {
        let url = search_url("rust lang", 3);
        assert!(url.contains("q=rust+lang"));
        assert!(url.contains("p=3"));
    }
}
