//! Browser-assisted login and private credentials, separate from connection profiles.
use crate::{
    cli::{Cli, Command},
    error::{CliError, ErrorKind, Result},
    output::{Console, HumanWriter, Report},
};
use piqueld_client::{Client, auth::User};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// Upper bound for one device login, regardless of the daemon's `expires_in`.
const MAX_DEVICE_LOGIN_SECS: u64 = 15 * 60;

/// Private per-user login store (`credentials.json`), keyed by daemon endpoint.
/// Kept separate from profiles so shareable connection config never holds tokens.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Credentials {
    endpoints: BTreeMap<String, Endpoint>,
}
/// Saved accounts for one daemon endpoint.
#[derive(Default, Serialize, Deserialize)]
struct Endpoint {
    /// Account ID used when `--account` is not given; empty when none remain.
    selected: String,
    /// Accounts keyed by stable user ID.
    accounts: BTreeMap<String, Account>,
}
/// One saved login: display username and its bearer token.
#[derive(Serialize, Deserialize)]
struct Account {
    username: String,
    token: String,
}
impl Credentials {
    /// Credentials file location: `PIQUELD_CREDENTIALS_FILE`, else
    /// `$XDG_CONFIG_HOME/piqueld/credentials.json`, else `$HOME/.config/piqueld/credentials.json`.
    fn path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("PIQUELD_CREDENTIALS_FILE") {
            return Ok(path.into());
        }
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
            .ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    "set PIQUELD_CREDENTIALS_FILE or HOME to store login credentials",
                )
            })?;
        Ok(root.join("piqueld/credentials.json"))
    }
    /// Endpoint key that scopes saved logins to one daemon.
    ///
    /// ```text
    /// --url https://ops.example/  →  https://ops.example
    /// --socket /tmp/p.sock        →  unix:/tmp/p.sock
    /// (neither)                   →  unix:/run/piqueld/piqueld.sock
    /// ```
    fn key(cli: &Cli) -> String {
        cli.url.as_ref().map_or_else(
            || {
                format!(
                    "unix:{}",
                    cli.socket.as_ref().map_or_else(
                        || crate::support::DEFAULT_SOCKET.into(),
                        |p| p.to_string_lossy().into_owned()
                    )
                )
            },
            |url| url.trim_end_matches('/').to_owned(),
        )
    }
    /// Reads the credentials file at its default location.
    fn read() -> Result<Self> {
        Self::read_at(&Self::path()?)
    }
    /// Reads credentials from `path`; a missing file is an empty store. On Unix,
    /// refuses files with any group or other permission bits so tokens are never silently exposed.
    fn read_at(path: &Path) -> Result<Self> {
        match std::fs::File::open(path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let metadata = file.metadata().map_err(|e| Self::io_error(path, &e))?;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(CliError::new(
                            ErrorKind::Input,
                            "credentials file must be readable only by its owner (chmod 600)",
                        ));
                    }
                }
                serde_json::from_reader(file).map_err(|e| {
                    CliError::new(ErrorKind::Input, format!("read credentials file: {e}"))
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Self::io_error(path, &error)),
        }
    }
    /// Reports a credential-file I/O failure with the file's path, so it is not
    /// mistaken for an output write error.
    fn io_error(path: &Path, error: &std::io::Error) -> CliError {
        CliError::new(
            ErrorKind::General,
            format!("credentials file {}: {error}", path.display()),
        )
    }
    /// Lock a stable sidecar (never the atomically replaced credentials inode)
    /// across the whole read/modify/write. Network requests happen outside it.
    /// The new contents are written to a synced temp file and renamed over `path`,
    /// so readers never observe a partial file.
    fn update_at(path: &Path, update: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let _lock = Self::lock_at(path).map_err(|e| Self::io_error(path, &e))?;
        let mut credentials = Self::read_at(path)?;
        update(&mut credentials)?;
        credentials
            .write_at(path)
            .map_err(|e| Self::io_error(path, &e))
    }
    /// Creates the parent directory and takes an exclusive lock on the sidecar
    /// `<path>.lock`, held until the returned file is dropped.
    fn lock_at(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(Self::parent(path))?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = options.open(PathBuf::from(lock_path))?;
        lock.lock()?;
        Ok(lock)
    }
    /// Writes to a synced temp file beside `path`, then renames it over `path`.
    fn write_at(&self, path: &Path) -> std::io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(Self::parent(path))?;
        serde_json::to_writer(&mut file, self).map_err(std::io::Error::other)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    /// Directory holding `path`, or `.` for a bare file name.
    fn parent(path: &Path) -> &Path {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }
    /// Removes account `id` under endpoint `key` only if it still holds `token`,
    /// then reselects the first remaining account if the selected one is gone.
    fn remove(&mut self, key: &str, id: &str, token: &str) {
        if let Some(endpoint) = self.endpoints.get_mut(key) {
            // A login completed while remote revocation was in flight. Keep its
            // replacement token, which the logout request did not revoke.
            if endpoint
                .accounts
                .get(id)
                .is_some_and(|account| account.token == token)
            {
                endpoint.accounts.remove(id);
            }
            if !endpoint.accounts.contains_key(&endpoint.selected) {
                endpoint.selected = endpoint.accounts.keys().next().cloned().unwrap_or_default();
            }
        }
    }
    /// Finds the active account for the CLI's endpoint: `--account` (matched by ID or
    /// username) if given, else the endpoint's selected account. Returns `(id, account)`.
    /// An explicit `--account` with no match is an error rather than an anonymous request.
    fn selected<'a>(&'a self, cli: &Cli) -> Result<Option<(&'a str, &'a Account)>> {
        let endpoint = self.endpoints.get(&Self::key(cli));
        let account = endpoint.and_then(|endpoint| {
            let selected = cli.auth.account.as_deref().unwrap_or(&endpoint.selected);
            endpoint
                .accounts
                .iter()
                .find(|(id, account)| id.as_str() == selected || account.username == selected)
                .map(|(id, account)| (id.as_str(), account))
        });
        if cli.auth.account.is_some() && account.is_none() {
            return Err(CliError::new(
                ErrorKind::Input,
                "no saved login for this account and daemon; run piquelctl login",
            ));
        }
        Ok(account)
    }
    /// Adds a bearer token to `client`: `PIQUELD_TOKEN` wins over the saved login.
    /// `login` skips this, and without any token the client is returned unchanged.
    pub(crate) fn attach(cli: &Cli, client: Client) -> Result<Client> {
        if matches!(cli.command, Command::Login) {
            return Ok(client);
        }
        let token = match std::env::var("PIQUELD_TOKEN") {
            Ok(token) => Some(token),
            Err(std::env::VarError::NotPresent) => Self::read()?
                .selected(cli)?
                .map(|(_, account)| account.token.clone()),
            Err(_) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    "PIQUELD_TOKEN must contain valid text",
                ));
            }
        };
        token.map_or(Ok(client.clone()), |token| {
            client.with_bearer(&token).map_err(Into::into)
        })
    }
}
/// Authenticated account result, shared by `login` and `whoami`.
pub(crate) struct AccountReport(pub User);
impl Report for AccountReport {
    type Json = User;
    fn json(&self) -> &User {
        &self.0
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> std::io::Result<()> {
        out.line(format_args!("{} ({})", self.0.username, self.0.id))
    }
}
/// Runs the browser device-code login:
/// 1. Refuses when the daemon has no first account yet (that needs the setup link).
/// 2. Starts a device login and prints the verification URL, code, and requester address.
/// 3. Polls at the daemon's interval (backing off on `slow_down`) until complete.
/// 4. Saves the token as the endpoint's selected account and emits it.
///
/// Bounded by the daemon's `expires_in` (capped at `MAX_DEVICE_LOGIN_SECS`) and Ctrl-C
/// rather than the whole-command `--timeout`, since approval waits on the operator.
/// Each individual request still uses `--timeout`.
pub(crate) async fn login(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    let status = client.auth_status().await?;
    if !status.initialized {
        return Err(CliError::new(
            ErrorKind::Input,
            "daemon needs its first account; open the setup-link file from its data directory in a browser",
        ));
    }
    let start = client.auth_device_start().await?;
    console.prompt_lines(&[
        format!("Open {}", start.verification_uri),
        format!("Enter code: {}", start.user_code),
        // The approval page shows the same address, so the approver can match them.
        format!(
            "The daemon sees this login coming from: {}",
            start
                .requester
                .as_deref()
                .unwrap_or("the daemon's local Unix socket")
        ),
        "Waiting for passkey login…".into(),
    ])?;
    let finish = async {
        // Never let a daemon response turn polling into a tight loop.
        let mut interval = u64::from(start.interval).max(1);
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let result = client.auth_device_poll(&start.device_code).await?;
            match result.status.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += 5,
                "complete" => {
                    let user = result.user.ok_or_else(|| {
                        CliError::new(ErrorKind::General, "device login omitted account")
                    })?;
                    let token = result.token.ok_or_else(|| {
                        CliError::new(ErrorKind::General, "device login omitted credential")
                    })?;
                    Credentials::update_at(&Credentials::path()?, |credentials| {
                        let endpoint = credentials
                            .endpoints
                            .entry(Credentials::key(cli))
                            .or_default();
                        endpoint.selected.clone_from(&user.id);
                        endpoint.accounts.insert(
                            user.id.clone(),
                            Account {
                                username: user.username.clone(),
                                token,
                            },
                        );
                        Ok(())
                    })?;
                    return console.emit(&AccountReport(user));
                }
                _ => {
                    return Err(CliError::new(
                        ErrorKind::General,
                        "unexpected device login response",
                    ));
                }
            }
        }
    };
    tokio::select! {
        result=finish=>result,
        ()=tokio::time::sleep(Duration::from_secs(u64::from(start.expires_in).min(MAX_DEVICE_LOGIN_SECS)))=>Err(CliError::new(ErrorKind::Input,"login expired; run piquelctl login again")),
        signal=tokio::signal::ctrl_c()=>{signal?;Err(CliError::new(ErrorKind::Interrupted,"login cancelled"))},
    }
}
/// Revokes the current credential on the daemon, then drops its saved copy.
/// A 401 counts as already revoked. With `PIQUELD_TOKEN` set, only the
/// environment token is revoked and the credentials file is left untouched.
pub(crate) async fn logout(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    let saved = if std::env::var_os("PIQUELD_TOKEN").is_none() {
        Credentials::read()?
            .selected(cli)?
            .map(|(id, account)| (id.to_owned(), account.token.clone()))
    } else {
        None
    };
    let client = if let Some((_, token)) = &saved {
        client.clone().with_bearer(token)?
    } else {
        client.clone()
    };
    match client.auth_logout().await {
        Ok(_) => {}
        Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 401 => {}
        Err(error) => return Err(error.into()),
    }
    if let Some((id, token)) = saved {
        Credentials::update_at(&Credentials::path()?, |credentials| {
            credentials.remove(&Credentials::key(cli), &id, &token);
            Ok(())
        })?;
    }
    console.info("Signed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_io_errors_name_the_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("credentials.json");
        let error = Credentials::update_at(&path, |_| Ok(())).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("credentials file {}: ", path.display())),
            "{error}"
        );
    }

    #[test]
    fn concurrent_credential_updates_preserve_every_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for index in 0..16 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    Credentials::update_at(path, |credentials| {
                        let endpoint = credentials.endpoints.entry("daemon".into()).or_default();
                        let id = index.to_string();
                        endpoint.accounts.insert(
                            id.clone(),
                            Account {
                                username: id,
                                token: "secret".into(),
                            },
                        );
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(
            Credentials::read_at(&path).unwrap().endpoints["daemon"]
                .accounts
                .len(),
            16
        );
    }

    #[test]
    fn logout_keeps_a_concurrent_replacement_login() {
        let mut credentials = Credentials::default();
        let endpoint = credentials.endpoints.entry("daemon".into()).or_default();
        endpoint.selected = "alice".into();
        endpoint.accounts.insert(
            "alice".into(),
            Account {
                username: "alice".into(),
                token: "new".into(),
            },
        );
        credentials.remove("daemon", "alice", "old");
        assert_eq!(
            credentials.endpoints["daemon"].accounts["alice"].token,
            "new"
        );
        credentials.remove("daemon", "alice", "new");
        assert!(credentials.endpoints["daemon"].accounts.is_empty());
    }
}
