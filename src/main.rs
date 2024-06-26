use std::{
    fs::File,
    io::{stdout, Read, Write},
    mem::take,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use ansi_colours::rgb_from_ansi256;
use anyhow::{bail, Result};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::{header, Response, StatusCode, Uri},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use clap::{
    builder::{StyledStr, Styles},
    Parser as ClapParser,
};
use crossterm::{cursor, execute, style::Stylize, terminal};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rust_embed::RustEmbed;
use serde::Serialize;
use termwiz::{
    color::ColorSpec,
    escape::{
        csi::{Edit, EraseInDisplay, EraseInLine, Sgr},
        parser::Parser,
        Action, ControlCode, Esc, EscCode, OperatingSystemCommand, CSI,
    },
};
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{channel, Receiver, Sender},
        Mutex,
    },
    time::{timeout_at, Instant},
};

#[derive(clap::Parser, Clone)]
#[command(author, version, about, long_about = None, styles = clap_v3_style(), after_help = after_help())]
struct Cli {
    /// The port for the web server
    #[arg(short, long, default_value = "3000")]
    port: u16,

    #[arg(short, long)]
    replay_file: Option<String>,

    /// Log stdout to a file (stdout.txt)
    #[arg(short, long, default_value = "false")]
    log_to_file: bool,

    /// Command to be launched, optionally with args. If not specified, will use the $SHELL environment variable
    #[arg(last = true)]
    argv: Vec<String>,
}

fn main() -> Result<()> {
    initialize_environment();
    let resize_signaled = Arc::new(AtomicBool::new(false));

    // No SIGWINCH on Windows, but it seems like there's no great alternative: https://github.com/microsoft/terminal/issues/281
    #[cfg(not(windows))]
    {
        use signal_hook::consts::SIGWINCH;
        let _ = signal_hook::flag::register(SIGWINCH, resize_signaled.clone());
    }

    let cli = Cli::parse();

    if cli.replay_file.is_some() && !cli.argv.is_empty() {
        bail!("Cannot specify a replay file and a command to run at the same time")
    }

    let (tx, _) = broadcast::channel::<VteEventDto>(10000); // capacity arbitrarily chosen
    let state = AppState {
        sequence_count: Arc::new(AtomicI64::new(0)),
        all_dtos: Arc::new(Mutex::new(vec![])),
        tx,
    };

    let runtime = tokio::runtime::Runtime::new()?;

    if let Some(file) = &cli.replay_file {
        println!(
            "{}{}{}{} 🎨",
            "Replaying ".cyan(),
            file.clone().magenta(),
            " in Escape Artist v".cyan(),
            env!("CARGO_PKG_VERSION").cyan(),
        );
        let (action_sender, action_receiver) = channel::<(Action, Vec<u8>)>(10000);

        let reader = File::open(file)?;
        // Watch the child's output, pump it into the VTE parser/performer, and forward it to the terminal
        // We use a thread here because reading from the pty is blocking
        thread::spawn(move || {
            parse_raw_output(cli.log_to_file, false, Box::new(reader), action_sender)
        });

        let cloned_state = state.clone();
        runtime.spawn(process_actions(action_receiver, cloned_state));

        println!(
            "{}{}{}",
            "Open ".cyan(),
            format!("http://localhost:{}", &cli.port).magenta(),
            " to view terminal escape codes, type CTRL+D to exit".cyan()
        );

        terminal::enable_raw_mode()?;
        let _clean_up = CleanUp;

        // start web server and attempt to open it in browser
        let cloned_state = state.clone();
        runtime.spawn(run_webserver(cloned_state, cli));

        // read stdin, exit on ctrl+d
        let mut stdin = std::io::stdin();
        let mut buffer = [0; 1024];
        loop {
            let n = stdin.read(&mut buffer)?;
            let bytes = buffer[..n].to_vec();
            if bytes.iter().any(|b| *b == 0x4) {
                // EOF
                break;
            }
        }

        return Ok(());
    }

    let argv = if cli.argv.is_empty() {
        if let Ok(shell) = std::env::var("SHELL") {
            vec![shell]
        } else {
            bail!("SHELL environment variable not found; either set it or use --shell")
        }
    } else {
        cli.argv.clone()
    };

    println!(
        "{}{}{}{} 🎨",
        "Launching ".cyan(),
        argv.join(" ").magenta(),
        " in Escape Artist v".cyan(),
        env!("CARGO_PKG_VERSION").cyan(),
    );

    let pty_system = native_pty_system();

    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    println!(
        "{}{}{}",
        "Open ".cyan(),
        format!("http://localhost:{}", &cli.port).magenta(),
        " to view terminal escape codes, type CTRL+D to exit".cyan()
    );
    println!();
    terminal::enable_raw_mode()?;
    let _clean_up = CleanUp;

    let mut stdin = std::io::stdin();

    let mut command = CommandBuilder::new(argv[0].clone());
    command.args(&argv[1..]);
    if let Ok(cwd) = std::env::current_dir() {
        command.cwd(cwd);
    }

    // Spawn the child process (shell usually), wired up to the PTY
    let child = pair.slave.spawn_command(command)?;
    // This reads output (stderr and stdout multiplexed into 1 stream) from child
    let mut reader = pair.master.try_clone_reader()?;

    if let Some(file) = &cli.replay_file {
        reader = Box::new(std::fs::File::open(file)?);
    }

    let (action_sender, action_receiver) = channel::<(Action, Vec<u8>)>(10000);

    // Watch the child's output, pump it into the VTE parser/performer, and forward it to the terminal
    // We use a thread here because reading from the pty is blocking
    thread::spawn(move || parse_raw_output(cli.log_to_file, true, reader, action_sender));

    let cloned_state = state.clone();
    runtime.spawn(process_actions(action_receiver, cloned_state));

    // start web server and attempt to open it in browser
    let cloned_state = state.clone();
    let _webserver = runtime.spawn(run_webserver(cloned_state, cli));

    let mut child_stdin = pair.master.take_writer()?;
    // forward all input from this process to the child
    loop {
        if resize_signaled.load(Ordering::Relaxed) {
            let (cols, rows) = terminal::size()?;
            pair.master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            resize_signaled.store(false, Ordering::Relaxed);
        }

        let mut buffer = [0; 1024];
        let n = stdin.read(&mut buffer[..])?;
        let bytes = buffer[..n].to_vec();
        child_stdin.write_all(&bytes)?;

        if bytes.iter().any(|b| *b == 0x4) {
            // EOF
            _ = child.clone_killer().kill();
            drop(_clean_up);
            let sequence_count = state.sequence_count.load(Ordering::Relaxed);
            println!(
                "\n{}{}",
                "Exited. Processed ".cyan(),
                format!("{} escape sequences", sequence_count).magenta()
            );
            // print_all_events(&state.all_events.blocking_lock());
            return Ok(());
        }
    }
}

