//! Local HTTP (SPEC §10, §13): loopback TCP via `ureq` and HTTP/1.1 over unix sockets (Docker, Podman,
//! Firecracker). Never follows redirects, never uses a proxy (ureq would otherwise honour `HTTP_PROXY`),
//! caps response bodies, and sends no credentials.

use crate::AdapterError;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Largest response body read from any local endpoint.
pub const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// A response from a local endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parses the body as JSON (2xx only).
    pub fn json(&self) -> Result<serde_json::Value, AdapterError> {
        if !self.is_success() {
            return Err(AdapterError::Status(self.status));
        }
        serde_json::from_str(&self.body).map_err(|e| AdapterError::Decode(e.to_string()))
    }
}

/// Only literal loopback addresses are ever contacted (`127.0.0.0/8`, `[::1]`); host names — even
/// `localhost` — are refused because resolution could point elsewhere.
pub fn ensure_loopback(url: &str) -> Result<(), AdapterError> {
    let bad = || AdapterError::NotLoopback(url.to_string());
    let rest = url.strip_prefix("http://").ok_or_else(bad)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(bad());
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        let end = v6.find(']').ok_or_else(bad)?;
        let after = &v6[end + 1..];
        if !(after.is_empty() || after.starts_with(':')) {
            return Err(bad());
        }
        &v6[..end]
    } else {
        let mut parts = authority.splitn(2, ':');
        let h = parts.next().unwrap_or("");
        if let Some(port) = parts.next() {
            if port.is_empty() || port.parse::<u16>().is_err() {
                return Err(bad());
            }
        }
        h
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => Ok(()),
        _ => Err(bad()),
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .user_agent(format!("oomtop/{}", oomtop_core::VERSION))
        .build()
        .into()
}

fn http_err(e: ureq::Error) -> AdapterError {
    match e {
        ureq::Error::Timeout(_) => AdapterError::Timeout,
        other => AdapterError::Http(other.to_string()),
    }
}

/// `GET url` on a loopback endpoint.
pub fn get(url: &str, timeout: Duration) -> Result<HttpResponse, AdapterError> {
    ensure_loopback(url)?;
    let mut resp = agent(timeout)
        .get(url)
        .header("Accept", "application/json, text/plain")
        .call()
        .map_err(http_err)?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_string()
        .map_err(http_err)?;
    Ok(HttpResponse { status, body })
}

/// `GET url` → JSON (2xx only).
pub fn get_json(url: &str, timeout: Duration) -> Result<serde_json::Value, AdapterError> {
    get(url, timeout)?.json()
}

/// `POST url` with a JSON body; the response body is returned (2xx only).
pub fn post_json(url: &str, body: &serde_json::Value, timeout: Duration) -> Result<String, AdapterError> {
    ensure_loopback(url)?;
    let mut resp = agent(timeout).post(url).send_json(body).map_err(http_err)?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_string()
        .unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(AdapterError::Status(status));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------------------------------------
// HTTP/1.1 over a unix socket (Docker Engine API, Podman compat API, Firecracker API)
// ---------------------------------------------------------------------------------------------------------

/// `GET path` over the unix socket at `socket`, within `timeout` overall.
#[cfg(unix)]
pub fn unix_get(socket: &Path, path: &str, timeout: Duration) -> Result<HttpResponse, AdapterError> {
    use std::os::unix::net::UnixStream;
    if !path.starts_with('/') || path.contains(['\r', '\n', ' ']) {
        return Err(AdapterError::Decode(format!("bad request path {path:?}")));
    }
    let deadline = Instant::now() + timeout;
    let mut stream: UnixStream = connect_unix(socket, timeout)?;
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(AdapterError::Timeout);
    }
    stream
        .set_write_timeout(Some(left))
        .map_err(|e| AdapterError::Io(e.to_string()))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: oomtop/{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        oomtop_core::VERSION
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| AdapterError::Io(e.to_string()))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(AdapterError::Timeout);
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| AdapterError::Io(e.to_string()))?;
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() as u64 > MAX_BODY_BYTES + 64 * 1024 {
                    return Err(AdapterError::Decode("response too large".into()));
                }
                // Stop as soon as a complete response is in (some servers keep the socket open).
                if response_complete(&raw) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(AdapterError::Timeout)
            }
            Err(e) => return Err(AdapterError::Io(e.to_string())),
        }
    }
    parse_http_response(&raw)
}

