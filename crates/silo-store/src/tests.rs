use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("silo-store-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn seal_open_round_trip() {
    let k = HostKey::generate();
    let sealed = seal(&k, b"aad", b"hello world");
    assert_eq!(&sealed[..4], MAGIC);
    assert_eq!(sealed.len(), 4 + 24 + b"hello world".len() + 16);
    assert_eq!(open(&k, b"aad", &sealed).unwrap(), b"hello world");
    // Fresh nonce each time.
    assert_ne!(seal(&k, b"aad", b"x"), seal(&k, b"aad", b"x"));
    assert_eq!(open(&k, b"", &seal(&k, b"", b"")).unwrap(), b"");
}

#[test]
fn tamper_wrong_key_truncation_rejected() {
    let k = HostKey::generate();
    let sealed = seal(&k, b"aad", b"secret payload");

    let mut t = sealed.clone();
    let last = t.len() - 1;
    t[last] ^= 1;
    assert!(matches!(open(&k, b"aad", &t), Err(Error::AuthFailed)));

    let mut t = sealed.clone();
    t[10] ^= 0x80; // inside nonce
    assert!(matches!(open(&k, b"aad", &t), Err(Error::AuthFailed)));

    assert!(matches!(open(&k, b"other", &sealed), Err(Error::AuthFailed)));
    assert!(matches!(open(&HostKey::generate(), b"aad", &sealed), Err(Error::AuthFailed)));

    for n in [0, 3, 5, 20, 43] {
        assert!(matches!(open(&k, b"aad", &sealed[..n]), Err(Error::Corrupt)), "len {n}");
    }
    assert!(matches!(open(&k, b"aad", &sealed[..sealed.len() - 1]), Err(Error::AuthFailed)));

    let mut t = sealed;
    t[0] = b'X';
    assert!(matches!(open(&k, b"aad", &t), Err(Error::Corrupt)));
}

#[test]
fn key_created_private_and_reloads() {
    let t = TempDir::new();
    let dir = t.path().join("state");
    let k1 = HostKey::load_or_create(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(KEY_FILE)), 0o600);
    assert_eq!(fs::read(dir.join(KEY_FILE)).unwrap().len(), KEY_LEN);
    let k2 = HostKey::load_or_create(&dir).unwrap();
    assert_eq!(k1, k2);
}

#[test]
fn key_symlink_rejected() {
    let t = TempDir::new();
    let dir = t.path().join("state");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let real = t.path().join("real");
    fs::write(&real, [7u8; KEY_LEN]).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&real, dir.join(KEY_FILE)).unwrap();
    assert!(matches!(HostKey::load_or_create(&dir), Err(Error::InsecureKey { .. })));
    // Dangling symlink must not be followed to create a file either.
    fs::remove_file(&real).unwrap();
    assert!(HostKey::load_or_create(&dir).is_err());
    assert!(!real.exists());
}

#[test]
fn key_insecure_permissions_and_length_rejected() {
    let t = TempDir::new();
    let dir = t.path().join("state");
    HostKey::load_or_create(&dir).unwrap();
    let key = dir.join(KEY_FILE);
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(HostKey::load_or_create(&dir), Err(Error::InsecureKey { .. })));

    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(HostKey::load_or_create(&dir), Err(Error::InsecureKey { .. })));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

    fs::write(&key, [1u8; 16]).unwrap();
    assert!(matches!(HostKey::load_or_create(&dir), Err(Error::InsecureKey { .. })));
    // Not overwritten.
    assert_eq!(fs::read(&key).unwrap(), [1u8; 16]);

    // Unreadable existing key is an error, never replaced.
    fs::write(&key, [2u8; KEY_LEN]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();
    let r = HostKey::load_or_create(&dir);
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(r.is_err());
    }
    assert_eq!(fs::read(&key).unwrap(), [2u8; KEY_LEN]);
}

