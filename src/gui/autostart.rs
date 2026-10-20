use auto_launch::{AutoLaunch, AutoLaunchBuilder, LinuxLaunchMode, MacOSLaunchMode, WindowsEnableMode};

const APP_NAME: &str = "Ludusavi";
const ARGS: &[&str] = &["gui", "--background"];

fn handle() -> Result<AutoLaunch, String> {
    let executable = std::env::current_exe().map_err(|why| why.to_string())?;
    // The crate writes the Windows registry value and Linux Exec line raw, so quote the path here.
    #[cfg(windows)]
    let path = format!("\"{}\"", executable.display());
    #[cfg(target_os = "linux")]
    let path = format!("\"{}\"", escape_desktop_exec(&executable.to_string_lossy()));
    #[cfg(target_os = "macos")]
    let path = executable.to_string_lossy().into_owned();

    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&path)
        .set_args(ARGS)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_target_is_the_gui_background_command() {
        let handle = handle().unwrap();
        assert_eq!(handle.get_app_name(), APP_NAME);
        assert_eq!(handle.get_args(), ARGS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_exec_quoting_escapes_reserved_characters() {
        assert_eq!(
            escape_desktop_exec(r#"/opt/My Game/$HOME`x"y\z%f"#),
            r#"/opt/My Game/\$HOME\`x\"y\\z%%f"#
        );
    }
}
