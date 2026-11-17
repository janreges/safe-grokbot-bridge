//! File transfer: uploads into a workspace's incoming folder, and downloads.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use common::{Harness, form, harness, json_body, url};
use futures_util::StreamExt;
use serde_json::Value;

const LIMIT: usize = 96_000_000;

fn upload_request(token: &str, dir: &Path, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(url("/v1/files/upload", "dir", dir))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "multipart/form-data; boundary=XBOUND")
        .body(body.into())
        .unwrap()
}

async fn upload(h: &Harness, dir: &Path, body: Vec<u8>) -> (StatusCode, Value) {
    let response = h.send(upload_request(&h.token, dir, body)).await;
    (response.status(), json_body(response).await)
}

/// The files in the incoming folder, apart from its `.gitignore`.
fn stored(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join(".sgb-inbox")) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.ends_with(".gitignore"))
        .collect();
    files.sort();
    files
}

/// `2026-11-19.14-32-53.k7q2.<extension>`
fn has_stored_shape(name: &str, extension: &str) -> bool {
    let Some(stem) = name.strip_suffix(&format!(".{extension}")) else {
        return false;
    };
    let parts: Vec<&str> = stem.split('.').collect();
    let digits = |text: &str, dashes: &[usize]| {
        text.char_indices().all(|(i, c)| {
            if dashes.contains(&i) {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
    };
    parts.len() == 3
        && parts[0].len() == 10
        && digits(parts[0], &[4, 7])
        && parts[1].len() == 8
        && digits(parts[1], &[2, 5])
        && parts[2].len() == 4
        && parts[2]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[tokio::test]
async fn an_upload_lands_in_the_inbox_under_generated_names() {
    let h = harness("[\"files\", \"upload\"]", "").await;
    let sub = h.root.join("app");
    std::fs::create_dir(&sub).unwrap();
    let pdf = b"%PDF-1.7 \x00\x01 binary".to_vec();
    let archive = vec![7u8; 70_000];
    let body = form(&[
        ("file", Some(("report.PDF", "application/pdf")), &pdf),
        (
            "file",
            Some(("backup.tar.gz", "application/gzip")),
            &archive,
        ),
        ("file", Some(("../../escape.sh", "text/plain")), b"echo hi"),
    ]);
    let (status, body) = upload(&h, &sub, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let inbox = h.root.join(".sgb-inbox");
    assert_eq!(body["inbox"], inbox.display().to_string());
    let (_, info) = h.get("/v1/info").await;
    assert_eq!(info["limits"]["max_transfer_bytes"], LIMIT);
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("until someone deletes"),
        "{body}"
    );
    let files = body["files"].as_array().unwrap();
    let expected = [
        ("report.PDF", "pdf", &pdf[..]),
        ("backup.tar.gz", "tar.gz", &archive[..]),
        ("../../escape.sh", "sh", b"echo hi"),
    ];
    assert_eq!(files.len(), expected.len());
    for (file, (name, extension, bytes)) in files.iter().zip(expected) {
        assert_eq!(file["name"], name);
        assert_eq!(file["size"], bytes.len());
        let path = PathBuf::from(file["path"].as_str().unwrap());
        assert_eq!(path.parent(), Some(inbox.as_path()));
        let stored_name = path.file_name().unwrap().to_str().unwrap();
        assert!(has_stored_shape(stored_name, extension), "{stored_name}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
    assert_eq!(stored(&h.root).len(), 3);
    assert_eq!(
        std::fs::read_to_string(inbox.join(".gitignore")).unwrap(),
        "*\n"
    );
    assert!(!h.root.join("escape.sh").exists() && !h.dir.path().join("escape.sh").exists());
}

#[tokio::test]
async fn uploads_are_refused_with_a_reason() {
    let h = harness("[\"files\", \"upload\"]", "").await;
    let file = form(&[("file", Some(("a.txt", "text/plain")), b"a")]);
    let code = |body: &Value| {
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    let (status, body) = upload(&h, h.dir.path(), file.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = upload(&h, &h.root.join("missing"), file.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let unnamed = form(&[
        ("file", Some(("a.txt", "text/plain")), b"a"),
        ("note", None, b"no file name"),
    ]);
    let (status, body) = upload(&h, &h.root, unnamed).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "invalid_request".into())
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`note`"),
        "{body}"
    );

    let parts: Vec<common::Part> = (0..21)
        .map(|_| ("file", Some(("a.txt", "text/plain")), &b"a"[..]))
        .collect();
    let (status, body) = upload(&h, &h.root, form(&parts)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("20"),
        "{body}"
    );

    let (status, body) = upload(&h, &h.root, form(&[])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no file"),
        "{body}"
    );

    let json = Request::builder()
        .method("POST")
        .uri(url("/v1/files/upload", "dir", &h.root))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = h.send(json).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("multipart"),
        "{body}"
    );

    let oversized = Request::builder()
        .method("POST")
        .uri(url("/v1/files/upload", "dir", &h.root))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "multipart/form-data; boundary=XBOUND")
        .header("content-length", (LIMIT + 1).to_string())
        .body(Body::empty())
        .unwrap();
    let response = h.send(oversized).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert!(stored(&h.root).is_empty(), "{:?}", stored(&h.root));

    let without = harness("[\"files\"]", "").await;
    let (status, body) = upload(&without, &without.root, file.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`upload`"),
        "{body}"
    );
    // Without `upload`, the answer does not tell a file from a folder.
    let sessions_only = harness("[\"sessions\"]", "").await;
    std::fs::write(sessions_only.root.join("notes.txt"), "x").unwrap();
    let (status, body) = upload(&sessions_only, &sessions_only.root.join("notes.txt"), file).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_inbox_that_is_a_link_is_refused() {
    let h = harness("[\"upload\"]", "").await;
    let elsewhere = h.dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, h.root.join(".sgb-inbox")).unwrap();
    let file = form(&[("file", Some(("a.txt", "text/plain")), b"a")]);
    let (status, body) = upload(&h, &h.root, file).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "inbox_unusable");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// A body that sends `first` and then fails or stalls. The failure comes after a pause, so that
/// the files before it are stored first (the form parser reads ahead whatever is ready).
fn broken_body(first: Vec<u8>, then_fail: bool) -> Body {
    let head = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(first))]);
    if then_fail {
        Body::from_stream(head.chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(std::io::Error::other("the client went away"))
        })))
    } else {
        Body::from_stream(head.chain(futures_util::stream::pending()))
    }
}

