// "# **bold** [link](url)" -> "bold link"
pub fn strip_markdown(input: &str) -> String {
    let text = strip_links(input);
    text.chars()
        .filter(|c| !matches!(c, '#' | '*' | '_' | '`' | '~' | '>'))
        .collect()
}

// "[text](url)" -> "text"
fn strip_links(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(close) = after.find("](") {
            let text = &after[..close];
            let after_paren = &after[close + 2..];
            if let Some(end) = after_paren.find(')') {
                out.push_str(text);
                rest = &after_paren[end + 1..];
                continue;
            }
        }
        out.push('[');
        rest = &rest[start + 1..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_links_and_markers() {
        assert_eq!(strip_markdown("[docs](https://x.com)"), "docs");
        assert_eq!(strip_markdown("**bold** and `code`"), "bold and code");
        assert_eq!(strip_markdown("# Heading"), "Heading");
    }
}
