use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::io::AsyncReadExt;

use super::{
    state::{TabBarCommandOutput, TabBarStatusSegment, TabBarStyledSpan},
    App,
};
use crate::config::TabBarRightEntryConfig;

const DATETIME_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_COMMAND_LINE_BYTES: usize = 4096;
const MAX_STATUS_TEXT_CHARS: usize = 80;

pub(super) struct TabBarDatetimeRuntime {
    segment_index: usize,
    format: time::format_description::OwnedFormatItem,
}

/// How a status command is launched: through the platform shell, or directly
/// as a program with arguments.
#[derive(Clone)]
pub(super) enum StatusCommandSpec {
    Shell(String),
    Argv(Vec<String>),
}

impl StatusCommandSpec {
    fn describe(&self) -> String {
        match self {
            Self::Shell(command) => command.clone(),
            Self::Argv(argv) => argv.join(" "),
        }
    }
}

pub(super) struct TabBarCommandRuntime {
    segment_index: usize,
    spec: StatusCommandSpec,
    ansi: bool,
    interval: Duration,
    timeout: Duration,
    next_run_at: std::time::Instant,
    task: Option<StatusCommandTask>,
    /// A metadata token change arrived while a run was in flight; run again
    /// as soon as that run finishes.
    rerun_when_finished: bool,
}

impl Drop for TabBarCommandRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort_handle.abort();
            // Kill the process group on the reconfiguring thread instead of waiting
            // for Tokio to schedule cancellation of the command task.
            task.control.terminate();
        }
    }
}

impl App {
    pub(super) fn configure_tab_bar_status(
        &mut self,
        entries: &[TabBarRightEntryConfig],
        separator: &str,
    ) {
        self.tab_bar_status_generation = self.tab_bar_status_generation.wrapping_add(1);
        self.tab_bar_datetimes.clear();
        self.tab_bar_commands.clear();
        self.state.tab_bar_right.clear();
        self.state.tab_bar_right_separator = sanitize_separator(separator);

        let now = std::time::Instant::now();
        for entry in entries
            .iter()
            .take(crate::config::MAX_TAB_BAR_RIGHT_ENTRIES)
        {
            match entry {
                TabBarRightEntryConfig::Zoom => {
                    self.state.tab_bar_right.push(TabBarStatusSegment::Zoom);
                }
                TabBarRightEntryConfig::Hostname => {
                    self.state
                        .tab_bar_right
                        .push(TabBarStatusSegment::Text(sanitize_status_text(
                            crate::platform::hostname().as_deref().unwrap_or_default(),
                        )));
                }
                TabBarRightEntryConfig::Datetime { format } => {
                    let Ok(format) = crate::config::parse_tab_bar_datetime_format(format) else {
                        continue;
                    };
                    let value = format_local_datetime(&format);
                    let segment_index = self.state.tab_bar_right.len();
                    self.state
                        .tab_bar_right
                        .push(TabBarStatusSegment::Text(value));
                    self.tab_bar_datetimes.push(TabBarDatetimeRuntime {
                        segment_index,
                        format,
                    });
                }
                TabBarRightEntryConfig::Text { text } => {
                    self.state
                        .tab_bar_right
                        .push(TabBarStatusSegment::Text(sanitize_literal_text(text)));
                }
                TabBarRightEntryConfig::Command {
                    command,
                    argv,
                    ansi,
                    interval_seconds,
                    timeout_seconds,
                } => {
                    if !crate::platform::status_commands_supported()
                        || crate::config::command_entry_source_diagnostic(command, argv).is_some()
                        || *interval_seconds == 0
                        || *interval_seconds > crate::config::MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS
                        || *timeout_seconds == 0
                        || *timeout_seconds > crate::config::MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS
                    {
                        continue;
                    }
                    let spec = if argv.is_empty() {
                        StatusCommandSpec::Shell(command.clone())
                    } else {
                        StatusCommandSpec::Argv(argv.clone())
                    };
                    let segment_index = self.state.tab_bar_right.len();
                    self.state.tab_bar_right.push(if *ansi {
                        TabBarStatusSegment::Styled(None)
                    } else {
                        TabBarStatusSegment::Text(None)
                    });
                    self.tab_bar_commands.push(TabBarCommandRuntime {
                        segment_index,
                        spec,
                        ansi: *ansi,
                        interval: Duration::from_secs(*interval_seconds),
                        timeout: Duration::from_secs(*timeout_seconds),
                        next_run_at: now,
                        task: None,
                        rerun_when_finished: false,
                    });
                }
            }
        }

        self.tab_bar_token_fingerprint = None;
        self.next_tab_bar_datetime_refresh =
            (!self.tab_bar_datetimes.is_empty()).then_some(now + DATETIME_REFRESH_INTERVAL);
    }

