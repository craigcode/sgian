//! Shared context notes (docs/design/shared-context-notes.md, step 1): the
//! pure pieces (names, front matter, dates) and the daemon path end to end
//! (write, list with scrub, ledger, dossier, remove, credentials, caps).

use super::*;
use crate::notes::*;

#[test]
fn note_names_front_matter_and_dates_are_pure() {
    assert_eq!(civil_date(0), "1970-01-01");
    // 1e9 seconds: 2001-09-09T01:46:40Z.
    assert_eq!(civil_date(1_000_000_000_000), "2001-09-09");
    // 2024-02-29T12:00:00Z (a leap day).
    assert_eq!(civil_date(1_709_208_000_000), "2024-02-29");

    assert_eq!(note_slug("Flaky auth test!"), "flaky-auth-test");
    assert_eq!(
        note_slug("  Decided: PG over SQLite  "),
        "decided-pg-over-sqlite"
    );
    assert_eq!(note_slug("***"), "note");
    assert!(note_slug(&"word ".repeat(40)).len() <= 48);

    let mut taken = HashSet::new();
    assert_eq!(
        note_file_name(1_000_000_000_000, "Flaky auth test", &taken),
        "2001-09-09-flaky-auth-test.md"
    );
    taken.insert("2001-09-09-flaky-auth-test.md".to_string());
    assert_eq!(
        note_file_name(1_000_000_000_000, "Flaky auth test", &taken),
        "2001-09-09-flaky-auth-test-2.md"
    );

    let meta = NoteMeta {
        title: "Flaky auth test".into(),
        holder: "craig@mac".into(),
        pane: Some("pane-3".into()),
        written_at_ms: 1_000_000_000_000,
    };
    let text = render_note(&meta, "The auth test flakes when the clock skews.");
    assert!(text.starts_with("---\ntitle: Flaky auth test\nholder: craig@mac\npane: pane-3\n"));
    let (parsed, body) = parse_note(&text);
    assert_eq!(parsed, Some(meta));
    assert_eq!(body, "The auth test flakes when the clock skews.\n");

    // Not the daemon's front matter: the whole text is the body.
    assert_eq!(parse_note("just text"), (None, "just text"));
    assert_eq!(
        parse_note("---\ntitle: only\n---\n\nbody").0,
        None,
        "a block without holder and timestamp is not ours"
    );

    assert!(validate_note_file("2026-10-09-flaky.md").is_ok());
    assert!(validate_note_file("../escape.md").is_err());
    assert!(validate_note_file(".hidden.md").is_err());
    assert!(validate_note_file("notes.txt").is_err());
    assert!(validate_note_file(".md").is_err());
}

#[test]
fn project_note_args_parse() {
    let args = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };
    let parsed = parse_project_args(&args("notes feature")).expect("notes");
    assert_eq!(parsed.verb, ProjectVerb::Notes);
    assert_eq!(parsed.name.as_deref(), Some("feature"));

    let parsed = parse_project_args(&args(
        "note add feature --title Flaky --body text --pane 2 --as kranz-run-1",
    ))
    .expect("note add");
    assert_eq!(parsed.verb, ProjectVerb::NoteAdd);
    assert_eq!(parsed.name.as_deref(), Some("feature"));
    assert_eq!(parsed.title.as_deref(), Some("Flaky"));
    assert_eq!(parsed.body.as_deref(), Some("text"));
    assert_eq!(parsed.pane.as_deref(), Some("2"));
    assert_eq!(parsed.holder.as_deref(), Some("kranz-run-1"));

    let parsed = parse_project_args(&args("note rm feature 2026-10-09-flaky.md")).expect("rm");
    assert_eq!(parsed.verb, ProjectVerb::NoteRm);
    assert_eq!(parsed.note_file.as_deref(), Some("2026-10-09-flaky.md"));

    assert!(
        parse_project_args(&args("note add feature")).is_err(),
        "title required"
    );
    assert!(
        parse_project_args(&args("note add feature --title T --body b --file f")).is_err(),
        "body and file are alternatives"
    );
    assert!(parse_project_args(&args("note rm feature")).is_err());
    assert!(parse_project_args(&args("note burn feature")).is_err());
    assert!(
        parse_project_args(&args("show feature --title T")).is_err(),
        "note flags are refused elsewhere"
    );
    assert!(
        parse_project_args(&args("ledger feature --as me")).is_err(),
        "--as is a note flag"
    );
    assert_eq!(
        request_scope(&DaemonRequest::ProjectNotes { name: "f".into() }),
        ClientScope::Read
    );
    assert_eq!(
        request_scope(&DaemonRequest::ProjectNoteAdd {
            name: "f".into(),
            title: "t".into(),
            body: "b".into(),
            holder: "h".into(),
            pane_id: None
        }),
        ClientScope::Write
    );
}

