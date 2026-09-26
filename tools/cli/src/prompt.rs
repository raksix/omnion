//! Prompting: flags first, a question when a terminal is attached, an error otherwise.

use std::io::{self, IsTerminal, Write};

/// Read one line from standard input (empty when the stream ends).
pub fn read_line() -> Result<String, String> {
    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .map_err(|err| format!("could not read the answer: {err}"))?;
    Ok(buffer.trim().to_owned())
}

/// A required value: the flag, or a prompt, or a usage error.
pub fn required(
    label: &str,
    flag: &str,
    value: Option<String>,
    interactive: bool,
) -> Result<String, String> {
    if let Some(value) = value {
        let trimmed = value.trim().to_owned();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    if !interactive {
        return Err(format!("{label} is required — pass {flag}"));
    }

    loop {
        print!("{label}: ");
        io::stdout()
            .flush()
            .map_err(|err| format!("could not write the prompt: {err}"))?;
        let answer = read_line()?;
        if !answer.is_empty() {
            return Ok(answer);
        }
        println!("  a value is required");
    }
}

/// An optional value with a default: the flag, or a prompt, or the default.
pub fn optional(
    label: &str,
    value: Option<String>,
    default: &str,
    interactive: bool,
) -> Result<String, String> {
    if let Some(value) = value.map(|value| value.trim().to_owned()) {
        return Ok(value);
    }
    if !interactive {
        return Ok(default.to_owned());
    }

    let prompt = if default.is_empty() {
        format!("{label} (optional, Enter to skip): ")
    } else {
        format!("{label} [{default}]: ")
    };
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|err| format!("could not write the prompt: {err}"))?;
    let answer = read_line()?;
    if answer.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(answer)
    }
}

/// Resolve the owner password without ever echoing it on a terminal.
///
/// Order: `--password`, `--password-stdin`, an interactive double prompt (hidden), or a line
/// from a pipe. A non-interactive run without a value is a usage error, because there is no one
/// to ask.
pub fn resolve_password(
    password: Option<&str>,
    from_stdin: bool,
    non_interactive: bool,
) -> Result<String, String> {
    if let Some(value) = password.filter(|value| !value.is_empty()) {
        return Ok(value.to_owned());
    }
    if from_stdin {
        let password = read_line()?;
        if password.is_empty() {
            return Err(
                "--password-stdin was given but nothing arrived on standard input".to_owned(),
            );
        }
        return Ok(password);
    }

    let terminal = io::stdin().is_terminal();
    if !terminal {
        if non_interactive {
            return Err("a password is required — pass --password or --password-stdin".to_owned());
        }
        // Piped input (a script, a Docker run): one line is the password.
        let password = read_line()?;
        if password.is_empty() {
            return Err("a password is required — pass --password or --password-stdin".to_owned());
        }
        return Ok(password);
    }

    let first = rpassword::prompt_password("Owner password: ")
        .map_err(|err| format!("could not read the password: {err}"))?;
    let second = rpassword::prompt_password("Repeat password: ")
        .map_err(|err| format!("could not read the password: {err}"))?;
    if first != second {
        return Err("the two passwords do not match".to_owned());
    }
    if first.is_empty() {
        return Err("a password is required".to_owned());
    }
    Ok(first)
}
