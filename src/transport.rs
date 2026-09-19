//! How bytes get here. The 1 MB budget is decided entirely by TLS: a rustls stack costs
//! about 1 MB on Linux, so the Linux build links NO TLS by default and borrows an HTTPS
//! client the host already has. On macOS and Windows the operating system's own TLS is
//! reachable through Rust bindings for about 300 KB, with no C compiled and no process
//! spawned, so there it is always in. `detect` tries, in order:
//!
//! 1. In-binary TLS: always on macOS and Windows (Security.framework / SChannel), and on
//!    Linux only with `--features tls` (rustls).
//! 2. `node` — a single long-lived child running `fetch`. Node is the one thing the use case
//!    guarantees on the box: whatever gets installed is about to be run by it. This is what
//!    makes the chain terminate on slim container images, where a 2026-09-17 survey of 16
//!    popular bases found curl on 4 and neither curl nor wget on 8.
//! 3. `curl` (most full Linux distributions).
//! 4. `wget` (busybox on Alpine).
//! 5. `python3` — the Python container images carry neither curl nor wget, but do carry
//!    Python with its ssl module and a CA bundle. Not tried on macOS, where a missing
//!    `python3` is a stub that opens a dialog offering to install the developer tools.
//!
//! The host clients are told to refuse a redirect off HTTPS: a tarball is protected by its
//! integrity hash, but a packument is not, so a downgrade could substitute a tarball and its
//! hash together. An embedder with its own HTTP client skips all of this by implementing
//! [`Transport`]. Every transport here is safe to call from many threads at once; the
//! installer does.

use crate::error::Error;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub trait Transport: Send + Sync {
    /// Fetch `url` with the given `Accept` header. A non-2xx status is an error.
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error>;
}

/// The first transport available, in the order documented above.
pub fn detect() -> Result<Box<dyn Transport>, Error> {
    #[cfg(any(feature = "tls", target_os = "macos", target_os = "windows"))]
    return Ok(Box::new(builtin::Builtin::new()));
    #[cfg(not(any(feature = "tls", target_os = "macos", target_os = "windows")))]
    detect_host()
}

/// The first HOST-provided transport, skipping in-binary TLS even when it is compiled in.
/// Worth reaching for deliberately behind a TLS-intercepting corporate proxy: the host's own
/// client carries the system trust store, where a bundled rustls root set does not.
pub fn detect_host() -> Result<Box<dyn Transport>, Error> {
    if let Some(t) = NodeFetch::spawn() {
        return Ok(Box::new(t));
    }
    if available("curl") {
        return Ok(Box::new(Curl));
    }
    if available("wget") {
        return Ok(Box::new(Wget));
    }
    if !cfg!(target_os = "macos") && available("python3") {
        return Ok(Box::new(Python));
    }
    Err(Error::NoTransport(vec!["node", "curl", "wget", "python3"]))
}

fn available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn check_status(url: &str, status: u16) -> Result<(), Error> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(Error::Status {
            url: url.to_string(),
            status,
        })
    }
}

// ---------------------------------------------------------------------------------------
// node: one child, many requests in flight. A request line is `<id>\t<accept>\t<url>\n`; a
// reply is `<id> <status> <length>\n` followed by exactly `length` body bytes, written by the
// child in a single write so replies never interleave. The child starts every fetch as it
// arrives, so replies come back in completion order and a reader thread routes each to its
// waiting caller. Node's undici pool then gives connection reuse for free.

const NODE_SCRIPT: &str = r#"
if (typeof fetch !== 'function') { process.stdout.write('NOFETCH\n'); process.exit(0); }
process.stdout.write('READY\n');
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  const [id, accept, url] = line.split('\t');
  let status = 0, body = Buffer.alloc(0);
  try {
    const r = await fetch(url, { headers: { accept } });
    status = r.status; body = Buffer.from(await r.arrayBuffer());
  } catch (e) {}
  process.stdout.write(Buffer.concat([Buffer.from(id + ' ' + status + ' ' + body.length + '\n'), body]));
});
rl.on('close', () => process.exit(0));
"#;

type Reply = Result<(u16, Vec<u8>), Error>;
type Pending = Arc<Mutex<HashMap<u64, Sender<Reply>>>>;

pub struct NodeFetch {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
}

