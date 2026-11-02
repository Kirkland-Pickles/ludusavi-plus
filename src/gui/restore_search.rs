use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    prelude::StrictPath,
    resource::config::ToggledPaths,
    scan::{ScanKind, layout::BackupLayout},
};

const FOLDER_NAME: &str = "ludusavi-backup";

#[derive(Clone, Debug)]
pub enum Event {
    Found(StrictPath),
    Finished { limited: bool },
}

#[derive(Default)]
pub struct Search {
    pub searching: bool,
    pub generation: u64,
    target: Option<StrictPath>,
    pub cancel: Arc<AtomicBool>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Search {
    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.searching = false;
        self.generation += 1;
    }

    pub fn start(&mut self, path: StrictPath) -> (u64, Arc<AtomicBool>) {
        self.stop();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.searching = true;
        self.target = Some(path);
        (self.generation, self.cancel.clone())
    }

    pub fn accepts(&self, generation: u64, path: &StrictPath) -> bool {
        self.searching && generation == self.generation && self.target.as_ref() == Some(path)
    }
}

fn candidate(path: &Path) -> Option<StrictPath> {
    if !path.file_name()?.to_string_lossy().eq_ignore_ascii_case(FOLDER_NAME) || !path.is_dir() {
        return None;
    }
    let path = StrictPath::from(path.to_path_buf());
    has_backups(&path).then_some(path)
}

pub fn has_backups(path: &StrictPath) -> bool {
    let layout = BackupLayout::new(path.clone());
    layout
        .restorable_games()
        .into_iter()
        .any(|game| game_has_payload(layout.game_layout(&game)))
}

fn game_has_payload(mut layout: crate::scan::layout::GameLayout) -> bool {
    // Older mappings have no `backups` entries until this read-only migration
    // reconstructs the legacy `.` backup in memory.
    layout.migrate_legacy_backup(false);

    layout.restorable_backups_flattened().into_iter().any(|backup| {
        if !backup.needed() {
            return false;
        }
        if backup.name().ends_with(".zip") {
            return layout
                .path
                .joined(backup.name())
                .as_std_path_buf()
                .ok()
                .and_then(|path| std::fs::File::open(path).ok())
                .and_then(|file| zip::ZipArchive::new(file).ok())
                .is_some_and(|archive| !archive.is_empty());
        }

        let id = backup.id();
        let files = layout.restorable_files(&id, ScanKind::Backup, &[], false, &ToggledPaths::default(), None);
        files.keys().any(StrictPath::is_file)
            || (backup.includes_registry() && layout.registry_content(&id).is_some_and(|content| !content.is_empty()))
    })
}

fn walk(roots: &[PathBuf], cancel: &AtomicBool, budget: Duration, mut found: impl FnMut(StrictPath)) -> bool {
    let start = Instant::now();
    let mut seen = HashSet::new();
    for root in roots {
        let mut entries = walkdir::WalkDir::new(root)
            .follow_links(false)
            .follow_root_links(false)
            .into_iter();
        while let Some(entry) = entries.next() {
            if cancel.load(Ordering::Relaxed) || start.elapsed() >= budget {
                return true;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            #[cfg(unix)]
            if matches!(entry.path().to_str(), Some("/proc" | "/sys" | "/dev" | "/run")) {
                entries.skip_current_dir();
                continue;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.file_attributes() & 0x400 != 0) {
                    entries.skip_current_dir();
                    continue;
                }
            }
            if let Some(path) = candidate(entry.path()) {
                let key = entry.path().to_path_buf();
                #[cfg(windows)]
                let key = PathBuf::from(key.to_string_lossy().to_lowercase());
                if seen.insert(key) {
                    found(path);
                }
            }
        }
    }
    false
}

