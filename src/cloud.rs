use std::{
    io::{BufRead, BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    lang::TRANSLATOR,
    prelude::{CommandError, CommandOutput, Error, Finality, Privacy, StrictPath, SyncDirection, run_command},
    resource::config::{App, Config},
    scan::ScanChange,
};

pub fn validate_cloud_config(config: &Config, cloud_path: &str) -> Result<Remote, Error> {
    if !config.apps.rclone.is_valid() {
        return Err(Error::RcloneUnavailable);
    }
    let Some(remote) = config.cloud.remote.clone() else {
        return Err(Error::CloudNotConfigured);
    };
    validate_cloud_path(cloud_path)?;
    Ok(remote)
}

pub fn validate_cloud_path(path: &str) -> Result<(), Error> {
    if path.is_empty() || path == "/" {
        Err(Error::CloudPathInvalid)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CloudChange {
    pub path: String,
    pub change: ScanChange,
}

#[derive(Clone, Debug)]
pub enum RcloneProcessEvent {
    Progress { current: f32, max: f32 },
    Change(CloudChange),
}

#[derive(Debug)]
pub struct RcloneProcess {
    program: String,
    args: Vec<String>,
    child: std::process::Child,
    stderr: Option<BufReader<std::process::ChildStderr>>,
}

impl RcloneProcess {
    pub fn launch(program: String, args: Vec<String>) -> Result<Self, CommandError> {
        let mut command = std::process::Command::new(&program);
        command
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
        }

        log::debug!("Running command: {} {:?}", &program, &args);

        let mut child = command.spawn().map_err(|e| {
            let e = CommandError::Launched {
                program: program.clone(),
                args: args.clone(),
                raw: e.to_string(),
            };
            log::error!("Rclone failed: {e:?}");
            e
        })?;

        let stderr = child.stderr.take().map(BufReader::new);
        Ok(Self {
            program,
            args,
            child,
            stderr,
        })
    }

    pub fn events(&mut self) -> Vec<RcloneProcessEvent> {
        match self.stderr.as_mut() {
            Some(stderr) => Self::parse_events(stderr.lines().take(10).filter_map(Result::ok)),
            None => vec![],
        }
    }

    fn parse_events(lines: impl Iterator<Item = String>) -> Vec<RcloneProcessEvent> {
        let mut events = vec![];

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase", untagged)]
        enum Log {
            Skip { skipped: String, object: String },
            Change { msg: String, object: String },
            Stats { stats: Stats },
        }

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Stats {
            bytes: f32,
            total_bytes: f32,
        }

        for line in lines {
            match serde_json::from_str::<Log>(&line) {
                Ok(Log::Skip { skipped, object }) => match skipped.as_str() {
                    "copy" => events.push(RcloneProcessEvent::Change(CloudChange {
                        path: object,
                        change: ScanChange::Different,
                    })),
                    "delete" => events.push(RcloneProcessEvent::Change(CloudChange {
                        path: object,
                        change: ScanChange::Removed,
                    })),
                    raw => {
                        log::trace!("Unhandled Rclone 'skipped': {raw}");
                    }
                },
                Ok(Log::Change { msg, object }) => match msg.as_str() {
                    "Copied (new)" => events.push(RcloneProcessEvent::Change(CloudChange {
                        path: object,
                        change: ScanChange::New,
                    })),
                    "Copied (replaced existing)" => events.push(RcloneProcessEvent::Change(CloudChange {
                        path: object,
                        change: ScanChange::Different,
                    })),
                    "Deleted" => events.push(RcloneProcessEvent::Change(CloudChange {
                        path: object,
                        change: ScanChange::Removed,
                    })),
                    raw => {
                        log::trace!("Unhandled Rclone 'msg': {raw}");
                    }
                },
                Ok(Log::Stats {
                    stats: Stats { bytes, total_bytes },
                }) => {
                    if total_bytes > 0.0 {
                        events.push(RcloneProcessEvent::Progress {
                            current: bytes,
                            max: total_bytes,
                        });
                    }
                }
                Err(_) => {
                    log::trace!("Unhandled Rclone message: {line}");
                }
            }
        }
        if !events.is_empty() {
            log::trace!("New Rclone events: {events:?}");
        }
        events
    }

    fn wait(&mut self, cancel: &AtomicBool) -> Result<CommandOutput, CommandError> {
        let stdout = self.child.stdout.take().unwrap();
        let stderr = self.stderr.take().unwrap();
        let terminated = || CommandError::Terminated {
            program: self.program.clone(),
            args: self.args.clone(),
        };
        let (status, stdout, stderr) = std::thread::scope(|scope| {
            fn read(mut reader: impl Read) -> std::io::Result<String> {
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map(|_| String::from_utf8_lossy(&bytes).into_owned())
            }
            let stdout = scope.spawn(move || read(stdout));
            let stderr = scope.spawn(move || read(stderr));
            let status = loop {
                if cancel.load(Ordering::Relaxed) {
                    let _ = self.child.kill();
                    break self.child.wait();
                }
                match self.child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(error) => {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break Err(error);
                    }
                }
            };
            (status, stdout.join(), stderr.join())
        });
        let status = status.map_err(|_| terminated())?;
        let stdout = stdout.map_err(|_| terminated())?.map_err(|_| terminated())?;
        let stderr = stderr.map_err(|_| terminated())?.map_err(|_| terminated())?;
        if cancel.load(Ordering::Relaxed) {
            return Err(terminated());
        }
        match status.code() {
            Some(0) => Ok(CommandOutput {
                code: 0,
                stdout,
                stderr,
            }),
            Some(code) => Err(CommandError::Exited {
                program: self.program.clone(),
                args: self.args.clone(),
                code,
                stdout: (!stdout.is_empty()).then_some(stdout),
                stderr: (!stderr.is_empty()).then_some(stderr),
            }),
            None => Err(terminated()),
        }
    }

    pub fn succeeded(&mut self) -> Option<Result<(), CommandError>> {
        let res = match self.child.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(0) => Some(Ok(())),
                Some(code) => {
                    let stdout = self.child.stdout.as_mut().and_then(|x| {
                        let lines = BufReader::new(x).lines().map_while(Result::ok).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });
                    let stderr = self.stderr.as_mut().and_then(|x| {
                        let lines = x.lines().map_while(Result::ok).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });

                    Some(Err(CommandError::Exited {
                        program: self.program.clone(),
                        args: self.args.clone(),
                        code,
                        stdout,
                        stderr,
                    }))
                }
                None => Some(Err(CommandError::Terminated {
                    program: self.program.clone(),
                    args: self.args.clone(),
                })),
            },
            Ok(None) => None,
            Err(_) => Some(Err(CommandError::Terminated {
                program: self.program.clone(),
                args: self.args.clone(),
            })),
        };

        if let Some(Ok(_)) = &res {
            log::debug!("Rclone succeeded");
        }
        if let Some(Err(e)) = &res {
            log::error!("Rclone failed: {e:?}");
        }

        res
    }

    pub fn kill(&mut self) -> Result<(), std::io::Error> {
        let res = self.child.kill();
        if let Err(e) = &res {
            log::error!("Unable to kill child process for Rclone: {e:?}");
        }
        res
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteChoice {
    None,
    Custom,
    Box,
    Dropbox,
    Ftp,
    GoogleDrive,
    OneDrive,
    Smb,
    WebDav,
}

impl RemoteChoice {
    pub const ALL: &'static [Self] = &[
        Self::None,
        Self::Box,
        Self::Dropbox,
        Self::GoogleDrive,
        Self::OneDrive,
        Self::Ftp,
        Self::Smb,
        Self::WebDav,
        Self::Custom,
    ];
}

