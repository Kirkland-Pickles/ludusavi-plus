mod cli;
mod gui;

#[cfg(test)]
mod testing;

use self::gui::Flags;
use ludusavi::{
    cloud,
    lang::{self, TRANSLATOR},
    metadata, path,
    prelude::{self, CONFIG_DIR, VERSION, app_dir},
    report, resource, scan, wrap,
};

/// The logger handle must be retained until the application closes.
/// https://docs.rs/flexi_logger/0.23.1/flexi_logger/error_info/index.html#write
fn prepare_logging(debug: bool) -> Result<flexi_logger::LoggerHandle, flexi_logger::FlexiLoggerError> {
    if debug {
        flexi_logger::Logger::try_with_str("ludusavi=trace")
            .unwrap()
            .log_to_file(
                flexi_logger::FileSpec::default()
                    .directory(app_dir().as_std_path_buf().unwrap())
                    .basename("ludusavi_debug")
                    .suppress_timestamp(),
            )
            .write_mode(flexi_logger::WriteMode::BufferAndFlush)
            .use_utc()
            .format_for_files(|w, now, record| {
                write!(
                    w,
                    "[{}] {} [{}] {}",
                    now.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    record.level(),
                    record.module_path().unwrap_or("<unnamed>"),
                    record.args(),
                )
            })
            .start()
    } else {
        flexi_logger::Logger::try_with_env_or_str("ludusavi=warn")
            .unwrap()
            .log_to_file(flexi_logger::FileSpec::default().directory(app_dir().as_std_path_buf().unwrap()))
            .write_mode(flexi_logger::WriteMode::BufferAndFlush)
            .rotate(
                flexi_logger::Criterion::Size(1024 * 1024 * 10),
                flexi_logger::Naming::Timestamps,
                flexi_logger::Cleanup::KeepLogFiles(4),
            )
            .use_utc()
            .format_for_files(|w, now, record| {
                write!(
                    w,
                    "[{}] {} [{}] {}",
                    now.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    record.level(),
                    record.module_path().unwrap_or("<unnamed>"),
                    record.args(),
                )
            })
            .start()
    }
}

/// Based on: https://github.com/Traverse-Research/panic-log/blob/874a61b24a8bc8f9b07f9c26dc10b13cbc2622f9/src/lib.rs#L26
/// Modified to flush a provided log handle.
fn prepare_panic_hook(handle: Option<flexi_logger::LoggerHandle>) {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread_name = std::thread::current().name().unwrap_or("<unnamed thread>").to_owned();

        let location = if let Some(panic_location) = info.location() {
            format!(
                "{}:{}:{}",
                panic_location.file(),
                panic_location.line(),
                panic_location.column()
            )
        } else {
            "<unknown location>".to_owned()
        };
        let message = info.payload().downcast_ref::<&str>().unwrap_or(&"");

        let backtrace = std::backtrace::Backtrace::force_capture();

        log::error!("thread '{thread_name}' panicked at {location}:\n{message}\nstack backtrace:\n{backtrace}");

        if let Some(handle) = handle.clone() {
            handle.flush();
        }

        original_hook(info);
    }));
}

fn prepare_winit() {
    if std::env::var("WGPU_POWER_PREF").is_err() {
        unsafe {
            std::env::set_var("WGPU_POWER_PREF", "high");
        }
    }
}

/// Detach GUI launches from the console without changing CLI output or shell waiting.
/// The manifest prevents console allocation on Windows 11 24H2+; older Windows can briefly show it.
/// Reset the standard handles after detaching so Rclone subprocesses can start.
/// https://learn.microsoft.com/en-us/windows/console/console-allocation-policy
#[cfg(target_os = "windows")]
unsafe fn detach_console(debug: bool) {
    unsafe {
        use windows::Win32::{
            Foundation::HANDLE,
            System::Console::{FreeConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle},
        };

        fn tell(msg: &str) {
            eprintln!("{msg}");
            log::error!("{}", msg);
        }

        if FreeConsole().is_err() {
            tell("Unable to detach the console");
            debug_on_exit(debug);
            std::process::exit(1);
        }
        if SetStdHandle(STD_INPUT_HANDLE, HANDLE::default()).is_err() {
            tell("Unable to reset stdin handle");
            debug_on_exit(debug);
            std::process::exit(1);
        }
        if SetStdHandle(STD_OUTPUT_HANDLE, HANDLE::default()).is_err() {
            tell("Unable to reset stdout handle");
            debug_on_exit(debug);
            std::process::exit(1);
        }
        if SetStdHandle(STD_ERROR_HANDLE, HANDLE::default()).is_err() {
            tell("Unable to reset stderr handle");
            debug_on_exit(debug);
            std::process::exit(1);
        }
    }
}

fn main() {
    let mut failed = false;
    let args = cli::parse();

    if let Some(config_dir) = args.as_ref().ok().and_then(|args| args.config.as_ref()) {
        *CONFIG_DIR.lock().unwrap() = Some(config_dir.clone());
    }
    let debug = args.as_ref().map(|x| x.debug).unwrap_or_default();

    #[cfg(target_os = "windows")]
    if args
        .as_ref()
        .is_ok_and(|args| matches!(args.sub, None | Some(cli::parse::Subcommand::Gui { .. })))
        && std::env::var_os("RTSSHooksProfileOverride").is_none()
    {
        unsafe {
            std::env::set_var("RTSSHooksProfileOverride", "EnableOSD,0");
        }
    }

    prepare_winit();
    let logger = prepare_logging(debug);
    #[allow(clippy::useless_asref)]
    prepare_panic_hook(logger.as_ref().map(|x| x.clone()).ok());
    let flush_logger = || {
        if let Ok(logger) = &logger {
            logger.flush();
        }
    };

    log::debug!("Version: {}", *VERSION);
    log::debug!("Invocation: {:?}", std::env::args());

    let args = match args {
        Ok(x) => x,
        Err(e) => {
            match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {}
                _ => {
                    log::error!("CLI failed to parse: {e}");
                }
            }
            flush_logger();
            debug_on_exit(debug);
            e.exit()
        }
    };

    match args.sub {
        None => {
            #[cfg(target_os = "windows")]
            if std::env::var(crate::prelude::ENV_DEBUG).is_err() {
                unsafe {
                    detach_console(debug);
                }
            }

            let flags = Flags {
                update_manifest: !args.no_manifest_update,
                custom_game: None,
                ..Default::default()
            };
            gui::run(flags);
        }
        Some(cli::parse::Subcommand::Gui {
            custom_game,
            background,
            watch,
            backup_delay,
        }) => {
            #[cfg(target_os = "windows")]
            if std::env::var(crate::prelude::ENV_DEBUG).is_err() {
                unsafe {
                    detach_console(debug);
                }
            }

            let flags = Flags {
                update_manifest: !args.no_manifest_update,
                custom_game,
                background,
                watches: watch,
                backup_delay,
            };
            gui::run(flags);
        }
        Some(sub) => {
            let gui = sub.gui();
            let force = sub.force();

            if let Err(e) = cli::run(sub, args.no_manifest_update, args.try_manifest_update) {
                failed = true;
                cli::show_error(&[], &e, gui, force);
            }
        }
    };

    flush_logger();
    debug_on_exit(debug);

    if failed {
        std::process::exit(1);
    }
}

fn debug_on_exit(debug: bool) {
    if debug {
        let path = app_dir();
        if let Err(e) = opener::open(path.raw()) {
            eprintln!("{}", TRANSLATOR.unable_to_open_dir(&path));
            log::error!("Unable to open directory: `{:?}` - {:?}", &path, e);
        }
    }
}
