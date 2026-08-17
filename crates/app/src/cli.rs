//! The non-graphical commands.
//!
//! Sign-in belongs here as well as in the window: a headless machine, a first
//! run over SSH, and a scripted setup all need an account without a display.
//! It is also the only way to sign in when the shipped build has no GitHub App
//! id, since the device flow needs one and adopting an existing `gh` login does
//! not.

use std::process::ExitCode;

/// A command that runs instead of opening a window.
pub enum Command {
    /// Adopt the `gh` CLI's token for a host.
    Login { host: String },
    /// Forget an account, removing both the roster row and the keyring entry.
    Logout { host: String, login: String },
    /// List signed-in accounts.
    Accounts,
}

impl Command {
    /// Parse a subcommand, or `None` when the arguments are for the GUI.
    pub fn parse(args: &[std::ffi::OsString]) -> Option<Self> {
        let first = args.first()?.to_str()?;
        let rest: Vec<&str> = args[1..].iter().filter_map(|a| a.to_str()).collect();

        // Split flags from positionals in one pass. Scanning separately meant
        // a flag's *value* was picked up as a positional, so
        // `logout --host git.corp.example someone` removed an account called
        // "git.corp.example".
        let mut host = auth::DEFAULT_HOST.to_string();
        let mut positionals: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            match rest[i] {
                "--host" => {
                    if let Some(v) = rest.get(i + 1) {
                        host = v.to_string();
                    }
                    // Skip the value too, or it becomes a positional.
                    i += 2;
                }
                other if other.starts_with("--") => i += 1,
                other => {
                    positionals.push(other);
                    i += 1;
                }
            }
        }

        match first {
            "login" => Some(Self::Login { host }),
            "logout" => Some(Self::Logout {
                host,
                login: positionals
                    .first()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            }),
            "accounts" => Some(Self::Accounts),
            _ => None,
        }
    }

    pub fn run(self) -> ExitCode {
        match self {
            Self::Login { host } => login(&host),
            Self::Logout { host, login } => logout(&host, &login),
            Self::Accounts => accounts(),
        }
    }
}

/// Adopt the token `gh` already holds, verify it, and store it.
///
/// The login name is not in the token — it has to be read from the API, and it
/// is the keyring key, so this step is not optional even for a known-good
/// token.
fn login(host: &str) -> ExitCode {
    let Ok(Some(token)) = auth::gh_import::import(host) else {
        eprintln!(
            "No GitHub CLI token for {host}.\n\n\
             Run `gh auth login` first, or sign in from the app window."
        );
        return ExitCode::FAILURE;
    };

    let store = match db::Db::open_default() {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            eprintln!("Could not open the local database: {e}");
            return ExitCode::FAILURE;
        }
    };

    // A probe account with an empty login: enough to make the request that
    // tells us the real login.
    let probe = auth::Account {
        host: host.to_string(),
        login: String::new(),
        is_default_for_host: true,
    };

    let client = match github::Client::new(probe, token.access.clone(), store.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Could not build an API client: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let user = match runtime.block_on(client.current_user()) {
        Ok(u) => u.data,
        Err(e) => {
            eprintln!("The token was rejected by {host}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let account = auth::Account {
        host: host.to_string(),
        login: user.login.clone(),
        is_default_for_host: true,
    };

    // Keyring first. A roster row pointing at a token that was never stored
    // makes the app look signed in and fail on every request.
    if let Err(e) = auth::store::save(&account, &token) {
        eprintln!("Could not store the token: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = store.upsert_account(host, &user.login, true) {
        eprintln!("Could not record the account: {e}");
        return ExitCode::FAILURE;
    }

    println!("Signed in to {host} as {}", user.login);
    ExitCode::SUCCESS
}

fn logout(host: &str, login: &str) -> ExitCode {
    if login.is_empty() {
        eprintln!("usage: forqen logout <login> [--host HOST]");
        return ExitCode::FAILURE;
    }

    let account = auth::Account {
        host: host.to_string(),
        login: login.to_string(),
        is_default_for_host: false,
    };

    // Keyring first again, for the mirror-image reason: a stored token with no
    // roster row is a credential nothing references and nothing will clean up.
    if let Err(e) = auth::store::delete(&account) {
        eprintln!("Could not remove the stored token: {e}");
        return ExitCode::FAILURE;
    }
    match db::Db::open_default().and_then(|db| db.remove_account(host, login)) {
        Ok(()) => {
            println!("Signed out {login} on {host}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Could not remove the account: {e}");
            ExitCode::FAILURE
        }
    }
}

fn accounts() -> ExitCode {
    let rows = match db::Db::open_default().and_then(|db| db.accounts()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not read accounts: {e}");
            return ExitCode::FAILURE;
        }
    };

    if rows.is_empty() {
        println!("No accounts. Run `forqen login`.");
        return ExitCode::SUCCESS;
    }
    for a in rows {
        println!(
            "{}{}\t{}",
            a.login,
            if a.is_default { " (default)" } else { "" },
            a.host
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn recognises_the_subcommands() {
        assert!(matches!(
            Command::parse(&args(&["login"])),
            Some(Command::Login { .. })
        ));
        assert!(matches!(
            Command::parse(&args(&["accounts"])),
            Some(Command::Accounts)
        ));
        assert!(matches!(
            Command::parse(&args(&["logout", "someone"])),
            Some(Command::Logout { .. })
        ));
    }

    #[test]
    fn a_path_is_not_a_subcommand() {
        // The common case: `forqen /path/to/repo` must still open a window.
        assert!(Command::parse(&args(&["/home/me/code/project"])).is_none());
        assert!(Command::parse(&args(&[])).is_none());
    }

    #[test]
    fn host_defaults_to_github_but_can_be_overridden() {
        match Command::parse(&args(&["login"])).unwrap() {
            Command::Login { host } => assert_eq!(host, auth::DEFAULT_HOST),
            _ => panic!("expected login"),
        }
        match Command::parse(&args(&["login", "--host", "git.corp.example"])).unwrap() {
            Command::Login { host } => assert_eq!(host, "git.corp.example"),
            _ => panic!("expected login"),
        }
    }

    #[test]
    fn logout_takes_the_login_as_a_positional_and_ignores_flags() {
        // Regression: scanning flags and positionals separately made the
        // *value* of --host the first non-flag argument, so this removed an
        // account named "git.corp.example".
        for form in [
            &["logout", "--host", "git.corp.example", "someone"][..],
            &["logout", "someone", "--host", "git.corp.example"][..],
        ] {
            match Command::parse(&args(form)).unwrap() {
                Command::Logout { host, login } => {
                    assert_eq!(host, "git.corp.example", "form {form:?}");
                    assert_eq!(login, "someone", "form {form:?}");
                }
                _ => panic!("expected logout"),
            }
        }
    }

    #[test]
    fn logout_without_a_login_is_reported_rather_than_guessed() {
        match Command::parse(&args(&["logout", "--host", "git.corp.example"])).unwrap() {
            Command::Logout { login, .. } => assert!(login.is_empty()),
            _ => panic!("expected logout"),
        }
    }
}
