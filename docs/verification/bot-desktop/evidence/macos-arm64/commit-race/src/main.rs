use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use sqlx::{Connection, SqliteConnection};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let file = format!("/private/tmp/else-pr62-review-{}.sqlite3",std::process::id());
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&file).create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
    let mut writer = SqliteConnection::connect_with(&options).await.unwrap();
    let mut reader = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE leases (holder TEXT PRIMARY KEY)").execute(&mut writer).await.unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new((Mutex::new(false),Condvar::new()));
    {
        let entered = entered.clone();let barrier = barrier.clone();
        writer.lock_handle().await.unwrap().set_commit_hook(move || {
            entered.store(true, Ordering::SeqCst);
            let (lock, cv) = &*barrier;
            let mut released = lock.lock().unwrap();
            while !*released { released = cv.wait(released).unwrap(); }
            true
        });
    }
    let task = tokio::spawn(async move {
        let mut tx = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("INSERT INTO leases VALUES ('abandoned-viewer')").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        writer
    });
    for _ in 0..1000 {
        if entered.load(Ordering::SeqCst) { break; }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(entered.load(Ordering::SeqCst));
    task.abort(); let _ = task.await;
    let treatment = std::env::args().nth(1).as_deref() == Some("barrier");
    let unlock = barrier.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (lock, cv) = &*unlock;
        *lock.lock().unwrap() = true;
        cv.notify_one();
    });
    let before: i64 = if treatment {
        let mut cleanup = reader.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let found = sqlx::query_scalar("SELECT COUNT(*) FROM leases WHERE holder='abandoned-viewer'")
            .fetch_one(&mut *cleanup).await.unwrap();
        cleanup.commit().await.unwrap();
        if found > 0 {
            sqlx::query("DELETE FROM leases WHERE holder='abandoned-viewer'").execute(&mut reader).await.unwrap();
        }
        found
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM leases WHERE holder='abandoned-viewer'")
            .fetch_one(&mut reader).await.unwrap()
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM leases WHERE holder='abandoned-viewer'").fetch_one(&mut reader).await.unwrap();
    println!("barrier={treatment}, cleanup saw {before}, remaining leases={after}");
    assert_eq!((before, after), if treatment { (1, 0) } else { (0, 1) });
    reader.close().await.unwrap();
    std::fs::remove_file(file).unwrap();
}
