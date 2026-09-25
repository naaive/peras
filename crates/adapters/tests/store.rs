use agent_adapters::*;
use agent_proto::*;
use agent_runtime::{BlobStore, JournalStore, MemoryStore, StoreError};

fn ev(seq: Seq, body: Event) -> Envelope<Event> {
    Envelope {
        id: EventId::new(format!("01E{seq}")),
        parent: None,
        seq,
        at: Timestamp(seq * 10),
        origin: Origin::User,
        trust: Trust::User,
        audience: Audience::Both,
        schema: EVENT_SCHEMA,
        body,
        rendered: None,
    }
}

fn msg(t: &str) -> Event {
    Event::UserMessage { text: t.into(), attachments: vec![] }
}

#[tokio::test]
async fn journal_append_load_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.db");
    let db = Sqlite::open(&path).unwrap();
    let j = db.journal.clone();
    let s = SessionId::new("s1");
    assert_eq!(j.next_seq(&s).await.unwrap(), 0);
    // No lease yet: gen 0 is not a valid holder after acquisition.
    let g1 = j.acquire_lease(&s).await.unwrap();
    assert_eq!(g1, LeaseGen(1));
    j.append(&s, g1, 0, &[ev(0, msg("a")), ev(1, msg("b"))]).await.unwrap();
    assert_eq!(j.next_seq(&s).await.unwrap(), 2);

    // Wrong expected seq.
    let e = j.append(&s, g1, 1, &[ev(1, msg("x"))]).await.unwrap_err();
    assert!(matches!(e, StoreError::SeqConflict { expected: 1, found: 2 }), "{e:?}");
    // Mismatched seq inside the batch: nothing written.
    let e = j.append(&s, g1, 2, &[ev(2, msg("c")), ev(4, msg("d"))]).await.unwrap_err();
    assert!(matches!(e, StoreError::SeqConflict { .. }));
    assert_eq!(j.next_seq(&s).await.unwrap(), 2);

    // Lease stolen: old generation rejected.
    let g2 = j.acquire_lease(&s).await.unwrap();
    assert_eq!(g2, LeaseGen(2));
    let e = j.append(&s, g1, 2, &[ev(2, msg("late"))]).await.unwrap_err();
    assert!(matches!(e, StoreError::StaleLease { held: LeaseGen(1), current: LeaseGen(2) }));
    j.append(&s, g2, 2, &[ev(2, msg("c"))]).await.unwrap();

    let all = j.load(&s, 0).await.unwrap();
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(all[2].body, msg("c"));
    assert_eq!(j.load(&s, 2).await.unwrap().len(), 1);

    // Snapshots.
    assert_eq!(j.load_snapshot(&s).await.unwrap(), None);
    j.save_snapshot(&s, 1, vec![1, 2]).await.unwrap();
    j.save_snapshot(&s, 2, vec![3]).await.unwrap();
    assert_eq!(j.load_snapshot(&s).await.unwrap(), Some((2, vec![3])));

    j.acquire_lease(&SessionId::new("s0")).await.unwrap();
    assert_eq!(j.list_sessions().await.unwrap(), vec![SessionId::new("s0"), s.clone()]);
    drop(db);

    // Reopen; old-schema and ignorable unknown rows are upgraded / skipped on read.
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        let v0 = serde_json::json!({
            "id": "01E3", "parent": null, "seq": 3, "at": 30, "origin": {"kind": "user"},
            "trust": {"kind": "user"}, "audience": "both", "schema": 0,
            "body": {"type": "user_message", "text": "old"}
        });
        let unknown = serde_json::json!({
            "id": "01E4", "parent": null, "seq": 4, "at": 40, "origin": {"kind": "user"},
            "trust": {"kind": "user"}, "audience": "both", "schema": EVENT_SCHEMA,
            "body": {"type": "from_the_future", "ignorable": true}
        });
        c.execute("INSERT INTO events VALUES ('s1', 3, ?1), ('s1', 4, ?2)", [v0.to_string(), unknown.to_string()])
            .unwrap();
    }
    let j = SqliteJournal::open(&path).unwrap();
    let all = j.load(&s, 3).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].body, msg("old"));
    assert_eq!(all[0].schema, EVENT_SCHEMA);
    assert_eq!(j.next_seq(&s).await.unwrap(), 5);
}

#[tokio::test]
async fn journal_refuses_unknown_non_ignorable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.db");
    let j = SqliteJournal::open(&path).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO events VALUES ('s', 0, ?1)",
            [serde_json::json!({"id":"x","parent":null,"seq":0,"at":0,"origin":{"kind":"user"},"trust":{"kind":"user"},
                "audience":"both","schema":EVENT_SCHEMA,"body":{"type":"nope"}})
            .to_string()],
        )
        .unwrap();
    assert!(matches!(j.load(&SessionId::new("s"), 0).await, Err(StoreError::Upgrade(_))));
}

async fn blob_suite(b: &dyn BlobStore) {
    let r1 = b.put(b"hello", Some("text/plain")).await.unwrap();
    assert_eq!(r1.sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    assert_eq!(r1.size, 5);
    let r1b = b.put(b"hello", Some("text/plain")).await.unwrap();
    assert_eq!(r1, r1b);
    let r2 = b.put(b"world", None).await.unwrap();
    assert_eq!(b.get(&r1).await.unwrap(), b"hello");
    assert_eq!(b.gc(std::slice::from_ref(&r1)).await.unwrap(), 1);
    assert!(matches!(b.get(&r2).await, Err(StoreError::NotFound(_))));
    assert_eq!(b.get(&r1).await.unwrap(), b"hello");
    assert_eq!(b.gc(std::slice::from_ref(&r1)).await.unwrap(), 0);
}

#[tokio::test]
async fn blob_stores() {
    blob_suite(&*Sqlite::in_memory().unwrap().blobs).await;
    let d = tempfile::tempdir().unwrap();
    let fs = FsBlobStore::new(d.path()).unwrap();
    blob_suite(&fs).await;
    assert!(d.path().join("2c").join("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").exists());
}

#[tokio::test]
async fn memory_store_is_a_port() {
    let d = tempfile::tempdir().unwrap();
    let m: std::sync::Arc<dyn MemoryStore> = std::sync::Arc::new(FileMemoryStore::new(d.path()).unwrap());
    m.remember("proj", "build", "cargo build --release").await.unwrap();
    assert_eq!(m.recall("proj", "release").await.unwrap().len(), 1);
    assert_eq!(m.recall("proj", "nothing").await.unwrap().len(), 0);
}