#[test]
fn atomic_write_leaves_no_temp_files() {
    let t = TempDir::new();
    let p = t.path().join("file.bin");
    write_atomic(&p, b"one").unwrap();
    write_atomic(&p, b"two").unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"two");
    assert_eq!(mode(&p), 0o600);
    let names: Vec<_> = fs::read_dir(t.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec!["file.bin".to_string()]);

    let k = HostKey::generate();
    let s = t.path().join("s.sealed");
    write_sealed(&s, &k, b"ad", b"payload").unwrap();
    assert_eq!(read_sealed(&s, &k, b"ad").unwrap(), b"payload");
    assert!(read_sealed(&s, &k, b"other").is_err());
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct Tree {
    tabs: Vec<(String, u64)>,
}

fn tree(refs: &[(&str, u64)]) -> Tree {
    Tree { tabs: refs.iter().map(|(i, g)| (i.to_string(), *g)).collect() }
}

#[test]
fn store_fresh_and_commit_load() {
    let t = TempDir::new();
    let s = Store::open(t.path().join("store"), HostKey::generate()).unwrap();
    let r = s.load::<Tree>().unwrap();
    assert!(r.manifest.is_none() && r.snapshots.is_empty() && r.errors.is_empty());

    let refs = [("a", 1), ("b", 1)];
    s.commit(&[("a", 1, b"A1"), ("b", 1, b"B1")], &refs, &tree(&refs)).unwrap();
    let r = s.load::<Tree>().unwrap();
    assert_eq!(r.manifest, Some(tree(&refs)));
    assert_eq!(r.commit, 1);
    assert_eq!(r.snapshots.len(), 2);
    assert_eq!(r.snapshots[0].bytes, b"A1");
    assert!(t.path().join("store/snapshots/a/1.snap").is_file());
    assert!(t.path().join("store").join(MANIFEST_FILE).is_file());
}

#[test]
fn interrupted_commit_loads_old_manifest() {
    let t = TempDir::new();
    let s = Store::open(t.path(), HostKey::generate()).unwrap();
    let refs = [("a", 1)];
    s.commit(&[("a", 1, b"A1")], &refs, &tree(&refs)).unwrap();
    // Crash after the snapshot write but before manifest replacement.
    s.write_snapshot("a", 2, b"A2").unwrap();
    let r = s.load::<Tree>().unwrap();
    assert_eq!(r.manifest, Some(tree(&refs)));
    assert_eq!(r.snapshots.len(), 1);
    assert_eq!((r.snapshots[0].generation, r.snapshots[0].bytes.as_slice()), (1, &b"A1"[..]));
    assert!(r.errors.is_empty());
}

#[test]
fn corrupt_snapshot_reported_per_item() {
    let t = TempDir::new();
    let s = Store::open(t.path(), HostKey::generate()).unwrap();
    s.commit(&[("a", 1, b"A1"), ("b", 1, b"B1")], &[("a", 1), ("b", 1)], &tree(&[])).unwrap();
    s.commit(&[("a", 2, b"A2")], &[("a", 2), ("b", 1)], &tree(&[])).unwrap();
    // Snapshot AAD binds id + generation: a swapped file is rejected.
    fs::copy(s.snapshot_path("a", 1).unwrap(), s.snapshot_path("a", 2).unwrap()).unwrap();
    let r = s.load::<Tree>().unwrap();
    assert_eq!(r.errors.len(), 1);
    assert_eq!((r.errors[0].id.as_str(), r.errors[0].generation), ("a", 2));
    assert!(matches!(r.errors[0].error, Error::AuthFailed));
    assert_eq!(r.snapshots.len(), 1);
    assert_eq!((r.snapshots[0].id.as_str(), &r.snapshots[0].bytes[..]), ("b", &b"B1"[..]));

    fs::remove_file(s.snapshot_path("b", 1).unwrap()).unwrap();
    let r = s.load::<Tree>().unwrap();
    assert_eq!(r.errors.len(), 2);
    assert!(r.snapshots.is_empty());
    assert!(matches!(&r.errors[1].error, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound));

    fs::write(s.manifest_path(), b"garbage").unwrap();
    assert!(matches!(s.load::<Tree>(), Err(Error::Corrupt)));
}

