use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Map, Value};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use crate::conversation_auth::{conversation_auth_prompt, validate_conversation_auth_token};
use crate::openai_tunnel::{validate_runtime_api_key, validate_tunnel_id};
use crate::util::home_dir;

pub const TUNNEL_SETTINGS_URL: &str = "https://platform.openai.com/settings/organization/tunnels";
pub const API_KEYS_URL: &str = "https://platform.openai.com/settings/organization/api-keys";
pub const DEVELOPER_MODE_GUIDE_URL: &str =
    "https://developers.openai.com/api/docs/guides/developer-mode";
pub const CHATGPT_PLUGINS_URL: &str = "https://chatgpt.com/plugins";

/// Inputs for the quickstart wizard, resolved from the global `--config` and
/// `--work-dir` options before dispatch.
#[derive(Debug, Clone)]
pub struct QuickstartArgs {
    /// Config file to create or update.
    pub config: PathBuf,

    /// Initial directory shown by the project-directory prompt.
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickstartOutcome {
    pub work_dir: PathBuf,
    pub config_path: PathBuf,
    pub start_server: bool,
}

struct QuickstartEnvironment {
    current_dir: PathBuf,
    home_dir: PathBuf,
    executable: PathBuf,
}

pub fn run(args: QuickstartArgs) -> anyhow::Result<QuickstartOutcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    if !stdin.is_terminal() || !stdout.is_terminal() {
        bail!("quickstart is interactive and requires a terminal");
    }

    let environment = QuickstartEnvironment {
        current_dir: std::env::current_dir().context("determine current directory")?,
        home_dir: home_dir().context("quickstart cannot locate the user's home directory")?,
        executable: std::env::current_exe().context("locate the running codexify binary")?,
    };
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    run_with_io(
        args,
        environment,
        &mut input,
        &mut output,
        prompt_hidden_password,
    )
}

fn prompt_hidden_password(output_label: &str, output: &mut impl Write) -> io::Result<String> {
    let terminal = TerminalEchoProbe::open()?;
    prompt_hidden_password_with(output_label, output, rpassword::read_password, || {
        terminal.is_disabled()
    })
}