/// Connects to a unix stream socket within `timeout`. `UnixStream::connect` blocks without a bound when the
/// listener's backlog is full (a wedged engine), so the connect is non-blocking + `poll`.
#[cfg(unix)]
pub fn connect_unix(
    socket: &Path,
    timeout: Duration,
) -> Result<std::os::unix::net::UnixStream, AdapterError> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;
    let io_err =
        |what: &str, e: std::io::Error| AdapterError::Io(format!("{}: {what}: {e}", socket.display()));
    let bytes = socket.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid (empty) address.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    // Leave room for the terminating NUL (the zeroed tail).
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(AdapterError::Io(format!(
            "{}: unusable socket path",
            socket.display()
        )));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, b) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        addr.sun_len = len as u8;
    }
    // Close-on-exec from creation where the OS allows it (Linux: SOCK_CLOEXEC, so a child forked by another
    // thread can never inherit the socket); macOS has no atomic flag, so FD_CLOEXEC is set right after.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let ty = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let ty = libc::SOCK_STREAM;
    // SAFETY: plain socket(2) call; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_UNIX, ty, 0) };
    if fd < 0 {
        return Err(io_err("socket", std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` is a fresh, owned descriptor; the stream closes it on every return path below.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fcntl on our own open descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    stream
        .set_nonblocking(true)
        .map_err(|e| io_err("nonblocking", e))?;
    // SAFETY: `addr` is a valid sockaddr_un of `len` bytes that outlives the call.
    let r = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        )
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINPROGRESS) | Some(libc::EINTR) => {
                let until = std::time::Instant::now() + timeout;
                let mut pfd = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // A signal (e.g. SIGCHLD with a handler installed by an embedding program) interrupts
                // poll with EINTR: retry for the remaining time instead of failing the connect.
                let n = loop {
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
                    // SAFETY: one valid pollfd.
                    let n = unsafe { libc::poll(&mut pfd, 1, ms) };
                    if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        if left.is_zero() {
                            break 0;
                        }
                        continue;
                    }
                    break n;
                };
                if n == 0 {
                    return Err(AdapterError::Timeout);
                }
                if n < 0 {
                    return Err(io_err("poll", std::io::Error::last_os_error()));
                }
                let mut soerr: libc::c_int = 0;
                let mut sl = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: SO_ERROR writes one c_int into `soerr`.
                let g = unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        &mut soerr as *mut libc::c_int as *mut libc::c_void,
                        &mut sl,
                    )
                };
                if g != 0 {
                    return Err(io_err("connect", std::io::Error::last_os_error()));
                }
                if soerr != 0 {
                    return Err(io_err("connect", std::io::Error::from_raw_os_error(soerr)));
                }
            }
            // Linux reports a full backlog as EAGAIN instead of waiting.
            Some(libc::EAGAIN) => {
                return Err(AdapterError::Io(format!("{}: listener busy", socket.display())))
            }
            _ => return Err(io_err("connect", e)),
        }
    }
    stream.set_nonblocking(false).map_err(|e| io_err("blocking", e))?;
    Ok(stream)
}

#[cfg(not(unix))]
pub fn unix_get(_socket: &Path, _path: &str, _timeout: Duration) -> Result<HttpResponse, AdapterError> {
    Err(AdapterError::Unsupported)
}

fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    Some((&raw[..i], &raw[i + 4..]))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn response_complete(raw: &[u8]) -> bool {
    let Some((head, body)) = split_head(raw) else {
        return false;
    };
    let head = String::from_utf8_lossy(head);
    if header(&head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return body.ends_with(b"0\r\n\r\n");
    }
    match header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        Some(n) => body.len() >= n,
        None => false,
    }
}

/// Parses a raw HTTP/1.x response (status line, headers, `Content-Length` or chunked body).
pub fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, AdapterError> {
    let (head, body) =
        split_head(raw).ok_or_else(|| AdapterError::Decode("incomplete HTTP response".into()))?;
    let head = String::from_utf8_lossy(head).to_string();
    let status_line = head.lines().next().unwrap_or("");
    let mut it = status_line.split_whitespace();
    let proto = it.next().unwrap_or("");
    if !proto.starts_with("HTTP/1.") {
        return Err(AdapterError::Decode(format!("bad status line {status_line:?}")));
    }
    let status: u16 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AdapterError::Decode(format!("bad status line {status_line:?}")))?;
    let body: Vec<u8> =
        if header(&head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            dechunk(body)?
        } else if let Some(n) = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
            body[..n.min(body.len())].to_vec()
        } else {
            body.to_vec()
        };
    Ok(HttpResponse {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, AdapterError> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| AdapterError::Decode("truncated chunk".into()))?;
        let size_str = String::from_utf8_lossy(&body[..line_end]);
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| AdapterError::Decode(format!("bad chunk size {size_hex:?}")))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size {
            return Err(AdapterError::Decode("truncated chunk".into()));
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size..];
        body = body.strip_prefix(b"\r\n").unwrap_or(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_only() {
        for ok in [
            "http://127.0.0.1:11434/api/ps",
            "http://127.0.0.1",
            "http://127.1.2.3:80/x",
            "http://[::1]:8080/metrics",
        ] {
            assert!(ensure_loopback(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://localhost:8080",
            "http://10.0.0.2:11434",
            "https://127.0.0.1",
            "http://127.0.0.1.evil.com/",
            "http://user:pw@127.0.0.1/",
            "http://evil.com@127.0.0.1/",
            "http://127.0.0.1:99999/",
            "http://[::1].evil/",
            "http://0.0.0.0:8080/",
            "ftp://127.0.0.1/",
        ] {
            assert!(ensure_loopback(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_content_length_and_chunked() {
        let r = parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, "hello"));
        let r = parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n[{\"a\r\n5;x=y\r\n\":1}]\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.body, "[{\"a\":1}]");
        assert_eq!(r.json().unwrap()[0]["a"], 1);
        let r = parse_http_response(b"HTTP/1.0 404 Not Found\r\n\r\nnope").unwrap();
        assert_eq!(r.status, 404);
        assert!(matches!(r.json(), Err(AdapterError::Status(404))));
        assert!(parse_http_response(b"garbage").is_err());
        assert!(parse_http_response(b"SSH-2.0\r\n\r\n").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
    }

    #[test]
    fn tcp_get_against_mock_server_and_no_redirects() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let h = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            let resp = tiny_http::Response::from_string("moved")
                .with_status_code(302)
                .with_header("Location: http://10.1.2.3/".parse::<tiny_http::Header>().unwrap());
            req.respond(resp).unwrap();
        });
        let r = get(&format!("http://127.0.0.1:{port}/x"), Duration::from_secs(2)).unwrap();
        assert_eq!(r.status, 302, "redirect is returned, never followed");
        h.join().unwrap();
        // Closed port: fast error, no panic.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(get(&format!("http://127.0.0.1:{port}/x"), Duration::from_millis(200)).is_err());
    }

    #[test]
    fn tcp_timeout_is_bounded() {
        // A listener that accepts but never answers.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let _keep = std::thread::spawn(move || {
            let _conns: Vec<_> = l.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(3));
        });
        let t = Instant::now();
        let r = get(&format!("http://127.0.0.1:{port}/"), Duration::from_millis(200));
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn unix_get_against_mock_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let server = tiny_http::Server::http_unix(&sock).unwrap();
        let h = std::thread::spawn(move || {
            for _ in 0..2 {
                let req = server.recv().unwrap();
                let body = format!("{{\"path\":\"{}\"}}", req.url());
                req.respond(tiny_http::Response::from_string(body)).unwrap();
            }
        });
        let r = unix_get(&sock, "/containers/json?all=0", Duration::from_secs(2)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.json().unwrap()["path"], "/containers/json?all=0");
        let r = unix_get(&sock, "/info", Duration::from_secs(2)).unwrap();
        assert_eq!(r.json().unwrap()["path"], "/info");
        h.join().unwrap();
        assert!(unix_get(
            &dir.path().join("missing.sock"),
            "/info",
            Duration::from_millis(100)
        )
        .is_err());
        assert!(unix_get(&sock, "bad path", Duration::from_millis(100)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn connect_unix_is_bounded_and_validates_paths() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ok.sock");
        let _l = UnixListener::bind(&sock).unwrap();
        assert!(connect_unix(&sock, Duration::from_millis(200)).is_ok());
        assert!(matches!(
            connect_unix(&dir.path().join("missing.sock"), Duration::from_millis(200)),
            Err(AdapterError::Io(_))
        ));
        let long = dir.path().join("x".repeat(200));
        assert!(
            connect_unix(&long, Duration::from_millis(200)).is_err(),
            "longer than sun_path"
        );
        assert!(connect_unix(Path::new(""), Duration::from_millis(200)).is_err());
        // A regular file is not a socket: refused quickly, never hangs.
        let f = dir.path().join("plain");
        std::fs::write(&f, "x").unwrap();
        let t = Instant::now();
        assert!(connect_unix(&f, Duration::from_millis(200)).is_err());
        assert!(t.elapsed() < Duration::from_millis(1000));
    }

    /// A listener that never accepts, with the smallest backlog: once the queue is full, connects must fail
    /// or time out within the bound (a blocking `UnixStream::connect` waits without limit on Linux).
    #[cfg(unix)]
    #[test]
    fn connect_unix_with_full_backlog_is_bounded() {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("full.sock");
        let l = UnixListener::bind(&sock).unwrap();
        // Shrink the backlog of the already-listening socket.
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 1) }, 0);
        let mut held = Vec::new();
        let t = Instant::now();
        let mut failures = 0;
        for _ in 0..16 {
            let t1 = Instant::now();
            match connect_unix(&sock, Duration::from_millis(100)) {
                Ok(s) => held.push(s),
                Err(_) => failures += 1,
            }
            assert!(t1.elapsed() < Duration::from_millis(600), "{:?}", t1.elapsed());
        }
        assert!(failures > 0, "backlog never filled ({} held)", held.len());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn unix_get_times_out() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("slow.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let _h = std::thread::spawn(move || {
            let _c = l.accept();
            std::thread::sleep(Duration::from_secs(3));
        });
        let t = Instant::now();
        assert!(matches!(
            unix_get(&sock, "/info", Duration::from_millis(150)),
            Err(AdapterError::Timeout)
        ));
        assert!(t.elapsed() < Duration::from_millis(1000));
    }
}
