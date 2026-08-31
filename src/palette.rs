use crate::color::normalize_hex;
use crate::constants::{CANONICAL_COLOR_KEYS, COLOR_ALIASES};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct ResolvedPalette {
    pub mode: String,
    pub colors: BTreeMap<String, String>,
    pub provenance: BTreeMap<String, Provenance>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provenance {
    Direct,
    Alias,
    Derived,
}

fn parse_tsv(output: &str) -> Result<BTreeMap<String, String>> {
    let mut records = BTreeMap::new();
    for (index, line) in output.lines().enumerate() {
        let mut parts = line.split('\t');
        let (Some(key), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(Error(format!(
                "resolver line {} is not key<TAB>value",
                index + 1
            )));
        };

        if key.is_empty() || value.is_empty() {
            return Err(Error(format!(
                "resolver line {} has an empty key or value",
                index + 1
            )));
        }

        if records.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(Error(format!("resolver emitted duplicate key {key:?}")));
        }
    }

    Ok(records)
}

fn parse_resolved_records(
    records: BTreeMap<String, String>,
    raw: BTreeMap<String, String>,
) -> Result<ResolvedPalette> {
    let missing: Vec<_> = CANONICAL_COLOR_KEYS
        .iter()
        .copied()
        .chain(["mode"])
        .filter(|key| !records.contains_key(*key))
        .collect();
    if !missing.is_empty() {
        return Err(Error(format!(
            "resolver omitted canonical keys: {}",
            missing.join(", ")
        )));
    }

    let mode = records["mode"].clone();
    if mode != "dark" && mode != "light" {
        return Err(Error(format!(
            "resolved mode must be 'dark' or 'light', got {mode:?}"
        )));
    }

    let mut colors = BTreeMap::new();
    for key in CANONICAL_COLOR_KEYS {
        colors.insert((*key).to_owned(), normalize_hex(&records[*key], key)?);
    }

    for (alias, canonical) in COLOR_ALIASES {
        if let Some(value) = records.get(*alias)
            && normalize_hex(value, alias)? != colors[*canonical]
        {
            return Err(Error(format!(
                "resolver alias {alias:?} disagrees with {canonical:?}"
            )));
        }
    }

    if records
        .get("theme_type")
        .is_some_and(|value| value != &mode)
    {
        return Err(Error(
            "resolver alias 'theme_type' disagrees with 'mode'".into(),
        ));
    }

    let alias_sources = BTreeMap::from([
        ("red", &["color1"][..]),
        ("green", &["color2"]),
        ("yellow", &["color3"]),
        ("blue", &["color4"]),
        ("magenta", &["purple", "color5"]),
        ("cyan", &["color6"]),
    ]);
    let provenance = CANONICAL_COLOR_KEYS
        .iter()
        .map(|key| {
            let kind = if raw.contains_key(*key) {
                Provenance::Direct
            } else if alias_sources
                .get(key)
                .is_some_and(|aliases| aliases.iter().any(|alias| raw.contains_key(*alias)))
            {
                Provenance::Alias
            } else {
                Provenance::Derived
            };
            ((*key).to_owned(), kind)
        })
        .collect();

    Ok(ResolvedPalette {
        mode,
        colors,
        provenance,
    })
}

pub fn resolve_palette(colors_file: &Path, resolver: Option<&Path>) -> Result<ResolvedPalette> {
    if !colors_file.is_file() {
        return Err(Error(format!(
            "colors.toml not found: {}",
            colors_file.display()
        )));
    }

    let executable = resolver
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var_os("OMARCHY_ZED_THEME_COLOR_RESOLVER").map(Into::into))
        .or_else(|| std::env::var_os("OMARCHY_ZED_THEME_COLOR").map(Into::into))
        .unwrap_or_else(|| "omarchy-theme-color".into());

    let mut resolved_child = Command::new(&executable)
        .args(["--file"])
        .arg(colors_file)
        .arg("--all")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error(format!("cannot run omarchy-theme-color: {error}")))?;

    let mut raw_child = match Command::new(&executable)
        .args(["--file"])
        .arg(colors_file)
        .arg("--raw")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = resolved_child.kill();
            let _ = resolved_child.wait();
            return Err(Error(format!(
                "cannot run omarchy-theme-color --raw: {error}"
            )));
        }
    };

    let output = match resolved_child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            let _ = raw_child.kill();
            let _ = raw_child.wait();
            return Err(Error(format!(
                "cannot read omarchy-theme-color output: {error}"
            )));
        }
    };

    let raw_output = raw_child.wait_with_output().map_err(|error| {
        Error(format!(
            "cannot read omarchy-theme-color --raw output: {error}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr
        };
        return Err(Error(format!("omarchy-theme-color failed: {detail}")));
    }

    if !raw_output.status.success() {
        let stderr = String::from_utf8_lossy(&raw_output.stderr)
            .trim()
            .to_owned();
        return Err(Error(format!("omarchy-theme-color --raw failed: {stderr}")));
    }

    parse_resolved_records(
        parse_tsv(&String::from_utf8_lossy(&output.stdout))?,
        parse_tsv(&String::from_utf8_lossy(&raw_output.stdout))?,
    )
}