fn prompt_hidden_password_with<R, E>(
    output_label: &str,
    output: &mut impl Write,
    read_password: R,
    mut echo_is_disabled: E,
) -> io::Result<String>
where
    R: FnOnce() -> io::Result<String> + Send + 'static,
    E: FnMut() -> io::Result<bool>,
{
    let reader = thread::spawn(read_password);

    // The reader configures the terminal first; exposing the prompt only after
    // echo is off prevents a fast paste from being echoed during setup.
    loop {
        if reader.is_finished() {
            return join_password_reader(reader);
        }
        if echo_is_disabled()? {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    write!(output, "{output_label}: ")?;
    output.flush()?;
    join_password_reader(reader)
}

fn join_password_reader(reader: thread::JoinHandle<io::Result<String>>) -> io::Result<String> {
    reader
        .join()
        .map_err(|_| io::Error::other("hidden-input reader panicked"))?
}

struct TerminalEchoProbe {
    input: fs::File,
}

#[cfg(unix)]
impl TerminalEchoProbe {
    fn open() -> io::Result<Self> {
        Ok(Self {
            input: fs::OpenOptions::new().read(true).open("/dev/tty")?,
        })
    }

    fn is_disabled(&self) -> io::Result<bool> {
        use std::mem::MaybeUninit;
        use std::os::fd::AsRawFd;

        let mut terminal = MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(self.input.as_raw_fd(), terminal.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let terminal = unsafe { terminal.assume_init() };
        Ok(terminal.c_lflag & libc::ECHO == 0)
    }
}

#[cfg(windows)]
impl TerminalEchoProbe {
    fn open() -> io::Result<Self> {
        Ok(Self {
            input: fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("CONIN$")?,
        })
    }

    fn is_disabled(&self) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{ENABLE_ECHO_INPUT, GetConsoleMode};

        let input = self.input.as_raw_handle();
        if input.is_null() || input == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut mode = 0;
        if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode & ENABLE_ECHO_INPUT == 0)
    }
}

fn run_with_io<R, W, F>(
    args: QuickstartArgs,
    environment: QuickstartEnvironment,
    input: &mut R,
    output: &mut W,
    read_secret: F,
) -> anyhow::Result<QuickstartOutcome>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    let mut wizard = Wizard {
        input,
        output,
        read_secret,
    };
    let config_path = absolute_path(&environment.current_dir, &args.config);
    refuse_symlinked_config(&config_path)?;
    let mut file_config = read_config(&config_path)?;
    if file_config
        .get("openaiTunnel")
        .is_some_and(|value| !value.is_object())
    {
        bail!(
            "openaiTunnel in the existing config must be a JSON object: {}",
            config_path.display()
        );
    }

    writeln!(wizard.output, "Codexify quickstart")?;
    writeln!(
        wizard.output,
        "This wizard configures an OpenAI Secure MCP Tunnel and a ChatGPT developer-mode connector."
    )?;
    writeln!(wizard.output, "Config file: {}\n", config_path.display())?;

    let default_work_dir = args
        .work_dir
        .as_deref()
        .map(|path| absolute_path(&environment.current_dir, path))
        .unwrap_or_else(|| environment.current_dir.clone());
    let work_dir = prompt_existing_directory(
        &mut wizard,
        &default_work_dir,
        &environment.current_dir,
        &environment.home_dir,
    )?;
    let multi_project_default = file_config
        .get("multiProject")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let multi_project = wizard.confirm(
        "Use this directory as an access root for several independent projects?",
        multi_project_default,
    )?;

    let default_connector_name = connector_name_default(&work_dir, multi_project);
    let connector_name = prompt_connector_name(&mut wizard, &default_connector_name)?;
    let conversation_auth_token = configured_conversation_auth_token(&file_config)?;

    if file_config
        .get("apiKey")
        .is_some_and(|value| !value.is_null())
    {
        writeln!(
            wizard.output,
            "\nThe existing config contains apiKey, which cannot be combined with the native OpenAI tunnel."
        )?;
        if !wizard.confirm("Remove apiKey from the config?", true)? {
            bail!("quickstart cancelled without changing the config");
        }
        file_config.remove("apiKey");
    }

    let existing_tunnel_id = configured_tunnel_id(&file_config);
    print_tunnel_creation_step(&mut wizard, &connector_name)?;
    wizard.pause("Press Enter after the tunnel has been created")?;
    let tunnel_id = prompt_tunnel_id(&mut wizard, existing_tunnel_id.as_deref())?;

    let key_path = credential_path(&environment.home_dir, &tunnel_id);
    print_api_key_step(&mut wizard, &key_path)?;
    configure_runtime_key(&mut wizard, &key_path)?;

    merge_tunnel_config(
        &mut file_config,
        &tunnel_id,
        &key_path,
        multi_project,
        conversation_auth_token.as_deref(),
    )?;
    write_config(
        &config_path,
        &file_config,
        conversation_auth_token.is_some(),
    )?;

    writeln!(wizard.output, "\nLocal configuration complete.")?;
    writeln!(wizard.output, "  Config: {}", config_path.display())?;
    writeln!(wizard.output, "  Runtime key: {}", key_path.display())?;
    writeln!(wizard.output, "  Project directory: {}", work_dir.display())?;
    if conversation_auth_token.is_some() {
        writeln!(
            wizard.output,
            "  Conversation token: stored in the config file; do not commit or share that file"
        )?;
    }
    let command = launch_command(&environment.executable, &work_dir, &config_path);
    writeln!(wizard.output, "\nLaunch command:\n  {command}")?;

    print_connector_step(&mut wizard, &connector_name, &tunnel_id, multi_project)?;
    if let Some(token) = conversation_auth_token.as_deref() {
        print_conversation_auth_step(&mut wizard, token)?;
    }
    let start_server = wizard.confirm(
        "Start Codexify now and keep this terminal open while ChatGPT scans the connector?",
        true,
    )?;
    if start_server {
        writeln!(
            wizard.output,
            "\nStarting Codexify. Wait for `OpenAI Secure MCP Tunnel: ready`, then complete the ChatGPT steps above."
        )?;
    } else {
        writeln!(
            wizard.output,
            "\nRun the launch command above before scanning or using the connector."
        )?;
    }

    Ok(QuickstartOutcome {
        work_dir,
        config_path,
        start_server,
    })
}