impl NodeFetch {
    /// `None` when `node` is absent or predates global `fetch` (Node < 18).
    pub fn spawn() -> Option<Self> {
        let mut child = Command::new("node")
            .args(["-e", NODE_SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let mut stdout = BufReader::new(child.stdout.take()?);
        let mut ready = String::new();
        stdout.read_line(&mut ready).ok()?;
        if ready.trim_end() != "READY" {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let pending: Pending = Arc::default();
        let routes = Arc::clone(&pending);
        std::thread::spawn(move || Self::route(stdout, routes));
        Some(NodeFetch {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
        })
    }

    /// Reader thread: deliver each reply to its caller. On EOF every waiter gets an error,
    /// which is how a crashed child surfaces instead of hanging its callers.
    fn route(mut stdout: BufReader<ChildStdout>, pending: Pending) {
        loop {
            let mut header = String::new();
            if matches!(stdout.read_line(&mut header), Ok(0) | Err(_)) {
                break;
            }
            let mut parts = header.split_whitespace();
            let parsed = (
                parts.next().and_then(|s| s.parse::<u64>().ok()),
                parts.next().and_then(|s| s.parse::<u16>().ok()),
                parts.next().and_then(|s| s.parse::<usize>().ok()),
            );
            let (Some(id), Some(status), Some(len)) = parsed else {
                break;
            };
            let mut body = vec![0; len];
            if stdout.read_exact(&mut body).is_err() {
                break;
            }
            let waiter = pending.lock().ok().and_then(|mut p| p.remove(&id));
            if let Some(tx) = waiter {
                let _ = tx.send(Ok((status, body)));
            }
        }
        if let Ok(mut p) = pending.lock() {
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(Error::Transport("node transport exited".into())));
            }
        }
    }
}

impl Transport for NodeFetch {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let poisoned = || Error::Transport("node transport poisoned".into());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.lock().map_err(|_| poisoned())?.insert(id, tx);
        {
            let mut stdin = self.stdin.lock().map_err(|_| poisoned())?;
            writeln!(stdin, "{id}\t{accept}\t{url}")
                .map_err(|e| Error::Transport(format!("node: {e}")))?;
        }
        let (status, body) = rx
            .recv()
            .map_err(|_| Error::Transport("node transport exited".into()))??;
        if status == 0 {
            return Err(Error::Transport(format!("node: fetch failed for {url}")));
        }
        check_status(url, status)?;
        Ok(body)
    }
}

impl Drop for NodeFetch {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------
// curl / wget: one process per request. `--compressed` lets curl negotiate gzip for the
// packument; wget (busybox included) has no equivalent and fetches identity.

pub struct Curl;

/// `-w` appends the status after the body, split back off in `get`; `--proto` and
/// `--proto-redir` pin both the request and any redirect to HTTPS.
fn curl_args(url: &str, accept: &str) -> Vec<String> {
    [
        "-fsSL",
        "--compressed",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "-w",
        "\n%{http_code}",
        "-H",
        &format!("accept: {accept}"),
        url,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn wget_args(url: &str, accept: &str) -> Vec<String> {
    [
        "-q",
        "--https-only",
        "-O",
        "-",
        &format!("--header=accept: {accept}"),
        url,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Transport for Curl {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let out = Command::new("curl")
            .args(curl_args(url, accept))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("curl: {e}")))?;
        let body = out.stdout;
        let cut = body.iter().rposition(|&b| b == b'\n').unwrap_or(body.len());
        let status: u16 = std::str::from_utf8(&body[cut..])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if !out.status.success() && status == 0 {
            return Err(Error::Transport(format!(
                "curl exited {} for {url}",
                out.status
            )));
        }
        check_status(url, status)?;
        Ok(body[..cut].to_vec())
    }
}

pub struct Wget;

impl Transport for Wget {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let out = Command::new("wget")
            .args(wget_args(url, accept))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("wget: {e}")))?;
        if !out.status.success() {
            return Err(Error::Transport(format!(
                "wget exited {} for {url}",
                out.status
            )));
        }
        Ok(out.stdout)
    }
}

// ---------------------------------------------------------------------------------------
// In-binary TLS (opt-in). ureq's Agent pools connections and is safe to share across threads.

/// One `python3` process per request, using the standard library's `urllib` and `ssl`.
/// The body goes to stdout; an HTTP error puts its status on stderr and exits 3; a redirect
/// off HTTPS is refused.
pub struct Python;

const PYTHON_SCRIPT: &str = r#"
import sys, urllib.request, urllib.error
url, accept = sys.argv[1], sys.argv[2]
class HttpsOnly(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not newurl.startswith("https://"):
            raise urllib.error.HTTPError(newurl, code, "redirect off https refused", headers, fp)
        return super().redirect_request(req, fp, code, msg, headers, newurl)
opener = urllib.request.build_opener(HttpsOnly())
try:
    with opener.open(urllib.request.Request(url, headers={"accept": accept})) as r:
        sys.stdout.buffer.write(r.read())
except urllib.error.HTTPError as e:
    sys.stderr.write(str(e.code))
    sys.exit(3)
"#;

impl Transport for Python {
    fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
        let out = Command::new("python3")
            .args(["-c", PYTHON_SCRIPT, url, accept])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("python3: {e}")))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        match (out.status.code(), stderr.trim().parse::<u16>()) {
            (Some(3), Ok(status)) => check_status(url, status).map(|()| Vec::new()),
            _ => Err(Error::Transport(format!(
                "python3 exited {} for {url}: {}",
                out.status,
                stderr.trim()
            ))),
        }
    }
}

