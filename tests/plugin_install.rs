//! Smart-source plugin install (`github:` / `npm:` / smarter errors), exercised
//! end-to-end against an in-process mock HTTP server. The `aivo` binary runs as
//! a child pointed at the mock via `AIVO_GITHUB_API` / `AIVO_NPM_REGISTRY`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

use serde_json::Value;
use tempfile::TempDir;

fn aivo_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_aivo") {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe().expect("current test exe");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("aivo");
    path
}

/// One mock route: requests whose path starts with `prefix` get this response.
struct Route {
    prefix: String,
    content_type: String,
    body: Vec<u8>,
}

fn route(prefix: &str, content_type: &str, body: Vec<u8>) -> Route {
    Route {
        prefix: prefix.to_string(),
        content_type: content_type.to_string(),
        body,
    }
}

/// Bind a loopback port, then serve `routes` from a background thread (one
/// response per connection, `Connection: close`). The thread is abandoned at
/// test exit. Returns `http://127.0.0.1:<port>`.
fn serve(listener: TcpListener, routes: Vec<Route>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain headers up to the blank line.
            loop {
                let mut h = String::new();
                match reader.read_line(&mut h) {
                    Ok(0) => break,
                    Ok(_) if h == "\r\n" || h == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let resp = routes.iter().find(|r| path.starts_with(&r.prefix));
            let (status, ctype, body): (u16, &str, &[u8]) = match resp {
                Some(r) => (200, &r.content_type, &r.body),
                None => (404, "text/plain", b"not found"),
            };
            let header = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
}

fn bind() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, format!("http://127.0.0.1:{port}"))
}

/// `aivo` child wired to a temp HOME and no proxy (so it reaches the mock).
fn aivo(home: &TempDir) -> Command {
    let mut cmd = Command::new(aivo_bin());
    cmd.env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .env("NO_COLOR", "1")
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("NO_PROXY", "127.0.0.1,localhost");
    cmd
}

fn tar_czf(workdir: &std::path::Path, entry: &str) -> Vec<u8> {
    let out = workdir.join("out.tgz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(workdir)
        .arg(entry)
        .status()
        .expect("run tar");
    assert!(status.success(), "tar -czf failed");
    std::fs::read(&out).unwrap()
}

fn registry(home: &TempDir) -> Value {
    let path = home.path().join(".config/aivo/plugins/.registry.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("registry file")).unwrap()
}

#[test]
fn github_install_resolves_release_asset() {
    // A release tarball wrapping an `aivo-widget` executable.
    let work = TempDir::new().unwrap();
    let exe = work.path().join("aivo-widget");
    std::fs::write(&exe, b"#!/bin/sh\necho 'widget ran'\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tgz = tar_czf(work.path(), "aivo-widget");

    let (listener, base) = bind();
    // Sole asset → accepted regardless of host triple.
    let release = format!(
        r#"{{"tag_name":"v1","assets":[{{"name":"aivo-widget.tar.gz","browser_download_url":"{base}/dl/aivo-widget.tar.gz","size":{}}}]}}"#,
        tgz.len()
    );
    serve(
        listener,
        vec![
            route(
                "/repos/o/aivo-widget/releases/latest",
                "application/json",
                release.into_bytes(),
            ),
            route("/dl/aivo-widget.tar.gz", "application/octet-stream", tgz),
        ],
    );

    let home = TempDir::new().unwrap();
    let out = aivo(&home)
        .env("AIVO_GITHUB_API", &base)
        .args(["plugins", "install", "github:o/aivo-widget"])
        .output()
        .expect("spawn aivo");
    assert!(
        out.status.success(),
        "install failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let installed = home.path().join(".config/aivo/plugins/aivo-widget");
    assert!(installed.exists(), "aivo-widget should be installed");
    // The scheme string is stored verbatim so `update` re-resolves.
    assert_eq!(
        registry(&home)["plugins"]["widget"]["source"],
        "github:o/aivo-widget"
    );

    // It dispatches.
    let run = aivo(&home).args(["widget"]).output().unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("widget ran"));
}

#[test]
fn npm_install_extracts_and_writes_a_node_shim() {
    // npm tarballs wrap everything in `package/`.
    let work = TempDir::new().unwrap();
    let pkg = work.path().join("package");
    std::fs::create_dir(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"aivo-foo","version":"1.0.0","bin":{"aivo-foo":"cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("cli.js"),
        "#!/usr/bin/env node\nconsole.log('foo');\n",
    )
    .unwrap();
    let tgz = tar_czf(work.path(), "package");

    let (listener, base) = bind();
    let meta = format!(
        r#"{{"dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"dist":{{"tarball":"{base}/tgz/aivo-foo.tgz"}},"bin":{{"aivo-foo":"cli.js"}}}}}}}}"#
    );
    serve(
        listener,
        vec![
            route("/aivo-foo", "application/json", meta.into_bytes()),
            route("/tgz/aivo-foo.tgz", "application/octet-stream", tgz),
        ],
    );

    let home = TempDir::new().unwrap();
    let out = aivo(&home)
        .env("AIVO_NPM_REGISTRY", &base)
        .args(["plugins", "install", "npm:aivo-foo"])
        .output()
        .expect("spawn aivo");
    assert!(
        out.status.success(),
        "npm install failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let shim = home.path().join(".config/aivo/plugins/aivo-foo");
    let shim_text = std::fs::read_to_string(&shim).expect("shim file");
    assert!(
        shim_text.contains("node"),
        "shim must invoke node: {shim_text}"
    );
    assert!(
        home.path()
            .join(".config/aivo/plugins/aivo-foo.d/cli.js")
            .exists(),
        "the npm payload should be extracted alongside the shim"
    );
    assert_eq!(registry(&home)["plugins"]["foo"]["source"], "npm:aivo-foo");
}

#[test]
fn direct_url_returning_html_is_rejected() {
    let (listener, base) = bind();
    serve(
        listener,
        vec![route(
            "/repo",
            "text/html",
            b"<!DOCTYPE html><html><body>a repo page</body></html>".to_vec(),
        )],
    );

    let home = TempDir::new().unwrap();
    let out = aivo(&home)
        .args([
            "plugins",
            "install",
            &format!("{base}/repo"),
            "--name",
            "broken",
        ])
        .output()
        .expect("spawn aivo");

    assert!(!out.status.success(), "installing an HTML page must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("HTML page"),
        "stderr should explain the HTML mistake:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !home
            .path()
            .join(".config/aivo/plugins/aivo-broken")
            .exists(),
        "no broken binary should be written",
    );
}
