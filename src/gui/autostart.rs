use auto_launch::{AutoLaunch, AutoLaunchBuilder, LinuxLaunchMode, MacOSLaunchMode, WindowsEnableMode};

const APP_NAME: &str = "Ludusavi";
fn handle() -> Result<AutoLaunch, String> {
    let executable = std::env::current_exe().map_err(|why| why.to_string())?;
    let config = crate::prelude::app_dir().resolve();
    // The crate writes the Windows registry value and Linux Exec line raw, so quote the path here.
    #[cfg(windows)]
    let path = format!("\"{}\"", executable.display());
    #[cfg(target_os = "linux")]
    let path = format!("\"{}\"", escape_desktop_exec(&executable.to_string_lossy()));
    #[cfg(target_os = "macos")]
    let path = executable.to_string_lossy().into_owned();

    #[cfg(windows)]
    let config = format!("\"{}\"", config.replace('\\', "/"));
    #[cfg(target_os = "linux")]
    let config = format!("\"{}\"", escape_desktop_exec(&config));

    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&path)
        .set_args(&[
            "--config".to_owned(),
            config,
            "gui".to_owned(),
            "--background".to_owned(),
        ])
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .set_linux_launch_mode(LinuxLaunchMode::XdgAutostart)
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .build()
        .map_err(|why| why.to_string())
}

#[cfg(target_os = "linux")]
fn escape_desktop_exec(argument: &str) -> String {
    let mut escaped = String::with_capacity(argument.len());
    for c in argument.chars() {
        match c {
            '\\' | '"' | '`' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '%' => escaped.push_str("%%"),
            _ => escaped.push(c),
        }
    }
    escaped
}

pub fn is_enabled() -> Result<bool, String> {
    handle()?.is_enabled().map_err(|why| why.to_string())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let handle = handle()?;
    if enabled { handle.enable() } else { handle.disable() }.map_err(|why| why.to_string())
}
