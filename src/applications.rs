use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    compositor::Window,
    hid::{AppFilter, LinkedApp},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Identity {
    pub app_id: String,
    pub title: String,
    pub process_names: Vec<String>,
    pub executable_paths: Vec<String>,
    pub directories: Vec<String>,
    pub steam_ids: Vec<u32>,
}

impl Identity {
    pub fn matches(&self, app: &LinkedApp) -> bool {
        if let Some(id) = app.steam_id {
            return self.steam_ids.contains(&id);
        }
        if app.filters.is_empty() {
            return false;
        }
        if app.match_type == 1 {
            app.filters.iter().all(|filter| self.matches_filter(filter))
        } else {
            app.filters.iter().any(|filter| self.matches_filter(filter))
        }
    }

    fn matches_filter(&self, filter: &AppFilter) -> bool {
        let wanted = filter.value.trim();
        if wanted.is_empty() {
            return false;
        }
        match filter.kind.as_str() {
            "window_title" => self.title.to_lowercase().contains(&wanted.to_lowercase()),
            "process_name" => {
                let wanted = filename(wanted).to_lowercase();
                self.process_names
                    .iter()
                    .any(|name| name.to_lowercase() == wanted)
            }
            "process_full_path" => self
                .executable_paths
                .iter()
                .any(|path| normalize_path(path) == normalize_path(wanted)),
            "process_directory" => self.directories.iter().any(|path| {
                let path = normalize_path(path);
                let wanted = normalize_path(wanted);
                path == wanted
                    || path
                        .strip_prefix(&wanted)
                        .is_some_and(|rest| wanted.ends_with('/') || rest.starts_with('/'))
            }),
            _ => false,
        }
    }
}

pub fn identify(window: &Window) -> Identity {
    let mut identity = Identity {
        app_id: window.app_id.clone(),
        title: window.title.clone(),
        ..Identity::default()
    };
    if let Some(id) = steam_app_id(window.app_id.trim_end_matches(".desktop")) {
        identity.steam_ids.push(id);
    }
    // A compositor may report the XWayland server PID rather than the client PID. Its
    // app_id/WM_CLASS is then the only process identity available to the focus event.
    let app_id = window.app_id.trim_end_matches(".desktop");
    if !app_id.is_empty() && steam_app_id(app_id).is_none() {
        push_unique(&mut identity.process_names, filename(app_id).to_string());
        if let Some(last) = app_id.rsplit('.').next().filter(|part| !part.is_empty()) {
            push_unique(&mut identity.process_names, last.to_string());
        }
    }
    if let Some(mut pid) = window.pid.filter(|pid| *pid > 1) {
        let mut seen = HashSet::new();
        for depth in 0..10 {
            if pid <= 1 || !seen.insert(pid) {
                break;
            }
            let proc = PathBuf::from(format!("/proc/{pid}"));
            if depth == 0 {
                let mut wine_loader = false;
                if let Ok(exe) = fs::read_link(proc.join("exe")) {
                    let path = exe.to_string_lossy();
                    let path = path.strip_suffix(" (deleted)").unwrap_or(&path);
                    wine_loader = is_wine_loader(filename(path));
                    // Wine/Proton's Unix loader is not the foreground Windows executable.
                    if !wine_loader && !filename(path).eq_ignore_ascii_case("xwayland") {
                        add_executable(&mut identity, path);
                    }
                }
                if wine_loader && let Ok(cmdline) = fs::read(proc.join("cmdline")) {
                    for arg in cmdline.split(|b| *b == 0).take(32) {
                        let token = String::from_utf8_lossy(arg);
                        let token = token.trim_matches(['\'', '"']);
                        if token.to_ascii_lowercase().ends_with(".exe")
                            && !is_wine_loader(filename(token))
                        {
                            add_executable(&mut identity, token);
                        }
                    }
                }
            }
            if let Ok(environ) = fs::read(proc.join("environ")) {
                for entry in environ.split(|b| *b == 0) {
                    if let Some(separator) = entry.iter().position(|b| *b == b'=') {
                        let (key, value) = entry.split_at(separator);
                        let value = &value[1..];
                        if [
                            b"SteamAppId".as_slice(),
                            b"SteamGameId",
                            b"STEAM_COMPAT_APP_ID",
                        ]
                        .contains(&key)
                            && let Ok(id) = std::str::from_utf8(value).unwrap_or("").parse::<u32>()
                            && id != 0
                        {
                            push_unique(&mut identity.steam_ids, id);
                        }
                    }
                }
            }
            let Ok(stat) = fs::read_to_string(proc.join("stat")) else {
                break;
            };
            // comm is parenthesized and may itself contain spaces or parentheses.
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                break;
            };
            pid = fields
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    identity.steam_ids.sort_unstable();
    identity
}

fn steam_app_id(app_id: &str) -> Option<u32> {
    let suffix = app_id.to_ascii_lowercase();
    let digits = suffix.strip_prefix("steam_app_")?;
    digits.parse::<u32>().ok().filter(|id| *id != 0)
}

fn is_wine_loader(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "wine"
            | "wine64"
            | "wine-preloader"
            | "wine64-preloader"
            | "wineserver"
            | "proton"
            | "steam"
            | "steam.exe"
            | "explorer.exe"
            | "services.exe"
            | "wineboot.exe"
    )
}