impl ToString for RemoteChoice {
    fn to_string(&self) -> String {
        match self {
            Self::None => TRANSLATOR.none_label(),
            Self::Custom => TRANSLATOR.custom_label(),
            Self::Box => "Box".to_string(),
            Self::Dropbox => "Dropbox".to_string(),
            Self::Ftp => "FTP".to_string(),
            Self::GoogleDrive => "Google Drive".to_string(),
            Self::OneDrive => "OneDrive".to_string(),
            Self::Smb => "SMB".to_string(),
            Self::WebDav => "WebDAV".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename = "camelCase")] // Legacy: Should have been `rename_all`
pub enum Remote {
    Custom {
        id: String,
    },
    Box {
        id: String,
    },
    Dropbox {
        id: String,
    },
    GoogleDrive {
        id: String,
    },
    OneDrive {
        id: String,
    },
    Ftp {
        id: String,
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    Smb {
        id: String,
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    WebDav {
        id: String,
        url: String,
        username: String,
        #[serde(skip, default)]
        password: String,
        provider: WebDavProvider,
    },
}

impl Remote {
    pub fn id(&self) -> &str {
        match self {
            Remote::Box { id } => id,
            Remote::Custom { id } => id,
            Remote::Dropbox { id } => id,
            Remote::GoogleDrive { id } => id,
            Remote::OneDrive { id } => id,
            Remote::Ftp { id, .. } => id,
            Remote::Smb { id, .. } => id,
            Remote::WebDav { id, .. } => id,
        }
    }

    pub fn slug(&self) -> &str {
        match self {
            Self::Custom { .. } => "",
            Self::Box { .. } => "box",
            Self::Dropbox { .. } => "dropbox",
            Self::Ftp { .. } => "ftp",
            Self::GoogleDrive { .. } => "drive",
            Self::OneDrive { .. } => "onedrive",
            Self::Smb { .. } => "smb",
            Self::WebDav { .. } => "webdav",
        }
    }

    pub fn config_args(&self) -> Option<Vec<String>> {
        match self {
            Self::Custom { .. } => None,
            Self::Box { .. } => None,
            Self::Dropbox { .. } => None,
            Self::GoogleDrive { .. } => Some(vec!["scope=drive".to_string()]),
            Self::Ftp {
                id: _,
                host,
                port,
                username,
                password,
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::OneDrive { .. } => Some(vec![
                "drive_type=personal".to_string(),
                "access_scopes=Files.ReadWrite,offline_access".to_string(),
            ]),
            Self::Smb {
                id: _,
                host,
                port,
                username,
                password,
                ..
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::WebDav {
                id: _,
                url,
                username,
                password,
                provider,
            } => Some(vec![
                format!("url={url}"),
                format!("user={username}"),
                format!("pass={password}"),
                format!("vendor={}", provider.slug()),
            ]),
        }
    }

    pub fn needs_configuration(&self) -> bool {
        match self {
            Self::Custom { .. } => false,
            Self::Box { .. }
            | Self::Dropbox { .. }
            | Self::Ftp { .. }
            | Self::GoogleDrive { .. }
            | Self::OneDrive { .. }
            | Self::Smb { .. }
            | Self::WebDav { .. } => true,
        }
    }

    pub fn description(&self) -> Option<String> {
        match self {
            Remote::Ftp {
                host, port, username, ..
            } => Some(format!("{username}@{host}:{port}")),
            Remote::Smb {
                host, port, username, ..
            } => Some(format!("{username}@{host}:{port}")),
            Remote::WebDav { url, provider, .. } => Some(format!("{} - {}", provider.to_string(), url)),
            _ => None,
        }
    }

    pub fn generate_id() -> String {
        format!("ludusavi-{}", chrono::Utc::now().timestamp())
    }
}

impl From<Option<&Remote>> for RemoteChoice {
    fn from(value: Option<&Remote>) -> Self {
        if let Some(value) = value {
            match value {
                Remote::Custom { .. } => RemoteChoice::Custom,
                Remote::Box { .. } => RemoteChoice::Box,
                Remote::Dropbox { .. } => RemoteChoice::Dropbox,
                Remote::Ftp { .. } => RemoteChoice::Ftp,
                Remote::GoogleDrive { .. } => RemoteChoice::GoogleDrive,
                Remote::OneDrive { .. } => RemoteChoice::OneDrive,
                Remote::Smb { .. } => RemoteChoice::Smb,
                Remote::WebDav { .. } => RemoteChoice::WebDav,
            }
        } else {
            RemoteChoice::None
        }
    }
}

impl TryFrom<RemoteChoice> for Remote {
    type Error = ();

    fn try_from(value: RemoteChoice) -> Result<Self, Self::Error> {
        match value {
            RemoteChoice::None => Err(()),
            RemoteChoice::Custom => Ok(Remote::Custom {
                id: "ludusavi".to_string(),
            }),
            RemoteChoice::Box => Ok(Remote::Box {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Dropbox => Ok(Remote::Dropbox {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Ftp => Ok(Remote::Ftp {
                id: Remote::generate_id(),
                host: String::new(),
                port: 21,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::GoogleDrive => Ok(Remote::GoogleDrive {
                id: Remote::generate_id(),
            }),
            RemoteChoice::OneDrive => Ok(Remote::OneDrive {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Smb => Ok(Remote::Smb {
                id: Remote::generate_id(),
                host: String::new(),
                port: 445,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::WebDav => Ok(Remote::WebDav {
                id: Remote::generate_id(),
                url: String::new(),
                username: String::new(),
                password: String::new(),
                provider: WebDavProvider::Other,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub enum WebDavProvider {
    #[default]
    Other,
    Nextcloud,
    Owncloud,
    Sharepoint,
    SharepointNtlm,
}

impl WebDavProvider {
    pub const ALL: &'static [Self] = &[
        Self::Other,
        Self::Nextcloud,
        Self::Owncloud,
        Self::Sharepoint,
        Self::SharepointNtlm,
    ];

    pub const ALL_CLI: &'static [&'static str] = &[
        Self::OTHER,
        Self::NEXTCLOUD,
        Self::OWNCLOUD,
        Self::SHAREPOINT,
        Self::SHAREPOINT_NTLM,
    ];
    pub const OTHER: &'static str = "other";
    const NEXTCLOUD: &'static str = "nextcloud";
    const OWNCLOUD: &'static str = "owncloud";
    const SHAREPOINT: &'static str = "sharepoint";
    const SHAREPOINT_NTLM: &'static str = "sharepoint-ntlm";
}

impl WebDavProvider {
    pub fn slug(&self) -> &str {
        match self {
            WebDavProvider::Other => Self::OTHER,
            WebDavProvider::Nextcloud => Self::NEXTCLOUD,
            WebDavProvider::Owncloud => Self::OWNCLOUD,
            WebDavProvider::Sharepoint => Self::SHAREPOINT,
            WebDavProvider::SharepointNtlm => Self::SHAREPOINT_NTLM,
        }
    }
}

impl ToString for WebDavProvider {
    fn to_string(&self) -> String {
        match self {
            Self::Other => crate::resource::manifest::Store::Other.to_string(),
            Self::Nextcloud => "Nextcloud".to_string(),
            Self::Owncloud => "Owncloud".to_string(),
            Self::Sharepoint => "Sharepoint".to_string(),
            Self::SharepointNtlm => "Sharepoint (NTLM)".to_string(),
        }
    }
}

impl std::str::FromStr for WebDavProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::OTHER => Ok(Self::Other),
            Self::NEXTCLOUD => Ok(Self::Nextcloud),
            Self::OWNCLOUD => Ok(Self::Owncloud),
            Self::SHAREPOINT => Ok(Self::Sharepoint),
            Self::SHAREPOINT_NTLM => Ok(Self::SharepointNtlm),
            _ => Err(format!("invalid provider: {s}")),
        }
    }
}

pub struct Rclone {
    app: App,
    remote: Remote,
}

#[derive(Clone, Debug)]
pub struct MetadataEdit {
    pub file: StrictPath,
    pub before: String,
    pub after: String,
}

fn escape_filter_pattern(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

impl Rclone {
    pub fn new(app: App, remote: Remote) -> Self {
        Self { app, remote }
    }

    fn path(&self, path: &str) -> String {
        // Rclone doesn't allow backslashes in non-local paths:
        // https://rclone.org/docs/#syntax-of-remote-paths
        format!("{}:{}", self.remote.id(), path.replace('\\', "/"))
    }

    fn args(&self, args: &[String]) -> Vec<String> {
        let mut collected = vec![];
        if !self.app.arguments.is_empty()
            && let Some(parts) = shlex::split(&self.app.arguments)
        {
            collected.extend(parts);
        }
        for arg in args {
            collected.push(arg.to_string());
        }
        collected
    }

    fn run(&self, args: &[String], success: &[i32], privacy: Privacy) -> Result<CommandOutput, CommandError> {
        let args = self.args(args);
        let args: Vec<_> = args.iter().map(|x| x.as_str()).collect();
        run_command(self.app.path.raw(), &args, success, privacy)
    }

    fn obscure(&self, credential: &str) -> Result<String, CommandError> {
        let out = self.run(&["obscure".to_string(), credential.to_string()], &[0], Privacy::Private)?;
        Ok(out.stdout)
    }

    pub fn configure_remote(&self) -> Result<(), CommandError> {
        if !self.remote.needs_configuration() {
            return Ok(());
        }

        let mut privacy = Privacy::Public;

        let mut remote = self.remote.clone();
        match &mut remote {
            Remote::Custom { .. }
            | Remote::Box { .. }
            | Remote::Dropbox { .. }
            | Remote::GoogleDrive { .. }
            | Remote::OneDrive { .. } => {}
            Remote::Ftp { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::Smb { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::WebDav { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
        }

        let mut args = vec![
            "config".to_string(),
            "create".to_string(),
            remote.id().to_string(),
            remote.slug().to_string(),
        ];

        if let Some(config_args) = remote.config_args() {
            args.extend(config_args);
        }

        self.run(&args, &[0], privacy)?;
        Ok(())
    }

    pub fn unconfigure_remote(&self) -> Result<(), CommandError> {
        if !self.remote.needs_configuration() {
            return Ok(());
        }

        let args = vec!["config".to_string(), "delete".to_string(), self.remote.id().to_string()];

        self.run(&args, &[0], Privacy::Public)?;
        Ok(())
    }

    fn sync_args(
        &self,
        local: &StrictPath,
        remote_path: &str,
        direction: SyncDirection,
        finality: Finality,
        game_dirs: &[String],
    ) -> Vec<String> {
        let mut args = vec![
            "sync".to_string(),
            "-v".to_string(),
            "--use-json-log".to_string(),
            "--stats=100ms".to_string(),
            // Terminal progress output is incompatible with the JSON log parser.
            // A command-line value also overrides RCLONE_PROGRESS.
            "--progress=false".to_string(),
        ];

        if finality.preview() {
            args.push("--dry-run".to_string());
        }

        for game_dir in game_dirs {
            // Inclusion rules are file-based, so we have to add `**`.
            args.push(format!("--include=/{game_dir}/**"));
        }

        match direction {
            SyncDirection::Upload => {
                args.push(local.render());
                args.push(self.path(remote_path));
            }
            SyncDirection::Download => {
                args.push(self.path(remote_path));
                args.push(local.render());
            }
        }

        args
    }

    pub fn sync(
        &self,
        local: &StrictPath,
        remote_path: &str,
        direction: SyncDirection,
        finality: Finality,
        game_dirs: &[String],
    ) -> Result<RcloneProcess, CommandError> {
        self.launch_sync(
            local,
            direction,
            self.sync_args(local, remote_path, direction, finality, game_dirs),
        )
    }

    fn launch_sync(
        &self,
        local: &StrictPath,
        direction: SyncDirection,
        args: Vec<String>,
    ) -> Result<RcloneProcess, CommandError> {
        if direction == SyncDirection::Upload && !local.exists() {
            // Rclone will fail with exit code 3 if the local folder does not exist.
            _ = local.create_dirs();
        }

        RcloneProcess::launch(self.app.path.raw().into(), self.args(&args))
    }

    pub fn upload_metadata(
        &self,
        root: &StrictPath,
        remote_path: &str,
        edit: &MetadataEdit,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        self.upload_metadata_with(
            root,
            remote_path,
            edit,
            |args| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::SomeEntriesFailed);
                }
                RcloneProcess::launch(self.app.path.raw().into(), self.args(args))
                    .and_then(|mut process| process.wait(cancel))
                    .map_err(Error::UnableToSynchronizeCloud)
            },
            |finality, games| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::SomeEntriesFailed);
                }
                let snapshot = if finality.preview() {
                    None
                } else {
                    let snapshot = tempfile::tempdir().map_err(|_| Error::SomeEntriesFailed)?;
                    let relative = edit.file.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
                    let root_path = root.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
                    let relative = relative.strip_prefix(root_path).map_err(|_| Error::CloudConflict)?;
                    let file = snapshot.path().join(relative);
                    std::fs::create_dir_all(file.parent().unwrap()).map_err(|_| Error::SomeEntriesFailed)?;
                    std::fs::write(file, &edit.after).map_err(|_| Error::SomeEntriesFailed)?;
                    let modified = std::fs::metadata(edit.file.as_std_path_buf().map_err(|_| Error::CloudConflict)?)
                        .and_then(|metadata| metadata.modified())
                        .map_err(|_| Error::SomeEntriesFailed)?;
                    std::fs::File::options()
                        .write(true)
                        .open(snapshot.path().join(relative))
                        .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(modified)))
                        .map_err(|_| Error::SomeEntriesFailed)?;
                    Some(snapshot)
                };
                let mut process = if finality.preview() {
                    let games: Vec<_> = games.iter().map(|game| escape_filter_pattern(game)).collect();
                    self.sync(root, remote_path, SyncDirection::Upload, finality, &games)
                } else {
                    let file = edit.file.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
                    let root_path = root.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
                    let relative = file.strip_prefix(root_path).map_err(|_| Error::CloudConflict)?;
                    let source = StrictPath::from(snapshot.as_ref().unwrap().path().join(relative));
                    RcloneProcess::launch(
                        self.app.path.raw().into(),
                        self.args(&[
                            "copyto".into(),
                            source.render(),
                            self.path(&format!(
                                "{}/{}",
                                remote_path.trim_end_matches('/'),
                                relative.to_string_lossy()
                            )),
                            "-v".into(),
                            "--use-json-log".into(),
                            "--progress=false".into(),
                            "--ignore-times".into(),
                        ]),
                    )
                }
                .map_err(Error::UnableToSynchronizeCloud)?;
                let output = process.wait(cancel).map_err(Error::UnableToSynchronizeCloud)?;
                Ok(RcloneProcess::parse_events(output.stderr.lines().map(str::to_owned))
                    .into_iter()
                    .filter_map(|event| match event {
                        RcloneProcessEvent::Change(change) => Some(change),
                        _ => None,
                    })
                    .collect())
            },
        )
    }

    fn upload_metadata_with(
        &self,
        root: &StrictPath,
        remote_path: &str,
        edit: &MetadataEdit,
        read: impl FnOnce(&[String]) -> Result<CommandOutput, Error>,
        mut sync: impl FnMut(Finality, &[String]) -> Result<Vec<CloudChange>, Error>,
    ) -> Result<(), Error> {
        if edit.before == edit.after {
            return Ok(());
        }
        let root = root.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
        let file = edit.file.as_std_path_buf().map_err(|_| Error::CloudConflict)?;
        let relative = file.strip_prefix(root).map_err(|_| Error::CloudConflict)?;
        // A game mapping is the only file an annotation can change.
        if relative.components().count() != 2 || relative.file_name().and_then(|x| x.to_str()) != Some("mapping.yaml") {
            return Err(Error::CloudConflict);
        }
        if edit.file.try_read().ok().as_deref() != Some(edit.after.as_str()) {
            return Err(Error::SomeEntriesFailed);
        }
        let destination = self.path(&format!(
            "{}/{}",
            remote_path.trim_end_matches('/'),
            relative.to_string_lossy()
        ));
        let remote = read(&["cat".into(), destination])?.stdout;
        if remote.trim() == edit.after.trim() {
            return Ok(());
        }
        if remote.trim() != edit.before.trim() {
            return Err(Error::CloudConflict);
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let game = relative.split('/').next().ok_or(Error::CloudConflict)?.to_string();
        let games = [game];
        let changes = sync(Finality::Preview, &games)?;
        if changes
            .iter()
            .any(|change| change.path.replace('\\', "/") != relative || change.change == ScanChange::Removed)
        {
            return Err(Error::CloudConflict);
        }
        sync(Finality::Final, &games)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires LUDUSAVI_TEST_GAME pointing to the process helper"]
    fn cancelling_metadata_wait_terminates_and_reaps_the_process() {
        let helper = std::env::var("LUDUSAVI_TEST_GAME").unwrap();
        let mut process = RcloneProcess::launch(helper, vec![]).unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            let start = std::time::Instant::now();
            assert!(matches!(process.wait(&cancel), Err(CommandError::Terminated { .. })));
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            assert!(process.child.try_wait().unwrap().is_some());
        });
    }

    #[test]
    #[ignore = "Requires LUDUSAVI_TEST_RCLONE pointing to a real Rclone executable"]
    fn note_sync_with_local_test_remote() {
        let temp = tempfile::tempdir().unwrap();
        let base = StrictPath::new(temp.path().to_string_lossy().into_owned());
        let root = base.joined("backups");
        let remote_root = base.joined("remote");
        let rclone_config = base.joined("rclone.conf");
        rclone_config.write_with_content("[test]\ntype = local\n").unwrap();
        let rclone = Rclone::new(
            App {
                path: StrictPath::new(std::env::var("LUDUSAVI_TEST_RCLONE").unwrap()),
                arguments: format!("--config \"{}\"", rclone_config.render()),
            },
            Remote::Custom { id: "test".into() },
        );
        let file = root.joined("game/mapping.yaml");
        file.create_parent_dir().unwrap();
        file.write_with_content("original note\n").unwrap();
        root.joined("game/save.txt").write_with_content("saved game").unwrap();
        let remote_path = remote_root.render();
        rclone
            .run(
                &rclone.sync_args(&root, &remote_path, SyncDirection::Upload, Finality::Final, &[]),
                &[0],
                Privacy::Public,
            )
            .unwrap();
        file.write_with_content("modified note\n").unwrap();
        let edit = MetadataEdit {
            file: file.clone(),
            before: "original note\n".into(),
            after: file.try_read().unwrap(),
        };
        rclone
            .upload_metadata(&root, &remote_path, &edit, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(remote_root.joined("game/mapping.yaml").try_read().unwrap(), edit.after);
        assert_eq!(remote_root.joined("game/save.txt").try_read().unwrap(), "saved game");
        let preview = rclone
            .run(
                &rclone.sync_args(&root, &remote_path, SyncDirection::Upload, Finality::Preview, &[]),
                &[0],
                Privacy::Public,
            )
            .unwrap();
        assert!(
            RcloneProcess::parse_events(preview.stderr.lines().map(str::to_owned))
                .iter()
                .all(|event| !matches!(event, RcloneProcessEvent::Change(_))),
            "{}",
            preview.stderr
        );
        // A change from another machine must still block the ordinary backup upload.
        remote_root
            .joined("game/save.txt")
            .write_with_content("a newer cloud save")
            .unwrap();
        file.write_with_content("another note\n").unwrap();
        let edit = MetadataEdit {
            file: file.clone(),
            before: edit.after,
            after: file.try_read().unwrap(),
        };
        assert!(matches!(
            rclone.upload_metadata(&root, &remote_path, &edit, &AtomicBool::new(false)),
            Err(Error::CloudConflict)
        ));
        assert_eq!(
            remote_root.joined("game/save.txt").try_read().unwrap(),
            "a newer cloud save"
        );
        assert_ne!(remote_root.joined("game/mapping.yaml").try_read().unwrap(), edit.after);

        for (selected, other) in [("Game [1]", "Game 1"), ("Game {1,2}", "Game 2")] {
            for base in [&root, &remote_root] {
                let mapping = base.joined(&format!("{selected}/mapping.yaml"));
                mapping.create_parent_dir().unwrap();
                mapping.write_with_content("original note\n").unwrap();
                let mapping = base.joined(&format!("{other}/mapping.yaml"));
                mapping.create_parent_dir().unwrap();
                mapping.write_with_content("unrelated note\n").unwrap();
            }
            let file = root.joined(&format!("{selected}/mapping.yaml"));
            file.write_with_content("modified note\n").unwrap();
            let edit = MetadataEdit {
                file,
                before: "original note\n".into(),
                after: "modified note\n".into(),
            };
            rclone
                .upload_metadata(&root, &remote_path, &edit, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(
                remote_root
                    .joined(&format!("{selected}/mapping.yaml"))
                    .try_read()
                    .unwrap(),
                edit.after
            );
            assert_eq!(
                remote_root.joined(&format!("{other}/mapping.yaml")).try_read().unwrap(),
                "unrelated note\n"
            );
        }

        let root = base.joined("delete-excluded/local");
        let remote = base.joined("delete-excluded/remote");
        for directory in [&root, &remote] {
            let file = directory.joined("game/mapping.yaml");
            file.create_parent_dir().unwrap();
            file.write_with_content("original note\n").unwrap();
            directory
                .joined("game/save.dat")
                .write_with_content("saved game")
                .unwrap();
        }
        let file = root.joined("game/mapping.yaml");
        file.write_with_content("modified note\n").unwrap();
        let rclone = Rclone::new(
            App {
                path: StrictPath::new(std::env::var("LUDUSAVI_TEST_RCLONE").unwrap()),
                arguments: format!("--config \"{}\" --delete-excluded", rclone_config.render()),
            },
            Remote::Custom { id: "test".into() },
        );
        rclone
            .upload_metadata(
                &root,
                &remote.render(),
                &MetadataEdit {
                    file,
                    before: "original note\n".into(),
                    after: "modified note\n".into(),
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            remote.joined("game/mapping.yaml").try_read().unwrap(),
            "modified note\n"
        );
        assert_eq!(remote.joined("game/save.dat").try_read().unwrap(), "saved game");
    }

    #[test]
    fn metadata_filters_treat_game_folder_names_literally() {
        assert_eq!(
            escape_filter_pattern(r"Game [1] {a,b} * ? \\"),
            r"Game \[1\] \{a,b\} \* \? \\\\"
        );
        let rclone = Rclone::new(App::default(), Remote::Custom { id: "test".into() });
        let args = rclone.sync_args(
            &StrictPath::new("backups"),
            "backups",
            SyncDirection::Upload,
            Finality::Final,
            &[escape_filter_pattern("Game [1]")],
        );
        assert!(args.contains(&r"--include=/Game \[1\]/**".to_owned()));
    }

    fn metadata_fixture() -> (tempfile::TempDir, StrictPath, MetadataEdit, Rclone) {
        let temp = tempfile::tempdir().unwrap();
        let root = StrictPath::new(temp.path().to_string_lossy().into_owned());
        let file = root.joined("game/mapping.yaml");
        file.create_parent_dir().unwrap();
        file.write_with_content("new note\n").unwrap();
        let edit = MetadataEdit {
            file,
            before: "old note\n".into(),
            after: "new note\n".into(),
        };
        let rclone = Rclone::new(App::default(), Remote::Custom { id: "cloud".into() });
        (temp, root, edit, rclone)
    }

    #[test]
    fn annotation_upload_uses_existing_sync_after_safe_preview() {
        let (_temp, root, edit, rclone) = metadata_fixture();
        let mut calls = Vec::new();
        rclone
            .upload_metadata_with(
                &root,
                "backups",
                &edit,
                |_| {
                    Ok(CommandOutput {
                        code: 0,
                        stdout: edit.before.clone(),
                        stderr: String::new(),
                    })
                },
                |finality, games| {
                    calls.push((finality, games.to_vec()));
                    Ok(vec![CloudChange {
                        path: "game/mapping.yaml".into(),
                        change: ScanChange::Different,
                    }])
                },
            )
            .unwrap();
        assert_eq!(
            calls,
            vec![
                (Finality::Preview, vec!["game".into()]),
                (Finality::Final, vec!["game".into()])
            ]
        );
        rclone
            .upload_metadata_with(
                &root,
                "backups",
                &edit,
                |_| {
                    Ok(CommandOutput {
                        code: 0,
                        stdout: edit.after.clone(),
                        stderr: String::new(),
                    })
                },
                |_, _| panic!("Already synchronized"),
            )
            .unwrap();
    }

    #[test]
    fn annotation_upload_preserves_independent_cloud_changes() {
        let (_temp, root, edit, rclone) = metadata_fixture();
        let result = rclone.upload_metadata_with(
            &root,
            "backups",
            &edit,
            |_| {
                Ok(CommandOutput {
                    code: 0,
                    stdout: "another machine's backup".into(),
                    stderr: String::new(),
                })
            },
            |_, _| panic!("Must not sync a conflict"),
        );
        assert!(matches!(result, Err(Error::CloudConflict)));
        let mut calls = 0;
        let result = rclone.upload_metadata_with(
            &root,
            "backups",
            &edit,
            |_| {
                Ok(CommandOutput {
                    code: 0,
                    stdout: edit.before.clone(),
                    stderr: String::new(),
                })
            },
            |finality, _| {
                calls += 1;
                assert_eq!(finality, Finality::Preview);
                Ok(vec![CloudChange {
                    path: "game/save.zip".into(),
                    change: ScanChange::Different,
                }])
            },
        );
        assert!(matches!(result, Err(Error::CloudConflict)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn annotation_upload_rejects_unsaved_edits_and_paths_outside_root() {
        let (_temp, root, mut edit, rclone) = metadata_fixture();
        edit.after = "not saved".into();
        assert!(matches!(
            rclone.upload_metadata_with(&root, "backups", &edit, |_| panic!(), |_, _| panic!()),
            Err(Error::SomeEntriesFailed)
        ));
        assert!(matches!(
            rclone.upload_metadata_with(&root.joined("other"), "backups", &edit, |_| panic!(), |_, _| panic!()),
            Err(Error::CloudConflict)
        ));
    }

    #[test]
    fn annotation_upload_does_not_sync_after_failed_cloud_read() {
        let (_temp, root, edit, rclone) = metadata_fixture();
        let result = rclone.upload_metadata_with(
            &root,
            "backups",
            &edit,
            |_| Err(Error::SomeEntriesFailed),
            |_, _| panic!("Must not sync after a failed read"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn sync_disables_terminal_progress_after_custom_arguments() {
        let rclone = Rclone::new(
            App {
                path: StrictPath::new("rclone".to_string()),
                arguments: "--progress".to_string(),
            },
            Remote::Custom {
                id: "cloud".to_string(),
            },
        );

        let sync_args = rclone.sync_args(
            &StrictPath::new("local".to_string()),
            "backups",
            SyncDirection::Upload,
            Finality::Final,
            &[],
        );
        let args = rclone.args(&sync_args);

        assert!(args.contains(&"--use-json-log".to_string()));
        assert!(args.contains(&"--stats=100ms".to_string()));
        let progress = args.iter().position(|x| x == "--progress").unwrap();
        let no_progress = args.iter().position(|x| x == "--progress=false").unwrap();
        assert!(progress < no_progress);
    }
}

#[cfg(feature = "app")]
pub mod rclone_monitor {
    use iced::{
        Subscription,
        futures::{StreamExt, channel::mpsc, stream},
    };

    use crate::{
        cloud::{RcloneProcess, RcloneProcessEvent},
        prelude::CommandError,
    };

    #[derive(Debug, Clone)]
    pub enum Event {
        Ready(mpsc::Sender<Input>),
        Data(Vec<RcloneProcessEvent>),
        Succeeded,
        Failed(CommandError),
        Cancelled,
    }

    #[derive(Debug)]
    pub enum Input {
        Process(RcloneProcess),
        Tick,
        Cancel,
    }

    enum State {
        Starting,
        Ready {
            receiver: mpsc::Receiver<Input>,
            process: Option<RcloneProcess>,
            interval: tokio::time::Interval,
        },
    }

    pub fn run() -> Subscription<Event> {
        let unfold = || {
            stream::unfold(State::Starting, |state| async move {
                match state {
                    State::Starting => {
                        let (sender, receiver) = mpsc::channel(10_000);

                        Some((
                            Event::Ready(sender),
                            State::Ready {
                                receiver,
                                process: None,
                                interval: tokio::time::interval(std::time::Duration::from_millis(1)),
                            },
                        ))
                    }
                    State::Ready {
                        mut receiver,
                        mut process,
                        mut interval,
                    } => loop {
                        let input = tokio::select!(
                            input = receiver.select_next_some() => {
                                input
                            }
                            _ = interval.tick() => {
                                Input::Tick
                            }
                        );

                        match input {
                            Input::Process(new_process) => {
                                if let Some(proc) = process.as_mut() {
                                    let _ = proc.child.kill();
                                }
                                process = Some(new_process);
                            }
                            Input::Tick => {
                                if let Some(proc) = process.as_mut() {
                                    let events = proc.events();
                                    if !events.is_empty() {
                                        return Some((
                                            Event::Data(events),
                                            State::Ready {
                                                receiver,
                                                process,
                                                interval,
                                            },
                                        ));
                                    }
                                    if let Some(outcome) = proc.succeeded() {
                                        match outcome {
                                            Ok(_) => {
                                                return Some((
                                                    Event::Succeeded,
                                                    State::Ready {
                                                        receiver,
                                                        process: None,
                                                        interval,
                                                    },
                                                ));
                                            }
                                            Err(e) => {
                                                return Some((
                                                    Event::Failed(e),
                                                    State::Ready {
                                                        receiver,
                                                        process: None,
                                                        interval,
                                                    },
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            Input::Cancel => {
                                if let Some(proc) = process.as_mut() {
                                    let _ = proc.kill();
                                }
                                return Some((
                                    Event::Cancelled,
                                    State::Ready {
                                        receiver,
                                        process: None,
                                        interval,
                                    },
                                ));
                            }
                        }
                    },
                }
            })
        };

        Subscription::run(unfold)
    }
}
