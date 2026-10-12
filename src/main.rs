//! OpenDeck action for the PdfCraft command line interface.
//!
//! Arguments are stored as a JSON string array and sent directly to the CLI.
//! Deliberately never invoking a shell preserves paths, JSON parameters, and user input exactly.

use std::{path::PathBuf, process::Stdio, time::Duration};

use openaction::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};
use tokio::process::Command;

const DEFAULT_EXECUTABLE: &str = "pdfcraft-cli";
const DEFAULT_DESKTOP_EXECUTABLE: &str = "pdfcraft";
const CATALOG_ARGUMENTS: &[&str] = &["tools"];
const DEFAULT_ARGUMENTS: &str = r#"["tools"]"#;
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct CommandSettings {
    /// CLI executable or absolute path. Empty uses the app's conventional CLI name.
    executable: String,
    /// JSON string array, e.g. ["exec", "timeline.play", "{}"].
    arguments: String,
    /// Optional directory in which to run the command.
    working_directory: String,
    /// A user-visible title shown before the optional command output.
    title: String,
    /// Keep stdout/stderr (trimmed) on the Stream Deck key after a successful run.
    show_output: bool,
    /// Wall-clock limit for a process. Zero uses 30 seconds; maximum is 300 seconds.
    timeout_seconds: u64,
}

impl Default for CommandSettings {
    fn default() -> Self {
        Self {
            executable: String::new(),
            arguments: DEFAULT_ARGUMENTS.to_string(),
            working_directory: String::new(),
            title: String::new(),
            show_output: false,
            timeout_seconds: 30,
        }
    }
}

impl CommandSettings {
    fn executable(&self) -> &str {
        let executable = self.executable.trim();
        if executable.is_empty() {
            DEFAULT_EXECUTABLE
        } else {
            executable
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(if self.timeout_seconds == 0 {
            30
        } else {
            self.timeout_seconds.clamp(1, 300)
        })
    }

    fn title(&self) -> Option<String> {
        (!self.title.trim().is_empty()).then(|| self.title.trim().to_string())
    }
}

#[derive(Debug)]
enum ExecutionError {
    Configuration(String),
    Process(String),
    TimedOut(u64),
    Failed { code: Option<i32>, output: String },
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(message) | Self::Process(message) => f.write_str(message),
            Self::TimedOut(seconds) => write!(f, "Timed out after {seconds} seconds"),
            Self::Failed { code, output } => {
                write!(
                    f,
                    "PdfCraft CLI exited with {}",
                    code.map_or_else(|| "a signal".into(), |code| format!("code {code}"))
                )?;
                if !output.is_empty() {
                    write!(f, ": {output}")?;
                }
                Ok(())
            }
        }
    }
}

fn parse_arguments(raw: &str) -> Result<Vec<String>, ExecutionError> {
    let parsed: Value = serde_json::from_str(raw.trim()).map_err(|error| {
        ExecutionError::Configuration(format!(
            "Arguments must be a JSON array of strings: {error}"
        ))
    })?;
    let values = parsed.as_array().ok_or_else(|| {
        ExecutionError::Configuration("Arguments must be a JSON array of strings.".into())
    })?;
    if values.is_empty() {
        return Err(ExecutionError::Configuration(
            "Add at least one CLI argument, for example [\"commands\", \"--json\"].".into(),
        ));
    }
    if values.len() > MAX_ARGUMENTS {
        return Err(ExecutionError::Configuration(format!(
            "At most {MAX_ARGUMENTS} arguments are allowed."
        )));
    }
    values
        .iter()
        .map(|value| {
            let value = value.as_str().ok_or_else(|| {
                ExecutionError::Configuration(
                    "Every argument must be a string; numbers and JSON values must be quoted."
                        .into(),
                )
            })?;
            if value.len() > MAX_ARGUMENT_BYTES {
                return Err(ExecutionError::Configuration(format!(
                    "An argument is longer than {MAX_ARGUMENT_BYTES} bytes."
                )));
            }
            Ok(value.to_string())
        })
        .collect()
}