    pub(crate) fn handle_tab_bar_status_tasks(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;

        if self
            .next_tab_bar_datetime_refresh
            .is_some_and(|deadline| now >= deadline)
        {
            for runtime in &self.tab_bar_datetimes {
                let value = format_local_datetime(&runtime.format);
                if let Some(TabBarStatusSegment::Text(current)) =
                    self.state.tab_bar_right.get_mut(runtime.segment_index)
                {
                    changed |= *current != value;
                    *current = value;
                }
            }
            self.next_tab_bar_datetime_refresh = Some(now + DATETIME_REFRESH_INTERVAL);
        }

        if !self.tab_bar_commands.is_empty() {
            let fingerprint = self
                .state
                .active
                .and_then(|index| self.state.workspaces.get(index))
                .map_or(0, |workspace| workspace.metadata_tokens.value_fingerprint());
            if self.tab_bar_token_fingerprint != Some(fingerprint) {
                // Skip the forced re-run on the first tick after a reconfigure:
                // fresh runtimes are already due immediately.
                let tokens_changed = self.tab_bar_token_fingerprint.is_some();
                self.tab_bar_token_fingerprint = Some(fingerprint);
                if tokens_changed {
                    for runtime in &mut self.tab_bar_commands {
                        if runtime.task.is_some() {
                            runtime.rerun_when_finished = true;
                        } else {
                            runtime.next_run_at = now;
                        }
                    }
                }
            }
        }

        let command_due = self
            .tab_bar_commands
            .iter()
            .any(|runtime| runtime.task.is_none() && now >= runtime.next_run_at);
        if !command_due {
            return changed;
        }

        let generation = self.tab_bar_status_generation;
        let (mut environment, cwd) = self.custom_command_env();
        environment.extend(focused_workspace_token_env(&self.state));
        for runtime in &mut self.tab_bar_commands {
            if runtime.task.is_some() || now < runtime.next_run_at {
                continue;
            }
            runtime.next_run_at = now.checked_add(runtime.interval).unwrap_or(now);
            runtime.task = Some(spawn_status_command(
                self.event_tx.clone(),
                generation,
                runtime.segment_index,
                runtime.spec.clone(),
                runtime.ansi,
                runtime.timeout,
                environment.clone(),
                cwd.clone(),
            ));
        }

        changed
    }

    pub(crate) fn next_tab_bar_status_deadline(&self) -> Option<std::time::Instant> {
        self.tab_bar_commands
            .iter()
            .filter(|runtime| runtime.task.is_none())
            .map(|runtime| runtime.next_run_at)
            .chain(self.next_tab_bar_datetime_refresh)
            .min()
    }

    pub(super) fn handle_tab_bar_command_finished(
        &mut self,
        generation: u64,
        segment_index: usize,
        result: Result<TabBarCommandOutput, String>,
    ) -> bool {
        if generation != self.tab_bar_status_generation {
            return false;
        }
        let Some(runtime) = self
            .tab_bar_commands
            .iter_mut()
            .find(|runtime| runtime.segment_index == segment_index)
        else {
            return false;
        };
        runtime.task = None;
        if runtime.rerun_when_finished {
            runtime.rerun_when_finished = false;
            runtime.next_run_at = std::time::Instant::now();
        }

        let output = match result {
            Ok(output) => Some(output),
            Err(error) => {
                let command = runtime.spec.describe();
                tracing::warn!(command = %command, error, "tab bar status command failed");
                None
            }
        };
        match self.state.tab_bar_right.get_mut(segment_index) {
            Some(TabBarStatusSegment::Text(current)) => {
                let output = match output {
                    Some(TabBarCommandOutput::Text(text)) => text,
                    _ => None,
                };
                let changed = *current != output;
                *current = output;
                changed
            }
            Some(TabBarStatusSegment::Styled(current)) => {
                let output = match output {
                    Some(TabBarCommandOutput::Styled(spans)) => spans,
                    _ => None,
                };
                let changed = *current != output;
                *current = output;
                changed
            }
            _ => false,
        }
    }
}

