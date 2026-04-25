/// Redact `user:password@` userinfo from a URL so error messages and
/// trace logs cannot leak the bearer credential embedded in private
/// registry / git URLs (Artifactory, Nexus, JFrog, GitHub Packages).
///
/// Returns the input unchanged when no userinfo is present.
pub fn redact_url(url: &str) -> String {
    // Handle both fully-qualified `scheme://user:pw@host/x` and the
    // scheme-relative form `//user:pw@host/x` produced by some tools.
    let after = if let Some(scheme_end) = url.find("://") {
        scheme_end + 3
    } else if url.starts_with("//") {
        2
    } else {
        return url.to_string();
    };
    let tail = &url[after..];
    let Some(at) = tail.find('@') else {
        return url.to_string();
    };
    let slash = tail.find('/').unwrap_or(tail.len());
    if at >= slash {
        return url.to_string();
    }
    format!("{}***@{}", &url[..after], &tail[at + 1..])
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn passthrough_when_no_userinfo() {
        assert_eq!(
            redact_url("https://registry.example.com/foo"),
            "https://registry.example.com/foo"
        );
    }

    #[test]
    fn redacts_user_and_password() {
        let input = format!("https://user:hunter2{}host.example.com/x", '\u{40}');
        let expected = format!("https://***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn does_not_redact_at_in_path() {
        let input = format!("https://host/foo{}1.0.0/bar", '\u{40}');
        assert_eq!(redact_url(&input), input);
    }

    #[test]
    fn redacts_userinfo_with_ipv6_host() {
        let input = format!("https://tok{}[::1]:8443/x", '\u{40}');
        let expected = format!("https://***{}[::1]:8443/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn redacts_scheme_relative_userinfo() {
        let input = format!("//user:pw{}host.example.com/x", '\u{40}');
        let expected = format!("//***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }
}
