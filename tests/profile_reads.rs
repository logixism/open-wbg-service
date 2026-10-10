use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

struct Session {
    dir: PathBuf,
    daemon: Option<Child>,
}

impl Session {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "open-wbg-profile-reads-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        // No real keyboard is selected, even on a developer's desktop.
        fs::write(
            dir.join("config.toml"),
            "serials = [\"open-wbg-regression-no-such-keyboard\"]\nenabled_sources = []\n",
        )
        .unwrap();
        Self { dir, daemon: None }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_open-wbg-service"));
        command
            .arg("--config")
            .arg(self.dir.join("config.toml"))
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("XDG_CONFIG_HOME", &self.dir)
            .env("XDG_STATE_HOME", &self.dir)
            .env("NIRI_SOCKET", self.dir.join("no-compositor.sock"))
            .env("RUST_LOG", "info");
        command
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(child) = &mut self.daemon {
            let _ = child.kill();
            let _ = child.wait();
        }
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn daemon_ownership_does_not_replace_the_profile_query_result() {
    let mut session = Session::new();
    let standalone = session.command().arg("profiles").output().unwrap();
    // With no matching keyboard, the read must report the same device-selection
    // failure whether or not the daemon is running. It must not fail on ownership.
    assert!(!standalone.status.success());
    session.daemon = Some(
        session
            .command()
            .args([
                "run",
                "--backend",
                "niri",
                "--no-api",
                "--dry-run",
                "--duration",
                "30",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = BufReader::new(session.daemon.as_mut().unwrap().stderr.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            stderr.read_line(&mut line).unwrap(),
            0,
            "daemon exited before startup"
        );
        if line.contains("open-wbg-service ready") {
            break;
        }
    }

    let concurrent = session.command().arg("profiles").output().unwrap();
    assert_eq!(concurrent.status.code(), standalone.status.code());
    assert_eq!(concurrent.stderr, standalone.stderr);
    assert!(
        session
            .daemon
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
}
