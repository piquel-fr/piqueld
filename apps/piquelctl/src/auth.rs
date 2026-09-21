//! Browser-assisted login and private credentials, separate from connection profiles.
use crate::{
    cli::{Cli, Command},
    error::{CliError, ErrorKind, Result},
    output::{Console, HumanWriter, Report},
};
use piqueld_client::{Client, auth::User};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::PathBuf, time::Duration};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Credentials {
    endpoints: BTreeMap<String, Endpoint>,
}
#[derive(Default, Serialize, Deserialize)]
struct Endpoint {
    selected: String,
    accounts: BTreeMap<String, Account>,
}
#[derive(Serialize, Deserialize)]
struct Account {
    username: String,
    token: String,
}
impl Credentials {
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
    fn read() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::File::open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if file.metadata()?.permissions().mode() & 0o077 != 0 {
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
            Err(error) => Err(error.into()),
        }
    }
    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, self).map_err(std::io::Error::other)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    fn selected<'a>(&'a self, cli: &Cli) -> Result<Option<(&'a str, &'a Account)>> {
        let endpoint = self.endpoints.get(&Self::key(cli));
        let account = endpoint.and_then(|endpoint| {
            let selected = cli.account.as_deref().unwrap_or(&endpoint.selected);
            endpoint
                .accounts
                .iter()
                .find(|(id, account)| id.as_str() == selected || account.username == selected)
                .map(|(id, account)| (id.as_str(), account))
        });
        if cli.account.is_some() && account.is_none() {
            return Err(CliError::new(
                ErrorKind::Input,
                "no saved login for this account and daemon; run piquelctl login",
            ));
        }
        Ok(account)
    }
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
        "Waiting for passkey login…".into(),
    ])?;
    let finish = async {
        let mut interval = u64::from(start.interval);
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
                    let mut credentials = Credentials::read()?;
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
                    credentials.save()?;
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
        ()=tokio::time::sleep(Duration::from_secs(u64::from(start.expires_in)))=>Err(CliError::new(ErrorKind::Input,"login expired; run piquelctl login again")),
        signal=tokio::signal::ctrl_c()=>{signal?;Err(CliError::new(ErrorKind::Interrupted,"login cancelled"))},
    }
}
pub(crate) async fn logout(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    match client.auth_logout().await {
        Ok(_) => {}
        Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 401 => {}
        Err(error) => return Err(error.into()),
    }
    if std::env::var_os("PIQUELD_TOKEN").is_none() {
        let mut credentials = Credentials::read()?;
        let id = credentials.selected(cli)?.map(|(id, _)| id.to_owned());
        if let Some(endpoint) = credentials.endpoints.get_mut(&Credentials::key(cli)) {
            if let Some(id) = id {
                endpoint.accounts.remove(&id);
            }
            if !endpoint.accounts.contains_key(&endpoint.selected) {
                endpoint.selected = endpoint.accounts.keys().next().cloned().unwrap_or_default();
            }
        }
        credentials.save()?;
    }
    console.info("Signed out")
}
