use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::{Map, Value};
use tempfile::{Builder, NamedTempFile};

use crate::config::{
    Cli, ConfigArgs, ConfigCommand, ConfigGetArgs, ConfigSetArgs, ConfigUnsetArgs,
    config_path_selection,
};
use crate::terminal::{ACCENT, MUTED, SUCCESS, paint, write_stdout};

pub fn run(cli: &Cli, args: ConfigArgs) -> anyhow::Result<()> {
    let path = selected_path(cli)?;
    match args.command {
        None => print_value(&Value::Object(read_document(&path)?)),
        Some(ConfigCommand::Path) => {
            write_stdout(&format!("{}\n", path.display())).map_err(Into::into)
        }
        Some(ConfigCommand::Get(args)) => get(&path, args),
        Some(ConfigCommand::Set(args)) => set(&path, args),
        Some(ConfigCommand::Unset(args)) => unset(&path, args),
        Some(ConfigCommand::Edit) => edit(&path),
    }
}

fn selected_path(cli: &Cli) -> anyhow::Result<PathBuf> {
    config_path_selection(cli)
        .map_err(anyhow::Error::msg)?
        .path
        .context("no configuration path is available; pass --config or set CODEXIFY_CONFIG")
}

fn get(path: &Path, args: ConfigGetArgs) -> anyhow::Result<()> {
    let document = Value::Object(read_document(path)?);
    match args.key {
        None => print_value(&document),
        Some(key) => {
            let segments = parse_key(&key)?;
            let value = get_value(&document, &segments)
                .with_context(|| format!("setting not found: {key}"))?;
            print_value(value)
        }
    }
}

fn set(path: &Path, args: ConfigSetArgs) -> anyhow::Result<()> {
    let segments = parse_key(&args.key)?;
    let value = serde_json::from_str(&args.value).unwrap_or(Value::String(args.value));
    let mut document = Value::Object(read_document(path)?);
    set_value(&mut document, &segments, value)?;
    write_document(path, document.as_object().expect("root remains an object"))?;
    print_mutation("Set", &args.key, path)
}

fn unset(path: &Path, args: ConfigUnsetArgs) -> anyhow::Result<()> {
    let segments = parse_key(&args.key)?;
    let mut document = Value::Object(read_document(path)?);
    if !remove_value(&mut document, &segments)? {
        bail!("setting not found: {}", args.key);
    }
    write_document(path, document.as_object().expect("root remains an object"))?;
    print_mutation("Removed", &args.key, path)
}

fn edit(path: &Path) -> anyhow::Result<()> {
    refuse_unsafe_target(path)?;
    let document = Value::Object(read_document(path)?);
    let parent = config_parent(path);
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;

    let mut staging = Builder::new()
        .prefix(".codexify-edit-")
        .suffix(".json")
        .tempfile_in(parent)
        .with_context(|| format!("create editor staging file beside {}", path.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    staging.write_all(&bytes)?;
    staging.as_file().sync_all()?;
    preserve_or_private_permissions(path, staging.path())?;

    let editor = editor_command()?;
    let status = Command::new(&editor.program)
        .args(&editor.args)
        .arg(staging.path())
        .status()
        .with_context(|| format!("start editor {}", editor.display))?;
    if !status.success() {
        bail!(
            "editor {} exited with {status}; configuration was not changed",
            editor.display
        );
    }

    let edited = fs::read_to_string(staging.path())
        .with_context(|| format!("read edited configuration {}", staging.path().display()))?;
    let edited: Value = serde_json::from_str(&edited)
        .with_context(|| format!("parse edited configuration from {}", editor.display))?;
    let edited = edited
        .as_object()
        .context("edited configuration must contain a JSON object")?;
    write_document(path, edited)?;

    let output = format!(
        "{} {}\n{}\n",
        paint(SUCCESS, "Saved configuration:"),
        paint(ACCENT, path.display()),
        paint(
            MUTED,
            "Restart a running service to apply changes: codexify service restart"
        )
    );
    write_stdout(&output)?;
    Ok(())
}

fn print_value(value: &Value) -> anyhow::Result<()> {
    let mut output = serde_json::to_string_pretty(value)?;
    output.push('\n');
    write_stdout(&output)?;
    Ok(())
}

fn print_mutation(action: &str, key: &str, path: &Path) -> anyhow::Result<()> {
    let output = format!(
        "{} {} in {}\n{}\n",
        paint(SUCCESS, action),
        paint(ACCENT, key),
        paint(ACCENT, path.display()),
        paint(
            MUTED,
            "Restart a running service to apply changes: codexify service restart"
        )
    );
    write_stdout(&output)?;
    Ok(())
}

fn read_document(path: &Path) -> anyhow::Result<Map<String, Value>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("refusing to read symlinked config file {}", path.display());
            }
            if !metadata.is_file() {
                bail!("config path is not a regular file: {}", path.display());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect config file {}", path.display()));
        }
    }

    let text =
        fs::read_to_string(path).with_context(|| format!("read config file {}", path.display()))?;
    serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parse config file {}", path.display()))?
        .as_object()
        .cloned()
        .context("configuration must contain a JSON object")
}

fn write_document(path: &Path, document: &Map<String, Value>) -> anyhow::Result<()> {
    refuse_unsafe_target(path)?;
    let parent = config_parent(path);
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary config beside {}", path.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(document.clone()))?;
    bytes.push(b'\n');
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    preserve_or_private_permissions(path, temp.path())?;
    persist_replacing(temp, path)
        .with_context(|| format!("replace config file {}", path.display()))?;
    sync_parent(parent)?;
    Ok(())
}

