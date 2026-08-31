use omarchy_zed_theme::publish::{ThemeUpdate, generate_and_publish};
use omarchy_zed_theme::zed_settings;
use omarchy_zed_theme::{Error, Result};

use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error("HOME is not set".into()))
}

fn current_paths(home: &Path) -> (PathBuf, PathBuf) {
    (
        home.join(".local/state/omarchy/current/theme/colors.toml"),
        zed_settings::config_home(home).join("zed/themes"),
    )
}

fn print_update(update: ThemeUpdate) {
    println!(
        "{}: {}",
        if update.cached {
            "cached"
        } else if update.changed {
            "generated"
        } else {
            "unchanged"
        },
        update.target.display()
    );

    let Some(audit) = update.audit else { return };

    eprintln!(
        "audit: mode={} repairs={} extras={} degradations={} syntax_min={:.2} terminal_min={:.2}",
        audit.mode,
        audit.repairs.len(),
        audit.extras.len(),
        audit.degradations.len(),
        audit.minimums.get("syntax").unwrap_or(&0.0),
        audit.minimums.get("terminal").unwrap_or(&0.0),
    );
    for warning in audit.warnings {
        eprintln!("omarchy-zed-theme: resolver warning: {warning}");
    }

    for degradation in audit.degradations {
        eprintln!("omarchy-zed-theme: degradation: {degradation}");
    }
}

fn sync() -> Result<()> {
    let home = home()?;
    let (colors, output) = current_paths(&home);
    let update = generate_and_publish(&colors, Some(&output), None, None)?;

    print_update(update);
    Ok(())
}

fn activation_owner(owner: OsString) -> Result<String> {
    owner
        .into_string()
        .map_err(|owner| Error(format!("activation owner is not valid UTF-8: {owner:?}")))
}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    match (arguments.next(), arguments.next(), arguments.next()) {
        (None, None, None) => sync(),
        (Some(argument), None, None) if argument == "--version" => {
            println!("omarchy-zed-theme {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        (Some(argument), Some(owner), None) if argument == "--activate" => {
            zed_settings::activate(&home()?, &activation_owner(owner)?)
        }
        (Some(argument), Some(owner), None) if argument == "--restore" => {
            zed_settings::restore(&home()?, &activation_owner(owner)?)
        }
        _ => Err(Error(
            "usage: omarchy-zed-theme [--version|--activate OWNER|--restore OWNER]".into(),
        )),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("omarchy-zed-theme: error: {error}");
        std::process::exit(1);
    }
}