struct Wizard<'a, R, W, F> {
    input: &'a mut R,
    output: &'a mut W,
    read_secret: F,
}

impl<R, W, F> Wizard<'_, R, W, F>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    fn prompt(&mut self, label: &str, default: Option<&str>) -> anyhow::Result<String> {
        match default {
            Some(default) => write!(self.output, "{label} [{default}]: ")?,
            None => write!(self.output, "{label}: ")?,
        }
        self.output.flush()?;

        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("quickstart input ended before setup was complete");
        }
        let value = line.trim().to_string();
        if value.is_empty() {
            return Ok(default.unwrap_or_default().to_string());
        }
        Ok(value)
    }

    fn confirm(&mut self, label: &str, default: bool) -> anyhow::Result<bool> {
        let suffix = if default { "Y/n" } else { "y/N" };
        loop {
            write!(self.output, "{label} [{suffix}]: ")?;
            self.output.flush()?;
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                bail!("quickstart input ended before setup was complete");
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => writeln!(self.output, "Please answer yes or no.")?,
            }
        }
    }

    fn pause(&mut self, label: &str) -> anyhow::Result<()> {
        write!(self.output, "{label}: ")?;
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("quickstart input ended before setup was complete");
        }
        Ok(())
    }

    fn secret(&mut self, label: &str) -> anyhow::Result<Zeroizing<String>> {
        let value =
            (self.read_secret)(label, self.output).context("read hidden runtime API key")?;
        writeln!(self.output)?;
        Ok(Zeroizing::new(value))
    }
}

fn prompt_existing_directory<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    default: &Path,
    current_dir: &Path,
    home_dir: &Path,
) -> anyhow::Result<PathBuf>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    let default = default.to_string_lossy();
    loop {
        let raw = wizard.prompt("Project directory", Some(&default))?;
        let candidate = resolve_user_path(&raw, current_dir, home_dir);
        match fs::canonicalize(&candidate) {
            Ok(path) if path.is_dir() => return Ok(path),
            Ok(path) => writeln!(
                wizard.output,
                "That path is not a directory: {}",
                path.display()
            )?,
            Err(error) => writeln!(wizard.output, "Cannot use {}: {error}", candidate.display())?,
        }
    }
}

fn prompt_connector_name<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    default: &str,
) -> anyhow::Result<String>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    loop {
        let name = wizard.prompt("ChatGPT connector name", Some(default))?;
        if name.chars().any(char::is_control) {
            writeln!(
                wizard.output,
                "The connector name cannot contain control characters."
            )?;
            continue;
        }
        return Ok(name);
    }
}

fn prompt_tunnel_id<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    default: Option<&str>,
) -> anyhow::Result<String>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    loop {
        let tunnel_id = wizard.prompt("Tunnel ID", default)?;
        match validate_tunnel_id(&tunnel_id) {
            Ok(()) => return Ok(tunnel_id),
            Err(error) => writeln!(wizard.output, "{error}")?,
        }
    }
}

fn configure_runtime_key<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    key_path: &Path,
) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    let can_keep = existing_runtime_key_is_valid(key_path);
    if can_keep {
        writeln!(
            wizard.output,
            "A valid stored runtime key already exists at {}.",
            key_path.display()
        )?;
    }

    loop {
        let label = if can_keep {
            "Paste a replacement runtime API key, or press Enter to keep the stored key"
        } else {
            "Paste the runtime API key (input is hidden)"
        };
        let raw = wizard.secret(label)?;
        let key = Zeroizing::new(raw.trim().to_string());
        if key.is_empty() && can_keep {
            return Ok(());
        }
        if let Err(error) = validate_runtime_api_key(&key) {
            writeln!(wizard.output, "{error}")?;
            continue;
        }
        write_private_key(key_path, key.as_bytes())?;
        return Ok(());
    }
}

