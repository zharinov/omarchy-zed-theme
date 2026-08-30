use omarchy_zed_theme::publish::{Generation, generate};
use omarchy_zed_theme::zed_settings;
use omarchy_zed_theme::{Error, Result};
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

fn print_generation(generation: Generation) {
    println!(
        "{}: {}",
        if generation.changed {
            "generated"
        } else {
            "unchanged"
        },
        generation.target.display()
    );

    eprintln!(
        "audit: mode={} repairs={} extras={} degradations={} syntax_min={:.2} terminal_min={:.2}",
        generation.audit.mode,
        generation.audit.repairs.len(),
        generation.audit.extras.len(),
        generation.audit.degradations.len(),
        generation.audit.minimums.get("syntax").unwrap_or(&0.0),
        generation.audit.minimums.get("terminal").unwrap_or(&0.0),
    );
    if std::env::var("OMARCHY_ZED_THEME_AUDIT").as_deref() == Ok("1") {
        eprintln!("audit-detail: {}", generation.audit.detail());
    }

    for warning in generation.audit.warnings {
        eprintln!("omarchy-zed-theme: resolver warning: {warning}");
    }
    for degradation in generation.audit.degradations {
        eprintln!("omarchy-zed-theme: degradation: {degradation}");
    }
}

fn sync() -> Result<()> {
    let home = home()?;
    let (colors, output) = current_paths(&home);
    let generation = generate(&colors, Some(&output), None, None)?;

    print_generation(generation);
    Ok(())
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
            zed_settings::activate(&home()?, &owner.to_string_lossy())
        }
        (Some(argument), Some(owner), None) if argument == "--restore" => {
            zed_settings::restore(&home()?, &owner.to_string_lossy())
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
