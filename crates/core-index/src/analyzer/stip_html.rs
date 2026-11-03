// "<b>Hello</b>" -> "  Hello  "
pub fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;

    for c in input.chars() {
        if c == '<' {
            in_tag = true;
            out.push(' '); // keep word separation
        } else if c == '>' {
            in_tag = false;
            out.push(' ');
        } else if !in_tag {
            out.push(c);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags() {
        assert_eq!(strip_html("<b>Hello</b>"), "  Hello  ");
        assert_eq!(
            strip_html("<p>Hello <b>World</b></p>"),
            "  Hello   World    "
        );
        assert_eq!(strip_html("no tags"), "no tags");
    }
}