fn print_tunnel_creation_step<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    connector_name: &str,
) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    writeln!(wizard.output, "\n1. Create an OpenAI Secure MCP Tunnel")?;
    writeln!(wizard.output, "   Open: {TUNNEL_SETTINGS_URL}")?;
    writeln!(wizard.output, "   Suggested name: {connector_name}")?;
    writeln!(
        wizard.output,
        "   Associate the tunnel with the Platform organization that owns it and the ChatGPT workspace where the connector will be used."
    )?;
    writeln!(
        wizard.output,
        "   Creating a tunnel requires Tunnels Read + Manage; running it and selecting it in ChatGPT require Tunnels Read + Use."
    )?;
    writeln!(
        wizard.output,
        "   Create the tunnel, then copy the identifier beginning with `tunnel_`."
    )?;
    Ok(())
}

fn print_api_key_step<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    key_path: &Path,
) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    writeln!(wizard.output, "\n2. Create a runtime API key")?;
    writeln!(wizard.output, "   Open: {API_KEYS_URL}")?;
    writeln!(
        wizard.output,
        "   Use a key whose principal has Tunnels Read + Use for this tunnel. Keep tunnel-management credentials separate."
    )?;
    writeln!(
        wizard.output,
        "   The key will be stored outside the project at {}; the JSON config stores only a file reference. On Unix, the wizard restricts the credential file to the current user.",
        key_path.display()
    )?;
    Ok(())
}

fn print_connector_step<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    connector_name: &str,
    tunnel_id: &str,
    multi_project: bool,
) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    let description = if multi_project {
        "Local Codex-style tools with per-chat project selection"
    } else {
        "Local Codex-style file, shell, Git, and MCP tools"
    };
    writeln!(wizard.output, "\n3. Add the connector in ChatGPT Web")?;
    writeln!(
        wizard.output,
        "   Developer-mode guide: {DEVELOPER_MODE_GUIDE_URL}"
    )?;
    writeln!(
        wizard.output,
        "   In ChatGPT, open Settings > Security and login and enable Developer mode. Managed workspaces may first require an admin grant under Workspace Settings > Permissions & Roles > Connected Data, then expose the toggle under Settings > Apps > Advanced Settings."
    )?;
    writeln!(wizard.output, "   Open: {CHATGPT_PLUGINS_URL}")?;
    writeln!(wizard.output, "   Click the + button and enter:")?;
    writeln!(wizard.output, "     Name: {connector_name}")?;
    writeln!(wizard.output, "     Description: {description}")?;
    writeln!(wizard.output, "     Connection: Tunnel")?;
    writeln!(
        wizard.output,
        "     Tunnel: select `{tunnel_id}` or paste that tunnel ID"
    )?;
    writeln!(wizard.output, "     Authentication: No Authentication")?;
    writeln!(
        wizard.output,
        "   Wait until this terminal reports `OpenAI Secure MCP Tunnel: ready`, then scan the tools and create the connector. Enable the read/write actions you intend to use; full Codex-style operation needs both."
    )?;
    writeln!(
        wizard.output,
        "   In a new chat, open the + menu, choose Developer mode, and select `{connector_name}`."
    )?;
    Ok(())
}

fn print_conversation_auth_step<R, W, F>(
    wizard: &mut Wizard<'_, R, W, F>,
    token: &str,
) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, &mut W) -> io::Result<String>,
{
    writeln!(
        wizard.output,
        "\n4. Configure ChatGPT conversation authentication"
    )?;
    writeln!(
        wizard.output,
        "   Paste this instruction into a chat, or add it to the ChatGPT Project's Project instructions:"
    )?;
    writeln!(wizard.output, "   {}", conversation_auth_prompt(token))?;
    Ok(())
}

