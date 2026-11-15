pub fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn escape_xml_attr(input: &str) -> String {
    input.replace('&', "&amp;").replace('"', "&quot;")
}

pub fn escape_xml_tags(input: &str) -> String {
    input.replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_xml_all() {
        assert_eq!(escape_xml("hello"), "hello");
        assert_eq!(escape_xml("<tag>"), "&lt;tag&gt;");
        assert_eq!(escape_xml("a & b"), "a &amp; b");
        assert_eq!(escape_xml(r#"attr="val""#), "attr=&quot;val&quot;");
    }

    #[test]
    fn test_escape_xml_attr() {
        assert_eq!(escape_xml_attr("hello"), "hello");
        assert_eq!(escape_xml_attr("a & b"), "a &amp; b");
        assert_eq!(escape_xml_attr(r#""quoted""#), "&quot;quoted&quot;");

        assert_eq!(escape_xml_attr("<tag>"), "<tag>");
    }

    #[test]
    fn test_escape_xml_tags() {
        assert_eq!(escape_xml_tags("hello"), "hello");
        assert_eq!(escape_xml_tags("<tag>"), "&lt;tag&gt;");

        assert_eq!(escape_xml_tags("a & b"), "a & b");
        assert_eq!(escape_xml_tags(r#""quoted""#), r#""quoted""#);
    }

    #[test]
    fn test_empty_input() {
        assert_eq!(escape_xml(""), "");
        assert_eq!(escape_xml_attr(""), "");
        assert_eq!(escape_xml_tags(""), "");
    }

    #[test]
    fn test_no_special_chars() {
        let input = "Hello, World! 123 abc.";
        assert_eq!(escape_xml(input), input);
        assert_eq!(escape_xml_attr(input), input);
        assert_eq!(escape_xml_tags(input), input);
    }

    #[test]
    fn test_cjk_characters() {
        assert_eq!(escape_xml("你好世界"), "你好世界");
        assert_eq!(escape_xml_attr("中文"), "中文");
        assert_eq!(escape_xml_tags("日本語"), "日本語");
    }

    #[test]
    fn test_matches_js_output() {
        let cases = vec![
            (
                "<skill name=\"test\">content & more</skill>",
                "&lt;skill name=&quot;test&quot;&gt;content &amp; more&lt;/skill&gt;",
            ),
            ("plugin \"test\" & arg", "plugin &quot;test&quot; &amp; arg"),
            (
                "<bash-input>ls -la</bash-input>",
                "&lt;bash-input&gt;ls -la&lt;/bash-input&gt;",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(escape_xml(input), expected, "escape_xml({:?})", input);
        }
    }
}
