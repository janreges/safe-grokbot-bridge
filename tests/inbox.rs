//! The inbox over HTTP and from the command line: what a bot reads, takes and gives back, and the
//! limits it sees.

mod common;

use std::time::SystemTime;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{Harness, extra_token, harness, toml_str};
use http_body_util::BodyExt;
use safe_grokbot_bridge::inbox::{self, Kind, NewMessage, Source};
use serde_json::json;

fn state_dir(h: &Harness) -> std::path::PathBuf {
    h.dir.path().join("state")
}

/// Stores a task directly in the state directory, as the CLI does.
fn send(h: &Harness, subject: &str, to: Option<&str>) -> inbox::Message {
    inbox::send(
        &state_dir(h),
        NewMessage {
            kind: Kind::Task,
            subject: subject.into(),
            project: None,
            to: to.map(str::to_owned),
            body: format!("# {subject}\n\nDetails."),
            attachments: vec![],
        },
        SystemTime::now(),
    )
    .unwrap()
}

async fn bytes_of(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn a_bot_reads_claims_and_gives_back_a_message() {
    let h = harness("[\"files\"]", "").await;
    let message = send(&h, "Zálohy", None);

    let (status, list) = h.get("/v1/inbox").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["messages"][0]["id"], message.id.as_str());
    assert_eq!(list["messages"][0]["state"], "unread");
    assert_eq!(
        (list["total"].as_u64(), list["truncated"].as_bool()),
        (Some(1), Some(false))
    );
    assert!(
        list["messages"][0].get("body").is_none(),
        "listings leave bodies out"
    );
    assert!(
        list.get("note").is_none(),
        "a full answer needs no note: {list}"
    );

    let (status, one) = h.get(&format!("/v1/inbox/{}", message.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(one["body"].as_str().unwrap().contains("Details."));

    let claim = format!("/v1/inbox/{}/claim", message.id);
    let (status, claimed) = h
        .post(&claim, json!({"bot": "Homelab", "note": "taking it"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["state"], "claimed");
    assert_eq!(claimed["claimed_by"], "Homelab");
    assert!(
        claimed.get("body").is_none(),
        "the claim answer leaves the body out"
    );

    let (_, unread) = h.get("/v1/inbox").await;
    assert_eq!(unread["total"], 0);
    assert!(
        unread["note"]
            .as_str()
            .unwrap()
            .contains("No message matches")
    );
    let (_, taken) = h.get("/v1/inbox?state=claimed").await;
    assert_eq!(taken["messages"][0]["claimed_by"], "Homelab");

    let (status, released) = h
        .post(
            &format!("/v1/inbox/{}/release", message.id),
            json!({"bot": "Homelab", "note": "for Adam"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert_eq!(released["state"], "released");
    assert_eq!(released["release_note"], "for Adam");
    let (_, again) = h.get("/v1/inbox?state=released").await;
    assert_eq!(again["messages"][0]["id"], message.id.as_str());
}

#[tokio::test]
async fn a_second_bot_is_refused_and_told_who_holds_the_message() {
    let h = harness("[\"files\"]", "").await;
    let message = send(&h, "Jedna věc", None);
    let claim = format!("/v1/inbox/{}/claim", message.id);
    let release = format!("/v1/inbox/{}/release", message.id);
    assert_eq!(
        h.post(&claim, json!({"bot": "Homelab"})).await.0,
        StatusCode::OK
    );

    let (status, body) = h.post(&claim, json!({"bot": "Chief of Staff"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "already_claimed");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Homelab")
    );

    let (status, body) = h.post(&release, json!({"bot": "Chief of Staff"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "not_yours");
}

#[tokio::test]
async fn a_model_only_token_cannot_read_or_claim_the_inbox() {
    let (brain, toml) = extra_token("brain", "models_only = true");
    let h = harness("[\"files\"]", &toml).await;
    let message = send(&h, "Private", None);

    let (status, _) = h.request("GET", "/v1/inbox", None, Some(&brain)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h
        .request(
            "POST",
            &format!("/v1/inbox/{}/claim", message.id),
            Some(json!({"bot": "Brain"})),
            Some(&brain),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        inbox::get(&state_dir(&h), &message.id).unwrap().state,
        inbox::State::Unread,
        "a refused claim changes nothing"
    );
}

#[tokio::test]
async fn an_attachment_comes_back_byte_for_byte() {
    let h = harness("[\"files\"]", "").await;
    let source = h.dir.path().join("report.bin");
    let content: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &content).unwrap();
    let message = inbox::send(
        &state_dir(&h),
        NewMessage {
            kind: Kind::Task,
            subject: "With a file".into(),
            project: None,
            to: None,
            body: "See the attachment.".into(),
            attachments: vec![Source {
                name: "report.bin".into(),
                path: source,
            }],
        },
        SystemTime::now(),
    )
    .unwrap();

    let (status, one) = h.get(&format!("/v1/inbox/{}", message.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["attachments"][0]["size"], content.len());
    assert_eq!(
        one["attachments"][0]["sha256"],
        message.attachments[0].sha256.as_str()
    );

    let response = h
        .send(get(
            &format!("/v1/inbox/{}/attachments/report.bin", message.id),
            &h.token,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("content-disposition"));
    assert_eq!(bytes_of(response).await, content);

    let missing = h
        .send(get(
            &format!("/v1/inbox/{}/attachments/other.bin", message.id),
            &h.token,
        ))
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_listing_says_when_it_is_cut_short_and_what_to_do_next() {
    let h = harness("[\"files\"]", "").await;
    for subject in ["one", "two", "three"] {
        send(&h, subject, None);
    }

    let (status, page) = h.get("/v1/inbox?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        (page["total"].as_u64(), page["truncated"].as_bool()),
        (Some(3), Some(true))
    );
    assert!(page["note"].as_str().unwrap().contains("offset=2"));
    // The note names the next offset. Messages sent within one second may come in either order,
    // so the two pages together must hold each of the three messages exactly once.
    let (_, next) = h.get("/v1/inbox?limit=2&offset=2").await;
    assert_eq!(next["messages"].as_array().unwrap().len(), 1);
    let mut subjects: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .chain(next["messages"].as_array().unwrap())
        .map(|message| message["subject"].as_str().unwrap())
        .collect();
    subjects.sort();
    assert_eq!(subjects, ["one", "three", "two"]);
    assert_eq!(next["truncated"].as_bool(), Some(false));
    assert!(next["note"].is_null());
    // Past the last message the page is empty, and it says so.
    let (_, past) = h.get("/v1/inbox?limit=2&offset=9").await;
    assert!(past["messages"].as_array().unwrap().is_empty());
    assert!(
        past["note"]
            .as_str()
            .unwrap()
            .contains("No message at offset 9")
    );

    assert_eq!(h.get("/v1/inbox?limit=0").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.get("/v1/inbox?limit=201").await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, body) = h.get("/v1/inbox?state=waiting").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unread")
    );
}

#[tokio::test]
async fn a_message_for_another_bot_is_hidden_and_cannot_be_taken() {
    let h = harness("[\"files\"]", "").await;
    let message = send(&h, "For Adam", Some("Adam"));

    let (_, homelab) = h.get("/v1/inbox?to=Homelab").await;
    assert_eq!(
        homelab["total"], 0,
        "a bot does not see messages for another bot"
    );
    let (_, adam) = h.get("/v1/inbox?to=Adam").await;
    assert_eq!(adam["total"], 1);

    let (status, body) = h
        .post(
            &format!("/v1/inbox/{}/claim", message.id),
            json!({"bot": "Homelab"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "addressed_to_other");
}

#[tokio::test]
async fn an_id_that_is_not_a_message_id_never_reaches_a_path() {
    let h = harness("[\"files\"]", "").await;
    std::fs::write(h.dir.path().join("secret.txt"), b"not for the inbox").unwrap();
    let (status, body) = h.get("/v1/inbox/..%2F..%2Fsecret.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn a_message_sent_from_the_command_line_is_listed_with_its_project() {
    let h = harness("[\"files\"]", "").await;
    let config = h.dir.path().join("cli.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nstate_dir = {}\n[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\nallow = [\"files\"]\n",
            toml_str(&state_dir(&h)),
            safe_grokbot_bridge::auth::hash_token(&h.token),
            toml_str(&h.root),
        ),
    )
    .unwrap();
    let body = h.dir.path().join("body.md");
    std::fs::write(&body, "Sent with the CLI.").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_safe-grokbot-bridge"))
        .current_dir(&h.root)
        .args(["inbox", "send", "--config"])
        .arg(&config)
        .args(["--kind", "task", "--subject", "From the CLI", "--body-file"])
        .arg(&body)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (status, list) = h.get("/v1/inbox?state=all").await;
    assert_eq!(status, StatusCode::OK);
    let stored = &list["messages"][0];
    assert_eq!(stored["subject"], "From the CLI");
    assert_eq!(stored["project"], h.root.to_str().unwrap(), "{stored}");
}

/// A GET request that returns the raw response, for downloads.
fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}
