use me_persist::financial::FinancialJournal;
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
};
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn file() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "me-financial-journal-{}-{}-{}.wal",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
#[test]
fn exclusive_writer_recovery_and_torn_last_record() {
    let path = file();
    let (mut journal, records) = FinancialJournal::<String>::open(&path).unwrap();
    assert!(records.is_empty());
    assert!(FinancialJournal::<String>::open(&path).is_err());
    journal.append(&"first".into()).unwrap();
    let committed = std::fs::metadata(&path).unwrap().len();
    journal.append(&"second".into()).unwrap();
    drop(journal);
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(std::fs::metadata(&path).unwrap().len() - 1)
        .unwrap();
    drop(f);
    let (mut journal, records) = FinancialJournal::<String>::open(&path).unwrap();
    assert_eq!(records, vec!["first"]);
    assert_eq!(journal.sequence(), 1);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), committed);
    assert_eq!(journal.append(&"replacement".into()).unwrap(), 2);
    drop(journal);
    let (journal, records) = FinancialJournal::<String>::open(&path).unwrap();
    assert_eq!(records, vec!["first", "replacement"]);
    drop(journal);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn complete_record_corruption_fails_closed() {
    let path = file();
    let (mut journal, _) = FinancialJournal::<String>::open(&path).unwrap();
    journal.append(&"first".into()).unwrap();
    drop(journal);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(28)).unwrap();
    f.write_all(b"x").unwrap();
    drop(f);
    assert!(FinancialJournal::<String>::open(&path).is_err());
    std::fs::remove_file(path).unwrap();
}
#[test]
fn incompatible_storage_is_rejected() {
    let path = file();
    std::fs::write(&path, b"MEFIN002").unwrap();
    assert!(FinancialJournal::<String>::open(&path).is_err());
    std::fs::remove_file(path).unwrap();
}