#[test]
fn gc_keeps_referenced_and_previous() {
    let t = TempDir::new();
    let s = Store::open(t.path(), HostKey::generate()).unwrap();
    s.commit(&[("a", 1, b"1"), ("b", 1, b"1")], &[("a", 1), ("b", 1)], &tree(&[])).unwrap();
    s.commit(&[("a", 2, b"2")], &[("a", 2), ("b", 1)], &tree(&[])).unwrap();
    s.commit(&[("a", 3, b"3")], &[("a", 3)], &tree(&[])).unwrap();
    s.write_snapshot("a", 9, b"orphan").unwrap();
    s.write_snapshot("c", 1, b"orphan").unwrap();
    s.write_snapshot("d", 1, b"pinned").unwrap();
    let removed = s.gc([("d", 1)]).unwrap();
    // a/1 (two commits old), a/9, c/1 removed; b/1 kept via previous commit.
    assert_eq!(removed, 3);
    for (id, g) in [("a", 3), ("a", 2), ("b", 1), ("d", 1)] {
        assert!(s.snapshot_path(id, g).unwrap().is_file(), "{id}/{g}");
    }
    for (id, g) in [("a", 1), ("a", 9), ("c", 1)] {
        assert!(!s.snapshot_path(id, g).unwrap().exists(), "{id}/{g}");
    }
    assert!(!t.path().join("snapshots/c").exists());
    assert_eq!(s.load::<Tree>().unwrap().snapshots[0].bytes, b"3");
    assert!(s.write_snapshot("../x", 1, b"").is_err());
}

#[test]
fn commit_fails_on_unreadable_manifest() {
    let t = TempDir::new();
    let s = Store::open(t.path(), HostKey::generate()).unwrap();
    s.commit(&[("a", 1, b"A1")], &[("a", 1)], &tree(&[])).unwrap();
    let good = fs::read(s.manifest_path()).unwrap();

    fs::write(s.manifest_path(), b"garbage").unwrap();
    assert!(matches!(s.commit(&[], &[("a", 1)], &tree(&[])), Err(Error::Corrupt)));
    assert_eq!(fs::read(s.manifest_path()).unwrap(), b"garbage");

    // Manifest sealed under another key: authentication error, not replaced.
    fs::write(s.manifest_path(), &good).unwrap();
    let other = Store::open(t.path(), HostKey::generate()).unwrap();
    assert!(matches!(other.commit(&[], &[("a", 1)], &tree(&[])), Err(Error::AuthFailed)));
    assert_eq!(fs::read(s.manifest_path()).unwrap(), good);
    assert!(matches!(other.gc([]), Err(Error::AuthFailed)));
    assert!(s.snapshot_path("a", 1).unwrap().is_file());
}

#[test]
fn gc_removes_stale_temp_files() {
    let t = TempDir::new();
    let s = Store::open(t.path(), HostKey::generate()).unwrap();
    s.commit(&[("a", 1, b"A1")], &[("a", 1)], &tree(&[])).unwrap();
    let stale = [t.path().join(".tmp-tree.json.sealed.123.0"), t.path().join("snapshots/.tmp-x.1.0"), t.path().join("snapshots/a/.tmp-2.snap.123.1")];
    for p in &stale {
        fs::write(p, b"partial").unwrap();
    }
    let unrelated = t.path().join("notes.txt");
    fs::write(&unrelated, b"keep").unwrap();
    // Another writer's file in the shared root, mid `write_atomic`.
    let foreign = t.path().join(".tmp-devices.sealed.123.0");
    fs::write(&foreign, b"in flight").unwrap();
    assert_eq!(s.gc([]).unwrap(), 3);
    for p in &stale {
        assert!(!p.exists(), "{}", p.display());
    }
    assert!(unrelated.exists());
    assert!(foreign.exists(), "only the store's own temp files are collected");
    assert!(s.snapshot_path("a", 1).unwrap().is_file());
    assert!(s.manifest_path().is_file());
    assert_eq!(s.load::<Tree>().unwrap().snapshots[0].bytes, b"A1");
}
