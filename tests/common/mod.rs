use std::process::Command;
use std::time::{Duration, Instant};

/**
 * Removes the Docker volumes, networks and results of the test's home, even when an assertion
 * fails. A run a hook started in the background can still hold a slot, so it waits for that run.
 * Declare it after the temp dir, so it drops first and cleans while the home still exists
 **/
pub struct CleanOnDrop;

impl Drop for CleanOnDrop {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(600);

        while shipcheck::run::clean(None, true).is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_secs(1));
        }

        let prefix = shipcheck::runner::act::home_prefix();
        let out = Command::new("docker")
            .args(["volume", "ls", "-q", "--filter"])
            .arg(format!("name={prefix}"))
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&out.stdout);

        if !std::thread::panicking() {
            assert!(left.trim().is_empty(), "the test left volumes: {left}");
        }
    }
}