async fn run_webserver(cloned_state: AppState, cli: Cli) {
    let app = Router::new()
        .route("/", get(root))
        .route("/events", get(events_websocket))
        .route("/*file", get(static_handler))
        .with_state(cloned_state);
    let url = format!("http://localhost:{}", cli.port);
    let _ = open::that(url);
    let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind to socket. Maybe another service is already using the same port");
    axum::serve(listener, app)
        .await
        .expect("Failed to start HTTP server.");
}

fn parse_raw_output(
    log_to_file: bool,
    write_to_stdout: bool,
    mut reader: Box<dyn Read + Send>,
    action_sender: Sender<(Action, Vec<u8>)>,
) -> Result<()> {
    let mut parser = Parser::new();
    let mut recording = if log_to_file {
        Some(std::fs::File::create("stdout.txt")?)
    } else {
        None
    };
    let mut buf = [0u8; 8192];
    let mut curr_cmd_bytes = Vec::new();
    loop {
        let size = reader.read(&mut buf)?;
        let bytes = buf[0..size].to_vec();

        for byte in &bytes {
            curr_cmd_bytes.push(*byte);

            let actions = parser.parse_as_vec(&[*byte]);
            if !actions.is_empty() {
                // 1 byte sequence can represent multiple actions
                let cmd_bytes = take(&mut curr_cmd_bytes);
                for action in actions {
                    // this may fail if the receiver has been dropped because we're exiting
                    let _ = action_sender.blocking_send((action, cmd_bytes.clone()));
                }
            }
        }

        if write_to_stdout {
            stdout().write_all(&bytes)?;
            stdout().flush()?;
        }

        if let Some(recording) = &mut recording {
            recording.write_all(&bytes)?;
        }
    }
}

