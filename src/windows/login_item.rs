use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Hex";
/// The Startup folder launcher: the user Run list was skipped at one logon,
/// while the Startup folder still ran its items.
const LAUNCHER_NAME: &str = "Hex.vbs";
/// Windows' own enabled/disabled state for a Startup folder item.
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";

/// Return the startup registration state. The Startup folder launcher is the
/// registration now; a Run value left by an older version still reads as on
/// until the next launch migrates it.
pub fn is_enabled() -> Result<bool> {
    if launcher_path().is_some_and(|path| path.exists()) {
        return launcher_approved();
    }
    run_value_present()
}

fn run_value_present() -> Result<bool> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let key = match current_user.open_subkey_with_flags(RUN_KEY, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).wrap_err("could not read Windows startup applications"),
    };
    match key.get_value::<String, _>(VALUE_NAME) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).wrap_err("could not read the Hex startup registration"),
    }
}

/// Register or unregister the current executable for the current Windows user.
/// The Startup folder launcher is the one that runs here; the old Run value is
/// removed so Windows does not list, or start, Hex twice.
pub fn set_enabled(enabled: bool) -> Result<()> {
    if enabled {
        let executable = std::env::current_exe().wrap_err("could not locate the Hex executable")?;
        let path = launcher_path().ok_or_else(|| eyre!("the Startup folder is unavailable"))?;
        write_launcher_at(&path, &executable)?;
        set_launcher_approved(true)?;
        remove_run_value()?;
    } else {
        remove_launcher()?;
        set_launcher_approved(false)?;
        remove_run_value()?;
    }
    Ok(())
}

/// Rewrite an existing startup registration to a new executable, e.g.
/// after a self-update activates a new version directory. Does nothing
/// when launch at login is disabled.
pub fn repoint(executable: &Path) -> Result<()> {
    if !is_enabled()? {
        return Ok(());
    }
    let Some(path) = launcher_path() else {
        return Ok(());
    };
    write_launcher_at(&path, executable)?;
    remove_run_value()
}

/// Writes the Startup folder launcher when Launch at login is on, so a logon
/// that skips the user Run list still starts Hex. Migrates a Run value left by
/// an older version, and refreshes the launcher after an update. Runs at every
/// launch before the single-instance claim.
pub fn ensure_launcher() -> Result<()> {
    let Some(path) = launcher_path() else {
        return Ok(());
    };
    let run = run_value_present()?;
    if !run && !path.exists() {
        return Ok(());
    }
    let executable = std::env::current_exe().wrap_err("could not locate the Hex executable")?;
    if !launcher_matches(&path, &executable)? {
        write_launcher_at(&path, &executable)?;
        tracing::info!(
            path = %path.display(),
            "wrote the Startup folder launcher"
        );
    }
    if run {
        remove_run_value()?;
        tracing::info!("moved Launch at login from the Run key to the Startup folder");
    }
    Ok(())
}

fn remove_run_value() -> Result<()> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    match current_user.open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
        Ok(key) => {
            if let Err(error) = key.delete_value(VALUE_NAME)
                && error.kind() != ErrorKind::NotFound
            {
                return Err(error).wrap_err("could not unregister Hex from Windows startup");
            }
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err("could not open Windows startup applications"),
    }
}

/// Whether Windows still approves the Startup folder launcher. A missing
/// approval is enabled; the first byte is `1` when Windows disabled it.
fn launcher_approved() -> Result<bool> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let key = match current_user.open_subkey_with_flags(APPROVED_KEY, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).wrap_err("could not read Windows startup approvals"),
    };
    match key.get_raw_value(LAUNCHER_NAME) {
        Ok(value) => Ok(approved(&value.bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).wrap_err("could not read the Hex startup approval"),
    }
}

fn approved(bytes: &[u8]) -> bool {
    bytes.first() != Some(&1)
}

fn set_launcher_approved(enabled: bool) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(APPROVED_KEY)
        .wrap_err("could not open Windows startup approvals")?;
    if enabled {
        key.set_raw_value(
            LAUNCHER_NAME,
            &RegValue {
                bytes: vec![3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                vtype: RegType::REG_BINARY,
            },
        )
        .wrap_err("could not approve the Hex startup launcher")
    } else if let Err(error) = key.delete_value(LAUNCHER_NAME)
        && error.kind() != ErrorKind::NotFound
    {
        Err(error).wrap_err("could not remove the Hex startup approval")
    } else {
        Ok(())
    }
}

fn launcher_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
            .join(LAUNCHER_NAME)
    })
}

fn launcher_script(executable: &Path) -> String {
    format!(
        "Set shell = CreateObject(\"WScript.Shell\")\r\nshell.Run \"\"\"{}\"\" app --hidden\", 0, False\r\n",
        executable.display()
    )
}

fn launcher_bytes(script: &str) -> Vec<u8> {
    // WSH reads UTF-16LE with a BOM, which keeps non-ASCII profile paths.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn write_launcher_at(path: &Path, executable: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err("could not create the Startup folder")?;
    }
    let temporary = path.with_extension("vbs.tmp");
    std::fs::write(&temporary, launcher_bytes(&launcher_script(executable)))
        .wrap_err("could not write the Startup folder launcher")?;
    std::fs::rename(&temporary, path).wrap_err("could not replace the Startup folder launcher")?;
    Ok(())
}

fn launcher_matches(path: &Path, executable: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read(path) else {
        return Ok(false);
    };
    Ok(existing == launcher_bytes(&launcher_script(executable)))
}

