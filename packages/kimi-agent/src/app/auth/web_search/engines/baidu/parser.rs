use scraper::{ElementRef, Html, Selector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaiduSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub source: String,
    pub engine: &'static str,
}

fn text_of_all(element: &ElementRef<'_>, selector: &Selector) -> String {
    element
        .select(selector)
        .map(|element| element.text().collect::<String>())
        .collect()
}

pub fn parse_baidu_search_results(html: &str) -> Vec<BaiduSearchResult> {
    let document = Html::parse_document(html);
    let content_selector = Selector::parse("#content_left").unwrap();
    let title_selector = Selector::parse("h3").unwrap();
    let link_selector = Selector::parse("a").unwrap();
    let snippet_baidu_selector = Selector::parse(".c-font-normal.c-color-text").unwrap();
    let snippet_selector = Selector::parse(".cos-row").unwrap();
    let source_selector = Selector::parse(".cosc-source").unwrap();
    let mut results = Vec::new();
    for content_block in document.select(&content_selector) {
        for element in content_block.children().filter_map(ElementRef::wrap) {
            let Some(title_element) = element.select(&title_selector).next() else {
                continue;
            };
            let Some(link_element) = element.select(&link_selector).next() else {
                continue;
            };
            let Some(url) = link_element.value().attr("href") else {
                continue;
            };
            if !url.starts_with("http") {
                continue;
            }
            let description = element
                .select(&snippet_baidu_selector)
                .next()
                .and_then(|element| element.value().attr("aria-label").map(str::to_string))
                .unwrap_or_else(|| {
                    element
                        .select(&snippet_selector)
                        .next()
                        .map(|element| element.text().collect::<String>().trim().to_string())
                        .unwrap_or_default()
                });
            let source = text_of_all(&element, &source_selector).trim().to_string();
            results.push(BaiduSearchResult {
                title: title_element.text().collect(),
                url: url.to_string(),
                description,
                source,
                engine: "baidu",
            });
        }
    }
    results
}