/// Exposes the focused workspace's metadata tokens (reported via
/// `workspace report-metadata`) to status commands as `HERDR_TOKEN_<NAME>`
/// environment variables. Token keys are ASCII alphanumeric plus `_`/`-`;
/// names are uppercased and `-` becomes `_`.
fn focused_workspace_token_env(state: &super::AppState) -> Vec<(String, String)> {
    let Some(workspace) = state.active.and_then(|index| state.workspaces.get(index)) else {
        return Vec::new();
    };
    workspace
        .metadata_tokens
        .values()
        .into_iter()
        .map(|(key, value)| {
            let name: String = key
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            (format!("HERDR_TOKEN_{name}"), value)
        })
        .collect()
}

fn format_local_datetime(format: &time::format_description::OwnedFormatItem) -> Option<String> {
    let datetime = crate::platform::local_datetime()?;
    datetime
        .format(format)
        .ok()
        .and_then(|value| sanitize_status_text(&value))
}

fn sanitize_separator(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn sanitize_literal_text(value: &str) -> Option<String> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    (!value.is_empty()).then_some(value)
}

fn sanitize_status_text(value: &str) -> Option<String> {
    let value: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control() && !is_unicode_format_control(*character))
        .take(MAX_STATUS_TEXT_CHARS)
        .collect();
    (!value.is_empty()).then_some(value)
}

fn is_unicode_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

fn command_output_text(output: &[u8]) -> Option<String> {
    let output = String::from_utf8_lossy(output);
    let output = strip_terminal_control_sequences(output.as_bytes());
    let output = String::from_utf8_lossy(&output);
    output.lines().next_back().and_then(sanitize_status_text)
}

#[derive(Clone, Copy)]
enum ControlSequenceState {
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    StString,
}