#[tokio::test]
async fn a_failed_upload_leaves_nothing_behind() {
    let h = harness("[\"upload\"]", "").await;
    // The first file is complete; the body breaks off inside the second one.
    let mut first = form(&[("file", Some(("one.txt", "text/plain")), b"complete")]);
    first.truncate(first.len() - "--XBOUND--\r\n".len());
    first.extend_from_slice(
        b"--XBOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"two.txt\"\r\n\r\npartial",
    );
    let response = h
        .send(upload_request(&h.token, &h.root, broken_body(first, true)))
        .await;
    assert!(response.status().is_client_error(), "{}", response.status());
    let body = json_body(response).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Nothing was stored"),
        "{body}"
    );
    assert!(stored(&h.root).is_empty(), "{:?}", stored(&h.root));

    // Above the limit, without a declared length: the second file is too large.
    let small = form(&[("file", Some(("one.txt", "text/plain")), b"complete")]);
    let mut head = small[..small.len() - "--XBOUND--\r\n".len()].to_vec();
    head.extend_from_slice(
        b"--XBOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\r\n",
    );
    let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
    let big = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(head))]).chain(
        futures_util::stream::iter((0..100).map(move |_| chunk.clone())).then(|chunk| async {
            tokio::task::yield_now().await;
            Ok(chunk)
        }),
    );
    let response = h
        .send(upload_request(&h.token, &h.root, Body::from_stream(big)))
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "too_large");
    assert!(stored(&h.root).is_empty(), "{:?}", stored(&h.root));
}

