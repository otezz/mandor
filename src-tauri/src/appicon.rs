//! Finds an installed app's icon through its desktop entry, for the editor and
//! terminal pickers.

use base64::Engine;
use std::path::{Path, PathBuf};

const MAX_ICON_BYTES: u64 = 1024 * 1024;
// Small enough to load quickly, big enough to stay sharp at 2x.
const SIZES: &[&str] = &[
    "64x64", "48x48", "96x96", "128x128", "256x256", "32x32", "512x512",
];

/// The XDG data dirs, user first, each holding `applications/` and `icons/`.
pub fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&system))
        .collect()
}

/// A `data:` URL of the icon of the app that `command` launches.
pub fn icon_for(command: &str, dirs: &[PathBuf]) -> Option<String> {
    let program = shell_words::split(command).ok()?.into_iter().next()?;
    let program = base_name(&program)?;
    let icon = dirs
        .iter()
        .flat_map(|d| desktop_files(&d.join("applications")))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|entry| icon_if_launches(&entry, &program))
        .unwrap_or_else(|| program.clone());
    data_url(&resolve(&icon, dirs)?)
}

fn desktop_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
        .collect();
    files.sort();
    files
}

/// The entry's `Icon` when its `Exec` or `TryExec` runs `program`.
fn icon_if_launches(entry: &str, program: &str) -> Option<String> {
    let (mut exec, mut try_exec, mut icon) = (None, None, None);
    let mut in_main = false;
    for line in entry.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
        } else if in_main {
            match line.split_once('=') {
                Some(("Exec", v)) => exec = Some(v.trim()),
                Some(("TryExec", v)) => try_exec = Some(v.trim()),
                Some(("Icon", v)) => icon = Some(v.trim()),
                _ => {}
            }
        }
    }
    let launches = |cmd: Option<&str>| cmd.and_then(exec_program).as_deref() == Some(program);
    (launches(exec) || launches(try_exec))
        .then(|| icon.filter(|i| !i.is_empty()).map(str::to_string))
        .flatten()
}

/// The program an Exec line runs, past an `env VAR=value` prefix.
fn exec_program(exec: &str) -> Option<String> {
    let words = shell_words::split(exec).ok()?;
    let mut words = words.iter().map(String::as_str).peekable();
    if words.peek() == Some(&"env") {
        words.next();
        while words.peek().is_some_and(|w| w.contains('=')) {
            words.next();
        }
    }
    base_name(words.next()?)
}

fn base_name(program: &str) -> Option<String> {
    Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// An icon name or absolute path to a file, looked up in the hicolor theme
/// (where apps install theirs) and then pixmaps.
fn resolve(icon: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(icon);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    dirs.iter().find_map(|d| {
        let theme = d.join("icons/hicolor");
        SIZES
            .iter()
            .map(|s| theme.join(s).join("apps").join(format!("{icon}.png")))
            .chain([
                theme.join("scalable/apps").join(format!("{icon}.svg")),
                d.join("pixmaps").join(format!("{icon}.png")),
                d.join("pixmaps").join(format!("{icon}.svg")),
            ])
            .find(|p| p.is_file())
    })
}

fn data_url(path: &Path) -> Option<String> {
    let mime = match path.extension()?.to_str()? {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    if std::fs::metadata(path).ok()?.len() > MAX_ICON_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{b64}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mandor-appicon-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("applications")).unwrap();
        dir
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn matches_exec_by_program_name() {
        let entry = "[Desktop Entry]\nExec=/usr/share/code/code %F\nIcon=vscode\n";
        assert_eq!(icon_if_launches(entry, "code").as_deref(), Some("vscode"));
        assert_eq!(icon_if_launches(entry, "codium"), None);
    }

    #[test]
    fn matches_try_exec_and_env_prefix() {
        let entry = "[Desktop Entry]\nTryExec=ghostty\nExec=other\nIcon=g\n";
        assert_eq!(icon_if_launches(entry, "ghostty").as_deref(), Some("g"));
        let entry = "[Desktop Entry]\nExec=env FOO=1 BAR=2 /opt/zed/bin/zed %U\nIcon=z\n";
        assert_eq!(icon_if_launches(entry, "zed").as_deref(), Some("z"));
    }

    #[test]
    fn ignores_keys_outside_the_main_group() {
        let entry = "[Desktop Entry]\nExec=kitty\n[Desktop Action new]\nIcon=wrong\n";
        assert_eq!(icon_if_launches(entry, "kitty"), None);
    }

    #[test]
    fn finds_the_icon_through_the_desktop_entry() {
        let dir = scratch("entry");
        write(
            dir.join("applications/com.example.Ed.desktop"),
            "[Desktop Entry]\nExec=/opt/ed/ed --new %F\nIcon=com.example.Ed\n",
        );
        write(
            dir.join("icons/hicolor/512x512/apps/com.example.Ed.png"),
            "big",
        );
        write(
            dir.join("icons/hicolor/64x64/apps/com.example.Ed.png"),
            "small",
        );
        let url = icon_for("ed --wait", &[dir.clone()]).unwrap();
        assert_eq!(url, "data:image/png;base64,c21hbGw=");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn falls_back_to_an_icon_named_after_the_program() {
        let dir = scratch("fallback");
        write(dir.join("pixmaps/kitty.svg"), "<svg/>");
        let url = icon_for("kitty --directory {path}", &[dir.clone()]).unwrap();
        assert!(url.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(icon_for("nothing-here", &[dir.clone()]), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn absolute_icon_paths_are_used_as_is() {
        let dir = scratch("absolute");
        let icon = dir.join("zed.png");
        write(icon.clone(), "z");
        write(
            dir.join("applications/dev.zed.Zed.desktop"),
            &format!("[Desktop Entry]\nExec=/x/zed %U\nIcon={}\n", icon.display()),
        );
        assert_eq!(
            icon_for("zed", &[dir.clone()]).as_deref(),
            Some("data:image/png;base64,eg==")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