async fn process_actions(mut action_receiver: Receiver<(Action, Vec<u8>)>, state: AppState) {
    let mut fg_color = ColorSpec::Default;
    let mut bg_color = ColorSpec::Default;
    let mut last_was_line_break = false;
    while let Some((action, raw_bytes)) = action_receiver.recv().await {
        // optimization: if the last DTO was a print and this action is a print, concatenate them
        // this greatly cuts down on the number of events sent to the front-end
        if let Some(VteEventDto::Print {
            string: last_string,
            ..
        }) = state.all_dtos.lock().await.last_mut()
        {
            if let Action::Print(c) = &action {
                last_string.push(*c);
                let tuple = (action, raw_bytes);
                let dto = VteEventDto::from(&tuple);
                let _ = state.tx.send(dto);
                continue;
            }
        } else {
            state.sequence_count.fetch_add(1, Ordering::Relaxed);
        }

        // otherwise, carry on; update global colours if needed and add the event to the list

        update_global_colors(&action, &mut fg_color, &mut bg_color);
        let tuple = (action, raw_bytes);
        let mut dto = VteEventDto::from(&tuple);
        update_print_colors(&mut dto, fg_color, bg_color);

        // emit an invisible line break DTO if we're transitioning from a line break to a non-line break or vice versa
        let is_line_break = matches!(&dto, VteEventDto::LineBreak { .. });
        let dtos_to_send = if is_line_break && !last_was_line_break {
            vec![VteEventDto::InvisibleLineBreak {}, dto]
        } else if !is_line_break && last_was_line_break {
            vec![VteEventDto::InvisibleLineBreak {}, dto]
        } else {
            vec![dto]
        };
        last_was_line_break = is_line_break;

        {
            let mut dtos = state.all_dtos.lock().await;
            for dto in dtos_to_send.iter() {
                dtos.push(dto.clone());
            }
        }

        for dto in dtos_to_send {
            let _ = state.tx.send(dto);
        }
    }
}

fn initialize_environment() {
    std::env::set_var("RUST_BACKTRACE", "1");
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        terminal::disable_raw_mode().expect("Could not disable raw mode");
        execute!(stdout(), cursor::SetCursorStyle::DefaultUserShape).unwrap();
        default_panic(info);
    }));
}

#[derive(Clone)]
struct AppState {
    sequence_count: Arc<AtomicI64>,
    all_dtos: Arc<Mutex<Vec<VteEventDto>>>,
    tx: broadcast::Sender<VteEventDto>,
}

#[axum::debug_handler]
async fn root() -> impl IntoResponse {
    Html(include_str!("../embed/index.html"))
}

#[axum::debug_handler]
async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/').to_string();
    StaticFile(path)
}

#[derive(RustEmbed)]
#[folder = "embed/"]
struct Asset;

#[axum::debug_handler]
async fn events_websocket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|ws: WebSocket| async { stream_events(state, ws).await })
}

fn hex_color(color: &ColorSpec) -> Option<String> {
    match color {
        ColorSpec::Default => None,
        ColorSpec::PaletteIndex(i) => {
            let (r, g, b) = rgb_from_ansi256(*i);
            Some(format!("#{:02x}{:02x}{:02x}", r, g, b))
        }
        ColorSpec::TrueColor(srgba) => Some(srgba.to_rgb_string()),
    }
}

// send all the already-logged events over the socket right away, then stream them as they occur
async fn stream_events(app_state: AppState, mut ws: WebSocket) {
    let dtos = app_state.all_dtos.lock().await;
    for chunk in dtos.chunks(100) {
        ws.send(Message::Text(serde_json::to_string(&chunk).unwrap()))
            .await
            .unwrap();
    }
    drop(dtos);

    let mut rx = app_state.tx.subscribe();
    // throttle event sending so we can cut down on renders
    const THROTTLE_DURATION: Duration = Duration::from_millis(100);
    let mut batch = vec![];
    let mut next_send = Instant::now() + THROTTLE_DURATION;

    loop {
        if let Ok(Ok(e)) = timeout_at(next_send, rx.recv()).await {
            // TODO rebuild this
            // optimization: if this is a string and the last item in the batch is also a string, concatenate them
            // this greatly cuts down on the number of events sent to the front-end
            if let VteEventDto::Print { string, .. } = &e {
                if let Some(VteEventDto::Print {
                    string: last_string,
                    ..
                }) = batch.last_mut()
                {
                    last_string.push_str(string);
                    continue;
                }
            }

            batch.push(e)
        }

        if Instant::now() > next_send {
            if !batch.is_empty() {
                if ws
                    .send(Message::Text(serde_json::to_string(&batch).unwrap()))
                    .await
                    .is_err()
                {
                    // if this failed it's probably because the client disconnected
                    return;
                }
                batch.clear();
            }
            next_send = Instant::now() + THROTTLE_DURATION;
        }
    }
}

