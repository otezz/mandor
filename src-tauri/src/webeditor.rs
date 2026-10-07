//! "Open in editor" for remote browsers: VS Code in the browser.
//!
//! A browser tab can't launch an editor on the host, so Mandor runs VS Code's
//! own `code serve-web` and reverse-proxies it under `/editor` on the same
//! server. That keeps it behind whatever already fronts Mandor (the tunnel's
//! login, HTTPS — VS Code's webviews need a secure context) with no second port
//! or tunnel route. The editor listens on a unix socket in the cache dir rather
//! than a TCP port, so nothing but Mandor can reach it.
//!
//! The server is started on the first request and stopped with the app.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use axum::{
    body::Body,
    http::{header, Request, Response, StatusCode},
};
use tauri::{AppHandle, Manager};

use crate::pty;

pub const BASE_PATH: &str = "/editor";

#[derive(Default)]
pub struct WebEditor {
    child: Mutex<Option<Child>>,
}

fn socket_path() -> Option<PathBuf> {
    pty::mandor_cache_dir().map(|c| c.join("vscode.sock"))
}

impl WebEditor {
    /// Start `code serve-web` if it isn't already running; returns its socket.
    fn ensure_running(&self) -> Result<PathBuf, String> {
        let sock = socket_path().ok_or("no cache directory")?;
        let mut guard = self.child.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => return Ok(sock),
                Ok(Some(status)) => {
                    *guard = None;
                    return Err(format!(
                        "the VS Code server exited ({status}); reload to try again"
                    ));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        *guard = Some(spawn_serve_web(&sock)?);
        Ok(sock)
    }

    /// `code` is a launcher script that spawns the real server several
    /// processes deep, so killing just the child would orphan it.
    pub fn stop(&self) {
        #[cfg(unix)]
        if let Some(mut child) = self.child.lock().unwrap().take() {
            // SAFETY: killpg takes only integers; the group is the one we created.
            unsafe { libc::killpg(child.id() as i32, libc::SIGTERM) };
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(unix)]
fn spawn_serve_web(sock: &Path) -> Result<Child, String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::process::CommandExt;

    let dir = sock.parent().ok_or("no cache directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(sock);
    let log = std::fs::File::create(dir.join("vscode-serve.log")).map_err(|e| e.to_string())?;

    // The server downloads on first use and its license terms apply to it:
    // https://aka.ms/vscode-server-license
    Command::new("code")
        .args(["serve-web", "--socket-path"])
        .arg(sock)
        .args([
            "--without-connection-token",
            "--accept-server-license-terms",
        ])
        .args(["--server-base-path", BASE_PATH])
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "VS Code's `code` command isn't on PATH on the machine running Mandor".to_string()
            }
            _ => format!("couldn't run `code serve-web`: {e}"),
        })
}

#[cfg(not(unix))]
fn spawn_serve_web(_sock: &Path) -> Result<Child, String> {
    Err("the web editor isn't supported on this platform".into())
}

/// Axum won't match `/editor/` (nothing after the slash) with `/editor/{*rest}`,
/// and that is exactly the URL the editor opens at, so all three are needed.
pub fn mount<S, H, T>(router: axum::Router<S>, handler: H) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    H: axum::handler::Handler<T, S> + Clone,
    T: 'static,
{
    use axum::routing::any;
    router
        .route(BASE_PATH, any(handler.clone()))
        .route("/editor/", any(handler.clone()))
        .route("/editor/{*rest}", any(handler))
}

pub async fn proxy(app: AppHandle, req: Request<Body>) -> Response<Body> {
    let started =
        tauri::async_runtime::spawn_blocking(move || app.state::<WebEditor>().ensure_running())
            .await;
    let sock = match started {
        Ok(Ok(sock)) => sock,
        Ok(Err(msg)) => return page(StatusCode::SERVICE_UNAVAILABLE, &msg, false),
        Err(e) => return page(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), false),
    };
    match forward(&sock, req).await {
        Ok(res) => res,
        Err(ForwardError::NotReady) => page(StatusCode::ACCEPTED, "Starting the editor…", true),
        Err(ForwardError::Failed(msg)) => page(StatusCode::BAD_GATEWAY, &msg, false),
    }
}