#[cfg(any(feature = "tls", target_os = "macos", target_os = "windows"))]
mod builtin {
    use super::{Transport, check_status};
    use crate::error::Error;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    use ureq_rustls as ureq;

    pub struct Builtin(ureq::Agent);

    impl Builtin {
        pub fn new() -> Self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                use ureq::tls::{TlsConfig, TlsProvider};
                let cfg = ureq::Agent::config_builder()
                    .tls_config(
                        TlsConfig::builder()
                            .provider(TlsProvider::NativeTls)
                            .build(),
                    )
                    .http_status_as_error(false)
                    .build();
                return Builtin(ureq::Agent::new_with_config(cfg));
            }
            #[allow(unreachable_code)]
            Builtin(ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .build(),
            ))
        }
    }

    impl Transport for Builtin {
        fn get(&self, url: &str, accept: &str) -> Result<Vec<u8>, Error> {
            let mut resp = self
                .0
                .get(url)
                .header("accept", accept)
                .call()
                .map_err(|e| Error::Transport(e.to_string()))?;
            check_status(url, resp.status().as_u16())?;
            resp.body_mut()
                .with_config()
                .limit(1 << 30)
                .read_to_vec()
                .map_err(|e| Error::Transport(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_clients_refuse_to_leave_https() {
        let curl = curl_args("https://r/x", "*/*");
        let proto = curl.iter().position(|a| a == "--proto").unwrap();
        let redir = curl.iter().position(|a| a == "--proto-redir").unwrap();
        assert_eq!(
            (&curl[proto + 1], &curl[redir + 1]),
            (&"=https".into(), &"=https".into())
        );
        assert!(wget_args("https://r/x", "*/*").contains(&"--https-only".to_string()));
        assert!(PYTHON_SCRIPT.contains("redirect off https refused"));
    }

    #[test]
    fn python_transport_reports_http_status() {
        if cfg!(target_os = "macos") || !available("python3") {
            return;
        }
        let port = barrier_server();
        assert_eq!(
            Python
                .get(&format!("http://127.0.0.1:{port}/fast"), "*/*")
                .unwrap(),
            b"fast-body"
        );
        let err = Python
            .get(&format!("http://127.0.0.1:{port}/missing"), "*/*")
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 404, .. }), "{err}");
    }

    #[test]
    fn missing_programs_are_reported_absent_not_as_errors() {
        assert!(!available("microbe-definitely-not-a-program"));
    }

    /// A local HTTP server whose `/slow` reply is held until `/fast` has been REQUESTED. A
    /// transport that sends one request at a time can never release it, so `/slow` times out
    /// into a 504 and the test fails; no wall-clock threshold is involved, so a loaded host
    /// cannot flake it. Distinct bodies prove each reply reached its own caller.
    fn barrier_server() -> u16 {
        use std::net::TcpListener;
        use std::sync::Condvar;
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fast_seen = Arc::new((Mutex::new(false), Condvar::new()));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let fast_seen = Arc::clone(&fast_seen);
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        return;
                    }
                    let (flag, cv) = &*fast_seen;
                    let (status, body) = if line.contains("/missing") {
                        ("404 Not Found", "no such path")
                    } else if line.contains("/fast") {
                        *flag.lock().unwrap() = true;
                        cv.notify_all();
                        ("200 OK", "fast-body")
                    } else {
                        let seen = flag.lock().unwrap();
                        let (seen, _) = cv
                            .wait_timeout_while(seen, Duration::from_secs(10), |s| !*s)
                            .unwrap();
                        if *seen {
                            ("200 OK", "slow-body")
                        } else {
                            ("504 Gateway Timeout", "never saw /fast")
                        }
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        port
    }

    #[test]
    fn node_transport_overlaps_requests_and_routes_each_reply_to_its_caller() {
        let Some(t) = NodeFetch::spawn() else { return };
        let port = barrier_server();
        std::thread::scope(|s| {
            let slow = s.spawn(|| t.get(&format!("http://127.0.0.1:{port}/slow"), "*/*"));
            // Give `/slow` a head start so it is in flight first; correctness does not
            // depend on this, only the strength of the check does.
            std::thread::sleep(std::time::Duration::from_millis(200));
            let fast = s.spawn(|| t.get(&format!("http://127.0.0.1:{port}/fast"), "*/*"));
            assert_eq!(fast.join().unwrap().unwrap(), b"fast-body");
            assert_eq!(slow.join().unwrap().unwrap(), b"slow-body");
        });
    }

    #[test]
    fn node_transport_reports_http_status() {
        let Some(t) = NodeFetch::spawn() else { return };
        // A URL no resolver answers fails at fetch, not with a status.
        let err = t.get("https://registry.invalid/x", "*/*").unwrap_err();
        assert!(matches!(err, Error::Transport(_)), "{err}");
    }
}
