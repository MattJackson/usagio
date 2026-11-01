//! One-shot launchd jobs sever macOS's responsible-process association.
//! A detached child, setsid(), or PPID 1 does not provide that boundary.
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::xml_escape;

// All variable data is passed as argv, never interpolated into shell source.
// Rename the result so the reader cannot observe a partially written status.
const RUN: &str = r#"result=$1; shift
child=
trap 'if [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null; wait "$child" 2>/dev/null; fi; exit 143' TERM INT HUP
"$@" </dev/null >/dev/null 2>&1 &
child=$!
wait "$child"
code=$?
child=
trap - TERM INT HUP
printf '%s' "$code" > "$result.tmp"
/bin/mv "$result.tmp" "$result"
"#;

struct Job {
    target: String,
    _directory: tempfile::TempDir,
}

impl Drop for Job {
    fn drop(&mut self) {
        // Never leave a loaded service after success, failure, or timeout.
        // AbandonProcessGroup preserves a successfully daemonized application.
        let _ = bounded_command(
            Command::new("/bin/launchctl").args(["bootout", &self.target]),
            Instant::now() + Duration::from_secs(2),
        );
    }
}

fn bounded_command(command: &mut Command, deadline: Instant) -> Result<ExitStatus> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result?;
                bail!("launchd command timed out");
            }
        }
    }
}

fn resolve_program(program: &Path) -> Result<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return std::fs::canonicalize(program).context("resolving independent executable");
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
        .context("independent executable not found in PATH")
}

fn string(value: &OsStr) -> Result<String> {
    Ok(format!(
        "<string>{}</string>",
        xml_escape(value.to_str().context("launchd requires UTF-8 arguments")?)
    ))
}