fn strip_terminal_control_sequences(value: &[u8]) -> Vec<u8> {
    use ControlSequenceState::*;

    let mut output = Vec::with_capacity(value.len());
    let mut state = Text;
    for &byte in value {
        state = match (state, byte) {
            (Text, b'\x1b') => Escape,
            (Text, _) => {
                output.push(byte);
                Text
            }
            (Escape, b'[') => Csi,
            (Escape, b']') => Osc,
            (Escape, b'P' | b'X' | b'^' | b'_') => StString,
            (Escape, 0x20..=0x2f) => EscapeIntermediate,
            (Escape, 0x30..=0x7e) => Text,
            (Escape, b'\x1b') => Escape,
            (Escape, b'\x18' | b'\x1a') => Text,
            (Escape, byte) if byte.is_ascii_control() => Escape,
            (Escape, _) => {
                output.push(byte);
                Text
            }
            (EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (EscapeIntermediate, 0x30..=0x7e) => Text,
            (EscapeIntermediate, b'\x1b') => Escape,
            (EscapeIntermediate, b'\x18' | b'\x1a') => Text,
            (EscapeIntermediate, byte) if byte.is_ascii_control() => EscapeIntermediate,
            (EscapeIntermediate, _) => {
                output.push(byte);
                Text
            }
            (Csi, 0x20..=0x3f) => Csi,
            (Csi, 0x40..=0x7e) => Text,
            (Csi, b'\x1b') => Escape,
            (Csi, b'\x18' | b'\x1a') => Text,
            (Csi, byte) if byte.is_ascii_control() => Csi,
            (Csi, _) => {
                output.push(byte);
                Text
            }
            (Osc, b'\x07') => Text,
            (Osc, b'\x1b') => Escape,
            (Osc, b'\x18' | b'\x1a') => Text,
            (Osc, _) => Osc,
            (StString, b'\x1b') => Escape,
            (StString, b'\x18' | b'\x1a') => Text,
            (StString, _) => StString,
        };
    }
    output
}

/// Parses the last output line into SGR-styled spans for an `ansi = true`
/// command entry. Only SGR (`CSI ... m`) sequences affect styling; every
/// other escape sequence is dropped, and the text itself passes through the
/// same character sanitization and length cap as plain status text.
fn styled_command_output(output: &[u8]) -> Option<Vec<TabBarStyledSpan>> {
    use ratatui::style::Style;
    use ControlSequenceState::*;

    let mut raw_spans: Vec<(Vec<u8>, Style)> = Vec::new();
    let mut current_text: Vec<u8> = Vec::new();
    let mut current_style = Style::default();
    let mut params: Vec<u8> = Vec::new();
    let mut state = Text;
    let flush = |text: &mut Vec<u8>, style: Style, spans: &mut Vec<(Vec<u8>, Style)>| {
        if !text.is_empty() {
            spans.push((std::mem::take(text), style));
        }
    };
    for &byte in output {
        state = match (state, byte) {
            (Text, b'\x1b') => Escape,
            (Text, _) => {
                current_text.push(byte);
                Text
            }
            (Escape, b'[') => {
                params.clear();
                Csi
            }
            (Escape, b']') => Osc,
            (Escape, b'P' | b'X' | b'^' | b'_') => StString,
            (Escape, 0x20..=0x2f) => EscapeIntermediate,
            (Escape, 0x30..=0x7e) => Text,
            (Escape, b'\x1b') => Escape,
            (Escape, b'\x18' | b'\x1a') => Text,
            (Escape, byte) if byte.is_ascii_control() => Escape,
            (Escape, _) => {
                current_text.push(byte);
                Text
            }
            (Csi, 0x20..=0x3f) => {
                if params.len() < 64 {
                    params.push(byte);
                }
                Csi
            }
            (Csi, b'm') => {
                let next_style = apply_sgr_params(current_style, &params);
                if next_style != current_style {
                    flush(&mut current_text, current_style, &mut raw_spans);
                    current_style = next_style;
                }
                Text
            }
            (Csi, 0x40..=0x7e) => Text,
            (Csi, b'\x1b') => Escape,
            (Csi, b'\x18' | b'\x1a') => Text,
            (Csi, byte) if byte.is_ascii_control() => Csi,
            (Csi, _) => {
                current_text.push(byte);
                Text
            }
            (EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (EscapeIntermediate, 0x30..=0x7e) => Text,
            (EscapeIntermediate, b'\x1b') => Escape,
            (EscapeIntermediate, b'\x18' | b'\x1a') => Text,
            (EscapeIntermediate, byte) if byte.is_ascii_control() => EscapeIntermediate,
            (EscapeIntermediate, _) => {
                current_text.push(byte);
                Text
            }
            (Osc, b'\x07') => Text,
            (Osc, b'\x1b') => Escape,
            (Osc, b'\x18' | b'\x1a') => Text,
            (Osc, _) => Osc,
            (StString, b'\x1b') => Escape,
            (StString, b'\x18' | b'\x1a') => Text,
            (StString, _) => StString,
        };
    }
    flush(&mut current_text, current_style, &mut raw_spans);

    let mut remaining = MAX_STATUS_TEXT_CHARS;
    let mut spans: Vec<TabBarStyledSpan> = Vec::new();
    for (bytes, style) in raw_spans {
        if remaining == 0 {
            break;
        }
        let text: String = String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|ch| !ch.is_control() && !is_unicode_format_control(*ch))
            .take(remaining)
            .collect();
        remaining -= text.chars().count();
        if !text.is_empty() {
            spans.push(TabBarStyledSpan { text, style });
        }
    }

    // Trim outer whitespace across span boundaries, matching plain status
    // text, without disturbing interior spacing.
    while let Some(first) = spans.first_mut() {
        first.text = first.text.trim_start().to_string();
        if first.text.is_empty() {
            spans.remove(0);
        } else {
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_string();
        if last.text.is_empty() {
            spans.pop();
        } else {
            break;
        }
    }
    (!spans.is_empty()).then_some(spans)
}

fn apply_sgr_params(mut style: ratatui::style::Style, raw: &[u8]) -> ratatui::style::Style {
    use ratatui::style::{Modifier, Style};

    // Private-mode or otherwise non-numeric parameter bytes: not SGR we
    // understand; leave the style untouched.
    if !raw
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b';' | b':'))
    {
        return style;
    }
    let codes: Vec<u16> = raw
        .split(|byte| matches!(byte, b';' | b':'))
        .map(|part| {
            if part.is_empty() {
                0
            } else {
                std::str::from_utf8(part)
                    .ok()
                    .and_then(|part| part.parse().ok())
                    .unwrap_or(u16::MAX)
            }
        })
        .collect();
    let codes = if codes.is_empty() { vec![0] } else { codes };

    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            5 => style = style.add_modifier(Modifier::SLOW_BLINK),
            6 => style = style.add_modifier(Modifier::RAPID_BLINK),
            7 => style = style.add_modifier(Modifier::REVERSED),
            8 => style = style.add_modifier(Modifier::HIDDEN),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            25 => style = style.remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            28 => style = style.remove_modifier(Modifier::HIDDEN),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            code @ 30..=37 => style.fg = Some(indexed_color(code - 30)),
            38 => match parse_extended_color(&codes, &mut index) {
                Some(color) => style.fg = Some(color),
                None => break,
            },
            39 => style.fg = None,
            code @ 40..=47 => style.bg = Some(indexed_color(code - 40)),
            48 => match parse_extended_color(&codes, &mut index) {
                Some(color) => style.bg = Some(color),
                None => break,
            },
            49 => style.bg = None,
            code @ 90..=97 => style.fg = Some(indexed_color(code - 90 + 8)),
            code @ 100..=107 => style.bg = Some(indexed_color(code - 100 + 8)),
            _ => {}
        }
        index += 1;
    }
    style
}

