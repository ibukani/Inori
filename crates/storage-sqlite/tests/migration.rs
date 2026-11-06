use inori_domain::Character;
use inori_runtime::Repository;
use inori_storage_sqlite::SqliteRepository;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{borrow::Cow, path::Path};

async fn pool(path: &Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .unwrap()
}
async fn version(path: &Path) -> i64 {
    let p = pool(path).await;
    let v = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&p)
        .await
        .unwrap();
    p.close().await;
    v
}
#[tokio::test]
async fn old_schema_backup_does_not_migrate_and_upgrade_requires_lock_and_preserves_old_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inori.sqlite");
    let mut older = sqlx::migrate!("./migrations");
    older.migrations = Cow::Owned(older.iter().filter(|m| m.version <= 2).cloned().collect());
    let p = pool(&path).await;
    older.run(&p).await.unwrap();
    sqlx::query("INSERT INTO character(singleton,id,name,persona,revision,active_provider) VALUES(1,'inori','Original','Original persona',7,'mock')").execute(&p).await.unwrap();
    p.close().await;
    let backup = dir.path().join("manual.sqlite");
    SqliteRepository::backup_file(&path, &backup).await.unwrap();
    assert_eq!(version(&path).await, 2);
    assert_eq!(version(&backup).await, 2);
    assert!(!dir.path().join("backups").exists());
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join("server.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(
        SqliteRepository::open(&path, Character::default())
            .await
            .is_err()
    );
    assert_eq!(version(&path).await, 2);
    drop(lock);
    let repo = SqliteRepository::open(&path, Character::default())
        .await
        .unwrap();
    assert_eq!(repo.character().await.unwrap().revision, 7);
    assert_eq!(repo.character().await.unwrap().persona, "Original persona");
    assert_eq!(version(&path).await, 3);
    let copies = std::fs::read_dir(dir.path().join("backups"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(version(&copies[0].path()).await, 2);
    repo.close().await;
}