pub(super) fn run(
    program: &Path,
    args: &[&str],
    environment: &[(&str, &OsStr)],
    timeout: Duration,
) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    let program = resolve_program(program)?;
    let directory = tempfile::Builder::new()
        .prefix("usagio-launch-")
        .tempdir()
        .context("creating private launch directory")?;
    let label = format!(
        "com.mattjackson.usagio.independent.{}.{}",
        std::process::id(),
        rand::random::<u64>()
    );
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let result = directory.path().join("exit-status");
    let plist = directory.path().join("job.plist");
    let mut arguments = [
        OsStr::new("/bin/sh"),
        OsStr::new("-c"),
        OsStr::new(RUN),
        OsStr::new("usagio-independent"),
        result.as_os_str(),
        program.as_os_str(),
    ]
    .into_iter()
    .map(string)
    .collect::<Result<Vec<_>>>()?;
    arguments.extend(
        args.iter()
            .map(|arg| string(OsStr::new(arg)))
            .collect::<Result<Vec<_>>>()?,
    );

    // Do not serialize the parent's whole environment: it may contain tokens.
    // Preserve only launch context, plus explicit application configuration.
    let mut variables = std::collections::BTreeMap::new();
    for key in ["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL", "SHELL"] {
        if let Some(value) = std::env::var_os(key) {
            variables.insert(key, value);
        }
    }
    for (key, value) in environment {
        variables.insert(key, value.to_os_string());
    }
    let variables = variables
        .into_iter()
        .map(|(key, value)| Ok(format!("<key>{}</key>{}", xml_escape(key), string(&value)?)))
        .collect::<Result<Vec<String>>>()?
        .join("");
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{}</array>
<key>EnvironmentVariables</key><dict>{variables}</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><false/>
<key>AbandonProcessGroup</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>StandardOutPath</key><string>/dev/null</string>
<key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>"#,
            arguments.join("")
        ),
    )?;
    let _job = Job {
        target: format!("{domain}/{label}"),
        _directory: directory,
    };
    let status = bounded_command(
        Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist),
        deadline,
    )?;
    anyhow::ensure!(
        status.success(),
        "independent launchd bootstrap failed: {status}"
    );
    loop {
        match std::fs::read_to_string(&result) {
            Ok(code) => {
                let code: u8 = code.parse().context("invalid independent exit status")?;
                return Ok(ExitStatus::from_raw(i32::from(code) << 8));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        anyhow::ensure!(Instant::now() < deadline, "independent launch timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = "platform::macos::independent::tests::responsibility_probe";
    const OUTPUT: &str = "USAGIO_RESPONSIBILITY_PROBE_OUTPUT";
    const SUPERVISOR: &str = "USAGIO_RESPONSIBILITY_SUPERVISOR";

    fn loaded_jobs() -> Vec<String> {
        let output = Command::new("/bin/launchctl").arg("list").output().unwrap();
        assert!(output.status.success());
        let prefix = format!("com.mattjackson.usagio.independent.{}.", std::process::id());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|label| label.starts_with(&prefix))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn missing_executable_fails_before_launch() {
        let dir = tempfile::tempdir().unwrap();
        let error = run(
            &dir.path().join("missing-codex"),
            &["app-server", "daemon", "start"],
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("resolving independent executable"));
    }

    #[test]
    #[ignore = "requires a logged-in macOS GUI session"]
    fn launchd_does_not_forward_private_parent_environment() {
        crate::env_lock::scoped_env_var(
            "USAGIO_PRIVATE_TEST_VALUE",
            Some("private-test-sentinel"),
            || {
                let dir = tempfile::tempdir().unwrap();
                let output = dir.path().join("environment");
                assert!(run(
                    Path::new("/bin/sh"),
                    &[
                        "-c",
                        "printf '%s' \"${USAGIO_PRIVATE_TEST_VALUE-absent}\" > \"$1\"",
                        "probe",
                        output.to_str().unwrap(),
                    ],
                    &[],
                    Duration::from_secs(10),
                )
                .unwrap()
                .success());
                assert_eq!(std::fs::read_to_string(output).unwrap(), "absent");
            },
        );
    }

    #[test]
    #[ignore = "requires a logged-in macOS GUI session"]
    fn launchd_cleanup_preserves_daemonized_application() {
        let before = loaded_jobs();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("daemon-pid");
        let result = run(
            Path::new("/bin/sh"),
            &[
                "-c",
                "/bin/sleep 60 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > \"$1\"",
                "probe",
                output.to_str().unwrap(),
            ],
            &[],
            Duration::from_secs(10),
        );
        let pid: i32 = std::fs::read_to_string(output).unwrap().parse().unwrap();
        assert!(pid > 1);
        struct Cleanup(i32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                }
            }
        }
        let _cleanup = Cleanup(pid);
        assert!(result.unwrap().success());
        assert_eq!(loaded_jobs(), before);
        let status = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        assert!(status.status.success(), "cleanup killed the application");
        assert!(
            !status.stdout.starts_with(b"Z"),
            "application became a zombie"
        );
    }

    // Diagnostic private symbol is deliberately confined to tests. Production
    // uses only launchctl and never calls private responsibility APIs.
    fn responsible_pid() -> i32 {
        unsafe {
            let symbol = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            );
            assert!(!symbol.is_null(), "responsibility diagnostic unavailable");
            let query: unsafe extern "C" fn(i32) -> i32 = std::mem::transmute(symbol);
            query(std::process::id() as i32)
        }
    }

    #[test]
    #[ignore = "subprocess fixture for the bundled responsibility regression"]
    fn responsibility_probe() {
        let output = PathBuf::from(std::env::var_os(OUTPUT).expect("missing probe output"));
        if let Some(executable) = std::env::var_os(SUPERVISOR) {
            let direct = output.with_extension("direct");
            let independent = output.with_extension("independent");
            assert!(Command::new(&executable)
                .args(["--exact", PROBE, "--ignored"])
                .env_remove(SUPERVISOR)
                .env(OUTPUT, &direct)
                .status()
                .unwrap()
                .success());
            let responsible = responsible_pid();
            let direct_pid: i32 = std::fs::read_to_string(direct).unwrap().parse().unwrap();
            assert_eq!(
                direct_pid, responsible,
                "fixture must reproduce inherited responsibility"
            );
            assert!(run(
                Path::new(&executable),
                &["--exact", PROBE, "--ignored"],
                &[(OUTPUT, independent.as_os_str())],
                Duration::from_secs(10),
            )
            .unwrap()
            .success());
            let independent_pid: i32 = std::fs::read_to_string(independent)
                .unwrap()
                .parse()
                .unwrap();
            assert!(independent_pid > 1);
            assert_ne!(
                independent_pid, responsible,
                "independent tools must not inherit the app's TCC identity"
            );
            std::fs::write(
                output,
                format!("app={responsible}, direct={direct_pid}, independent={independent_pid}"),
            )
            .unwrap();
        } else {
            std::fs::write(output, responsible_pid().to_string()).unwrap();
        }
    }

    #[test]
    #[ignore = "requires a logged-in macOS GUI session and codesign"]
    fn launchd_breaks_bundled_app_responsibility() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("LaunchProbe.app");
        let macos = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let executable = macos.join("probe");
        let original = std::env::current_exe().unwrap();
        std::fs::copy(&original, &executable).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.mattjackson.usagio.launch-probe</string>
<key>CFBundleExecutable</key><string>probe</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>"#,
        )
        .unwrap();
        assert!(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&bundle)
            .status()
            .unwrap()
            .success());
        let output = dir.path().join("responsibility");
        let log = dir.path().join("probe.log");
        assert!(bounded_command(
            Command::new("/usr/bin/open")
                .args(["-W", "-n", "-g", "--env"])
                .arg(format!("{SUPERVISOR}={}", original.display()))
                .arg("--env")
                .arg(format!("{OUTPUT}={}", output.display()))
                .arg("--stdout")
                .arg(&log)
                .arg("--stderr")
                .arg(&log)
                .arg(&bundle)
                .args(["--args", "--exact", PROBE, "--ignored", "--nocapture"]),
            Instant::now() + Duration::from_secs(20),
        )
        .unwrap()
        .success());
        assert!(
            output.exists(),
            "{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        eprintln!("{}", std::fs::read_to_string(output).unwrap());
    }

    #[test]
    #[ignore = "requires a logged-in macOS GUI session"]
    fn launchd_preserves_literal_arguments_environment_and_exit_status() {
        let before = loaded_jobs();
        let dir = tempfile::Builder::new()
            .prefix("usagio '&<$(false)-")
            .tempdir()
            .unwrap();
        let output = dir.path().join("output");
        let home = dir.path().join("custom codex home");
        let status = run(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf '%s' \"$CODEX_HOME\" > \"$1\"; exit 17",
                "probe",
                output.to_str().unwrap(),
            ],
            &[("CODEX_HOME", home.as_os_str())],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(status.code(), Some(17));
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            home.to_str().unwrap()
        );
        assert_eq!(loaded_jobs(), before, "completed job must be unloaded");
    }

    #[test]
    #[ignore = "requires a logged-in macOS GUI session"]
    fn launchd_timeout_stops_launcher_and_unloads_job() {
        let before = loaded_jobs();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("pid");
        let start = Instant::now();
        let error = run(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 60",
                "probe",
                output.to_str().unwrap(),
            ],
            &[],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(6));
        assert_eq!(loaded_jobs(), before, "timed-out job must be unloaded");
        let pid = std::fs::read_to_string(output).expect("launcher did not start");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = Command::new("/bin/ps")
                .args(["-p", &pid, "-o", "stat="])
                .output()
                .unwrap();
            if !status.status.success() || status.stdout.starts_with(b"Z") {
                break;
            }
            assert!(Instant::now() < deadline, "timed-out launcher survived");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
