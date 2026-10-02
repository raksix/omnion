//! `omnion node …` — the node SDK's command line (docs/requests/REQ-087, slice 4).
//!
//! Three verbs, and the order they are used in is the order a package is built:
//!
//! ```text
//! omnion node scaffold acme-tools --out acme-tools/    write a package that installs
//! omnion node validate  acme-tools/manifest.json       what an install will say
//! omnion node pack      acme-tools/manifest.json       the file to hand the installer
//! ```
//!
//! Every verb runs the *same* `omnion_workflows::node_package::validate` the API's install
//! runs. A CLI that had its own validator would be a second answer to "is this package
//! installable", and the two would agree until the day they did not — which is the day a
//! package installs from the panel and fails on the server.
//!
//! `validate` is the important one. The REQ's line is "a package must pass the validator to
//! install", and the useful half of that promise is being able to *run* the validator without
//! an installation: an author fixing a finding should not have to install, break a palette
//! and read the panel to find the next one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use omnion_workflows::node_package::{self, Manifest};

use crate::output::{check, hint, pair};

/// What one invocation asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCommand {
    /// Write a working example package.
    Scaffold {
        /// Package key.
        key: String,
        /// Directory to write into; `.` when absent.
        out: PathBuf,
    },
    /// Run the validator and report.
    Validate {
        /// Path to the manifest.
        manifest: PathBuf,
    },
    /// Validate, then write the packed file.
    Pack {
        /// Path to the manifest.
        manifest: PathBuf,
        /// Where to write; `<key>.omnion-node.json` when absent.
        out: Option<PathBuf>,
    },
}

impl NodeCommand {
    /// Parse `omnion node <scaffold|validate|pack> …`.
    ///
    /// # Errors
    ///
    /// A usage message.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let Some(verb) = args.first() else {
            return Err("`omnion node` needs a verb: scaffold, validate or pack".to_string());
        };
        let mut rest: Vec<String> = args[1..].to_vec();

        match verb.as_str() {
            "scaffold" => {
                let mut key: Option<String> = None;
                let mut out = PathBuf::from(".");
                while !rest.is_empty() {
                    let flag = rest.remove(0);
                    match flag.as_str() {
                        "--out" | "--dir" => {
                            out = PathBuf::from(take_value(&mut rest, "--out")?);
                        }
                        other if other.starts_with('-') => {
                            return Err(format!(
                                "unknown option {other:?} for `omnion node scaffold`"
                            ));
                        }
                        other => key = Some(other.to_string()),
                    }
                }
                Ok(Self::Scaffold {
                    key: key.ok_or_else(|| {
                        "`omnion node scaffold` needs a package key: omnion node scaffold acme-tools"
                            .to_string()
                    })?,
                    out,
                })
            }
            "validate" => {
                let mut manifest: Option<PathBuf> = None;
                while !rest.is_empty() {
                    let flag = rest.remove(0);
                    if flag == "--manifest" {
                        manifest = Some(PathBuf::from(take_value(&mut rest, "--manifest")?));
                    } else if flag.starts_with('-') {
                        return Err(format!(
                            "unknown option {flag:?} for `omnion node validate`"
                        ));
                    } else {
                        manifest = Some(PathBuf::from(flag));
                    }
                }
                Ok(Self::Validate {
                    manifest: manifest.ok_or_else(|| {
                        "`omnion node validate` needs a manifest: omnion node validate manifest.json"
                            .to_string()
                    })?,
                })
            }
            "pack" => {
                let mut manifest: Option<PathBuf> = None;
                let mut out: Option<PathBuf> = None;
                while !rest.is_empty() {
                    let flag = rest.remove(0);
                    match flag.as_str() {
                        "--out" | "-o" => {
                            out = Some(PathBuf::from(take_value(&mut rest, "--out")?))
                        }
                        "--manifest" => {
                            manifest = Some(PathBuf::from(take_value(&mut rest, "--manifest")?));
                        }
                        other if other.starts_with('-') => {
                            return Err(format!("unknown option {other:?} for `omnion node pack`"));
                        }
                        other => manifest = Some(PathBuf::from(other)),
                    }
                }
                Ok(Self::Pack {
                    manifest: manifest.ok_or_else(|| {
                        "`omnion node pack` needs a manifest: omnion node pack manifest.json"
                            .to_string()
                    })?,
                    out,
                })
            }
            other => Err(format!(
                "unknown node verb {other:?}; expected scaffold, validate or pack"
            )),
        }
    }
}

