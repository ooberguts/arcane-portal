#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native {
    use std::ffi::OsStr;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::path::{Component, Path, PathBuf};
    use std::process::{ChildStdin, Command, Stdio};
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use zip::ZipArchive;

    const STAGE_PREFIX: &str = ".arcane-portal-update-";
    const APPLY_ARG: &str = "--arcane-apply-update";
    const RESTART_ARG: &str = "--arcane-restart-update";
    const ERROR_ENV: &str = "ARCANE_PORTAL_UPDATE_ERROR";
    const INSTALL_LOCK: &str = ".arcane-portal-update.lock";
    const READY: &[u8] = b"APPLY-READY\n";
    const ARM: u8 = 1;
    const CONFIRMED: &str = "startup-confirmed";
    const COMPLETE: &str = "cleanup-ready";
    const ERROR_LOG: &str = "update-error.txt";
    const MAX_PACKAGE_BYTES: u64 = 128 * 1024 * 1024;
    const MAX_ENTRIES: usize = 512;
    const HELPER_START_TIMEOUT: Duration = Duration::from_secs(8);
    const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
    const RESTART_TIMEOUT: Duration = Duration::from_secs(25);
    const FILE_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(100);

    #[cfg(windows)]
    const PACKAGE_ROOT: &str = "Arcane Portal.exe";
    #[cfg(target_os = "macos")]
    const PACKAGE_ROOT: &str = "Arcane Portal.app";
    #[cfg(windows)]
    const HELPER_NAME: &str = "apply-update.exe";
    #[cfg(target_os = "macos")]
    const HELPER_NAME: &str = "apply-update";
    #[cfg(windows)]
    const BACKUP_NAME: &str = "Previous Arcane Portal.exe";
    #[cfg(target_os = "macos")]
    const BACKUP_NAME: &str = "Previous Arcane Portal.app";
    #[cfg(target_os = "macos")]
    const MAC_EXECUTABLE: &str = "Contents/MacOS/arcane-portal";

    static EXIT_PIPE: Mutex<Option<ChildStdin>> = Mutex::new(None);

    pub(crate) struct StagedUpdate {
        paths: Paths,
        handed_off: bool,
    }

    struct Paths {
        stage: PathBuf,
        target: PathBuf,
    }

    impl Paths {
        fn from_helper(executable: &Path, name: &OsStr) -> io::Result<Self> {
            let stage = parent(executable)?;
            let stage = checked_stage(parent(stage)?, stage.file_name().ok_or_else(invalid_path)?)?;
            if executable.file_name() != Some(OsStr::new(HELPER_NAME)) || !is_single_name(name) {
                return Err(invalid_path());
            }
            let target = parent(&stage)?.join(name);
            validate_target(&target)?;
            Ok(Self { stage, target })
        }

        fn helper(&self) -> PathBuf {
            self.stage.join(HELPER_NAME)
        }

        fn payload(&self) -> PathBuf {
            self.stage.join(PACKAGE_ROOT)
        }

        fn backup(&self) -> PathBuf {
            self.stage.join(BACKUP_NAME)
        }

        fn record_error(&self, error: &str) {
            if let Ok(mut log) = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.stage.join(ERROR_LOG))
            {
                let _ = log.write_all(error.as_bytes());
            }
        }
    }

    pub(crate) fn stage(
        download: impl FnOnce() -> Result<File, String>,
    ) -> Result<StagedUpdate, String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let target = installed_target(&executable).map_err(|error| error.to_string())?;
        let folder = parent(&target).map_err(|error| error.to_string())?;
        #[cfg(target_os = "macos")]
        if is_translocated(folder) {
            return Err("macOS opened Arcane Portal from a temporary read-only location. Quit Arcane Portal, move Arcane Portal.app into Applications using Finder, then open the moved copy and check for updates again.".into());
        }
        drop(tempfile::tempfile_in(folder).map_err(|error| staging_error(folder, &error))?);
        let archive = download()?;
        let stage = create_stage(folder).map_err(|error| staging_error(folder, &error))?;
        let update = StagedUpdate {
            paths: Paths { stage, target },
            handed_off: false,
        };
        let files = &update.paths;
        extract_release(archive, &files.stage)
            .and_then(|()| verify_payload(&files.payload()))
            .and_then(|()| fs::copy(executable, files.helper()).map(|_| ()))
            .map_err(|error| format!("Couldn't prepare the release package: {error}"))?;
        Ok(update)
    }

    #[cfg(any(target_os = "macos", test))]
    fn is_translocated(path: &Path) -> bool {
        (path.starts_with("/private/var/folders") || path.starts_with("/var/folders"))
            && path
                .components()
                .any(|part| part.as_os_str() == "AppTranslocation")
    }

    fn staging_error(folder: &Path, error: &io::Error) -> String {
        #[cfg(target_os = "macos")]
        if matches!(
            error.kind(),
            io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::PermissionDenied
        ) {
            return "Arcane Portal’s current folder won’t allow updates. Quit the app, move Arcane Portal.app into a writable Applications folder in Finder, then open the moved copy. If /Applications requires administrator access, use Applications in your home folder.".into();
        }
        format!(
            "Couldn't prepare the update in {}. Move Arcane Portal to a folder you can write to, then reopen it. {error}",
            folder.display()
        )
    }

    impl StagedUpdate {
        pub(crate) fn launch(mut self) -> Result<(), String> {
            let mut exit_pipe = EXIT_PIPE.lock().unwrap();
            if exit_pipe.is_some() {
                return Err("Arcane Portal is already restarting for an update.".to_owned());
            }
            let name = self
                .paths
                .target
                .file_name()
                .ok_or_else(|| invalid_path().to_string())?;
            let mut helper = Command::new(self.paths.helper())
                .arg(APPLY_ARG)
                .arg(name)
                .current_dir(&self.paths.stage)
                .env_remove(ERROR_ENV)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("Couldn't open the update installer: {error}"))?;
            let mut input = helper.stdin.take().expect("installer input is piped");
            let mut output = helper.stdout.take().expect("installer output is piped");
            let (ready, response) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let mut message = vec![0; READY.len()];
                let result = output.read_exact(&mut message).is_ok() && message == READY;
                let _ = ready.send(result);
            });
            if response.recv_timeout(HELPER_START_TIMEOUT) != Ok(true) {
                let _ = helper.kill();
                let _ = helper.wait();
                return Err(fs::read_to_string(self.paths.stage.join(ERROR_LOG)).unwrap_or_else(|_| {
                    "The update installer could not get ready. Leave Arcane Portal open and try again.".to_owned()
                }));
            }
            if let Err(error) = input.write_all(&[ARM]) {
                let _ = helper.kill();
                let _ = helper.wait();
                return Err(format!("Couldn't start the update installer: {error}"));
            }
            self.handed_off = true;
            // The OS closes this pipe after Arcane Portal exits, allowing replacement to begin.
            *exit_pipe = Some(input);
            Ok(())
        }
    }

    impl Drop for StagedUpdate {
        fn drop(&mut self) {
            if !self.handed_off {
                let _ = remove_stage(&self.paths.stage);
            }
        }
    }

    pub(crate) fn helper_exit_code() -> Option<i32> {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() != Some(OsStr::new(APPLY_ARG)) {
            return None;
        }
        let result = args.next().ok_or_else(invalid_path).and_then(|target| {
            let executable = std::env::current_exe()?;
            let paths = Paths::from_helper(&executable, &target)?;
            apply(&paths).inspect_err(|error| {
                paths.record_error(&error.to_string());
            })
        });
        Some(i32::from(result.is_err()))
    }

    fn apply(paths: &Paths) -> io::Result<()> {
        let _lock = installation_lock(parent(&paths.target)?)?;
        validate_target(&paths.target)?;
        check_installed_version(paths)?;
        validate_target(&paths.payload())?;
        ensure_missing(&paths.backup())?;
        let mut output = io::stdout().lock();
        output.write_all(READY)?;
        output.flush()?;
        drop(output);
        wait_for_exit()?;

        match replace_and_restart(paths, || launch_updated(paths)) {
            Ok(()) => {
                delete_package(&paths.backup())?;
                make_marker(&paths.stage.join(COMPLETE))
            }
            Err(error) => {
                let message = format!("Arcane Portal couldn't install the update. {error}");
                paths.record_error(&message);
                // An existing backup means a rollback needs manual recovery. Keep it in place.
                if paths.backup().try_exists()? {
                    return Err(io::Error::other(message));
                }
                restart(&paths.target, &paths.stage, Some(&message))?;
                delete_package(&paths.payload())?;
                make_marker(&paths.stage.join(COMPLETE))
            }
        }
    }

    fn wait_for_exit() -> io::Result<()> {
        let (closed, wait) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut input = io::stdin().lock();
            let mut byte = [0];
            let armed = input.read_exact(&mut byte).is_ok() && byte == [ARM];
            let exited = armed && matches!(input.read(&mut byte), Ok(0));
            let _ = closed.send(exited);
        });
        if wait.recv_timeout(EXIT_TIMEOUT) == Ok(true) {
            Ok(())
        } else {
            Err(io::Error::other(
                "Close other Arcane Portal windows and try the update again.",
            ))
        }
    }

    fn replace_and_restart(
        paths: &Paths,
        launch: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let backup = paths.backup();
        retry_file_operation(|| fs::rename(&paths.target, &backup)).map_err(|error| {
            io::Error::other(format!("Couldn't move the existing app: {error}"))
        })?;

        let result = match retry_file_operation(|| fs::rename(paths.payload(), &paths.target)) {
            Ok(()) => match launch() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    match retry_file_operation(|| fs::rename(&paths.target, paths.payload())) {
                        Ok(()) => error,
                        Err(restore_error) => {
                            return Err(recovery_error(&backup, &error, &restore_error));
                        }
                    }
                }
            },
            Err(error) => error,
        };

        if let Err(error) = retry_file_operation(|| fs::rename(&backup, &paths.target)) {
            if !paths.target.exists() {
                let _ = fs::rename(paths.payload(), &paths.target);
            }
            return Err(recovery_error(&backup, &result, &error));
        }
        Err(io::Error::other(format!(
            "The previous version was restored. {result}"
        )))
    }

    fn recovery_error(backup: &Path, failed: &io::Error, restore: &io::Error) -> io::Error {
        io::Error::other(format!(
            "Couldn't finish the update or restore the previous version automatically. Its backup is at {}. Update error: {failed}. Recovery error: {restore}.",
            backup.display()
        ))
    }

    fn launch_updated(paths: &Paths) -> io::Result<()> {
        let mut child = restart(&paths.target, &paths.stage, None)?;
        let deadline = Instant::now() + RESTART_TIMEOUT;
        let confirmed = paths.stage.join(CONFIRMED);
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "The new version exited during startup ({status})."
                )));
            }
            if confirmed.is_file() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                child.wait()?;
                return Err(io::Error::other("The new version did not confirm startup."));
            }
            thread::sleep(POLL);
        }
    }

    fn restart(
        target: &Path,
        stage: &Path,
        error: Option<&str>,
    ) -> io::Result<std::process::Child> {
        let mut command = Command::new(executable_in(target));
        command
            .arg(RESTART_ARG)
            .arg(stage.file_name().ok_or_else(invalid_path)?)
            .current_dir(parent(target)?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove(ERROR_ENV);
        if let Some(message) = error {
            command.env(ERROR_ENV, message);
        }
        command.spawn()
    }

    pub(crate) fn confirm_relaunch() {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() != Some(OsStr::new(RESTART_ARG)) {
            return;
        }
        let stage = (|| {
            let executable = std::env::current_exe()?;
            let target = installed_target(&executable)?;
            let name = args.next().ok_or_else(invalid_path)?;
            checked_stage(parent(&target)?, &name)
        })();
        if let Ok(stage) = stage {
            let _ = make_marker(&stage.join(CONFIRMED));
            let _ = thread::Builder::new()
                .name("arcane-update-cleanup".into())
                .spawn(move || cleanup_after_helper(&stage));
        }
    }

    pub(crate) fn previous_error() -> Option<String> {
        let is_restart = std::env::args_os().nth(1).as_deref() == Some(OsStr::new(RESTART_ARG));
        is_restart.then(|| std::env::var(ERROR_ENV).ok()).flatten()
    }

    fn cleanup_after_helper(stage: &Path) {
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            if stage.join(COMPLETE).is_file() && !stage.join(BACKUP_NAME).exists() {
                // Windows keeps the running helper locked. Leave the marker intact until it exits.
                match fs::remove_file(stage.join(HELPER_NAME)) {
                    Ok(()) => {
                        let _ = remove_stage(stage);
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let _ = remove_stage(stage);
                        return;
                    }
                    Err(_) => {}
                }
            }
            thread::sleep(POLL);
        }
    }

    fn extract_release(archive: File, stage: &Path) -> io::Result<()> {
        if archive.metadata()?.len() > MAX_PACKAGE_BYTES {
            return Err(io::Error::other("The release ZIP exceeds the size limit."));
        }
        let mut zip = ZipArchive::new(archive)?;
        if zip.is_empty() || zip.len() > MAX_ENTRIES {
            return Err(io::Error::other(
                "The release ZIP has an unexpected number of entries.",
            ));
        }
        let mut remaining = MAX_PACKAGE_BYTES;
        for index in 0..zip.len() {
            let entry = zip.by_index(index)?;
            let name = entry.name()?;
            let is_dir = entry.is_dir();
            let path = checked_entry(&name, is_dir, entry.unix_mode())?;
            let destination = stage.join(path);
            let size = entry.size();
            remaining = remaining.checked_sub(size).ok_or_else(|| {
                io::Error::other("The uncompressed release exceeds the size limit.")
            })?;
            if is_dir {
                if size != 0 {
                    return Err(io::Error::other(
                        "The release contains a directory with file data.",
                    ));
                }
                fs::create_dir_all(&destination)?;
            } else {
                fs::create_dir_all(parent(&destination)?)?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)?;
                if io::copy(&mut entry.take(size + 1), &mut output)? != size {
                    return Err(io::Error::other(
                        "A file in the release did not match its declared size.",
                    ));
                }
            }
        }
        Ok(())
    }

    fn checked_entry(name: &str, directory: bool, mode: Option<u32>) -> io::Result<PathBuf> {
        let has_valid_type = match mode.unwrap_or(0) & 0o170000 {
            0 => true,
            0o040000 => directory,
            0o100000 => !directory,
            _ => false,
        };
        if !has_valid_type || name.contains(['\\', ':']) || name.chars().any(char::is_control) {
            return Err(invalid_archive_path());
        }
        #[cfg(windows)]
        if !directory && name == PACKAGE_ROOT {
            return Ok(PathBuf::from(name));
        }
        #[cfg(target_os = "macos")]
        {
            let name = name.strip_suffix('/').unwrap_or(name);
            let mut components = name.split('/');
            if components.next() == Some(PACKAGE_ROOT)
                && (directory || name.contains('/'))
                && components.all(|component| {
                    !component.is_empty()
                        && !component.ends_with([' ', '.'])
                        && is_single_name(OsStr::new(component))
                })
            {
                return Ok(PathBuf::from(name));
            }
        }
        Err(invalid_archive_path())
    }

    fn invalid_archive_path() -> io::Error {
        io::Error::other("The ZIP contains an unexpected path or a link.")
    }

    #[cfg(windows)]
    fn verify_payload(path: &Path) -> io::Result<()> {
        validate_target(path)?;
        let mut header = [0; 2];
        File::open(path)?.read_exact(&mut header)?;
        if header != *b"MZ" {
            return Err(io::Error::other(
                "The release is missing a Windows executable.",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn verify_payload(path: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        validate_target(path)?;
        fs::set_permissions(executable_in(path), fs::Permissions::from_mode(0o755))?;
        let result = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(path)
            .output()?;
        if !result.status.success() {
            return Err(io::Error::other(format!(
                "The macOS app signature could not be verified. {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        Ok(())
    }

    fn check_installed_version(paths: &Paths) -> io::Result<()> {
        use sha2::{Digest, Sha256};

        fn digest(path: &Path) -> io::Result<[u8; 32]> {
            let mut file = File::open(path)?;
            let mut digest = Sha256::new();
            let mut buffer = [0; 16 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    return Ok(digest.finalize().into());
                }
                digest.update(&buffer[..read]);
            }
        }

        if digest(&executable_in(&paths.target))? != digest(&paths.helper())? {
            return Err(io::Error::other(
                "The installed executable changed after this update was prepared. Reopen Arcane Portal and check for updates.",
            ));
        }
        Ok(())
    }

    fn installed_target(executable: &Path) -> io::Result<PathBuf> {
        let executable = executable.canonicalize()?;
        #[cfg(windows)]
        let target = executable;
        #[cfg(target_os = "macos")]
        let target = executable
            .ancestors()
            .nth(3)
            .ok_or_else(invalid_path)?
            .to_owned();
        validate_target(&target)?;
        Ok(target)
    }

    #[cfg(windows)]
    fn executable_in(target: &Path) -> PathBuf {
        target.to_owned()
    }

    #[cfg(target_os = "macos")]
    fn executable_in(target: &Path) -> PathBuf {
        target.join(MAC_EXECUTABLE)
    }

    #[cfg(windows)]
    fn validate_target(target: &Path) -> io::Result<()> {
        if target
            .extension()
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("exe"))
            || !plain_metadata(target)?.is_file()
        {
            return Err(invalid_path());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn validate_target(target: &Path) -> io::Result<()> {
        if target
            .extension()
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("app"))
            || !plain_metadata(target)?.is_dir()
            || !plain_metadata(&target.join("Contents"))?.is_dir()
            || !plain_metadata(&target.join("Contents/MacOS"))?.is_dir()
            || !plain_metadata(&executable_in(target))?.is_file()
            || !plain_metadata(&target.join("Contents/Info.plist"))?.is_file()
        {
            return Err(io::Error::other(
                "Open Arcane Portal from its app bundle in a writable Applications folder.",
            ));
        }
        Ok(())
    }

    fn delete_package(path: &Path) -> io::Result<()> {
        match plain_metadata(path) {
            Ok(metadata) if metadata.is_file() => fs::remove_file(path),
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
            Ok(_) => Err(invalid_path()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn installation_lock(folder: &Path) -> io::Result<File> {
        let path = folder.join(INSTALL_LOCK);
        match plain_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(invalid_path()),
            Err(error) => return Err(error),
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(|_| {
            io::Error::other("An Arcane Portal update is already running for this folder.")
        })?;
        Ok(file)
    }

    fn create_stage(folder: &Path) -> io::Result<PathBuf> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut builder = fs::DirBuilder::new();
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        #[cfg(windows)]
        let builder = &mut builder;
        for attempt in 0..32 {
            let path = folder.join(format!(
                "{STAGE_PREFIX}{}-{stamp:x}-{attempt}",
                std::process::id()
            ));
            match builder.create(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(
            "Couldn't reserve a directory for the update.",
        ))
    }

    fn checked_stage(folder: &Path, name: &OsStr) -> io::Result<PathBuf> {
        if !is_single_name(name)
            || !name.to_str().is_some_and(|name| {
                name.strip_prefix(STAGE_PREFIX).is_some_and(|suffix| {
                    !suffix.is_empty()
                        && suffix
                            .bytes()
                            .all(|part| part.is_ascii_hexdigit() || part == b'-')
                })
            })
        {
            return Err(invalid_path());
        }
        let folder = folder.canonicalize()?;
        let stage = folder.join(name);
        if !plain_metadata(&stage)?.is_dir() || stage.canonicalize()? != stage {
            return Err(invalid_path());
        }
        Ok(stage)
    }

    fn remove_stage(path: &Path) -> io::Result<()> {
        let stage = checked_stage(parent(path)?, path.file_name().ok_or_else(invalid_path)?)?;
        fs::remove_dir_all(stage)
    }

    fn plain_metadata(path: &Path) -> io::Result<fs::Metadata> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_path());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(invalid_path());
            }
        }
        Ok(metadata)
    }

    fn make_marker(path: &Path) -> io::Result<()> {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map(|_| ())
    }

    fn is_single_name(name: &OsStr) -> bool {
        let mut components = Path::new(name).components();
        matches!(components.next(), Some(Component::Normal(component)) if component == name)
            && components.next().is_none()
    }

    fn parent(path: &Path) -> io::Result<&Path> {
        path.parent().ok_or_else(invalid_path)
    }

    fn ensure_missing(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::other(
                "An earlier update backup needs to be recovered first.",
            )),
        }
    }

    fn retry_file_operation(operation: impl Fn() -> io::Result<()>) -> io::Result<()> {
        let deadline = Instant::now() + FILE_RETRY_TIMEOUT;
        loop {
            match operation() {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
                    ) && Instant::now() < deadline =>
                {
                    thread::sleep(POLL)
                }
                result => return result,
            }
        }
    }

    fn invalid_path() -> io::Error {
        io::Error::other("The current installation or update directory is invalid.")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn recognizes_gatekeepers_temporary_mount() {
            assert!(is_translocated(Path::new(
                "/private/var/folders/ab/cache/T/AppTranslocation/id/d"
            )));
            assert!(is_translocated(Path::new(
                "/var/folders/ab/cache/T/AppTranslocation/id/d"
            )));
            assert!(!is_translocated(Path::new("/Applications")));
            assert!(!is_translocated(Path::new(
                "/Users/test/AppTranslocation/Applications"
            )));
            assert!(!is_translocated(Path::new(
                "/private/var/folders/ab/cache/T"
            )));
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) use native::{StagedUpdate, confirm_relaunch, helper_exit_code, previous_error, stage};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) struct StagedUpdate;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl StagedUpdate {
    pub(crate) fn launch(self) -> Result<(), String> {
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn stage(
    _: impl FnOnce() -> Result<std::fs::File, String>,
) -> Result<StagedUpdate, String> {
    Err(unsupported())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn helper_exit_code() -> Option<i32> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn confirm_relaunch() {}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn previous_error() -> Option<String> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn unsupported() -> String {
    "In-app updates are available on Windows and macOS.".to_owned()
}
