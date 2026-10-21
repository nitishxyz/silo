//! PTYs spawned from many threads at once.

use silo_pty::{Pty, PtySize, SpawnConfig};

const SIZE: PtySize = PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };

/// Concurrent `/dev/ptmx` opens race for the next minor number on macOS,
/// and the loser used to fail with "Unknown error: -6" (EREDRIVEOPEN).
#[test]
fn concurrent_spawns_all_start() {
    let threads: Vec<_> = (0..16)
        .map(|_| {
            std::thread::spawn(|| {
                let mut errors = Vec::new();
                for _ in 0..40 {
                    let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()], ..Default::default() };
                    let (tx, rx) = std::sync::mpsc::channel();
                    match Pty::spawn(&config, SIZE, move |status| {
                        let _ = tx.send(status);
                    }) {
                        Ok(pty) => {
                            let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
                            drop(pty);
                        }
                        Err(error) => errors.push(error.to_string()),
                    }
                }
                errors
            })
        })
        .collect();
    let errors: Vec<_> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
}