fn indexed_color(index: u16) -> ratatui::style::Color {
    ratatui::style::Color::Indexed(u8::try_from(index).unwrap_or(u8::MAX))
}

/// Parses the `5;n` / `2;r;g;b` tail of an SGR 38/48 parameter, advancing
/// `index` past the consumed values. `None` means the tail is malformed and
/// the caller should stop interpreting this sequence.
fn parse_extended_color(codes: &[u16], index: &mut usize) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    match codes.get(*index + 1)? {
        5 => {
            let value = *codes.get(*index + 2)?;
            *index += 2;
            u8::try_from(value).ok().map(Color::Indexed)
        }
        2 => {
            let red = u8::try_from(*codes.get(*index + 2)?).ok()?;
            let green = u8::try_from(*codes.get(*index + 3)?).ok()?;
            let blue = u8::try_from(*codes.get(*index + 4)?).ok()?;
            *index += 4;
            Some(Color::Rgb(red, green, blue))
        }
        _ => None,
    }
}

async fn read_last_output_line(
    mut stdout: tokio::process::ChildStdout,
) -> std::io::Result<Vec<u8>> {
    let mut current_line = Vec::new();
    let mut last_line = Vec::new();
    let mut ended_with_newline = false;
    let mut buffer = [0_u8; 1024];

    loop {
        let count = stdout.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                last_line = std::mem::take(&mut current_line);
                ended_with_newline = true;
            } else {
                if current_line.len() < MAX_COMMAND_LINE_BYTES {
                    current_line.push(byte);
                }
                ended_with_newline = false;
            }
        }
    }

    Ok(if ended_with_newline {
        last_line
    } else {
        current_line
    })
}

struct StatusCommandTask {
    abort_handle: tokio::task::AbortHandle,
    control: Arc<StatusCommandControl>,
}

struct StatusCommandControl {
    terminated: AtomicBool,
    process_group: Mutex<Option<crate::platform::StatusCommandGuard>>,
}

impl StatusCommandControl {
    fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
        if let Some(mut process_group) = self
            .process_group
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            process_group.terminate();
        }
    }

    fn register(&self, mut process_group: crate::platform::StatusCommandGuard) {
        let mut registered = self
            .process_group
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_terminated() {
            process_group.terminate();
        } else {
            *registered = Some(process_group);
        }
    }
}

fn spawn_status_command(
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    generation: u64,
    segment_index: usize,
    spec: StatusCommandSpec,
    ansi: bool,
    timeout: Duration,
    environment: Vec<(String, String)>,
    cwd: Option<std::path::PathBuf>,
) -> StatusCommandTask {
    let control = Arc::new(StatusCommandControl {
        terminated: AtomicBool::new(false),
        process_group: Mutex::new(None),
    });
    let task_control = Arc::clone(&control);
    let deadline = tokio::time::Instant::now() + timeout;
    let task = tokio::spawn(async move {
        let result = run_status_command(
            task_control.as_ref(),
            spec,
            ansi,
            timeout,
            deadline,
            environment,
            cwd,
        )
        .await;
        task_control.terminate();
        let _ = event_tx
            .send(crate::events::AppEvent::TabBarCommandFinished {
                generation,
                segment_index,
                result,
            })
            .await;
    });
    StatusCommandTask {
        abort_handle: task.abort_handle(),
        control,
    }
}

