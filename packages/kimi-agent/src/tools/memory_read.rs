use crate::i18n::{LocalizedText, i18n_params};

pub fn normalize_path(path: &str) -> String {
    if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    }
}

pub fn read_path_required() -> String {
    LocalizedText::new("engine.tools.memory.readPathRequired").render()
}

pub fn not_found(path: &str) -> String {
    LocalizedText::with_params("engine.tools.memory.notFound", i18n_params!["path" => path])
        .render()
}

pub fn render_read(
    path: &str,
    scope: &str,
    scope_id: &str,
    memory_type: &str,
    title: &str,
    updated: &str,
    body: &str,
) -> String {
    let scope_line = if scope_id.is_empty() {
        scope.to_string()
    } else {
        format!("{scope} ({scope_id})")
    };
    format!(
        "# {title}\npath: {path}\nscope: {scope_line}\ntype: {memory_type}\nupdated: {updated}\n\n{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_md_suffix_is_appended() {
        assert_eq!(normalize_path("global/auth"), "global/auth.md");
        assert_eq!(normalize_path("global/auth.md"), "global/auth.md");
    }

    #[test]
    fn the_result_has_v2s_seven_lines_and_keeps_the_body_verbatim() {
        let rendered = render_read(
            "projects/abc123/auth.md",
            "project",
            "abc123",
            "pattern",
            "Authentication Pattern",
            "2026-11-08T00:00:00.000Z",
            "# Authentication Pattern\n\nUse JWT.\n",
        );
        let lines: Vec<&str> = rendered.split('\n').collect();
        assert_eq!(lines[0], "# Authentication Pattern");
        assert_eq!(lines[1], "path: projects/abc123/auth.md");
        assert_eq!(lines[2], "scope: project (abc123)");
        assert_eq!(lines[3], "type: pattern");
        assert_eq!(lines[4], "updated: 2026-11-08T00:00:00.000Z");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "# Authentication Pattern");
        assert!(rendered.ends_with("Use JWT.\n"));
    }

    #[test]
    fn a_global_scope_has_no_parenthesised_suffix() {
        let rendered = render_read(
            "global/auth.md",
            "global",
            "",
            "note",
            "Auth",
            "2026-11-08T00:00:00.000Z",
            "body",
        );
        assert!(rendered.contains("scope: global\n"));
        assert!(!rendered.contains("scope: global ("));
    }

    #[test]
    fn the_missing_path_refusal_is_a_result_not_a_dropped_call() {
        assert!(!read_path_required().is_empty());
    }

    #[test]
    fn the_not_found_message_names_the_normalized_path() {
        let message = not_found("global/auth.md");
        assert!(message.contains("global/auth.md"), "{message}");
    }
}