fn add_executable(identity: &mut Identity, path: &str) {
    if path.is_empty() {
        return;
    }
    let path = path.replace('\\', "/");
    push_unique(&mut identity.process_names, filename(&path).to_string());
    if let Some((directory, _)) = path.rsplit_once('/') {
        push_unique(&mut identity.executable_paths, path.clone());
        if !directory.is_empty() {
            push_unique(&mut identity.directories, directory.to_string());
        }
    }
}

fn push_unique<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn filename(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn normalize_path(path: &str) -> String {
    let path = path.trim().replace('\\', "/").to_lowercase();
    let absolute = path.starts_with('/');
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => (),
            ".." if components.last().is_some_and(|part| *part != "..") => {
                components.pop();
            }
            ".." if !absolute => components.push(".."),
            ".." => (),
            other => components.push(other),
        }
    }
    let joined = components.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AppList {
    pub steam: Vec<SteamApp>,
    pub disk: Vec<DiskApp>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SteamApp {
    pub id: u32,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiskApp {
    pub name: String,
    pub executable: String,
}

pub fn installed_apps() -> AppList {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs =
        env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut desktop_dirs = vec![data_home.join("applications")];
    desktop_dirs.extend(env::split_paths(&data_dirs).map(|dir| dir.join("applications")));
    let mut desktop_files = BTreeSet::new();
    // A higher-priority XDG entry, including Hidden=true, masks the same desktop ID.
    let mut seen_desktop_ids = HashSet::new();
    let mut disk = Vec::new();
    for directory in desktop_dirs {
        desktop_files.clear();
        collect_desktops(&directory, &directory, &mut desktop_files);
        for file in &desktop_files {
            let id = file
                .strip_prefix(&directory)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('/', "-");
            if !seen_desktop_ids.insert(id) {
                continue;
            }
            if let Ok(contents) = fs::read_to_string(file)
                && let Some(app) = parse_desktop(&contents)
            {
                disk.push(app);
            }
        }
    }
    disk.sort_by(|a, b| (&a.name, &a.executable).cmp(&(&b.name, &b.executable)));
    disk.dedup_by(|a, b| a.name == b.name && a.executable == b.executable);

    let roots = [
        home.join(".steam/steam"),
        home.join(".steam/root"),
        home.join(".steam/debian-installation"),
        home.join(".local/share/Steam"),
        data_home.join("Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    let mut libraries = BTreeSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        libraries.insert(root.join("steamapps"));
        if let Ok(vdf) = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for path in library_paths(&vdf) {
                libraries.insert(PathBuf::from(path).join("steamapps"));
            }
        }
    }
    let mut steam = BTreeMap::new();
    for library in libraries {
        let Ok(entries) = fs::read_dir(&library) else {
            continue;
        };
        let mut manifests: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("appmanifest_") && name.ends_with(".acf"))
            })
            .collect();
        manifests.sort();
        for manifest in manifests {
            let Ok(contents) = fs::read_to_string(&manifest) else {
                continue;
            };
            let tokens = vdf_tokens(&contents);
            let Some(fields) = vdf_object(&tokens, "AppState") else {
                continue;
            };
            let (Some(id), Some(name), Some(install_dir)) = (
                fields.get("appid").and_then(|id| id.parse::<u32>().ok()),
                fields.get("name"),
                fields.get("installdir"),
            ) else {
                continue;
            };
            if id == 0
                || name.is_empty()
                || install_dir.is_empty()
                || install_dir.contains(['/', '\\'])
                || install_dir == "."
                || install_dir == ".."
            {
                continue;
            }
            steam.entry(id).or_insert_with(|| SteamApp {
                id,
                name: name.clone(),
                path: library
                    .join("common")
                    .join(install_dir)
                    .to_string_lossy()
                    .into_owned(),
            });
        }
    }
    AppList {
        steam: steam.into_values().collect(),
        disk,
    }
}

