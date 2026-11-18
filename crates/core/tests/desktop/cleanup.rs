use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
pub(super) async fn remove_dirs(dirs: [PathBuf; 2]) {
    for d in dirs {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match std::fs::remove_dir_all(&d) {
                Ok(()) => break,
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(75)).await
                }
                Err(e) => panic!("{}: {e}", d.display()),
            }
        }
    }
}