pub fn find(cancel: Arc<AtomicBool>, preferred: Vec<PathBuf>, send: impl Fn(Event) + Sync) {
    for path in &preferred {
        for path in [path.clone(), path.join(FOLDER_NAME)] {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = StrictPath::from(path);
            if has_backups(&path) {
                send(Event::Found(path));
                send(Event::Finished { limited: false });
                return;
            }
        }
    }
    if let Some(paths) = indexed_paths(&cancel) {
        let mut valid = false;
        for path in paths {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(path) = candidate(&path) {
                valid = true;
                send(Event::Found(path));
            }
        }
        if valid {
            send(Event::Finished { limited: false });
            return;
        }
    }
    let roots = search_roots();
    use rayon::prelude::*;
    let start = Instant::now();
    let limited = AtomicBool::new(false);
    let preferred_limited = walk(&preferred, &cancel, Duration::from_secs(10), |path| {
        send(Event::Found(path))
    });
    if preferred_limited && cancel.load(Ordering::Relaxed) {
        return;
    }
    roots.par_iter().for_each(|root| {
        let remaining = Duration::from_secs(60).saturating_sub(start.elapsed());
        if walk(std::slice::from_ref(root), &cancel, remaining, |path| {
            send(Event::Found(path))
        }) {
            limited.store(true, Ordering::Relaxed);
        }
    });
    send(Event::Finished {
        limited: limited.load(Ordering::Relaxed),
    });
}

#[cfg(windows)]
fn indexed_paths(cancel: &AtomicBool) -> Option<Vec<PathBuf>> {
    everything::query(cancel)
}

#[cfg(target_os = "linux")]
fn indexed_paths(_cancel: &AtomicBool) -> Option<Vec<PathBuf>> {
    None
}

#[cfg(target_os = "macos")]
fn indexed_paths(cancel: &AtomicBool) -> Option<Vec<PathBuf>> {
    use std::{
        io::Read,
        process::{Command, Stdio},
    };
    let mut child = Command::new("/usr/bin/mdfind")
        .args(["-0", "-name", FOLDER_NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(1024 * 1024).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let succeeded = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            Ok(None) => {}
        }
        if cancel.load(Ordering::Relaxed) || start.elapsed() >= Duration::from_secs(3) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !succeeded {
        let _ = child.kill();
    }
    let _ = child.wait();
    let bytes = reader.join().ok()?.ok()?;
    succeeded.then(|| {
        bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
            .collect()
    })
}

#[cfg(windows)]
fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    use windows::{
        Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives},
        core::PCWSTR,
    };
    let drives = unsafe { GetLogicalDrives() };
    for letter in b'A'..=b'Z' {
        if drives & (1 << (letter - b'A')) == 0 {
            continue;
        }
        let root = format!("{}:\\", letter as char);
        let wide: Vec<_> = root.encode_utf16().chain(Some(0)).collect();
        // Fixed disks and removable storage; leave network and optical drives alone.
        if matches!(unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) }, 2 | 3) {
            roots.push(root.into());
        }
    }
    roots
}