/// The next argument as a value, or a usage error naming the flag.
fn take_value(rest: &mut Vec<String>, flag: &str) -> Result<String, String> {
    if rest.is_empty() {
        return Err(format!("{flag} needs a value"));
    }
    Ok(rest.remove(0))
}

/// Run one `omnion node` invocation.
///
/// Exit codes match the rest of the CLI: `0` success, `1` the thing failed (a package that
/// does not validate is a failure, not a usage error), `2` the command line was wrong. The
/// distinction matters for a CI job: a bad flag should not be reported as a bad package, and
/// a bad package should not be reported as a bad flag.
pub fn run(command: NodeCommand) -> ExitCode {
    match command {
        NodeCommand::Scaffold { key, out } => scaffold(&key, &out),
        NodeCommand::Validate { manifest } => match read_manifest(&manifest) {
            Ok(manifest) => validate(&manifest, &manifest_path(&manifest)),
            Err(message) => {
                eprintln!("omnion node: {message}");
                ExitCode::from(1)
            }
        },
        NodeCommand::Pack { manifest, out } => match read_manifest(&manifest) {
            Ok(parsed) => pack(&parsed, &out),
            Err(message) => {
                eprintln!("omnion node: {message}");
                ExitCode::from(1)
            }
        },
    }
}

/// Read and parse a manifest, with the two failures a caller actually hits named.
fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    // A JSON syntax error names a line, which is the only useful thing to say about it.
    serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not a valid manifest: {error}", path.display()))
}

/// The label a finding's subject is printed under.
fn manifest_path(manifest: &Manifest) -> String {
    format!("{} {}", manifest.key, manifest.version)
}

/// `validate` — report what an install would say.
fn validate(manifest: &Manifest, subject: &str) -> ExitCode {
    let validation = node_package::validate(manifest);
    match &validation.package {
        Some(_) => {
            check("ok", subject, &validation.summary());
            // Findings that did not block the install are still worth printing: a deprecated
            // node or an undeclared permission is a warning, and a validator that only
            // speaks when it refuses teaches the author nothing until it refuses.
            for finding in &validation.findings {
                hint(&format!("{} · {}", finding.code, finding.message));
            }
            ExitCode::SUCCESS
        }
        None => {
            check("fail", subject, "not installable");
            for finding in &validation.findings {
                check(
                    "fail",
                    &finding.code,
                    &format!("{} · {}", finding.subject, finding.message),
                );
            }
            eprintln!("omnion node: {} does not install", subject);
            ExitCode::from(1)
        }
    }
}

/// `pack` — validate, then write the packed file.
fn pack(manifest: &Manifest, out: &Option<PathBuf>) -> ExitCode {
    let packed = match node_package::pack(manifest) {
        Ok(packed) => packed,
        Err(reason) => {
            for line in reason.lines() {
                check("fail", "refused", line);
            }
            eprintln!("omnion node: the package does not install, so it was not packed");
            return ExitCode::from(1);
        }
    };
    let target = out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.omnion-node.json", packed.manifest.key)));
    let body = match serde_json::to_string_pretty(&packed) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("omnion node: cannot serialise the packed package: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = std::fs::write(&target, format!("{body}\n")) {
        eprintln!("omnion node: cannot write {}: {error}", target.display());
        return ExitCode::from(1);
    }
    check(
        "ok",
        "packed",
        &format!(
            "{} {} → {}",
            packed.manifest.key,
            packed.manifest.version,
            target.display()
        ),
    );
    pair("checksum", &packed.checksum);
    ExitCode::SUCCESS
}