fn collect_desktops(root: &Path, directory: &Path, files: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    // Bound traversal to desktop-entry directories; don't follow symlinked directories.
    if directory
        .strip_prefix(root)
        .map_or(true, |relative| relative.components().count() > 4)
    {
        return;
    }
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_desktops(root, &path, files);
        } else if (kind.is_file() || kind.is_symlink())
            && path.extension().is_some_and(|ext| ext == "desktop")
        {
            files.insert(path);
        }
    }
}

fn parse_desktop(contents: &str) -> Option<DiskApp> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut hidden = false;
    let mut visible = true;
    let mut application = true;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = first_exec_token(value),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => visible = !value.eq_ignore_ascii_case("true"),
            "Type" => application = value == "Application",
            _ => (),
        }
    }
    if hidden || !visible || !application {
        return None;
    }
    let name = name.filter(|v| !v.is_empty())?;
    let executable = exec.filter(|v| !v.is_empty())?;
    Some(DiskApp { name, executable })
}

fn first_exec_token(command: &str) -> Option<String> {
    let mut chars = command.chars().peekable();
    let mut words = Vec::new();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|ch| ch.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        let mut quoted = false;
        let mut escaped = false;
        for ch in chars.by_ref() {
            if escaped {
                word.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => break,
                c => word.push(c),
            }
        }
        if escaped || quoted {
            return None;
        }
        words.push(word);
    }
    let mut args = words.iter().map(String::as_str);
    let command = args.next()?;
    let executable = if filename(command) == "env" {
        let mut arg = args.next()?;
        loop {
            if arg == "-u" || arg == "--unset" {
                args.next()?;
            } else if arg == "-i" || arg == "--ignore-environment" {
            } else if arg.starts_with('-') {
                return None;
            } else if !arg.contains('=') {
                break;
            }
            arg = args.next()?;
        }
        arg
    } else if filename(command) == "flatpak" {
        if args.next()? != "run" {
            return None;
        }
        args.find_map(|arg| arg.strip_prefix("--command="))?
    } else if ["sh", "bash", "dash"].contains(&filename(command)) {
        return None;
    } else {
        command
    };
    if executable.is_empty() || executable.starts_with('%') {
        None
    } else {
        Some(executable.to_owned())
    }
}

fn vdf_tokens(contents: &str) -> Vec<String> {
    let mut chars = contents.chars().peekable();
    let mut result = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '{' | '}' => result.push(ch.to_string()),
            '"' => {
                let mut value = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('\\') => value.push('\\'),
                            Some('"') => value.push('"'),
                            Some(next) => {
                                value.push('\\');
                                value.push(next);
                            }
                            None => break,
                        },
                        _ => value.push(c),
                    }
                }
                result.push(value);
            }
            c if c.is_whitespace() => (),
            other => {
                let mut value = other.to_string();
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || next == '{' || next == '}' {
                        break;
                    }
                    value.push(chars.next().unwrap());
                }
                result.push(value);
            }
        }
    }
    result
}

fn vdf_object(tokens: &[String], object: &str) -> Option<BTreeMap<String, String>> {
    let start = tokens
        .windows(2)
        .position(|pair| pair[0].eq_ignore_ascii_case(object) && pair[1] == "{")?
        + 2;
    let mut fields = BTreeMap::new();
    let mut depth = 0;
    let mut i = start;
    while i < tokens.len() {
        if tokens[i] == "}" {
            if depth == 0 {
                break;
            }
            depth -= 1;
            i += 1;
        } else if tokens[i] == "{" {
            depth += 1;
            i += 1;
        } else if let Some(value) = tokens.get(i + 1) {
            if value == "{" {
                depth += 1;
                i += 2;
            } else {
                if depth == 0 {
                    fields.insert(tokens[i].to_lowercase(), value.clone());
                }
                i += 2;
            }
        } else {
            break;
        }
    }
    Some(fields)
}

