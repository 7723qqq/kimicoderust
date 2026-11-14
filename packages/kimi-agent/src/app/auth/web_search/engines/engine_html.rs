use scraper::{ElementRef, Html, Selector};

#[derive(Clone, Copy)]
pub struct EngineElement<'a> {
    element: ElementRef<'a>,
}

impl<'a> EngineElement<'a> {
    pub fn text_content(&self) -> String {
        self.element.text().collect()
    }

    pub fn get_attribute(&self, name: &str) -> Option<&'a str> {
        self.element.attr(name)
    }

    pub fn query_selector_all(&self, selector: &str) -> EngineQueryResult<'a> {
        query_element(self.element, selector)
    }

    pub fn query_selector(&self, selector: &str) -> Option<EngineElement<'a>> {
        self.query_selector_all(selector).first().elements.first().copied()
    }
}

#[derive(Clone)]
pub struct EngineQueryResult<'a> {
    elements: Vec<EngineElement<'a>>,
}

impl<'a> EngineQueryResult<'a> {
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn text(&self) -> String {
        self.elements
            .iter()
            .map(EngineElement::text_content)
            .collect()
    }

    pub fn text_without(&self, selector: &str) -> String {
        let selector = parse_selector(selector);
        self.elements
            .iter()
            .map(|element| {
                let mut text = String::new();
                collect_text_without(element.element, &selector, &mut text);
                text
            })
            .collect()
    }

    pub fn attr(&self, name: &str) -> Option<&'a str> {
        self.elements.first()?.get_attribute(name)
    }

    pub fn find(&self, selector: &str) -> EngineQueryResult<'a> {
        let mut elements = Vec::new();
        for element in &self.elements {
            elements.extend(element.query_selector_all(selector).elements);
        }
        EngineQueryResult { elements }
    }

    pub fn first(&self) -> EngineQueryResult<'a> {
        EngineQueryResult {
            elements: self.elements.iter().take(1).copied().collect(),
        }
    }

    pub fn each(&self, mut callback: impl FnMut(usize, EngineElement<'a>)) {
        for (index, element) in self.elements.iter().copied().enumerate() {
            callback(index, element);
        }
    }

    pub fn map<T>(&self, mut callback: impl FnMut(usize, EngineElement<'a>) -> T) -> Vec<T> {
        self.elements
            .iter()
            .copied()
            .enumerate()
            .map(|(index, element)| callback(index, element))
            .collect()
    }

    pub fn to_array(&self) -> Vec<EngineElement<'a>> {
        self.elements.clone()
    }
}

fn collect_text_without(element: ElementRef<'_>, selector: &Selector, output: &mut String) {
    for child in element.children() {
        match child.value() {
            scraper::node::Node::Text(text) => output.push_str(text),
            scraper::node::Node::Element(_) => {
                let Some(child_element) = ElementRef::wrap(child) else {
                    continue;
                };
                if !selector.matches(&child_element) {
                    collect_text_without(child_element, selector, output);
                }
            }
            _ => {}
        }
    }
}

fn parse_selector(selector: &str) -> Selector {
    Selector::parse(selector).unwrap_or_else(|error| panic!("invalid selector {selector:?}: {error}"))
}

fn query_element<'a>(element: ElementRef<'a>, selector: &str) -> EngineQueryResult<'a> {
    let selector = parse_selector(selector);
    EngineQueryResult {
        elements: element
            .select(&selector)
            .map(|element| EngineElement { element })
            .collect(),
    }
}

pub fn load_html(html: &str) -> Html {
    Html::parse_document(html)
}

pub fn query_html<'a>(document: &'a Html, selector: &str) -> EngineQueryResult<'a> {
    let selector = parse_selector(selector);
    EngineQueryResult {
        elements: document
            .select(&selector)
            .map(|element| EngineElement { element })
            .collect(),
    }
}