#[tokio::test]
async fn hanging_up_leaves_nothing_behind() {
    let h = harness("[\"upload\"]", "").await;
    let mut first = form(&[("file", Some(("one.txt", "text/plain")), b"complete")]);
    first.truncate(first.len() - "--XBOUND--\r\n".len());
    first.extend_from_slice(
        b"--XBOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"two.txt\"\r\n\r\npartial",
    );
    let request = upload_request(&h.token, &h.root, broken_body(first, false));
    let router = h.router.clone();
    let mut request = request;
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let call = tokio::spawn(async move {
        use tower::ServiceExt;
        router.oneshot(request).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while stored(&h.root).len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the upload never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    call.abort();
    let _ = call.await;
    assert!(stored(&h.root).is_empty(), "{:?}", stored(&h.root));
}

#[tokio::test]
async fn transfers_are_limited() {
    let h = harness("[\"upload\"]", "").await;
    let mut calls = Vec::new();
    for _ in 0..4 {
        let head = b"--XBOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"slow.bin\"\r\n\r\nstart".to_vec();
        let mut request = upload_request(&h.token, &h.root, broken_body(head, false));
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let router = h.router.clone();
        calls.push(tokio::spawn(async move {
            use tower::ServiceExt;
            router.oneshot(request).await
        }));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while stored(&h.root).len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the uploads never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let file = form(&[("file", Some(("a.txt", "text/plain")), b"a")]);
    let response = h.send(upload_request(&h.token, &h.root, file)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "5");
    assert_eq!(json_body(response).await["error"]["code"], "transfers_busy");
    for call in calls {
        call.abort();
    }
}

/// Over a real connection, from a client that sends its whole body before it reads the answer:
/// a refusal must arrive as the JSON error, not as a reset.
#[tokio::test]
async fn a_refused_client_still_sending_gets_the_answer() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = harness("[\"files\"]", "").await;
    let address = h.serve().await.replace("http://", "");
    let file = vec![b'x'; 30 * 1024 * 1024];
    let body = form(&[("file", Some(("big.bin", "application/octet-stream")), &file)]);
    let target = url("/v1/files/upload", "dir", &h.root);
    let head = format!(
        "POST {target} HTTP/1.1\r\nhost: {address}\r\nauthorization: Bearer {}\r\ncontent-type: multipart/form-data; boundary=XBOUND\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        h.token,
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    let answer = async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await?;
        Ok::<_, std::io::Error>(answer)
    }
    .await
    .expect("the refusal arrives instead of a reset");
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert!(answer.contains("`upload`"), "{answer}");
}

#[tokio::test]
async fn models_only_tokens_cannot_transfer() {
    let (brain, toml) = common::extra_token("brain", "models_only = true");
    let h = harness("[\"upload\", \"download\"]", &toml).await;
    std::fs::write(h.root.join("a.bin"), b"a").unwrap();
    let file = form(&[("file", Some(("a.txt", "text/plain")), b"a")]);
    let response = h.send(upload_request(&brain, &h.root, file)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let (status, _) = h
        .request(
            "GET",
            &url("/v1/files/download", "path", &h.root.join("a.bin")),
            None,
            Some(&brain),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn download(h: &Harness, path: &Path) -> axum::response::Response {
    h.send(
        Request::builder()
            .uri(url("/v1/files/download", "path", path))
            .header("authorization", format!("Bearer {}", h.token))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn bytes_of(response: axum::response::Response) -> Vec<u8> {
    use http_body_util::BodyExt;
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn a_download_arrives_byte_for_byte() {
    let h = harness("[\"download\"]", "").await;
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let path = h.root.join("data.bin");
    std::fs::write(&path, &data).unwrap();
    let response = download(&h, &path).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["content-length"], data.len().to_string().as_str());
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"data.bin\"; filename*=UTF-8''data.bin"
    );
    assert_eq!(bytes_of(response).await, data);

    // A name that is valid on every platform; quotes and backslashes are covered in the unit
    // tests of `disposition`, because Windows does not allow them in file names.
    let odd = h.root.join("zpráva 1.pdf");
    std::fs::write(&odd, b"%PDF").unwrap();
    let response = download(&h, &odd).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"zpr_va 1.pdf\"; filename*=UTF-8''zpr%C3%A1va%201.pdf"
    );
}

#[tokio::test]
async fn downloads_are_refused_with_a_reason() {
    let h = harness("[\"download\"]", "").await;
    let status_and_code = |response: axum::response::Response| async move {
        let status = response.status();
        let body = json_body(response).await;
        (
            status,
            body["error"]["code"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    };
    std::fs::write(h.root.join(".env"), "SECRET=1").unwrap();
    assert_eq!(
        status_and_code(download(&h, &h.root.join(".env")).await).await,
        (StatusCode::FORBIDDEN, "denied".to_string())
    );
    std::fs::create_dir(h.root.join("folder")).unwrap();
    assert_eq!(
        download(&h, &h.root.join("folder")).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        download(&h, &h.root.join("missing.bin")).await.status(),
        StatusCode::NOT_FOUND
    );
    let big = std::fs::File::create(h.root.join("big.bin")).unwrap();
    big.set_len(LIMIT as u64 + 1).unwrap();
    let response = download(&h, &h.root.join("big.bin")).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = json_body(response).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("96000001"),
        "{body}"
    );
    #[cfg(unix)]
    {
        let outside = h.dir.path().join("outside.bin");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, h.root.join("link.bin")).unwrap();
        assert_eq!(
            download(&h, &h.root.join("link.bin")).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    let without = harness("[\"files\"]", "").await;
    std::fs::write(without.root.join("data.bin"), b"\x00\x01binary").unwrap();
    let response = download(&without, &without.root.join("data.bin")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json_body(response).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`download`"),
        "{body}"
    );
    // `files` alone still refuses binary files, as before.
    let (status, body) = without
        .get(&url(
            "/v1/files/content",
            "path",
            &without.root.join("data.bin"),
        ))
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &Value::from("binary_file")
        )
    );
}

#[tokio::test]
async fn an_uploaded_file_can_be_downloaded_again() {
    let h = harness("[\"upload\", \"download\"]", "").await;
    let data = vec![0u8, 1, 2, 255, 254, 0, 9];
    let (status, body) = upload(
        &h,
        &h.root,
        form(&[(
            "file",
            Some(("blob.dat", "application/octet-stream")),
            &data,
        )]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let path = PathBuf::from(body["files"][0]["path"].as_str().unwrap());
    let response = download(&h, &path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, data);
}
