//! Hiding credentials in camera URLs before they reach logs or API responses.

/// Query parameters whose values are always hidden.
const SECRET_PARAMS: &[&str] = &["user", "username", "password", "pass", "pwd", "token"];

/// Returns `url` with credentials hidden: the `user:pass@` part becomes `***@`, and the values
/// of query parameters such as `user=` and `password=` (used by Reolink FLV URLs) become `***`.
///
/// Works on any `scheme://authority/path?query` string without fully parsing it, so odd camera
/// URLs are never rejected, only redacted.
pub fn redact_url(url: &str) -> String {
    let (before_query, query) = match url.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (url, None),
    };

    let mut out = String::with_capacity(url.len());
    match before_query.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(authority_end);
            out.push_str(scheme);
            out.push_str("://");
            match authority.rfind('@') {
                Some(at) => {
                    out.push_str("***");
                    out.push_str(&authority[at..]);
                }
                None => out.push_str(authority),
            }
            out.push_str(path);
        }
        None => out.push_str(before_query),
    }

    if let Some(query) = query {
        out.push('?');
        let params: Vec<String> = query
            .split('&')
            .map(|param| match param.split_once('=') {
                Some((key, _)) if SECRET_PARAMS.contains(&key.to_ascii_lowercase().as_str()) => {
                    format!("{key}=***")
                }
                _ => param.to_string(),
            })
            .collect();
        out.push_str(&params.join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn hides_userinfo() {
        assert_eq!(redact_url("rtsp://u:p@h/x"), "rtsp://***@h/x");
        assert_eq!(
            redact_url("rtsp://admin:s3cr@t@192.168.1.2:554/h264Preview_01_sub"),
            "rtsp://***@192.168.1.2:554/h264Preview_01_sub"
        );
    }

    #[test]
    fn hides_reolink_flv_query_credentials() {
        let url = "http://10.0.0.5/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user=admin&password=hunter2";
        assert_eq!(
            redact_url(url),
            "http://10.0.0.5/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user=***&password=***"
        );
    }

    #[test]
    fn leaves_urls_without_credentials_alone() {
        assert_eq!(redact_url("rtsp://h:554/live"), "rtsp://h:554/live");
        assert_eq!(redact_url("file:///tmp/a.h264"), "file:///tmp/a.h264");
        assert_eq!(redact_url("http://hub"), "http://hub");
    }
}