fn library_paths(contents: &str) -> Vec<String> {
    let tokens = vdf_tokens(contents);
    let mut paths = Vec::new();
    let Some(start) = tokens
        .windows(2)
        .position(|pair| pair[0].eq_ignore_ascii_case("libraryfolders") && pair[1] == "{")
    else {
        return paths;
    };
    let mut i = start + 2;
    while i + 1 < tokens.len() && tokens[i] != "}" {
        let key = &tokens[i];
        let value = &tokens[i + 1];
        if key.parse::<u32>().is_ok() {
            if value == "{" {
                let mut depth = 1;
                i += 2;
                while i + 1 < tokens.len() && depth > 0 {
                    if tokens[i] == "{" {
                        depth += 1;
                        i += 1;
                    } else if tokens[i] == "}" {
                        depth -= 1;
                        i += 1;
                    } else if depth == 1 && tokens[i].eq_ignore_ascii_case("path") {
                        if !tokens[i + 1].is_empty() {
                            paths.push(tokens[i + 1].clone());
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            } else {
                if !value.is_empty() {
                    paths.push(value.clone());
                }
                i += 2;
            }
        } else {
            i += 2;
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked(filters: &[(&str, &str)], match_type: u32) -> LinkedApp {
        LinkedApp {
            name: String::new(),
            steam_id: None,
            match_type,
            filters: filters
                .iter()
                .map(|(kind, value)| AppFilter {
                    kind: (*kind).into(),
                    value: (*value).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn matches_path_boundary_and_boolean_semantics() {
        let identity = Identity {
            title: "My Game - Settings".into(),
            process_names: vec!["Game.EXE".into()],
            executable_paths: vec![r"Z:\games\Game.EXE".into()],
            directories: vec![r"Z:\games\subdir".into()],
            ..Identity::default()
        };
        assert!(identity.matches(&linked(
            &[
                ("process_directory", "z:/games"),
                ("window_title", "settings")
            ],
            1
        )));
        assert!(!identity.matches(&linked(
            &[
                ("process_directory", "z:/game"),
                ("window_title", "settings")
            ],
            1
        )));
        assert!(identity.matches(&linked(
            &[("process_name", "game.exe"), ("window_title", "missing")],
            0
        )));
        assert!(!identity.matches(&linked(
            &[("process_full_path", "z:/games/game.exe/other")],
            0
        )));
        assert!(!identity.matches(&linked(&[], 0)));
    }

    #[test]
    fn steam_ids_are_exact_and_take_precedence() {
        let identity = Identity {
            steam_ids: vec![123],
            title: "Matching title".into(),
            ..Identity::default()
        };
        let mut app = linked(&[("window_title", "Matching")], 0);
        app.steam_id = Some(12);
        assert!(!identity.matches(&app));
        app.steam_id = Some(123);
        assert!(identity.matches(&app));
        assert_eq!(steam_app_id("steam_app_123"), Some(123));
        assert_eq!(steam_app_id("steam_app_1234suffix"), None);
    }

    #[test]
    fn focus_identity_does_not_consider_unrelated_processes() {
        let window = Window {
            app_id: "org.example.Editor.desktop".into(),
            title: "Editor".into(),
            pid: None,
        };
        let identity = identify(&window);
        assert!(identity.matches(&linked(&[("process_name", "editor")], 0)));
        assert!(!identity.matches(&linked(&[("process_name", "unrelated-game")], 0)));
        assert!(identity.executable_paths.is_empty());
        let steam = identify(&Window {
            app_id: "steam_app_123".into(),
            title: String::new(),
            pid: None,
        });
        assert_eq!(steam.steam_ids, [123]);
        assert!(!steam.matches(&linked(&[("process_name", "steam")], 0)));
    }

    #[test]
    fn parses_extra_libraries_and_desktop_visibility() {
        let vdf = r#""libraryfolders" { "0" { "path" "/home/me/.steam/steam" "apps" { "1" "1" } } "1" { "path" "/mnt/games" } "2" "/mnt/legacy" }"#;
        assert_eq!(
            library_paths(vdf),
            ["/home/me/.steam/steam", "/mnt/games", "/mnt/legacy"]
        );
        assert!(
            parse_desktop("[Desktop Entry]\nName=Hidden\nExec=hidden\nHidden=true\n").is_none()
        );
        assert!(
            parse_desktop("[Desktop Entry]\nName=Invisible\nExec=invisible\nNoDisplay=true\n")
                .is_none()
        );
        assert_eq!(
            parse_desktop(
                "[Desktop Entry]\nType=Application\nName=Game\nExec=\"/games/My Game\" %U\n"
            )
            .unwrap()
            .executable,
            "/games/My Game"
        );
        assert_eq!(
            first_exec_token("env -u WAYLAND_DISPLAY FOO=1 \"/games/My Game\" %U").as_deref(),
            Some("/games/My Game")
        );
        assert_eq!(
            first_exec_token(
                "/usr/bin/flatpak run --branch=stable --command=game org.example.Game"
            )
            .as_deref(),
            Some("game")
        );
        assert_eq!(first_exec_token("/usr/bin/env FOO=1").as_deref(), None);
    }
}
