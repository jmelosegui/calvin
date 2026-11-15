//! Text that looks like a credential. Cheap substring checks, not a secret scanner.

/// Markers that start a credential, and what each one suggests.
pub const MARKERS: &[(&str, &str)] = &[
    ("PRIVATE KEY-----", "contains a private key"),
    ("AKIA", "may contain an AWS access key"),
    ("ghp_", "may contain a GitHub token"),
    ("github_pat_", "may contain a GitHub token"),
    ("sk-ant-", "may contain an Anthropic API key"),
    ("xoxb-", "may contain a Slack token"),
    ("xoxp-", "may contain a Slack token"),
    ("AccountKey=", "may contain an Azure storage key"),
    ("SharedAccessSignature=", "may contain an Azure SAS token"),
    ("Password=", "may contain a connection-string password"),
];

const REDACTED: &str = "[redacted]";

/// Why `text` looks like it holds a credential, if it does.
pub fn find(text: &str) -> Option<&'static str> {
    MARKERS
        .iter()
        .find(|(m, _)| {
            text.match_indices(m)
                .any(|(i, _)| has_value(&text[i + m.len()..]))
        })
        .map(|(_, why)| *why)
}

/// `text` with the value after each credential marker replaced.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (marker, _) in MARKERS {
        let mut from = 0;
        while let Some(found) = out[from..].find(marker) {
            let start = from + found + marker.len();
            if !has_value(&out[start..]) {
                from = start;
                continue;
            }
            let len = out[start..].find(ends_value).unwrap_or(out.len() - start);
            out.replace_range(start..start + len, REDACTED);
            from = start + REDACTED.len();
        }
    }
    out
}

fn ends_value(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | ';' | ',' | '&')
}

/// A value right after the marker, so text that merely mentions it passes.
fn has_value(rest: &str) -> bool {
    rest.chars()
        .take_while(|&c| !ends_value(c))
        .take(12)
        .filter(|c| c.is_ascii_alphanumeric())
        .count()
        >= 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_values_but_not_mentions() {
        assert_eq!(
            redact("auth failed for ghp_abcdefghijklmnop0123 at host"),
            "auth failed for ghp_[redacted] at host"
        );
        assert_eq!(
            redact("Server=x;Password=hunter2hunter2;User=y"),
            "Server=x;Password=[redacted];User=y"
        );
        assert_eq!(redact("set a ghp_ token first"), "set a ghp_ token first");
        assert_eq!(find("set a ghp_ token first"), None);
    }
}