fn connector_name_default(work_dir: &Path, multi_project: bool) -> String {
    let suffix = if multi_project {
        "Projects".to_string()
    } else {
        work_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Local".to_string())
    };
    format!("Codexify - {suffix}")
}

fn configured_tunnel_id(config: &Map<String, Value>) -> Option<String> {
    config
        .get("openaiTunnel")
        .and_then(Value::as_object)
        .and_then(|tunnel| tunnel.get("tunnelId"))
        .and_then(Value::as_str)
        .filter(|value| validate_tunnel_id(value).is_ok())
        .map(str::to_string)
}

fn configured_conversation_auth_token(
    config: &Map<String, Value>,
) -> anyhow::Result<Option<String>> {
    let Some(value) = config.get("conversationAuthToken") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let token = value
        .as_str()
        .context("conversationAuthToken in the existing config must be a string or null")?;
    validate_conversation_auth_token(token).map_err(anyhow::Error::msg)?;
    Ok(Some(token.to_string()))
}

fn credential_path(home_dir: &Path, tunnel_id: &str) -> PathBuf {
    home_dir
        .join(".codexify")
        .join("openai-tunnel")
        .join("credentials")
        .join(format!("{tunnel_id}.key"))
}

fn merge_tunnel_config(
    config: &mut Map<String, Value>,
    tunnel_id: &str,
    key_path: &Path,
    multi_project: bool,
    conversation_auth_token: Option<&str>,
) -> anyhow::Result<()> {
    let tunnel = config
        .entry("openaiTunnel".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let tunnel = tunnel
        .as_object_mut()
        .context("openaiTunnel in the existing config must be a JSON object")?;
    tunnel.insert("tunnelId".to_string(), Value::String(tunnel_id.to_string()));
    tunnel.insert(
        "apiKeyRef".to_string(),
        Value::String(format!("file:{}", key_path.display())),
    );
    config.insert("multiProject".to_string(), Value::Bool(multi_project));
    match conversation_auth_token {
        Some(token) => {
            config.insert(
                "conversationAuthToken".to_string(),
                Value::String(token.to_string()),
            );
        }
        None => {
            config.remove("conversationAuthToken");
        }
    }
    Ok(())
}

fn read_config(path: &Path) -> anyhow::Result<Map<String, Value>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .with_context(|| format!("parse existing config {}", path.display()))?
            .as_object()
            .cloned()
            .context("existing config must contain a JSON object"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error).with_context(|| format!("read config {}", path.display())),
    }
}

fn write_config(
    path: &Path,
    config: &Map<String, Value>,
    contains_conversation_token: bool,
) -> anyhow::Result<()> {
    refuse_symlinked_config(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary config beside {}", path.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(config.clone()))?;
    bytes.push(b'\n');
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    if contains_conversation_token {
        make_private(temp.path())?;
    } else {
        preserve_permissions(path, temp.path())?;
    }
    persist_replacing(temp, path)
        .with_context(|| format!("replace config file {}", path.display()))?;
    Ok(())
}

fn refuse_symlinked_config(path: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "refusing to replace symlinked config file {}",
            path.display()
        );
    }
    Ok(())
}

fn write_private_key(path: &Path, key: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("runtime key path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create runtime-key directory {}", parent.display()))?;
    make_private(parent)?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary runtime-key file in {}", parent.display()))?;
    make_private(temp.path())?;
    temp.write_all(key)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    persist_replacing(temp, path)
        .with_context(|| format!("replace runtime-key file {}", path.display()))?;
    make_private(path)?;
    Ok(())
}

fn existing_runtime_key_is_valid(path: &Path) -> bool {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        _ => return false,
    };
    if metadata.len() == 0 || metadata.len() > 64 * 1024 {
        return false;
    }
    if make_private(path).is_err() {
        return false;
    }
    let contents = match fs::read_to_string(path) {
        Ok(contents) => Zeroizing::new(contents),
        Err(_) => return false,
    };
    validate_runtime_api_key(contents.trim()).is_ok()
}