fn summary(stdout: &[u8], stderr: &[u8]) -> String {
    let output = if !stderr.is_empty() { stderr } else { stdout };
    let text = String::from_utf8_lossy(output).replace(['\n', '\r'], " ");
    let text = text.trim();
    let mut end = text.len().min(120);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if text.len() > end { "…" } else { "" };
    format!("{}{}", &text[..end], suffix)
}

async fn execute(
    settings: &CommandSettings,
    arguments: Vec<String>,
) -> Result<String, ExecutionError> {
    let mut command = Command::new(settings.executable());
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !settings.working_directory.trim().is_empty() {
        command.current_dir(PathBuf::from(settings.working_directory.trim()));
    }
    let seconds = settings.timeout().as_secs();
    let output = tokio::time::timeout(settings.timeout(), command.output())
        .await
        .map_err(|_| ExecutionError::TimedOut(seconds))?
        .map_err(|error| {
            ExecutionError::Process(format!(
                "Could not start {}: {error}",
                settings.executable()
            ))
        })?;
    let output_summary = summary(&output.stdout, &output.stderr);
    if output.status.success() {
        Ok(output_summary)
    } else {
        Err(ExecutionError::Failed {
            code: output.status.code(),
            output: output_summary,
        })
    }
}

async fn report(
    instance: &Instance,
    settings: &CommandSettings,
    result: Result<String, ExecutionError>,
    to_inspector: bool,
) {
    match result {
        Ok(output) => {
            let title = if settings.show_output && !output.is_empty() {
                output.clone()
            } else {
                settings.title().unwrap_or_else(|| "Done".to_string())
            };
            let _ = instance.set_title(Some(title), None).await;
            let _ = instance.show_ok().await;
            if to_inspector {
                let _ = instance
                    .send_to_property_inspector(
                        json!({"kind":"result", "ok":true, "message": output}),
                    )
                    .await;
            }
        }
        Err(error) => {
            let message = error.to_string();
            log::warn!("PdfCraft CLI action failed: {message}");
            let _ = instance.set_title(Some("Error".to_string()), None).await;
            let _ = instance.show_alert().await;
            if to_inspector {
                let _ = instance
                    .send_to_property_inspector(
                        json!({"kind":"result", "ok":false, "message": message}),
                    )
                    .await;
            }
        }
    }
}

async fn run_action(instance: &Instance, settings: &CommandSettings, to_inspector: bool) {
    let result = parse_arguments(&settings.arguments).and_then(|arguments| {
        // Validation above is synchronous; the process work stays on the async runtime.
        Ok(arguments)
    });
    let result = match result {
        Ok(arguments) => execute(settings, arguments).await,
        Err(error) => Err(error),
    };
    report(instance, settings, result, to_inspector).await;
}

/// Run a command assembled by one of the focused Stream Deck actions.  The generic action still
/// accepts arbitrary JSON arguments, while these actions build only documented PdfCraft commands.
async fn run_arguments(
    instance: &Instance,
    settings: &CommandSettings,
    arguments: Result<Vec<String>, ExecutionError>,
    to_inspector: bool,
) {
    let result = match arguments {
        Ok(arguments) => execute(settings, arguments).await,
        Err(error) => Err(error),
    };
    report(instance, settings, result, to_inspector).await;
}

fn required(value: &str, name: &str) -> Result<String, ExecutionError> {
    let value = value.trim();
    (!value.is_empty())
        .then(|| value.to_string())
        .ok_or_else(|| ExecutionError::Configuration(format!("{name} is required.")))
}

fn nonempty_lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PdfWorkflowSettings {
    executable: String,
    operation: String,
    input: String,
    additional_inputs: String,
    output: String,
    pages: String,
    every: u64,
    rotation: i16,
    dpi: u16,
    password: String,
    title: String,
    show_output: bool,
    timeout_seconds: u64,
}

impl Default for PdfWorkflowSettings {
    fn default() -> Self {
        Self {
            executable: String::new(),
            operation: "extract".to_string(),
            input: String::new(),
            additional_inputs: String::new(),
            output: String::new(),
            pages: "1".to_string(),
            every: 10,
            rotation: 90,
            dpi: 144,
            password: String::new(),
            title: String::new(),
            show_output: false,
            timeout_seconds: 60,
        }
    }
}