enum ForwardError {
    NotReady,
    Failed(String),
}

#[cfg(unix)]
async fn forward(sock: &Path, mut req: Request<Body>) -> Result<Response<Body>, ForwardError> {
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(sock)
        .await
        .map_err(|_| ForwardError::NotReady)?;
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| ForwardError::Failed(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    // Must be taken before the request is consumed. The Host header is passed
    // through untouched on purpose: VS Code derives the address its browser
    // client connects back to from it.
    let wants_upgrade = req.headers().contains_key(header::UPGRADE);
    let client_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut req));

    let mut res = sender
        .send_request(req)
        .await
        .map_err(|e| ForwardError::Failed(e.to_string()))?;

    if res.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let upstream_upgrade = hyper::upgrade::on(&mut res);
            tokio::spawn(async move {
                if let (Ok(client), Ok(upstream)) = (client_upgrade.await, upstream_upgrade.await) {
                    let _ = tokio::io::copy_bidirectional(
                        &mut TokioIo::new(client),
                        &mut TokioIo::new(upstream),
                    )
                    .await;
                }
            });
        }
    }
    Ok(res.map(Body::new))
}

#[cfg(not(unix))]
async fn forward(_sock: &Path, _req: Request<Body>) -> Result<Response<Body>, ForwardError> {
    Err(ForwardError::Failed(
        "the web editor isn't supported on this platform".into(),
    ))
}