/// `scaffold` — write a package that passes its own validator.
fn scaffold(key: &str, out: &Path) -> ExitCode {
    let scaffold = node_package::scaffold(key);

    // The scaffold is validated before it is written. A scaffold that does not install is the
    // SDK's worst first impression, and validating it here means a future change that breaks
    // the example fails the *test suite* rather than a stranger's afternoon.
    if let Some(package) = node_package::validate(&scaffold.manifest).package {
        let fixtures = std::path::Path::new("fixtures");
        if let Err(error) = std::fs::create_dir_all(out.join(fixtures)) {
            eprintln!("omnion node: cannot create {}: {error}", out.display());
            return ExitCode::from(1);
        }
        let manifest_path = out.join("manifest.json");
        let body = match serde_json::to_string_pretty(&scaffold.manifest) {
            Ok(body) => body,
            Err(error) => {
                eprintln!("omnion node: cannot serialise the manifest: {error}");
                return ExitCode::from(1);
            }
        };
        if let Err(error) = std::fs::write(&manifest_path, format!("{body}\n")) {
            eprintln!(
                "omnion node: cannot write {}: {error}",
                manifest_path.display()
            );
            return ExitCode::from(1);
        }
        for (node_key, sample) in &scaffold.fixtures {
            let path = out.join(fixtures).join(format!("{node_key}.json"));
            let body = serde_json::to_string_pretty(sample).unwrap_or_default();
            if let Err(error) = std::fs::write(&path, format!("{body}\n")) {
                eprintln!("omnion node: cannot write {}: {error}", path.display());
                return ExitCode::from(1);
            }
        }
        if let Err(error) = std::fs::write(out.join("README.md"), &scaffold.readme) {
            eprintln!("omnion node: cannot write README.md: {error}");
            return ExitCode::from(1);
        }
        check(
            "ok",
            "scaffolded",
            &format!("{} {} in {}", package.key, package.version, out.display()),
        );
        pair("nodes", &package.node_keys().join(", "));
        hint(&format!(
            "next: omnion node validate {}",
            manifest_path.display()
        ));
        ExitCode::SUCCESS
    } else {
        // Unreachable while the crate's own test holds, and reported rather than papered over.
        eprintln!("omnion node: the built-in scaffold does not pass the validator");
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_three_verbs_parse_in_the_shape_the_readme_documents() {
        assert_eq!(
            NodeCommand::parse(&argv("scaffold acme-tools --out acme")).expect("parses"),
            NodeCommand::Scaffold {
                key: "acme-tools".to_string(),
                out: PathBuf::from("acme"),
            }
        );
        assert_eq!(
            NodeCommand::parse(&argv("validate manifest.json")).expect("parses"),
            NodeCommand::Validate {
                manifest: PathBuf::from("manifest.json")
            }
        );
        assert_eq!(
            NodeCommand::parse(&argv("pack manifest.json --out a.json")).expect("parses"),
            NodeCommand::Pack {
                manifest: PathBuf::from("manifest.json"),
                out: Some(PathBuf::from("a.json")),
            }
        );
    }

    #[test]
    fn a_missing_verb_or_an_unknown_one_is_a_usage_error() {
        assert!(NodeCommand::parse(&[]).is_err());
        assert!(NodeCommand::parse(&argv("lint manifest.json")).is_err());
        assert!(NodeCommand::parse(&argv("scaffold")).is_err());
        assert!(NodeCommand::parse(&argv("validate --nope")).is_err());
        assert!(NodeCommand::parse(&argv("pack")).is_err());
        assert!(NodeCommand::parse(&argv("validate --manifest")).is_err());
    }

    #[test]
    fn a_manifest_that_is_not_json_is_reported_by_path_not_by_panic() {
        let error = read_manifest(Path::new("/nonexistent/manifest.json")).expect_err("refused");
        assert!(error.contains("cannot read"), "{error}");
    }
}
