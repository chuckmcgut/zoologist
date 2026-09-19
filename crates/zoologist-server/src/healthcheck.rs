//! `zoologist healthcheck` (plan Step 11.2): the Docker `HEALTHCHECK`. Distroless images have
//! no curl, so this is a minimal HTTP/1.0 GET over `std::net`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, bail};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Splits `http://host[:port]/path` into the address and the path.
fn parse(url: &str) -> Result<(String, String, String)> {
    let rest = url
        .strip_prefix("http://")
        .context("only http:// URLs are supported")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        bail!("no host in {url:?}");
    }
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let addr = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((addr, host.to_string(), path.to_string()))
}

/// GETs `url` and returns the HTTP status code.
pub fn status(url: &str) -> Result<u16> {
    let (addr, host, path) = parse(url)?;
    let sock = addr
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {addr}"))?
        .next()
        .with_context(|| format!("no address for {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT)
        .with_context(|| format!("cannot connect to {addr}"))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    // The status line is all we need: "HTTP/1.1 200 OK".
    let mut head = [0u8; 64];
    let mut len = 0;
    while len < head.len() {
        let n = stream.read(&mut head[len..])?;
        if n == 0 {
            break;
        }
        len += n;
        if head[..len].contains(&b'\n') {
            break;
        }
    }
    let line = String::from_utf8_lossy(&head[..len]);
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("not an HTTP response: {line:?}"))
}

/// Exit code for Docker: 0 when the health endpoint answers 200, 1 otherwise.
pub fn run(url: &str) -> std::process::ExitCode {
    match status(url) {
        Ok(200) => std::process::ExitCode::SUCCESS,
        Ok(code) => {
            eprintln!("unhealthy: {url} answered {code}");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("unhealthy: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse("http://127.0.0.1:8090/api/v1/health").unwrap(),
            (
                "127.0.0.1:8090".into(),
                "127.0.0.1".into(),
                "/api/v1/health".into()
            )
        );
        assert_eq!(
            parse("http://example").unwrap(),
            ("example:80".into(), "example".into(), "/".into())
        );
        assert!(parse("https://x/").is_err());
    }

    /// Serves one connection with `response` and returns the request it received.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1/health", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 256];
            while !request.ends_with(b"\r\n\r\n") {
                let n = conn.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            conn.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn reads_the_status_code() {
        let (url, server) = serve_once("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        assert_eq!(status(&url).unwrap(), 200);
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /api/v1/health HTTP/1.0\r\n"),
            "{request}"
        );

        let (url, server) = serve_once("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        assert_eq!(status(&url).unwrap(), 503);
        server.join().unwrap();
    }

    #[test]
    fn nothing_listening_is_an_error() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(status(&format!("http://127.0.0.1:{port}/")).is_err());
    }
}
