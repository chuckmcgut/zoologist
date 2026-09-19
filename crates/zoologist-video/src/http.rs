//! A minimal HTTP/1.1 GET client for long-lived streams (Reolink HTTP-FLV).
//!
//! Written on `std::net::TcpStream` instead of an HTTP crate because a live stream needs a
//! *per-read* timeout (the stall watchdog): if the camera stops sending for 10 s, the read fails
//! and the source reconnects. General HTTP clients only offer whole-body timeouts, which never
//! fit an endless stream. Plain `http://` only: the build has no TLS client.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Largest response header block accepted.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Errors opening a stream.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("invalid URL: {0}")]
    Url(String),
    #[error("network error: {0}")]
    Io(#[from] io::Error),
    #[error("unexpected HTTP response: {0}")]
    Status(String),
}

/// Opens `GET url` and returns a reader over the response body (de-chunked if needed).
/// `timeout` applies to connecting and to every individual read.
pub fn get_stream(url: &str, timeout: Duration) -> Result<Box<dyn Read + Send>, HttpError> {
    let parsed = url::Url::parse(url).map_err(|e| HttpError::Url(e.to_string()))?;
    if parsed.scheme() != "http" {
        return Err(HttpError::Url("only http:// is supported".into()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| HttpError::Url("missing host".into()))?
        .to_string();
    let port = parsed.port().unwrap_or(80);
    let addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| HttpError::Url(format!("cannot resolve {host}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;

    let mut target = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    let host_header = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: zoologist\r\n\
         Accept: */*\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    let (status, chunked) = read_head(&mut reader)?;
    if status != 200 {
        return Err(HttpError::Status(format!("status {status}")));
    }
    Ok(if chunked {
        Box::new(ChunkedReader::new(reader))
    } else {
        Box::new(reader)
    })
}

/// Reads the status line and headers. Returns the status code and whether the body is chunked.
fn read_head(reader: &mut impl BufRead) -> Result<(u16, bool), HttpError> {
    let mut total = 0;
    let mut line = String::new();
    let mut status = None;
    let mut chunked = false;
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(HttpError::Status(
                "connection closed before headers ended".into(),
            ));
        }
        total += n;
        if total > MAX_HEADER_BYTES {
            return Err(HttpError::Status("response headers too large".into()));
        }
        let trimmed = line.trim_end();
        if status.is_none() {
            let code = trimmed
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<u16>().ok())
                .ok_or_else(|| HttpError::Status(format!("bad status line {trimmed:?}")))?;
            status = Some(code);
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    Ok((status.unwrap_or(0), chunked))
}

/// Decodes `Transfer-Encoding: chunked` bodies.
struct ChunkedReader<R> {
    inner: R,
    remaining: usize,
    done: bool,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
        }
    }

    fn next_chunk_size(&mut self) -> io::Result<usize> {
        let mut line = String::new();
        // Skip the CRLF that ends the previous chunk (and any blank lines).
        while line.trim().is_empty() {
            line.clear();
            if self.inner.read_line(&mut line)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        let size = line.trim().split(';').next().unwrap_or("");
        usize::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.remaining = self.next_chunk_size()?;
            if self.remaining == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n;
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    /// Reads the whole request head. Closing a socket with unread request bytes makes the OS
    /// send a reset, which the client would see as an error.
    fn read_request_head(sock: &mut std::net::TcpStream) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && sock.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
    }

    /// Serves `body` once on a local port, optionally chunked, then closes. Returns the URL.
    pub(crate) fn serve_once(body: Vec<u8>, chunked: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            read_request_head(&mut sock);
            if chunked {
                sock.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .unwrap();
                for part in body.chunks(1000) {
                    write!(sock, "{:x}\r\n", part.len()).unwrap();
                    sock.write_all(part).unwrap();
                    sock.write_all(b"\r\n").unwrap();
                }
                sock.write_all(b"0\r\n\r\n").unwrap();
            } else {
                sock.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: video/x-flv\r\n\r\n")
                    .unwrap();
                sock.write_all(&body).unwrap();
            }
        });
        format!("http://{addr}/flv?port=1935&app=bcs&stream=channel0_sub.bcs")
    }

    #[test]
    fn reads_plain_and_chunked_bodies() {
        let body: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        for chunked in [false, true] {
            let url = serve_once(body.clone(), chunked);
            let mut reader = get_stream(&url, Duration::from_secs(5)).unwrap();
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap();
            assert_eq!(got, body, "chunked={chunked}");
        }
    }

    #[test]
    fn rejects_non_200_and_https() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            read_request_head(&mut sock);
            sock.write_all(b"HTTP/1.1 401 Unauthorized\r\n\r\n")
                .unwrap();
        });
        let err = get_stream(&format!("http://{addr}/x"), Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(err.to_string().contains("401"), "{err}");
        assert!(get_stream("https://camera/x", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn chunked_reader_handles_extensions_and_small_reads() {
        let raw = b"4;ext=1\r\nabcd\r\n2\r\nef\r\n0\r\n\r\n".to_vec();
        let mut reader = ChunkedReader::new(Cursor::new(raw));
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while reader.read(&mut byte).unwrap() == 1 {
            out.push(byte[0]);
        }
        assert_eq!(out, b"abcdef");
    }
}