#[cfg(unix)]
fn search_roots() -> Vec<PathBuf> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut roots: Vec<_> = disks
        .iter()
        .filter(|disk| {
            !matches!(
                disk.file_system().to_str(),
                Some("nfs" | "nfs4" | "cifs" | "smbfs" | "sshfs")
            )
        })
        .map(|disk| disk.mount_point().to_path_buf())
        .collect();
    if roots.is_empty() {
        roots.push(PathBuf::from("/"));
    }
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(windows)]
mod everything {
    use super::*;
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            UI::WindowsAndMessaging::*,
        },
        core::w,
    };

    // Everything IPC v1 (Unicode).
    // https://www.voidtools.com/support/everything/sdk/ipc/
    const REPLY: usize = 0x4c554455;
    #[repr(C)]
    struct CopyData {
        tag: usize,
        size: u32,
        data: *const u8,
    }

    unsafe extern "system" fn receive(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if message == WM_COPYDATA && lp.0 != 0 {
                let data = &*(lp.0 as *const CopyData);
                let reply = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Option<Vec<u8>>;
                if data.tag == REPLY && !reply.is_null() && !data.data.is_null() && data.size <= 4 * 1024 * 1024 {
                    *reply = Some(std::slice::from_raw_parts(data.data, data.size as usize).to_vec());
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, message, wp, lp)
        }
    }

    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
                #[cfg(target_pointer_width = "64")]
                let original = GetClassLongPtrW(self.0, GCLP_WNDPROC);
                #[cfg(target_pointer_width = "32")]
                let original = GetClassLongW(self.0, GCLP_WNDPROC);
                if original != 0 {
                    #[cfg(target_pointer_width = "64")]
                    SetWindowLongPtrW(self.0, GWLP_WNDPROC, original as isize);
                    #[cfg(target_pointer_width = "32")]
                    SetWindowLongPtrW(self.0, GWLP_WNDPROC, original as i32);
                }
                let _ = DestroyWindow(self.0);
            }
        }
    }

    pub fn query(cancel: &AtomicBool) -> Option<Vec<PathBuf>> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        unsafe {
            let server = FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION"), None)
                .or_else(|_| FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION_(1.5a)"), None))
                .ok()?;
            query_server(server, cancel)
        }
    }

    fn query_server(server: HWND, cancel: &AtomicBool) -> Option<Vec<PathBuf>> {
        unsafe {
            let window = Window(
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    None,
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
                .ok()?,
            );
            let mut reply: Option<Vec<u8>> = None;
            #[cfg(target_pointer_width = "64")]
            {
                SetWindowLongPtrW(window.0, GWLP_USERDATA, (&mut reply as *mut Option<Vec<u8>>) as isize);
                SetWindowLongPtrW(window.0, GWLP_WNDPROC, receive as *const () as isize);
            }
            #[cfg(target_pointer_width = "32")]
            {
                SetWindowLongPtrW(window.0, GWLP_USERDATA, (&mut reply as *mut Option<Vec<u8>>) as i32);
                SetWindowLongPtrW(window.0, GWLP_WNDPROC, receive as *const () as i32);
            }
            let mut query = Vec::new();
            // HWNDs are 32 bits on both Windows architectures in this wire protocol.
            for field in [window.0.0 as usize as u32, REPLY as u32, 0, 0, 128] {
                query.extend_from_slice(&field.to_le_bytes());
            }
            for unit in "folder: regex:^ludusavi-backup$".encode_utf16().chain(Some(0)) {
                query.extend_from_slice(&unit.to_le_bytes());
            }
            let data = CopyData {
                tag: 2,
                size: query.len() as u32,
                data: query.as_ptr(),
            };
            let mut accepted = 0;
            if SendMessageTimeoutW(
                server,
                WM_COPYDATA,
                WPARAM(window.0.0 as usize),
                LPARAM((&data as *const CopyData) as isize),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&mut accepted),
            )
            .0 == 0
                || accepted == 0
            {
                return None;
            }
            let start = Instant::now();
            while reply.is_none() && start.elapsed() < Duration::from_secs(3) && !cancel.load(Ordering::Relaxed) {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, Some(window.0), 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&message);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            parse(&reply?)
        }
    }

    fn parse(data: &[u8]) -> Option<Vec<PathBuf>> {
        use std::os::windows::ffi::OsStringExt;
        fn number(data: &[u8], at: usize) -> Option<usize> {
            Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize)
        }
        fn wide(data: &[u8], at: usize) -> Option<std::ffi::OsString> {
            if !at.is_multiple_of(2) {
                return None;
            }
            let mut units = Vec::new();
            for bytes in data.get(at..)?.as_chunks::<2>().0.iter() {
                let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
                if unit == 0 {
                    return Some(std::ffi::OsString::from_wide(&units));
                }
                units.push(unit);
            }
            None
        }
        let count = number(data, 20)?;
        if count > 128 || data.len() < 28 + 12 * count {
            return None;
        }
        let mut paths = Vec::new();
        for i in 0..count {
            let item = 28 + 12 * i;
            if number(data, item)? & 1 == 0 {
                continue;
            }
            let name = wide(data, number(data, item + 4)?)?;
            let parent = wide(data, number(data, item + 8)?)?;
            paths.push(PathBuf::from(parent).join(name));
        }
        Some(paths)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn result() -> Vec<u8> {
            let mut data = vec![0; 40];
            for at in [0, 8, 12, 20, 28] {
                data[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
            }
            data[32..36].copy_from_slice(&40u32.to_le_bytes());
            for unit in FOLDER_NAME.encode_utf16().chain(Some(0)) {
                data.extend_from_slice(&unit.to_le_bytes());
            }
            let offset = data.len() as u32;
            data[36..40].copy_from_slice(&offset.to_le_bytes());
            for unit in "C:\\sandbox\\日本語".encode_utf16().chain(Some(0)) {
                data.extend_from_slice(&unit.to_le_bytes());
            }
            data
        }

        #[test]
        fn parses_unicode_and_rejects_truncated_or_invalid_ipc_results() {
            let data = result();
            assert_eq!(
                parse(&data).unwrap(),
                vec![PathBuf::from("C:\\sandbox\\日本語").join(FOLDER_NAME)]
            );
            assert!(parse(&data[..38]).is_none());
            let mut invalid = data.clone();
            invalid[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(parse(&invalid).is_none());
            let mut file = data;
            file[28..32].copy_from_slice(&0u32.to_le_bytes());
            assert!(parse(&file).unwrap().is_empty());
        }

        unsafe extern "system" fn answer(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            unsafe {
                if message == WM_COPYDATA && lp.0 != 0 {
                    let request = &*(lp.0 as *const CopyData);
                    let result = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Vec<u8>;
                    if request.tag != 2 || request.size < 22 || result.is_null() {
                        return LRESULT(0);
                    }
                    let query = std::slice::from_raw_parts(request.data, request.size as usize);
                    let text: Vec<_> = query[20..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .take_while(|c| *c != 0)
                        .collect();
                    if String::from_utf16_lossy(&text) != "folder: regex:^ludusavi-backup$" {
                        return LRESULT(0);
                    }
                    let receiver = u32::from_le_bytes([query[0], query[1], query[2], query[3]]);
                    let data = CopyData {
                        tag: REPLY,
                        size: (*result).len() as u32,
                        data: (*result).as_ptr(),
                    };
                    SendMessageTimeoutW(
                        HWND(receiver as usize as *mut _),
                        WM_COPYDATA,
                        wp,
                        LPARAM((&data as *const CopyData) as isize),
                        SMTO_ABORTIFHUNG,
                        1000,
                        None,
                    );
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, message, wp, lp)
            }
        }

        #[test]
        fn queries_a_sandboxed_ipc_server_without_external_tools() {
            unsafe {
                let server = Window(
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        None,
                        WINDOW_STYLE::default(),
                        0,
                        0,
                        0,
                        0,
                        Some(HWND_MESSAGE),
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                );
                let data = result();
                SetWindowLongPtrW(server.0, GWLP_USERDATA, (&data as *const Vec<u8>) as isize);
                SetWindowLongPtrW(server.0, GWLP_WNDPROC, answer as *const () as isize);
                assert_eq!(
                    query_server(server.0, &AtomicBool::new(false)).unwrap(),
                    vec![PathBuf::from("C:\\sandbox\\日本語").join(FOLDER_NAME)]
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(root: &Path) {
        std::fs::create_dir_all(root.join("game")).unwrap();
        std::fs::write(
            root.join("game/mapping.yaml"),
            include_str!("../../tests/backup/game3-renamed/mapping.yaml"),
        )
        .unwrap();
    }

    fn backup(root: &Path) {
        mapping(root);
        std::fs::write(
            root.join("game/registry.yaml"),
            include_str!("../../tests/backup/game3-renamed/registry.yaml"),
        )
        .unwrap();
    }

    #[test]
    fn finds_exact_folder_names_with_original_mapping_parser() {
        let root = tempfile::tempdir().unwrap();
        backup(&root.path().join("nested/日本語/LUDUSAVI-BACKUP"));
        backup(&root.path().join("other/ludusavi-backup"));
        backup(&root.path().join("ludusavi-backup-old"));
        std::fs::create_dir_all(root.path().join("empty/ludusavi-backup")).unwrap();
        std::fs::write(root.path().join("ludusavi-backup"), "file, not directory").unwrap();
        let mut found = Vec::new();
        assert!(!walk(
            &[root.path().to_path_buf(), root.path().to_path_buf()],
            &AtomicBool::new(false),
            Duration::from_secs(5),
            |path| found.push(path)
        ));
        assert_eq!(found.len(), 2);
        assert_eq!(
            found.into_iter().collect::<HashSet<_>>(),
            [
                StrictPath::from(root.path().join("nested").join("日本語").join("LUDUSAVI-BACKUP")),
                StrictPath::from(root.path().join("other").join("ludusavi-backup")),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn rejects_mapping_only_candidates_and_missing_payloads() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join(FOLDER_NAME);
        std::fs::create_dir_all(empty.join("empty-game")).unwrap();
        std::fs::write(
            empty.join("empty-game/mapping.yaml"),
            "---\nname: empty-game\ndrives: {}\nbackups: []\n",
        )
        .unwrap();
        assert!(candidate(&empty).is_none());

        let missing = root.path().join("missing").join(FOLDER_NAME);
        mapping(&missing);
        assert!(candidate(&missing).is_none());
    }

    #[test]
    fn accepts_configured_backup_folders_with_other_names() {
        let root = tempfile::tempdir().unwrap();
        let configured = root.path().join("my-backups");
        backup(&configured);
        assert!(has_backups(&StrictPath::from(configured.clone())));
        let events = std::sync::Mutex::new(Vec::new());
        find(Arc::new(AtomicBool::new(false)), vec![configured.clone()], |event| {
            events.lock().unwrap().push(event);
        });
        assert!(
            matches!(events.lock().unwrap().first(), Some(Event::Found(path)) if *path == StrictPath::from(configured))
        );
    }

    #[test]
    fn accepts_legacy_payload_without_writing_migrated_mapping() {
        let root = tempfile::tempdir().unwrap();
        let backup_root = root.path().join(FOLDER_NAME);
        let game = backup_root.join("legacy-game");
        std::fs::create_dir_all(game.join("drive-0")).unwrap();
        let mapping = "---\nname: legacy-game\ndrives:\n  drive-0: \"\"\nbackups: []\n";
        std::fs::write(game.join("mapping.yaml"), mapping).unwrap();
        std::fs::write(game.join("drive-0/save.dat"), b"harmless test payload").unwrap();

        assert!(candidate(&backup_root).is_some());
        assert_eq!(mapping, std::fs::read_to_string(game.join("mapping.yaml")).unwrap());
    }

    #[test]
    fn rejects_corrupt_archives_and_accepts_readable_payloads() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let backup_root = root.path().join(FOLDER_NAME);
        let game = backup_root.join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("mapping.yaml"),
            "name: game\ndrives: {}\nbackups:\n  - name: backup.zip\n    when: 2000-01-02T03:04:05Z\n    files:\n      /save.dat: {hash: test, size: 1}\n    registry: {hash: null}\n    children: []\n").unwrap();
        std::fs::write(game.join("backup.zip"), "not a zip").unwrap();
        assert!(candidate(&backup_root).is_none());
        let mut archive = zip::ZipWriter::new(std::fs::File::create(game.join("backup.zip")).unwrap());
        archive
            .start_file("save.dat", zip::write::FileOptions::default())
            .unwrap();
        archive.write_all(b"x").unwrap();
        archive.finish().unwrap();
        assert!(candidate(&backup_root).is_some());
    }

    #[test]
    fn cancellation_and_time_limit_stop_disk_search() {
        let root = tempfile::tempdir().unwrap();
        backup(&root.path().join(FOLDER_NAME));
        for (cancel, budget) in [(true, Duration::from_secs(5)), (false, Duration::ZERO)] {
            assert!(walk(
                &[root.path().to_path_buf()],
                &AtomicBool::new(cancel),
                budget,
                |_| panic!("must stop before finding candidates")
            ));
        }
    }

    #[test]
    fn cancellation_and_changed_paths_reject_old_search_results() {
        let root = tempfile::tempdir().unwrap();
        let missing = StrictPath::from(root.path().join("missing"));
        let changed = StrictPath::from(root.path().join("changed"));
        let mut search = Search::default();
        let (old, cancel) = search.start(missing.clone());
        assert!(search.accepts(old, &missing));
        assert!(!search.accepts(old, &changed));
        search.start(changed.clone());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!search.accepts(old, &missing));
        search.stop();
        assert!(!search.searching);
        assert!(!search.accepts(search.generation, &changed));
    }
}