fn remove_launcher() -> Result<()> {
    let Some(path) = launcher_path() else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err("could not remove the Startup folder launcher"),
    }
}

/// Points Launch at login at this Hex when it starts another version of the
/// same managed install. Setup and install-windows.ps1 switch
/// `current-version` without touching the Run value, which names a
/// version's `hex.exe`, so after a manual upgrade signing in started the
/// version it replaced. Runs at every launch, before the single-instance
/// claim, so opening the new version once fixes it even while the old one
/// still runs. A disabled registration, a value outside the managed
/// `versions` folder, and a copy that isn't the current version stay as they
/// are.
pub fn follow_current_version() -> Result<()> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let key = match current_user.open_subkey_with_flags(RUN_KEY, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).wrap_err("could not read Windows startup applications"),
    };
    let command = match key.get_value::<String, _>(VALUE_NAME) {
        Ok(command) => command,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).wrap_err("could not read the Hex startup registration"),
    };
    let executable = std::env::current_exe().wrap_err("could not locate the Hex executable")?;
    let support = crate::app_paths::support_dir()?;
    let current = std::fs::read_to_string(support.join("current-version")).ok();
    if !starts_another_version(&command, &executable, &support, current.as_deref()) {
        return Ok(());
    }
    write_startup_command(
        &executable,
        "could not repoint the Hex startup registration",
    )?;
    tracing::info!(
        previous = %command,
        "pointed Launch at login at this version of Hex"
    );
    Ok(())
}

/// Whether the Run value `command` starts a `hex.exe` in `support\versions`
/// other than `executable`, which is that install's current version.
fn starts_another_version(
    command: &str,
    executable: &Path,
    support: &Path,
    current: Option<&str>,
) -> bool {
    let versions = format!(
        r"{}\",
        support
            .join("versions")
            .display()
            .to_string()
            .to_lowercase()
    );
    let running = executable.display().to_string().to_lowercase();
    let Some(version) = running.strip_prefix(&versions) else {
        return false;
    };
    let is_current = current.is_some_and(|current| {
        version.eq_ignore_ascii_case(&format!(r"{}\hex.exe", current.trim()))
    });
    is_current
        && command.to_lowercase().contains(&versions)
        && !command.eq_ignore_ascii_case(&startup_command(executable))
}

fn write_startup_command(executable: &Path, context: &'static str) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY)
        .wrap_err("could not open Windows startup applications")?;
    key.set_value(VALUE_NAME, &startup_command(executable))
        .wrap_err(context)?;
    Ok(())
}

fn startup_command(executable: &Path) -> String {
    format!(r#""{}" app --hidden"#, executable.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_paths_with_spaces() {
        assert_eq!(
            startup_command(Path::new(r"C:\Program Files\Hex\hex.exe")),
            r#""C:\Program Files\Hex\hex.exe" app --hidden"#
        );
    }

    #[test]
    fn the_launcher_runs_the_app_hidden() {
        let script = launcher_script(Path::new(r"C:\Program Files\Hex\hex.exe"));
        assert!(script.contains(
            "shell.Run \"\"\"C:\\Program Files\\Hex\\hex.exe\"\" app --hidden\", 0, False"
        ));
        assert!(launcher_bytes(&script).starts_with(&[0xFF, 0xFE]));
    }

    #[test]
    fn only_the_disabled_startup_approval_reads_off() {
        assert!(approved(&[3, 0, 0, 0]));
        assert!(approved(&[2, 0, 0, 0]));
        assert!(approved(&[]));
        assert!(!approved(&[1, 0, 0, 0]));
    }

    #[test]
    fn a_changed_executable_rewrites_the_launcher() {
        let directory = std::env::temp_dir().join(format!("hex-login-item-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Hex.vbs");
        let old = Path::new(r"C:\Old\hex.exe");
        write_launcher_at(&path, old).unwrap();
        assert!(launcher_matches(&path, old).unwrap());
        assert!(!launcher_matches(&path, Path::new(r"C:\New\hex.exe")).unwrap());
        assert_eq!(&std::fs::read(&path).unwrap()[..2], &[0xFF, 0xFE]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// Setup or install-windows.ps1 put 3.4.0 over 3.3.0 and left Launch at
    /// login on the 3.3.0 executable.
    #[test]
    fn a_manual_upgrade_moves_launch_at_login_to_the_current_version() {
        let support = Path::new(r"C:\Users\Jürgen\AppData\Roaming\hex");
        let running = support.join(r"versions\3.4.0\hex.exe");
        let old = r#""C:\Users\Jürgen\AppData\Roaming\hex\versions\3.3.0\hex.exe" app --hidden"#;
        let moves = |command: &str, executable: &Path, current: Option<&str>| {
            starts_another_version(command, executable, support, current)
        };
        assert!(moves(old, &running, Some("3.4.0\n")));
        // Windows paths compare without case.
        assert!(moves(&old.to_uppercase(), &running, Some("3.4.0")));
        // Already this version's.
        assert!(!moves(&startup_command(&running), &running, Some("3.4.0")));
        // Not the version current-version names: an older copy started by
        // hand, or an update staged while this one runs.
        assert!(!moves(old, &running, Some("3.5.0")));
        assert!(!moves(old, &running, None));
        // A value outside the managed layout is the user's.
        assert!(!moves(
            r#""D:\Tools\hex.exe" app --hidden"#,
            &running,
            Some("3.4.0")
        ));
        // A copy running outside the layout, such as a build.
        assert!(!moves(
            old,
            Path::new(r"C:\src\hex\target\release\hex.exe"),
            Some("3.4.0")
        ));
    }
}