impl PdfWorkflowSettings {
    fn command_settings(&self) -> CommandSettings {
        CommandSettings {
            executable: self.executable.clone(),
            arguments: DEFAULT_ARGUMENTS.to_string(),
            working_directory: String::new(),
            title: self.title.clone(),
            show_output: self.show_output,
            timeout_seconds: self.timeout_seconds,
        }
    }

    fn arguments(&self) -> Result<Vec<String>, ExecutionError> {
        let input = required(&self.input, "Input PDF")?;
        let output = |label: &str| required(&self.output, label);
        let password = |args: &mut Vec<String>| {
            if !self.password.is_empty() {
                args.extend(["--password".to_string(), self.password.clone()]);
            }
        };
        match self.operation.as_str() {
            "combine" => {
                let mut args = vec!["combine".to_string(), input];
                args.extend(nonempty_lines(&self.additional_inputs));
                if args.len() < 3 {
                    return Err(ExecutionError::Configuration(
                        "Combine needs an input PDF plus at least one additional PDF (one path per line).".into(),
                    ));
                }
                args.extend(["--out".to_string(), output("Combined PDF output")?]);
                Ok(args)
            }
            "extract" => {
                let pages = required(&self.pages, "Pages (for example 1,3,5)")?;
                let mut args = vec![
                    "extract".to_string(),
                    input,
                    "--pages".to_string(),
                    pages,
                    "--out".to_string(),
                    output("Extracted PDF output")?,
                ];
                password(&mut args);
                Ok(args)
            }
            "split" => {
                if self.every == 0 {
                    return Err(ExecutionError::Configuration(
                        "Pages per split must be at least 1.".into(),
                    ));
                }
                let mut args = vec![
                    "split".to_string(),
                    input,
                    "--every".to_string(),
                    self.every.to_string(),
                    "--out-dir".to_string(),
                    output("Output directory")?,
                ];
                password(&mut args);
                Ok(args)
            }
            "rotate" => {
                let pages = required(&self.pages, "Pages to rotate")?;
                if !matches!(self.rotation, 90 | 180 | 270 | -90) {
                    return Err(ExecutionError::Configuration(
                        "Rotation must be 90, 180, 270, or -90 degrees.".into(),
                    ));
                }
                let mut args = vec![
                    "edit".to_string(),
                    input,
                    "--rotate".to_string(),
                    format!("{pages}:{}", self.rotation),
                    "--out".to_string(),
                    output("Rotated PDF output")?,
                ];
                password(&mut args);
                Ok(args)
            }
            "render" => {
                let page = required(&self.pages, "Page number")?;
                if self.dpi == 0 {
                    return Err(ExecutionError::Configuration(
                        "PNG DPI must be at least 1.".into(),
                    ));
                }
                let mut args = vec![
                    "render".to_string(),
                    input,
                    "--page".to_string(),
                    page,
                    "--dpi".to_string(),
                    self.dpi.to_string(),
                    "--out".to_string(),
                    output("PNG output")?,
                ];
                password(&mut args);
                Ok(args)
            }
            "audit" => {
                let mut args = vec!["audit".to_string(), input];
                password(&mut args);
                Ok(args)
            }
            _ => Err(ExecutionError::Configuration(
                "Choose a PdfCraft workflow operation.".into(),
            )),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PdfAppSettings {
    executable: String,
    control_file: String,
    command: String,
    title: String,
    show_output: bool,
    timeout_seconds: u64,
}

impl Default for PdfAppSettings {
    fn default() -> Self {
        Self {
            executable: String::new(),
            control_file: String::new(),
            command: "highlight".to_string(),
            title: String::new(),
            show_output: false,
            timeout_seconds: 30,
        }
    }
}

impl PdfAppSettings {
    fn command_settings(&self) -> CommandSettings {
        CommandSettings {
            executable: self.executable.clone(),
            arguments: DEFAULT_ARGUMENTS.to_string(),
            working_directory: String::new(),
            title: self.title.clone(),
            show_output: self.show_output,
            timeout_seconds: self.timeout_seconds,
        }
    }

    fn arguments(&self) -> Result<Vec<String>, ExecutionError> {
        let control = required(&self.control_file, "PdfCraft control file")?;
        let mut args = vec!["ui".to_string(), "--control".to_string(), control];
        match self.command.as_str() {
            "previous_page" => args.extend(["key".to_string(), "key=ArrowLeft".to_string()]),
            "next_page" => args.extend(["key".to_string(), "key=ArrowRight".to_string()]),
            "read_mode" => args.extend(["command".to_string(), "id=view.read_mode".to_string()]),
            "organize" => args.extend(["command".to_string(), "id=page.organize".to_string()]),
            "rotate_cw" => args.extend(["command".to_string(), "id=page.rotate".to_string()]),
            "rotate_ccw" => args.extend(["command".to_string(), "id=page.rotate_ccw".to_string()]),
            "comments" => args.extend(["command".to_string(), "id=comment.list".to_string()]),
            "highlight" => args.extend(["command".to_string(), "id=comment.highlight".to_string()]),
            "rectangle" => args.extend(["command".to_string(), "id=comment.square".to_string()]),
            "redact" => args.extend(["command".to_string(), "id=redact.mark".to_string()]),
            // PdfCraft itself displays the irreversible-action confirmation.
            "apply_redactions" => {
                args.extend(["command".to_string(), "id=redact.apply".to_string()])
            }
            "save" => args.extend(["command".to_string(), "id=file.save".to_string()]),
            _ => {
                return Err(ExecutionError::Configuration(
                    "Choose a PdfCraft app command.".into(),
                ))
            }
        }
        Ok(args)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PdfRotarySettings {
    executable: String,
    control_file: String,
    mode: String,
    step: u8,
    press: String,
    title: String,
    timeout_seconds: u64,
}

impl Default for PdfRotarySettings {
    fn default() -> Self {
        Self {
            executable: String::new(),
            control_file: String::new(),
            mode: "zoom".to_string(),
            step: 1,
            press: "fit_width".to_string(),
            title: "PDF Zoom".to_string(),
            timeout_seconds: 30,
        }
    }
}

impl PdfRotarySettings {
    fn command_settings(&self) -> CommandSettings {
        CommandSettings {
            executable: self.executable.clone(),
            arguments: DEFAULT_ARGUMENTS.to_string(),
            working_directory: String::new(),
            title: self.title.clone(),
            show_output: false,
            timeout_seconds: self.timeout_seconds,
        }
    }

    fn control(&self) -> Result<String, ExecutionError> {
        required(&self.control_file, "PdfCraft control file")
    }

    fn tick_arguments(&self, ticks: i16) -> Result<Vec<String>, ExecutionError> {
        let control = self.control()?;
        let forward = ticks >= 0;
        let mut args = vec!["ui".to_string(), "--control".to_string(), control];
        match self.mode.as_str() {
            "zoom" => {
                args.extend([
                    "key".to_string(),
                    format!("key={}", if forward { "Plus" } else { "Minus" }),
                    "modifiers=[\"command\"]".to_string(),
                ]);
            }
            "page" => args.extend([
                "key".to_string(),
                format!("key={}", if forward { "ArrowRight" } else { "ArrowLeft" }),
            ]),
            "rotate" => args.extend([
                "command".to_string(),
                format!(
                    "id={}",
                    if forward {
                        "page.rotate"
                    } else {
                        "page.rotate_ccw"
                    }
                ),
            ]),
            _ => {
                return Err(ExecutionError::Configuration(
                    "Rotary mode must be zoom, page, or rotate.".into(),
                ))
            }
        }
        Ok(args)
    }

    fn press_arguments(&self) -> Result<Vec<String>, ExecutionError> {
        let control = self.control()?;
        let command = match self.press.as_str() {
            "fit_width" => "view.fit_width_scrolling",
            "fit_page" => "view.fit_one_page",
            "read_mode" => "view.read_mode",
            "organize" => "page.organize",
            "none" => return Ok(Vec::new()),
            _ => {
                return Err(ExecutionError::Configuration(
                    "Choose a rotary press action.".into(),
                ))
            }
        };
        Ok(vec![
            "ui".to_string(),
            "--control".to_string(),
            control,
            "command".to_string(),
            format!("id={command}"),
        ])
    }
}

fn catalog_items(stdout: &str) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let items = value
        .as_array()
        .cloned()
        .or_else(|| value.get("commands").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            if let Some(id) = item.as_str() {
                return Some(json!({"id": id, "label": id}));
            }
            let id = ["id", "name", "command", "tool"]
                .into_iter()
                .find_map(|key| item.get(key).and_then(Value::as_str))?;
            let label = item
                .get("label")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(id);
            Some(json!({"id": id, "label": label}))
        })
        .collect()
}

async fn send_catalog(instance: &Instance, settings: &CommandSettings) {
    let mut command = Command::new(settings.executable());
    command
        .args(CATALOG_ARGUMENTS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !settings.working_directory.trim().is_empty() {
        command.current_dir(PathBuf::from(settings.working_directory.trim()));
    }
    let result = tokio::time::timeout(Duration::from_secs(15), command.output()).await;
    let reply = match result {
        Ok(Ok(output)) if output.status.success() && output.stdout.len() <= MAX_CATALOG_BYTES => {
            let text = String::from_utf8_lossy(&output.stdout);
            let items = catalog_items(&text);
            if items.is_empty() {
                json!({"kind":"catalog", "ok":false, "message":"The installed CLI did not return a JSON command catalog."})
            } else {
                json!({"kind":"catalog", "ok":true, "items":items})
            }
        }
        Ok(Ok(output)) => {
            json!({"kind":"catalog", "ok":false, "message":format!("Could not read the command catalog: {}", summary(&output.stdout, &output.stderr))})
        }
        Ok(Err(error)) => {
            json!({"kind":"catalog", "ok":false, "message":format!("Could not start {}: {error}", settings.executable())})
        }
        Err(_) => {
            json!({"kind":"catalog", "ok":false, "message":"Timed out while loading the command catalog."})
        }
    };
    let _ = instance.send_to_property_inspector(reply).await;
}

/// PdfCraft publishes a per-launch, authenticated loopback endpoint through this private file.
/// It intentionally does not expose a fixed TCP port to callers.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PdfLaunchSettings {
    executable: String,
    control_file: String,
    working_directory: String,
    title: String,
}

impl Default for PdfLaunchSettings {
    fn default() -> Self {
        Self {
            executable: String::new(),
            control_file: String::new(),
            working_directory: String::new(),
            title: "Open PdfCraft".into(),
        }
    }
}

impl PdfLaunchSettings {
    fn executable(&self) -> &str {
        let value = self.executable.trim();
        if value.is_empty() {
            DEFAULT_DESKTOP_EXECUTABLE
        } else {
            value
        }
    }
    fn arguments(&self) -> Result<Vec<String>, ExecutionError> {
        let control_file = required(&self.control_file, "Private PdfCraft control file")?;
        if !PathBuf::from(&control_file).is_absolute() {
            return Err(ExecutionError::Configuration(
                "Use an absolute path for the private PdfCraft control file.".into(),
            ));
        }
        Ok(vec!["--control".into(), control_file])
    }
}

async fn launch_pdfcraft(settings: &PdfLaunchSettings) -> Result<String, ExecutionError> {
    let mut command = Command::new(settings.executable());
    command
        .args(settings.arguments()?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    if !settings.working_directory.trim().is_empty() {
        command.current_dir(PathBuf::from(settings.working_directory.trim()));
    }
    command.spawn().map_err(|error| {
        ExecutionError::Process(format!("Could not open {}: {error}", settings.executable()))
    })?;
    Ok("Starting PdfCraft with its private local control file.".into())
}

async fn report_pdf_launch(
    instance: &Instance,
    settings: &PdfLaunchSettings,
    result: Result<String, ExecutionError>,
    inspector: bool,
) {
    match result {
        Ok(message) => {
            let _ = instance.set_title(Some(settings.title.clone()), None).await;
            let _ = instance.show_ok().await;
            if inspector {
                let _ = instance
                    .send_to_property_inspector(
                        json!({"kind":"launch","ok":true,"message":message}),
                    )
                    .await;
            }
        }
        Err(error) => {
            let message = error.to_string();
            log::warn!("PdfCraft launcher failed: {message}");
            let _ = instance.set_title(Some("Error".to_string()), None).await;
            let _ = instance.show_alert().await;
            if inspector {
                let _ = instance
                    .send_to_property_inspector(
                        json!({"kind":"launch","ok":false,"message":message}),
                    )
                    .await;
            }
        }
    }
}

struct LaunchPdfCraft;
#[async_trait]
impl Action for LaunchPdfCraft {
    const UUID: &'static str = "wt.luizgustavo.getcraft.pdfcraft.launch";
    type Settings = PdfLaunchSettings;
    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        report_pdf_launch(instance, settings, launch_pdfcraft(settings).await, false).await;
        Ok(())
    }
    async fn will_appear(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let _ = instance.set_title(Some(settings.title.clone()), None).await;
        Ok(())
    }
    async fn send_to_plugin(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
        payload: &Value,
    ) -> OpenActionResult<()> {
        if payload.get("request").and_then(Value::as_str) == Some("launch") {
            let effective = payload
                .get("settings")
                .and_then(|value| serde_json::from_value::<PdfLaunchSettings>(value.clone()).ok())
                .unwrap_or_else(|| settings.clone());
            report_pdf_launch(
                instance,
                &effective,
                launch_pdfcraft(&effective).await,
                true,
            )
            .await;
        }
        Ok(())
    }
}

struct RunPdfCraftCli;

#[async_trait]
impl Action for RunPdfCraftCli {
    const UUID: &'static str = "wt.luizgustavo.getcraft.pdfcraft.run";
    type Settings = CommandSettings;

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        run_action(instance, settings, false).await;
        Ok(())
    }

    async fn dial_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.key_down(instance, settings).await
    }

    async fn will_appear(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        if let Some(title) = settings.title() {
            let _ = instance.set_title(Some(title), None).await;
        }
        Ok(())
    }

    async fn send_to_plugin(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
        payload: &Value,
    ) -> OpenActionResult<()> {
        let effective = payload
            .get("settings")
            .and_then(|value| serde_json::from_value::<CommandSettings>(value.clone()).ok())
            .unwrap_or_else(|| settings.clone());
        match payload.get("request").and_then(Value::as_str) {
            Some("catalog") => send_catalog(instance, &effective).await,
            Some("test") => run_action(instance, &effective, true).await,
            _ => {}
        }
        Ok(())
    }
}

struct PdfWorkflow;

#[async_trait]
impl Action for PdfWorkflow {
    const UUID: &'static str = "wt.luizgustavo.getcraft.pdfcraft.workflow";
    type Settings = PdfWorkflowSettings;

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let command = settings.command_settings();
        run_arguments(instance, &command, settings.arguments(), false).await;
        Ok(())
    }