fn page(status: StatusCode, message: &str, refresh: bool) -> Response<Body> {
    let reload = if refresh {
        "<script>setTimeout(()=>location.reload(),1500)</script>"
    } else {
        ""
    };
    let message = message.replace('&', "&amp;").replace('<', "&lt;");
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(format!(
            "<!doctype html><meta charset=utf-8><title>Editor</title>\
             <body style=\"font:15px system-ui;padding:2rem\">{message}{reload}"
        )))
        .unwrap()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::Router;
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixListener};

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mandor-editor-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plays VS Code: echoes the Host header, and turns an Upgrade request into
    /// a raw byte echo.
    async fn fake_upstream(sock: &Path) {
        let listener = UnixListener::bind(sock).unwrap();
        let app = mount(Router::new(), |mut req: Request<Body>| async move {
            if req.headers().contains_key(header::UPGRADE) {
                let on_upgrade = hyper::upgrade::on(&mut req);
                tokio::spawn(async move {
                    let mut io = hyper_util::rt::TokioIo::new(on_upgrade.await.unwrap());
                    let mut buf = [0u8; 64];
                    loop {
                        match io.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => io.write_all(&buf[..n]).await.unwrap(),
                        }
                    }
                });
                return Response::builder()
                    .status(StatusCode::SWITCHING_PROTOCOLS)
                    .header(header::UPGRADE, "websocket")
                    .header(header::CONNECTION, "Upgrade")
                    .body(Body::empty())
                    .unwrap();
            }
            let host = req
                .headers()
                .get(header::HOST)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            Response::new(Body::from(format!(
                "host={host} uri={}",
                req.uri().path_and_query().unwrap()
            )))
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    }

    async fn proxy_in_front_of(sock: PathBuf) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = mount(Router::new(), move |req: Request<Body>| {
            let sock = sock.clone();
            async move {
                match forward(&sock, req).await {
                    Ok(res) => res,
                    Err(ForwardError::NotReady) => page(StatusCode::ACCEPTED, "starting", true),
                    Err(ForwardError::Failed(m)) => page(StatusCode::BAD_GATEWAY, &m, false),
                }
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    fn raw(port: u16, request: &str) -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[tokio::test]
    async fn forwards_requests_with_the_original_host_and_uri() {
        let dir = scratch("http");
        let sock = dir.join("up.sock");
        fake_upstream(&sock).await;
        let port = proxy_in_front_of(sock).await;
        let res = tokio::task::spawn_blocking(move || {
            raw(
                port,
                "GET /editor/?folder=/a%20b HTTP/1.1\r\nHost: mandor.example\r\nConnection: close\r\n\r\n",
            )
        })
        .await
        .unwrap();
        assert!(res.starts_with("HTTP/1.1 200"), "{res}");
        assert!(
            res.ends_with("host=mandor.example uri=/editor/?folder=/a%20b"),
            "{res}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tunnels_upgraded_connections_both_ways() {
        let dir = scratch("ws");
        let sock = dir.join("up.sock");
        fake_upstream(&sock).await;
        let port = proxy_in_front_of(sock).await;
        let echoed = tokio::task::spawn_blocking(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            s.write_all(
                b"GET /editor/x HTTP/1.1\r\nHost: h\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
            )
            .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
            s.write_all(b"ping-through-the-proxy").unwrap();
            let mut back = [0u8; 22];
            s.read_exact(&mut back).unwrap();
            back
        })
        .await
        .unwrap();
        assert_eq!(&echoed, b"ping-through-the-proxy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reports_not_ready_while_the_editor_has_no_socket() {
        let dir = scratch("notready");
        let port = proxy_in_front_of(dir.join("missing.sock")).await;
        let res = tokio::task::spawn_blocking(move || {
            raw(
                port,
                "GET /editor/ HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
            )
        })
        .await
        .unwrap();
        assert!(res.starts_with("HTTP/1.1 202"), "{res}");
        assert!(res.contains("location.reload"), "{res}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn error_pages_escape_the_message() {
        let res = page(StatusCode::BAD_GATEWAY, "<script>x</script> & y", false);
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("&lt;script>x&lt;/script> &amp; y"), "{body}");
        assert!(!body.contains("<script>x"), "{body}");
        assert!(!body.contains("location.reload"), "{body}");
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    #[ignore = "needs VS Code's `code` CLI; downloads its server on first run"]
    async fn real_vscode_serves_through_the_proxy_and_stops_cleanly() {
        let cache = scratch("real");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let editor = WebEditor::default();
        let sock = editor.ensure_running().unwrap();
        let pgid = editor.child.lock().unwrap().as_ref().unwrap().id() as i32;
        let port = proxy_in_front_of(sock).await;

        let get = move |path: &'static str| {
            tokio::task::spawn_blocking(move || {
                raw(
                    port,
                    &format!("GET {path} HTTP/1.1\r\nHost: mandor.test:7420\r\nConnection: close\r\n\r\n"),
                )
            })
        };
        let mut page = String::new();
        for _ in 0..90 {
            page = get("/editor/?folder=/tmp").await.unwrap();
            if page.starts_with("HTTP/1.1 200") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(
            page.contains("&quot;remoteAuthority&quot;:&quot;mandor.test:7420"),
            "{page}"
        );
        assert!(
            page.contains("&quot;serverBasePath&quot;:&quot;/editor"),
            "{page}"
        );

        let asset = page
            .split('"')
            .find(|t| t.starts_with("/editor/") && t.ends_with("workbench.js"))
            .expect("workbench.js referenced")
            .to_string();
        let asset = get(Box::leak(asset.into_boxed_str())).await.unwrap();
        assert!(
            asset.starts_with("HTTP/1.1 200") && asset.len() > 10_000,
            "asset"
        );

        let commit = page
            .split('/')
            .find(|t| t.starts_with("stable-"))
            .expect("commit path")
            .to_string();
        let upgrade = tokio::task::spawn_blocking(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            write!(
                s,
                "GET /editor/{commit}?reconnectionToken=t&reconnection=false HTTP/1.1\r\nHost: mandor.test:7420\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && s.read_exact(&mut byte).is_ok() {
                head.push(byte[0]);
            }
            String::from_utf8_lossy(&head).into_owned()
        })
        .await
        .unwrap();
        assert!(upgrade.starts_with("HTTP/1.1 101"), "{upgrade}");

        editor.stop();
        let mut gone = false;
        for _ in 0..50 {
            // SAFETY: signal 0 only probes whether the group still exists.
            if unsafe { libc::kill(-pgid, 0) } == -1 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(gone, "VS Code server processes survived stop()");
        let _ = std::fs::remove_dir_all(&cache);
    }
}