async fn run_status_command(
    control: &StatusCommandControl,
    spec: StatusCommandSpec,
    ansi: bool,
    timeout: Duration,
    deadline: tokio::time::Instant,
    environment: Vec<(String, String)>,
    cwd: Option<std::path::PathBuf>,
) -> Result<TabBarCommandOutput, String> {
    if control.is_terminated() || tokio::time::Instant::now() >= deadline {
        return Err(format!("timed out after {}s", timeout.as_secs()));
    }

    let mut process = match &spec {
        StatusCommandSpec::Shell(command) => {
            crate::platform::detached_custom_command_process(command)
        }
        StatusCommandSpec::Argv(argv) => crate::platform::detached_argv_command_process(argv),
    };
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(environment);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    crate::platform::configure_status_command(&mut process);

    let mut process = tokio::process::Command::from(process);
    process.kill_on_drop(true);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let process_group =
        crate::platform::StatusCommandGuard::new(&child).map_err(|error| error.to_string())?;
    control.register(process_group);
    if control.is_terminated() {
        return Err("status command was cancelled".into());
    }

    let operation = async {
        let stdout = child.stdout.take();
        let read_output = async {
            let Some(stdout) = stdout else {
                return std::io::Result::Ok(Vec::new());
            };
            read_last_output_line(stdout).await
        };
        let (status, output) = tokio::join!(child.wait(), read_output);
        let status = status.map_err(|error| error.to_string())?;
        let output = output.map_err(|error| error.to_string())?;
        if status.success() {
            Ok(if ansi {
                TabBarCommandOutput::Styled(styled_command_output(&output))
            } else {
                TabBarCommandOutput::Text(command_output_text(&output))
            })
        } else {
            Err(format!("exited with {status}"))
        }
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, events::AppEvent};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[cfg(unix)]
    const MULTILINE_COMMAND: &str = "printf 'old\\nfinal\\n'";
    #[cfg(windows)]
    const MULTILINE_COMMAND: &str = "echo old & echo final";

    #[cfg(unix)]
    const OVER_CAP_COMMAND: &str = "head -c 5000 /dev/zero | tr '\\0' x; printf '\\nREADY\\n'";

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        std::path::PathBuf::from("/var/tmp").join(format!(
            "herdr-tab-status-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn status_command_reports_its_sanitized_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            3,
            StatusCommandSpec::Shell(MULTILINE_COMMAND.into()),
            false,
            Duration::from_secs(2),
            Vec::new(),
            None,
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                generation: 7,
                segment_index: 3,
                result: Ok(TabBarCommandOutput::Text(Some(ref output))),
            } if output == "final"
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "current_thread")]
    async fn status_command_timeout_starts_before_task_is_polled() {
        let ran = unique_temp_path("ran-after-timeout");
        let command = format!("printf ran > {}", ran.display());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            3,
            StatusCommandSpec::Shell(command),
            false,
            Duration::from_secs(1),
            Vec::new(),
            None,
        );

        std::thread::sleep(Duration::from_millis(1100));
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        let command_ran = ran.exists();
        let _ = std::fs::remove_file(ran);
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Err(ref error),
                ..
            } if error == "timed out after 1s"
        ));
        assert!(!command_ran, "status command ran after its deadline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_command_drains_large_output_and_keeps_the_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            7,
            3,
            StatusCommandSpec::Shell(OVER_CAP_COMMAND.into()),
            false,
            Duration::from_secs(2),
            Vec::new(),
            None,
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Ok(TabBarCommandOutput::Text(Some(ref output))),
                ..
            } if output == "READY"
        ));
    }

    #[test]
    fn stale_command_result_does_not_replace_reloaded_status() {
        let mut app = test_app();
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Command {
                command: MULTILINE_COMMAND.into(),
                argv: Vec::new(),
                ansi: false,
                interval_seconds: 5,
                timeout_seconds: 2,
            }],
            " ",
        );
        let stale_generation = app.tab_bar_status_generation;
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Text {
                text: "fresh".into(),
            }],
            " ",
        );

        app.handle_tab_bar_command_finished(
            stale_generation,
            0,
            Ok(TabBarCommandOutput::Text(Some("stale".into()))),
        );

        assert_eq!(
            app.state.tab_bar_right,
            vec![TabBarStatusSegment::Text(Some("fresh".into()))]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "current_thread")]
    async fn reload_aborts_an_in_flight_command_task_and_its_descendants() {
        let descendant_started = unique_temp_path("descendant-started");
        let survived = unique_temp_path("survived");
        let command = format!(
            "(printf descendant-started > {}; sleep 0.3; printf survived > {}) & wait",
            descendant_started.display(),
            survived.display()
        );
        let mut app = test_app();
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Command {
                command,
                argv: Vec::new(),
                ansi: false,
                interval_seconds: 5,
                timeout_seconds: 20,
            }],
            " ",
        );
        app.handle_tab_bar_status_tasks(std::time::Instant::now());
        for _ in 0..50 {
            if descendant_started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            descendant_started.exists(),
            "status command descendant did not start"
        );

        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Text {
                text: "reloaded".into(),
            }],
            " ",
        );

        // Task cancellation is delivered when Tokio next polls the task. Block
        // this current-thread test runtime long enough for the descendant to
        // run, proving config reload kills its process group synchronously.
        std::thread::sleep(Duration::from_millis(400));
        let descendant_survived = survived.exists();
        let _ = std::fs::remove_file(&descendant_started);
        let _ = std::fs::remove_file(&survived);
        assert!(!descendant_survived, "status command descendant survived");

        assert!(
            tokio::time::timeout(Duration::from_millis(100), app.event_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn in_flight_command_has_no_second_deadline() {
        let mut app = test_app();
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Command {
                command: MULTILINE_COMMAND.into(),
                argv: Vec::new(),
                ansi: false,
                interval_seconds: 5,
                timeout_seconds: 2,
            }],
            " ",
        );

        let now = std::time::Instant::now();
        assert!(app.next_tab_bar_status_deadline().is_some());
        app.handle_tab_bar_status_tasks(now);

        assert!(app.tab_bar_commands[0].task.is_some());
        assert_eq!(app.next_tab_bar_status_deadline(), None);
    }

    #[test]
    fn datetime_refresh_updates_its_segment_once_per_deadline() {
        let mut app = test_app();
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Datetime {
                format: "%Y-%m-%d %H:%M:%S".into(),
            }],
            " ",
        );
        app.state.tab_bar_right[0] = TabBarStatusSegment::Text(None);
        let deadline = app
            .next_tab_bar_datetime_refresh
            .expect("datetime refresh deadline");

        assert!(app.handle_tab_bar_status_tasks(deadline));
        assert!(matches!(
            &app.state.tab_bar_right[0],
            TabBarStatusSegment::Text(Some(value)) if !value.is_empty()
        ));
        assert!(!app.handle_tab_bar_status_tasks(deadline));
    }

    #[test]
    fn command_output_uses_sanitized_last_line() {
        assert_eq!(
            command_output_text(b"old\n win\x1b[31mter\r\n"),
            Some("winter".into())
        );
        assert_eq!(command_output_text(b"\r\n"), None);
    }

    #[test]
    fn command_output_strips_ansi_style_sequences() {
        assert_eq!(
            command_output_text(b"\x1b[32mHELLO\x1b[0m"),
            Some("HELLO".into())
        );
    }

    #[test]
    fn command_output_strips_terminal_control_sequence_families() {
        assert_eq!(
            command_output_text(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
            Some("link".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x1b\\visible\x1b7"),
            Some("visible".into())
        );
        assert_eq!(command_output_text(b"\x1b[31m\x1b[0m"), None);
        assert_eq!(
            command_output_text(b"\x1b\x07[32mHELLO\x1b[0m"),
            Some("HELLO".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x18VISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x1b7VISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(
            command_output_text(b"\x1b]ignored\x1aVISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(command_output_text(b"\xc2\x1b[31m\xa2"), Some("��".into()));

        let styled = format!("\x1b[38;2;1;2;3m{}\x1b[0m", "x".repeat(80));
        assert_eq!(command_output_text(styled.as_bytes()), Some("x".repeat(80)));
    }

    #[test]
    fn status_text_strips_bidi_and_zero_width_format_controls() {
        assert_eq!(
            sanitize_status_text("safe\u{202e}evil\u{200b}"),
            Some("safeevil".into())
        );
    }

    fn token_patch(items: &[(&str, &str)]) -> std::collections::HashMap<String, Option<String>> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_string(), Some((*value).to_string())))
            .collect()
    }

    #[test]
    fn styled_output_parses_sgr_colors_and_resets() {
        use ratatui::style::{Color, Modifier, Style};

        let spans = styled_command_output(b"\x1b[32mOK\x1b[0m plain \x1b[1;38;2;1;2;3mhot")
            .expect("styled spans");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "OK");
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(2)));
        assert_eq!(spans[1].text, " plain ");
        assert_eq!(spans[1].style, Style::default());
        assert_eq!(spans[2].text, "hot");
        assert_eq!(spans[2].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn styled_output_strips_non_sgr_sequences_and_hides_blank_output() {
        use ratatui::style::Color;

        let spans = styled_command_output(
            b"\x1b]8;;https://example.com\x1b\\\x1b[91mred\x1b]8;;\x1b\\\x1b[2Kx",
        )
        .expect("styled spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "redx");
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(9)));

        assert_eq!(styled_command_output(b"\x1b[31m   \x1b[0m"), None);
        assert_eq!(styled_command_output(b""), None);
    }

    #[test]
    fn styled_output_caps_length_and_survives_malformed_sgr() {
        let long = format!("\x1b[36m{}", "x".repeat(200));
        let spans = styled_command_output(long.as_bytes()).expect("styled spans");
        let total: usize = spans.iter().map(|span| span.text.chars().count()).sum();
        assert_eq!(total, MAX_STATUS_TEXT_CHARS);

        // A malformed extended color stops interpreting that sequence but
        // keeps the text.
        let spans = styled_command_output(b"\x1b[38;5mX").expect("styled spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "X");
        assert_eq!(spans[0].style.fg, None);
    }

    #[test]
    fn focused_workspace_tokens_become_env_vars() {
        let mut app = test_app();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("one"));
        app.state.active = Some(0);
        app.state.workspaces[0].metadata_tokens.patch(
            token_patch(&[("cpu-load", "42%"), ("MEM_x", "ok")]),
            None,
            std::time::Instant::now(),
        );

        let mut env = focused_workspace_token_env(&app.state);
        env.sort();
        assert_eq!(
            env,
            vec![
                ("HERDR_TOKEN_CPU_LOAD".to_string(), "42%".to_string()),
                ("HERDR_TOKEN_MEM_X".to_string(), "ok".to_string()),
            ]
        );

        app.state.active = None;
        assert!(focused_workspace_token_env(&app.state).is_empty());
    }

    #[tokio::test]
    async fn token_change_reruns_status_commands() {
        let mut app = test_app();
        if !crate::platform::status_commands_supported() {
            return;
        }
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("one"));
        app.state.active = Some(0);
        app.configure_tab_bar_status(
            &[TabBarRightEntryConfig::Command {
                command: MULTILINE_COMMAND.into(),
                argv: Vec::new(),
                ansi: false,
                interval_seconds: 500,
                timeout_seconds: 2,
            }],
            " ",
        );

        let start = std::time::Instant::now();
        app.handle_tab_bar_status_tasks(start);
        assert!(app.tab_bar_commands[0].task.is_some());

        // A token change while the run is in flight re-runs it on completion.
        app.state.workspaces[0].metadata_tokens.patch(
            token_patch(&[("cpu", "10")]),
            None,
            std::time::Instant::now(),
        );
        app.handle_tab_bar_status_tasks(start + Duration::from_millis(1));
        assert!(app.tab_bar_commands[0].rerun_when_finished);
        app.handle_tab_bar_command_finished(
            app.tab_bar_status_generation,
            0,
            Ok(TabBarCommandOutput::Text(Some("ready".into()))),
        );
        assert!(!app.tab_bar_commands[0].rerun_when_finished);
        let deadline = app
            .next_tab_bar_status_deadline()
            .expect("rerun deadline after token change");
        assert!(deadline <= std::time::Instant::now());

        // Run it, finish it: the next run sits a full interval out.
        app.handle_tab_bar_status_tasks(std::time::Instant::now());
        app.handle_tab_bar_command_finished(
            app.tab_bar_status_generation,
            0,
            Ok(TabBarCommandOutput::Text(Some("ready".into()))),
        );
        let deadline = app
            .next_tab_bar_status_deadline()
            .expect("steady-state deadline");
        assert!(deadline > std::time::Instant::now() + Duration::from_secs(400));

        // An idle command re-runs immediately when a token changes.
        app.state.workspaces[0].metadata_tokens.patch(
            token_patch(&[("cpu", "99")]),
            None,
            std::time::Instant::now(),
        );
        let now = std::time::Instant::now();
        app.handle_tab_bar_status_tasks(now);
        assert!(app.tab_bar_commands[0].task.is_some());
    }

    #[test]
    fn separator_preserves_printable_spacing_and_drops_controls() {
        assert_eq!(sanitize_separator(" \x1b|\n "), " | ");
    }
}