fn update_print_colors(dto: &mut VteEventDto, fg_color: ColorSpec, bg_color: ColorSpec) {
    if let VteEventDto::Print {
        color: dto_color,
        bg_color: dto_bg_color,
        ..
    } = dto
    {
        *dto_color = hex_color(&fg_color);
        *dto_bg_color = hex_color(&bg_color);
    }
}

fn update_global_colors(action: &Action, fg_color: &mut ColorSpec, bg_color: &mut ColorSpec) {
    if let Action::CSI(CSI::Sgr(sgr)) = action {
        match sgr {
            Sgr::Foreground(color) => {
                *fg_color = *color;
            }
            Sgr::Background(color) => {
                *bg_color = *color;
            }
            Sgr::Reset => {
                *fg_color = ColorSpec::Default;
                *bg_color = ColorSpec::Default;
            }
            _ => {}
        }
    }
}

struct CleanUp;

impl Drop for CleanUp {
    fn drop(&mut self) {
        terminal::disable_raw_mode().expect("Could not disable raw mode");
        execute!(stdout(), cursor::SetCursorStyle::DefaultUserShape).unwrap();
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")] // give each JSON record a "type" field indicating the enum type, easier to consume from JS
enum VteEventDto {
    Print {
        string: String,
        color: Option<String>,
        bg_color: Option<String>,
    },
    GenericEscape {
        title: Option<String>,
        icon_svg: Option<String>,
        tooltip: Option<String>,
        raw_bytes: String,
    },
    ColorEscape {
        title: Option<String>,
        icon_svg: Option<String>,
        tooltip: Option<String>,
        color: String,
        raw_bytes: String,
    },
    InvisibleLineBreak {},
    LineBreak {
        title: String,
    },
}

impl From<&(Action, Vec<u8>)> for VteEventDto {
    fn from(value: &(Action, Vec<u8>)) -> Self {
        let (action, raw_bytes) = value;
        match action {
            Action::Print(c) => VteEventDto::Print {
                string: c.to_string(),
                color: None,
                bg_color: None,
            },
            Action::PrintString(s) => VteEventDto::Print {
                string: s.clone(),
                color: None,
                bg_color: None,
            },
            Action::Control(ctrl) => ctrl_to_dto(ctrl),
            Action::DeviceControl(dcm) => VteEventDto::GenericEscape {
                title: Some("DCM".into()),
                icon_svg: None,
                tooltip: Some(format!("{dcm:?}")),
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            Action::OperatingSystemCommand(osc) => osc_to_dto(osc, raw_bytes),
            Action::CSI(csi) => csi_to_dto(csi, sanitize_raw_bytes(raw_bytes)),
            Action::Esc(e) => esc_to_dto(e, raw_bytes),
            Action::Sixel(_) => VteEventDto::GenericEscape {
                title: Some("Sixel".into()),
                icon_svg: Some(iconify::svg!("mdi:image").into()),
                tooltip: Some("Sixel image".into()),
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            Action::XtGetTcap(x) => VteEventDto::GenericEscape {
                title: Some("XTGETTCAP".into()),
                icon_svg: None,
                tooltip: Some(format!("Get termcap, terminfo for: {}", x.join(", "))),
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            Action::KittyImage(_) => VteEventDto::GenericEscape {
                title: Some("Kitty".into()),
                icon_svg: Some(iconify::svg!("mdi:image").into()),
                tooltip: Some("Kitty image".into()),
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
        }
    }
}

fn osc_to_dto(osc: &OperatingSystemCommand, raw_bytes: &[u8]) -> VteEventDto {
    let raw_bytes_str = sanitize_raw_bytes(raw_bytes);
    match osc {
        OperatingSystemCommand::SetHyperlink(link) => match link {
            Some(link) => VteEventDto::GenericEscape {
                title: None,
                icon_svg: Some(iconify::svg!("mdi:link").into()),
                tooltip: Some(format!("Set hyperlink: {link}")),
                raw_bytes: raw_bytes_str,
            },
            None => VteEventDto::GenericEscape {
                title: None,
                icon_svg: Some(iconify::svg!("mdi:link-off").into()),
                tooltip: Some("Clear hyperlink".into()),
                raw_bytes: raw_bytes_str,
            },
        },
        _ => VteEventDto::GenericEscape {
            title: Some("OSC".into()),
            icon_svg: None,
            tooltip: Some(format!("{osc:?}")),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        },
    }
}

fn esc_to_dto(esc: &Esc, raw_bytes: &[u8]) -> VteEventDto {
    let raw_bytes_str = sanitize_raw_bytes(raw_bytes);
    match esc {
        Esc::Unspecified { .. } => VteEventDto::GenericEscape {
            title: None,
            icon_svg: Some(iconify::svg!("mdi:question-mark-box").into()),
            tooltip: Some("Unspecified escape sequence".into()),
            raw_bytes: raw_bytes_str,
        },
        Esc::Code(code) => match code {
            EscCode::StringTerminator => VteEventDto::GenericEscape {
                title: Some("\\".into()),
                icon_svg: None,
                tooltip: Some("ST / String Terminator".into()),
                raw_bytes: raw_bytes_str,
            },
            EscCode::DecSaveCursorPosition => VteEventDto::GenericEscape {
                title: None,
                icon_svg: Some(iconify::svg!("mdi:content-save").into()),
                tooltip: Some("Save cursor position".into()),
                raw_bytes: raw_bytes_str,
            },
            EscCode::DecRestoreCursorPosition => VteEventDto::GenericEscape {
                title: None,
                icon_svg: Some(iconify::svg!("mdi:file-restore").into()),
                tooltip: Some("Restore cursor position".into()),
                raw_bytes: raw_bytes_str,
            },
            EscCode::AsciiCharacterSetG0 | EscCode::AsciiCharacterSetG1 => {
                VteEventDto::GenericEscape {
                    title: None,
                    icon_svg: Some(iconify::svg!("mdi:alphabetical-variant").into()),
                    tooltip: Some(format!("{code:?}")),
                    raw_bytes: raw_bytes_str,
                }
            }
            _ => VteEventDto::GenericEscape {
                title: Some("ESC".into()),
                icon_svg: None,
                tooltip: Some(format!("{code:?}")),
                raw_bytes: raw_bytes_str,
            },
        },
    }
}

fn ctrl_to_dto(ctrl: &ControlCode) -> VteEventDto {
    let as_byte = *ctrl as u8;
    let raw_bytes = format!("{:#02x}", as_byte);

    match ctrl {
        ControlCode::Bell => VteEventDto::GenericEscape {
            title: None,
            icon_svg: Some(iconify::svg!("mdi:bell").into()),
            tooltip: Some("Bell".into()),
            raw_bytes,
        },
        ControlCode::Backspace => VteEventDto::GenericEscape {
            title: None,
            icon_svg: Some(iconify::svg!("mdi:backspace").into()),
            tooltip: Some("Backspace".into()),
            raw_bytes,
        },
        ControlCode::HorizontalTab => VteEventDto::GenericEscape {
            title: None,
            icon_svg: Some(iconify::svg!("mdi:keyboard-tab").into()),
            tooltip: Some("Tab".into()),
            raw_bytes,
        },
        ControlCode::LineFeed => VteEventDto::LineBreak { title: "LF".into() },
        ControlCode::CarriageReturn => VteEventDto::LineBreak { title: "CR".into() },
        _ => VteEventDto::GenericEscape {
            title: Some(format!("{ctrl:?}")),
            icon_svg: None,
            tooltip: None,
            raw_bytes,
        },
    }
}

fn csi_to_dto(csi: &CSI, raw_bytes: String) -> VteEventDto {
    let (title, tooltip, icon_svg) = match csi {
        CSI::Sgr(sgr) => match sgr {
            Sgr::Reset => (
                None,
                Some("SGR (Select Graphic Rendition) Reset (reset all styles)".into()),
                Some(iconify::svg!("carbon:reset").into()),
            ),
            Sgr::Foreground(color) => {
                return VteEventDto::ColorEscape {
                    title: Some("FG".into()),
                    icon_svg: None,
                    tooltip: Some(format!("Set foreground color to: {color:?}")),
                    color: hex_color(color).unwrap_or("black".into()),
                    raw_bytes,
                }
            }
            Sgr::Background(color) => {
                return VteEventDto::ColorEscape {
                    title: Some("BG".into()),
                    icon_svg: None,
                    tooltip: Some(format!("Set background color to: {color:?}")),
                    color: hex_color(color).unwrap_or("black".into()),
                    raw_bytes,
                }
            }
            _ => (Some("SGR".into()), Some(format!("Set {sgr:?}")), None),
        },
        CSI::Cursor(cursor) => (
            None,
            Some(format!("Update cursor: {cursor:?}")),
            Some(iconify::svg!("ph:cursor-text-fill").into()),
        ),
        CSI::Edit(edit) => match edit {
            Edit::EraseInLine(erase) => (
                None,
                Some(match erase {
                    EraseInLine::EraseToEndOfLine => "Erase to end of line".into(),
                    EraseInLine::EraseToStartOfLine => "Erase to start of line".into(),
                    EraseInLine::EraseLine => "Erase line".into(),
                }),
                Some(iconify::svg!("mdi:eraser").into()),
            ),
            Edit::EraseInDisplay(erase) => (
                None,
                Some(match erase {
                    EraseInDisplay::EraseToEndOfDisplay => "Erase to end of display".into(),
                    EraseInDisplay::EraseToStartOfDisplay => "Erase to start of display".into(),
                    EraseInDisplay::EraseDisplay => "Erase display".into(),
                    EraseInDisplay::EraseScrollback => "Erase scrollback".into(),
                }),
                Some(iconify::svg!("mdi:eraser").into()),
            ),
            _ => (Some("Edit".into()), Some(format!("{edit:?}")), None),
        },
        // CSI::Edit(_) => todo!(),
        // CSI::Mode(_) => todo!(),
        // CSI::Device(_) => todo!(),
        // CSI::Mouse(_) => todo!(),
        // CSI::Window(_) => todo!(),
        // CSI::Keyboard(_) => todo!(),
        // CSI::SelectCharacterPath(_, _) => todo!(),
        // CSI::Unspecified(_) => todo!(),
        _ => (Some("CSI".into()), Some(format!("{csi:?}")), None),
    };

    VteEventDto::GenericEscape {
        title,
        tooltip,
        icon_svg,
        raw_bytes,
    }
}

/// Convert escape code bytes into a user-facing string,
/// replacing control codes with their \0x hex representations
fn sanitize_raw_bytes(raw_bytes: &[u8]) -> String {
    let ret = String::from_utf8_lossy(raw_bytes);
    // TODO: there's gotta be a better way to do this than a line for every interesting control char
    ret.replace("", r"\x1b")
}

pub struct StaticFile<T>(pub T);

impl<T> IntoResponse for StaticFile<T>
where
    T: Into<String>,
{
    fn into_response(self) -> Response<Body> {
        let path = self.0.into();

        match Asset::get(path.as_str()) {
            Some(content) => {
                let body = Body::from(content.data);
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                Response::builder()
                    .header(header::CONTENT_TYPE, mime.as_ref())
                    .body(body)
                    .unwrap()
            }
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("404"))
                .unwrap(),
        }
    }
}

// IMO the v3 style was nice and it's dumb that clap removed colour in v4
pub fn clap_v3_style() -> Styles {
    use clap::builder::styling::AnsiColor;
    Styles::styled()
        .header(AnsiColor::Yellow.on_default())
        .usage(AnsiColor::Green.on_default())
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Green.on_default())
}

fn after_help() -> StyledStr {
    format!("{}\n{}\n\n{}",
    "More Info:".yellow(),
    "This is a tool for seeing ANSI escape codes in terminal applications. You interact with your shell, and it shows the normally-invisible escape codes in a web UI.",
    "It's written+maintained by Reilly Wood, and the latest version can be found at https://github.com/rgwood/escape-artist/")
    .into()
}