#[test]
fn project_notes_are_written_listed_scrubbed_and_ledgered() {
    let repo = tempfile::tempdir().expect("tempdir");
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let _: Project = client
        .request(DaemonRequest::ProjectCreate {
            name: "feature".into(),
            goal: Some("ship it".into()),
            repo: Some(repo.path().display().to_string()),
        })
        .expect("create project");

    // A note through the daemon: attributed, hashed, ledgered.
    let added: Value = client
        .request(DaemonRequest::ProjectNoteAdd {
            name: "feature".into(),
            title: "Flaky auth test".into(),
            body: "The auth test flakes when\u{200B}the clock skews.".into(),
            holder: "craig@mac".into(),
            pane_id: Some("pane-1".into()),
        })
        .expect("add note");
    let file = added["file"].as_str().expect("file").to_string();
    assert!(file.ends_with("-flaky-auth-test.md"), "{file}");
    let notes_dir = repo
        .path()
        .join(".sgian")
        .join("projects")
        .join("feature")
        .join("notes");
    assert!(notes_dir.join(&file).is_file());
    let stored = fs::read_to_string(notes_dir.join(&file)).expect("read note");
    assert!(stored.contains("holder: craig@mac\npane: pane-1\n"));
    assert!(
        stored.contains('\u{200B}'),
        "the file keeps what was written; the scrub is on read"
    );

    // A file written outside the daemon: listed with evidence `file`.
    fs::write(
        notes_dir.join("2020-01-01-raw.md"),
        "\u{1b}[31mred\u{1b}[0m text\n",
    )
    .expect("write raw note");

    let listing: Value = client
        .request(DaemonRequest::ProjectNotes {
            name: "feature".into(),
        })
        .expect("list notes");
    assert_eq!(listing["format"], json!(NOTES_FORMAT));
    assert_eq!(listing["total"], json!(2));
    let notes = listing["notes"].as_array().expect("notes");
    assert_eq!(notes[0]["file"], json!(file));
    assert_eq!(notes[0]["evidence"], json!("daemon"));
    assert_eq!(notes[0]["holder"], json!("craig@mac"));
    assert_eq!(notes[0]["pane"], json!("pane-1"));
    assert_eq!(notes[0]["title"], json!("Flaky auth test"));
    assert_eq!(
        notes[0]["body"],
        json!("The auth test flakes whenthe clock skews."),
        "the zero-width space is removed from what a person reads"
    );
    assert_eq!(notes[0]["tricks"]["invisible"], json!(1));
    assert_eq!(notes[0]["hash"], added["hash"]);
    assert_eq!(notes[1]["file"], json!("2020-01-01-raw.md"));
    assert_eq!(notes[1]["evidence"], json!("file"));
    assert!(notes[1]["holder"].is_null());
    assert_eq!(notes[1]["title"], json!("2020-01-01-raw"));
    assert_eq!(
        notes[1]["body"],
        json!("red text"),
        "colour codes are dropped silently and are not tricks"
    );
    assert!(notes[1]["tricks"].is_null());
    assert_eq!(listing["tricks"]["invisible"], json!(1));

    // The project's own ledger carries the write; the merged ledger and the
    // dossier include it.
    let ledger: Value = client
        .request(DaemonRequest::ProjectLedger {
            name: "feature".into(),
            limit: 0,
        })
        .expect("project ledger");
    let records = ledger["records"].as_array().expect("records");
    let note_added = records
        .iter()
        .find(|record| record["type"] == json!("note.added"))
        .expect("note.added record");
    assert_eq!(note_added["pane_id"], json!("project-feature"));
    assert_eq!(note_added["payload"]["file"], json!(file));
    assert_eq!(note_added["payload"]["holder"], json!("craig@mac"));
    assert_eq!(note_added["payload"]["hash"], added["hash"]);
    let dossier: Value = client
        .request(DaemonRequest::ProjectDossier {
            name: "feature".into(),
            lines: 0,
        })
        .expect("dossier");
    assert_eq!(dossier["notes"]["total"], json!(2));
    assert_eq!(dossier["ledger"]["chain"]["verified"], json!(true));
    assert_eq!(dossier["ledger"]["chain"]["records"], json!(1));

    // Caps and shape.
    let too_long: Result<Value, String> = client.request(DaemonRequest::ProjectNoteAdd {
        name: "feature".into(),
        title: "big".into(),
        body: "a".repeat(NOTE_MAX_BYTES + 1),
        holder: "craig@mac".into(),
        pane_id: None,
    });
    assert!(too_long.unwrap_err().contains("longer than"));
    let two_lines: Result<Value, String> = client.request(DaemonRequest::ProjectNoteAdd {
        name: "feature".into(),
        title: "one\ntwo".into(),
        body: "b".into(),
        holder: "craig@mac".into(),
        pane_id: None,
    });
    assert!(two_lines.unwrap_err().contains("one line"));
    let escape: Result<Value, String> = client.request(DaemonRequest::ProjectNoteRemove {
        name: "feature".into(),
        file: "../../escape.md".into(),
        holder: "craig@mac".into(),
    });
    assert!(escape.unwrap_err().contains("may only contain"));
    let unknown: Result<Value, String> = client.request(DaemonRequest::ProjectNotes {
        name: "nope".into(),
    });
    assert!(unknown.unwrap_err().contains("unknown project"));

    // Credentials: a viewer reads, cannot write; a writer writes as itself.
    let viewer: Value = client
        .request(DaemonRequest::IdentityIssue {
            holder: "phone".into(),
            scopes: vec![],
        })
        .expect("issue viewer");
    let mut viewer_conn =
        connect_with_client_token(&daemon, viewer["token"].as_str().expect("token"))
            .expect("viewer hello");
    let seen = viewer_conn
        .request(&DaemonRequest::ProjectNotes {
            name: "feature".into(),
        })
        .expect("viewer lists");
    assert!(seen.ok, "{seen:?}");
    let refused = viewer_conn
        .request(&DaemonRequest::ProjectNoteAdd {
            name: "feature".into(),
            title: "t".into(),
            body: "b".into(),
            holder: "phone".into(),
            pane_id: None,
        })
        .expect("refusal is a response");
    assert!(refused
        .error
        .as_deref()
        .unwrap_or("")
        .contains("read-only credential"));
    let writer: Value = client
        .request(DaemonRequest::IdentityIssue {
            holder: "kranz-run-7".into(),
            scopes: vec!["write".into()],
        })
        .expect("issue writer");
    let mut writer_conn =
        connect_with_client_token(&daemon, writer["token"].as_str().expect("token"))
            .expect("writer hello");
    let mismatch = writer_conn
        .request(&DaemonRequest::ProjectNoteAdd {
            name: "feature".into(),
            title: "t".into(),
            body: "b".into(),
            holder: "craig@mac".into(),
            pane_id: None,
        })
        .expect("refusal is a response");
    assert!(
        mismatch
            .error
            .as_deref()
            .unwrap_or("")
            .contains("does not match"),
        "{mismatch:?}"
    );
    let as_self = writer_conn
        .request(&DaemonRequest::ProjectNoteAdd {
            name: "feature".into(),
            title: "Decided PG over SQLite".into(),
            body: "Postgres: the migration tooling is already there.".into(),
            holder: "kranz-run-7".into(),
            pane_id: None,
        })
        .expect("writer adds");
    assert!(as_self.ok, "{as_self:?}");
    assert_eq!(as_self.result["holder"], json!("kranz-run-7"));

    // Remove: ledgered with the hash of what went.
    let removed: Value = client
        .request(DaemonRequest::ProjectNoteRemove {
            name: "feature".into(),
            file: file.clone(),
            holder: "craig@mac".into(),
        })
        .expect("remove note");
    assert_eq!(removed["hash"], added["hash"]);
    assert!(!notes_dir.join(&file).exists());
    let again: Result<Value, String> = client.request(DaemonRequest::ProjectNoteRemove {
        name: "feature".into(),
        file: file.clone(),
        holder: "craig@mac".into(),
    });
    assert!(again.unwrap_err().contains("no note"));
    let ledger: Value = client
        .request(DaemonRequest::ProjectLedger {
            name: "feature".into(),
            limit: 0,
        })
        .expect("project ledger");
    let kinds: Vec<&str> = ledger["records"]
        .as_array()
        .expect("records")
        .iter()
        .filter(|record| record["pane_id"] == json!("project-feature"))
        .filter_map(|record| record["type"].as_str())
        .collect();
    assert_eq!(kinds, vec!["note.added", "note.added", "note.removed"]);

    // The ctl document is bounded and marks the guarded note.
    let listing: Value = client
        .request(DaemonRequest::ProjectNotes {
            name: "feature".into(),
        })
        .expect("list notes");
    let mut out = Vec::new();
    write_notes_document(&mut out, &listing).expect("document");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.starts_with("feature\t2 note(s)\t"), "{text}");
    assert!(text.contains("## 2020-01-01-raw.md\tfile\t-\n2020-01-01-raw\n\nred text\n"));
    assert!(text.contains("\tkranz-run-7\t"));

    daemon.shutdown();
}