fn make_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn preserve_permissions(source: &Path, target: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::metadata(source) {
        fs::set_permissions(target, metadata.permissions())?;
    }
    Ok(())
}

fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    match temp.persist(path) {
        Ok(_) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                let temp = error.file;
                if path.exists() {
                    fs::remove_file(path)?;
                    return temp.persist(path).map(|_| ()).map_err(|error| error.error);
                }
                Err(error.error)
            }
            #[cfg(not(windows))]
            {
                Err(error.error)
            }
        }
    }
}

fn absolute_path(current_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
}

fn resolve_user_path(raw: &str, current_dir: &Path, home_dir: &Path) -> PathBuf {
    if raw == "~" {
        return home_dir.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir.join(rest);
    }
    absolute_path(current_dir, Path::new(raw))
}

fn launch_command(executable: &Path, work_dir: &Path, config_path: &Path) -> String {
    format!(
        "{} --work-dir {} --config {}",
        quote_shell_arg(&executable.to_string_lossy()),
        quote_shell_arg(&work_dir.to_string_lossy()),
        quote_shell_arg(&config_path.to_string_lossy())
    )
}

#[cfg(not(windows))]
fn quote_shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn quote_shell_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::config::{Cli, CliCommand};

    const TUNNEL_ID: &str = "tunnel_0123456789abcdef0123456789abcdef";
    const RUNTIME_KEY: &str = "sk-runtime-test-key_123";
    const CONVERSATION_TOKEN: &str = "codexify_chat_0123456789abcdef0123456789abcdef";

    struct GuardedPromptOutput {
        bytes: Vec<u8>,
        echo_is_disabled: Arc<AtomicBool>,
        reader_release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for GuardedPromptOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(self.echo_is_disabled.load(Ordering::Acquire));
            self.bytes.extend_from_slice(bytes);
            let (released, wake_reader) = &*self.reader_release;
            *released.lock().unwrap() = true;
            wake_reader.notify_one();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn environment(root: &TempDir, work_dir: &Path) -> QuickstartEnvironment {
        let home_dir = root.path().join("home");
        fs::create_dir_all(&home_dir).unwrap();
        QuickstartEnvironment {
            current_dir: work_dir.to_path_buf(),
            home_dir,
            executable: root.path().join("bin").join("codexify"),
        }
    }

    fn args(config: PathBuf, work_dir: &Path) -> QuickstartArgs {
        QuickstartArgs {
            config,
            work_dir: Some(work_dir.to_path_buf()),
        }
    }

    fn run_test_wizard(
        args: QuickstartArgs,
        environment: QuickstartEnvironment,
        input: &str,
        secrets: &[&str],
    ) -> (anyhow::Result<QuickstartOutcome>, String) {
        let mut input = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut secrets: VecDeque<String> = secrets.iter().map(|value| value.to_string()).collect();
        let result = run_with_io(
            args,
            environment,
            &mut input,
            &mut output,
            |label, output| {
                write!(output, "{label}: ")?;
                output.flush()?;
                secrets.pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "no test secret remains")
                })
            },
        );
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn hidden_secret_prompt_is_published_only_after_echo_is_disabled() {
        let echo_is_disabled = Arc::new(AtomicBool::new(false));
        let reader_release = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_release_for_thread = Arc::clone(&reader_release);
        let read_password = move || {
            let (released, wake_reader) = &*reader_release_for_thread;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake_reader.wait(released).unwrap();
            }
            Ok("secret".to_string())
        };
        let echo_for_probe = Arc::clone(&echo_is_disabled);
        let mut probe_count = 0;
        let mut output = GuardedPromptOutput {
            bytes: Vec::new(),
            echo_is_disabled,
            reader_release,
        };

        let secret =
            prompt_hidden_password_with("Runtime key", &mut output, read_password, move || {
                probe_count += 1;
                let disabled = probe_count > 1;
                echo_for_probe.store(disabled, Ordering::Release);
                Ok(disabled)
            })
            .unwrap();

        assert_eq!(secret, "secret");
        assert_eq!(output.bytes, b"Runtime key: ");
    }

    #[test]
    fn cli_exposes_quickstart_without_weakening_server_requirements() {
        let cli = Cli::try_parse_from(["codexify", "quickstart"]).unwrap();
        assert!(matches!(cli.command, Some(CliCommand::Quickstart)));
        assert!(cli.work_dir.is_none());

        // The wizard reads the global --config/--work-dir options rather than
        // owning its own, so they parse on the subcommand too.
        let cli = Cli::try_parse_from([
            "codexify",
            "quickstart",
            "--config",
            "custom.json",
            "--work-dir",
            "/tmp/project",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(CliCommand::Quickstart)));
        assert_eq!(cli.config.as_deref(), Some("custom.json"));
        assert_eq!(cli.work_dir.as_deref(), Some("/tmp/project"));

        // `--work-dir` is a global option so subcommands can run without it, but
        // the server path still refuses to start when it is missing — the
        // requirement is now enforced while the configuration is loaded rather
        // than at argument-parse time.
        let cli = Cli::try_parse_from(["codexify"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.work_dir.is_none());
        let error = crate::config::load_config(cli).unwrap_err();
        assert!(
            error.contains("--work-dir"),
            "expected a missing work-dir error, got: {error}"
        );
    }

    #[test]
    fn new_install_writes_a_private_key_and_a_secret_free_config() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config_path = project.join("codex.config.json");
        let environment = environment(&root, &project);
        let home_dir = environment.home_dir.clone();
        let input = format!("\n\n\n\n{TUNNEL_ID}\nn\n");

        let (result, output) = run_test_wizard(
            args(config_path.clone(), &project),
            environment,
            &input,
            &[RUNTIME_KEY],
        );
        let outcome = result.unwrap();
        assert!(!outcome.start_server);
        assert_eq!(outcome.work_dir, fs::canonicalize(&project).unwrap());
        assert_eq!(outcome.config_path, config_path);

        let key_path = credential_path(&home_dir, TUNNEL_ID);
        assert_eq!(fs::read_to_string(&key_path).unwrap().trim(), RUNTIME_KEY);
        let config: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(config["multiProject"], json!(false));
        assert!(config.get("conversationAuthToken").is_none());
        assert_eq!(config["openaiTunnel"]["tunnelId"], json!(TUNNEL_ID));
        assert_eq!(
            config["openaiTunnel"]["apiKeyRef"],
            json!(format!("file:{}", key_path.display()))
        );
        assert!(
            !fs::read_to_string(&config_path)
                .unwrap()
                .contains(RUNTIME_KEY)
        );

        assert!(output.contains(TUNNEL_SETTINGS_URL));
        assert!(output.contains(API_KEYS_URL));
        assert!(output.contains(DEVELOPER_MODE_GUIDE_URL));
        assert!(output.contains(CHATGPT_PLUGINS_URL));
        assert!(output.contains("Connection: Tunnel"));
        assert!(output.contains("Authentication: No Authentication"));
        assert!(output.contains(TUNNEL_ID));
        assert!(!output.contains("Require each new ChatGPT conversation"));
        assert!(!output.contains("Configure ChatGPT conversation authentication"));
        assert!(!output.contains(RUNTIME_KEY));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(key_path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn rerun_preserves_advanced_conversation_auth_and_unrelated_config() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("projects");
        fs::create_dir_all(&project).unwrap();
        let config_path = project.join("custom.json");
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&json!({
                "apiKey": "legacy-local-token",
                "multiProject": true,
                "conversationAuthToken": CONVERSATION_TOKEN,
                "allowedCommands": ["git"],
                "openaiTunnel": {
                    "tunnelId": TUNNEL_ID,
                    "apiKeyRef": "env:OLD_KEY",
                    "organizationId": "org_example"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let environment = environment(&root, &project);
        let key_path = credential_path(&environment.home_dir, TUNNEL_ID);
        write_private_key(&key_path, RUNTIME_KEY.as_bytes()).unwrap();

        let (result, output) = run_test_wizard(
            args(config_path.clone(), &project),
            environment,
            "\n\n\n\n\n\nn\n",
            &[""],
        );
        let outcome = result.unwrap();
        assert!(!outcome.start_server);
        assert!(output.contains("valid stored runtime key already exists"));
        assert_eq!(fs::read_to_string(&key_path).unwrap().trim(), RUNTIME_KEY);

        let config: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(config.get("apiKey").is_none());
        assert_eq!(config["multiProject"], json!(true));
        assert_eq!(config["conversationAuthToken"], json!(CONVERSATION_TOKEN));
        assert!(output.contains(&conversation_auth_prompt(CONVERSATION_TOKEN)));
        assert!(!output.contains("Require each new ChatGPT conversation"));
        assert!(!output.contains("Generate a new conversation token"));
        assert_eq!(config["allowedCommands"], json!(["git"]));
        assert_eq!(
            config["openaiTunnel"]["organizationId"],
            json!("org_example")
        );
        assert_eq!(
            config["openaiTunnel"]["apiKeyRef"],
            json!(format!("file:{}", key_path.display()))
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn invalid_tunnel_ids_and_keys_are_reprompted_without_echoing_secrets() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config_path = project.join("codex.config.json");
        let environment = environment(&root, &project);
        let input = format!("\nn\n\n\ninvalid-tunnel\n{TUNNEL_ID}\nn\n");
        let invalid_key = "not a valid key!";

        let (result, output) = run_test_wizard(
            args(config_path, &project),
            environment,
            &input,
            &[invalid_key, RUNTIME_KEY],
        );
        result.unwrap();
        assert!(output.contains("tunnel_ followed by 32 lowercase letters or digits"));
        assert!(output.contains("OpenAI tunnel API key is malformed"));
        assert!(!output.contains(invalid_key));
        assert!(!output.contains(RUNTIME_KEY));
    }

    #[test]
    fn malformed_existing_conversation_token_fails_before_writing_credentials() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config_path = project.join("codex.config.json");
        fs::write(
            &config_path,
            r#"{"conversationAuthToken":"not valid whitespace"}"#,
        )
        .unwrap();
        let environment = environment(&root, &project);
        let credentials = environment.home_dir.join(".codexify");

        let (result, _) = run_test_wizard(
            args(config_path.clone(), &project),
            environment,
            "\n\n\n",
            &[],
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("conversationAuthToken"));
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            r#"{"conversationAuthToken":"not valid whitespace"}"#
        );
        assert!(!credentials.exists());
    }

    #[test]
    fn malformed_existing_tunnel_config_fails_before_writing_credentials() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config_path = project.join("codex.config.json");
        fs::write(&config_path, r#"{"openaiTunnel":"invalid"}"#).unwrap();
        let environment = environment(&root, &project);
        let credentials = environment.home_dir.join(".codexify");

        let (result, _) =
            run_test_wizard(args(config_path.clone(), &project), environment, "", &[]);
        let error = result.unwrap_err().to_string();
        assert!(error.contains("openaiTunnel in the existing config must be a JSON object"));
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            r#"{"openaiTunnel":"invalid"}"#
        );
        assert!(!credentials.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_fails_before_writing_credentials() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let target = project.join("actual.json");
        let config_path = project.join("codex.config.json");
        fs::write(&target, "{}").unwrap();
        symlink(&target, &config_path).unwrap();
        let environment = environment(&root, &project);
        let credentials = environment.home_dir.join(".codexify");

        let (result, _) = run_test_wizard(args(config_path, &project), environment, "", &[]);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("refusing to replace symlinked config file")
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "{}");
        assert!(!credentials.exists());
    }

    #[test]
    fn tilde_paths_resolve_against_the_injected_home_directory() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let project = home.join("src").join("project");
        fs::create_dir_all(&project).unwrap();
        assert_eq!(
            resolve_user_path("~/src/project", root.path(), &home),
            project
        );
    }
}