    async fn dial_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.key_down(instance, settings).await
    }

    async fn will_appear(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let title = if settings.title.trim().is_empty() {
            "PDF Workflow"
        } else {
            settings.title.trim()
        };
        let _ = instance.set_title(Some(title.to_string()), None).await;
        Ok(())
    }
}

struct PdfAppCommand;

#[async_trait]
impl Action for PdfAppCommand {
    const UUID: &'static str = "wt.luizgustavo.getcraft.pdfcraft.appcommand";
    type Settings = PdfAppSettings;

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let command = settings.command_settings();
        run_arguments(instance, &command, settings.arguments(), false).await;
        Ok(())
    }

    async fn dial_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.key_down(instance, settings).await
    }

    async fn will_appear(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let title = if settings.title.trim().is_empty() {
            "PDF Tool"
        } else {
            settings.title.trim()
        };
        let _ = instance.set_title(Some(title.to_string()), None).await;
        Ok(())
    }
}

struct PdfRotary;

#[async_trait]
impl Action for PdfRotary {
    const UUID: &'static str = "wt.luizgustavo.getcraft.pdfcraft.rotary";
    type Settings = PdfRotarySettings;

    async fn dial_rotate(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        if ticks == 0 {
            return Ok(());
        }
        // A physical encoder can coalesce turns. Bound the burst so a stuck device cannot flood
        // the app, while preserving the direction and the configured sensitivity.
        let count = usize::from(ticks.unsigned_abs())
            .saturating_mul(usize::from(settings.step.max(1)))
            .min(32);
        let command = settings.command_settings();
        for _ in 0..count {
            run_arguments(instance, &command, settings.tick_arguments(ticks), false).await;
        }
        Ok(())
    }