fn refuse_unsafe_target(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to replace symlinked config file {}",
                path.display()
            )
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("config path is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect config file {}", path.display())),
    }
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn preserve_or_private_permissions(source: &Path, target: &Path) -> io::Result<()> {
    match fs::metadata(source) {
        Ok(metadata) => fs::set_permissions(target, metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(target, fs::Permissions::from_mode(0o600))
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    temp.persist(path).map(|_| ()).map_err(|error| error.error)
}

#[cfg(windows)]
fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = temp.into_temp_path();
    let wide = |value: &OsStr| {
        value
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let source_wide = wide(source.as_os_str());
    let target_wide = wide(path.as_os_str());
    let moved = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

fn parse_key(key: &str) -> anyhow::Result<Vec<String>> {
    if key.is_empty() {
        bail!("setting path must not be empty");
    }
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in key.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '.' {
            if current.is_empty() {
                bail!("setting path contains an empty segment: {key}");
            }
            segments.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    if escaped {
        bail!("setting path ends with an incomplete escape: {key}");
    }
    if current.is_empty() {
        bail!("setting path contains an empty segment: {key}");
    }
    segments.push(current);
    Ok(segments)
}

fn get_value<'a>(root: &'a Value, segments: &[String]) -> Option<&'a Value> {
    segments
        .iter()
        .try_fold(root, |value, segment| value.as_object()?.get(segment))
}

fn set_value(root: &mut Value, segments: &[String], value: Value) -> anyhow::Result<()> {
    let mut current = root;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let object = current
            .as_object_mut()
            .with_context(|| format!("{} is not an object", segments[..index].join(".")))?;
        if last {
            object.insert(segment.clone(), value);
            return Ok(());
        }
        if !object.contains_key(segment) {
            object.insert(segment.clone(), Value::Object(Map::new()));
        }
        current = object
            .get_mut(segment)
            .expect("inserted or existing setting");
        if !current.is_object() {
            bail!("{} is not an object", segments[..=index].join("."));
        }
    }
    unreachable!("parse_key always returns at least one segment")
}

fn remove_value(root: &mut Value, segments: &[String]) -> anyhow::Result<bool> {
    let mut current = root;
    for (index, segment) in segments.iter().enumerate() {
        let object = current
            .as_object_mut()
            .with_context(|| format!("{} is not an object", segments[..index].join(".")))?;
        if index + 1 == segments.len() {
            return Ok(object.remove(segment).is_some());
        }
        let Some(next) = object.get_mut(segment) else {
            return Ok(false);
        };
        if !next.is_object() {
            bail!("{} is not an object", segments[..=index].join("."));
        }
        current = next;
    }
    Ok(false)
}

struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
    display: String,
}

fn editor_command() -> anyhow::Result<EditorCommand> {
    for variable in ["VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            return parse_editor(value)
                .with_context(|| format!("parse editor command from {variable}"));
        }
    }

    #[cfg(windows)]
    {
        return Ok(EditorCommand {
            program: OsString::from("notepad.exe"),
            args: Vec::new(),
            display: "notepad.exe".into(),
        });
    }
    #[cfg(not(windows))]
    {
        for editor in ["nano", "vi"] {
            if executable_on_path(OsStr::new(editor)) {
                return Ok(EditorCommand {
                    program: OsString::from(editor),
                    args: Vec::new(),
                    display: editor.into(),
                });
            }
        }
        bail!("no editor is configured; set VISUAL or EDITOR, or install nano or vi")
    }
}

fn parse_editor(value: OsString) -> anyhow::Result<EditorCommand> {
    let text = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("editor command is not valid UTF-8"))?;
    let mut words = shell_words::split(&text).context("editor command has invalid quoting")?;
    if words.is_empty() {
        bail!("editor command is empty");
    }
    let program = words.remove(0);
    Ok(EditorCommand {
        program: OsString::from(&program),
        args: words.into_iter().map(OsString::from).collect(),
        display: text,
    })
}

#[cfg(not(windows))]
fn executable_on_path(name: &OsStr) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .any(|path| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dotted_paths_support_escaped_dots_and_backslashes() {
        assert_eq!(
            parse_key(r"servers.alpha\.beta.path\\name").unwrap(),
            ["servers", "alpha.beta", "path\\name"]
        );
        for invalid in ["", ".a", "a.", "a..b", "a\\"] {
            assert!(parse_key(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn nested_updates_preserve_siblings_and_reject_scalar_parents() {
        let mut value = json!({"a":{"sibling":1}});
        set_value(&mut value, &["a".into(), "value".into()], Value::Bool(true)).unwrap();
        assert_eq!(value, json!({"a":{"sibling":1,"value":true}}));
        assert!(remove_value(&mut value, &["a".into(), "value".into()]).unwrap());
        assert_eq!(value, json!({"a":{"sibling":1}}));
        assert!(
            set_value(
                &mut value,
                &["a".into(), "sibling".into(), "nested".into()],
                Value::Null
            )
            .is_err()
        );
    }

    #[test]
    fn editor_environment_accepts_arguments() {
        let editor = parse_editor(OsString::from("code --wait")).unwrap();
        assert_eq!(editor.program, "code");
        assert_eq!(editor.args, [OsString::from("--wait")]);
    }
}
