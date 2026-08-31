use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omarchy-zed-theme"));
    command.env("HOME", std::env::temp_dir());
    command
}

#[test]
fn version_is_stable_and_successful() {
    let output = command().arg("--version").output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("omarchy-zed-theme {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_arguments_report_usage() {
    let output = command().arg("--unknown").output().unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "omarchy-zed-theme: error: usage: omarchy-zed-theme [--version|--activate OWNER|--restore OWNER]\n"
    );
}

#[cfg(unix)]
#[test]
fn invalid_utf8_owner_is_rejected() {
    use std::os::unix::ffi::OsStringExt;

    for argument in ["--activate", "--restore"] {
        let owner = std::ffi::OsString::from_vec(b"owner-\xff".to_vec());
        let output = command().arg(argument).arg(owner).output().unwrap();

        assert!(!output.status.success(), "{argument}");
        assert!(output.stdout.is_empty(), "{argument}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("activation owner is not valid UTF-8"),
            "{argument}"
        );
    }
}