    async fn dial_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let args = settings.press_arguments();
        if matches!(args, Ok(ref values) if values.is_empty()) {
            return Ok(());
        }
        let command = settings.command_settings();
        run_arguments(instance, &command, args, false).await;
        Ok(())
    }

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.dial_down(instance, settings).await
    }

    async fn will_appear(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let _ = instance.set_title(Some(settings.title.clone()), None).await;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> OpenActionResult<()> {
    let _ = TermLogger::init(
        LevelFilter::Info,
        Config::default(),
        TerminalMode::Stdout,
        ColorChoice::Never,
    );
    register_action(LaunchPdfCraft).await;
    register_action(RunPdfCraftCli).await;
    register_action(PdfWorkflow).await;
    register_action(PdfAppCommand).await;
    register_action(PdfRotary).await;
    run(std::env::args().collect()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_uses_the_private_control_file() {
        let settings = PdfLaunchSettings {
            control_file: "/tmp/pdfcraft-control.json".into(),
            ..Default::default()
        };
        assert_eq!(
            settings.arguments().unwrap(),
            ["--control", "/tmp/pdfcraft-control.json"]
        );
    }

    #[test]
    fn accepts_a_string_argument_vector() {
        assert_eq!(
            parse_arguments(r#"["exec","timeline.play","{}"]"#).unwrap(),
            ["exec", "timeline.play", "{}"]
        );
    }

    #[test]
    fn rejects_non_string_arguments() {
        assert!(parse_arguments(r#"["exec", 4]"#).is_err());
    }

    #[test]
    fn stderr_is_preferred_in_a_failure_summary() {
        assert_eq!(summary(b"normal", b"bad\nnews"), "bad news");
    }

    #[test]
    fn has_a_usable_default_command() {
        assert_eq!(
            parse_arguments(&CommandSettings::default().arguments).unwrap(),
            CATALOG_ARGUMENTS
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn honors_a_one_second_timeout() {
        assert_eq!(
            CommandSettings {
                timeout_seconds: 1,
                ..Default::default()
            }
            .timeout(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn builds_an_extract_workflow_without_a_shell() {
        let settings = PdfWorkflowSettings {
            input: "in.pdf".into(),
            output: "out.pdf".into(),
            pages: "1,3".into(),
            ..Default::default()
        };
        assert_eq!(
            settings.arguments().unwrap(),
            ["extract", "in.pdf", "--pages", "1,3", "--out", "out.pdf"]
        );
    }

    #[test]
    fn refuses_a_destructive_workflow_without_a_new_output_path() {
        let settings = PdfWorkflowSettings {
            input: "in.pdf".into(),
            ..Default::default()
        };
        assert!(settings.arguments().is_err());
    }

    #[test]
    fn rotary_page_control_uses_real_ui_key_events() {
        let settings = PdfRotarySettings {
            control_file: "/home/me/.pdfcraft-control.json".into(),
            mode: "page".into(),
            ..Default::default()
        };
        assert_eq!(
            settings.tick_arguments(-1).unwrap(),
            [
                "ui",
                "--control",
                "/home/me/.pdfcraft-control.json",
                "key",
                "key=ArrowLeft"
            ]
        );
    }
}
