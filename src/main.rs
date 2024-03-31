use std::{
    io::{stdout, Read, Write},
    mem::take,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
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
use clap::Parser as ClapParser;
use crossterm::{cursor, execute, style::Stylize, terminal};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rand::seq::SliceRandom;
use rust_embed::RustEmbed;
use serde::Serialize;
use termwiz::{
    color::ColorSpec,
    escape::{csi::Sgr, parser::Parser, Action, ControlCode, Esc, EscCode, CSI},
};
use tokio::{
    net::TcpListener,
    sync::{broadcast, Mutex},
    time::{timeout_at, Instant},
};

#[derive(clap::Parser, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the shell to be launched. If not specified, will use the $SHELL environment variable
    #[arg(short, long)]
    shell: Option<String>,

    /// The port for the web server
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// Log stdout to a file (stdout.txt)
    #[arg(short, long, default_value = "false")]
    log_to_file: bool,
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
    let shell_path = if let Some(s) = cli.shell {
        s
    } else if let Ok(path) = std::env::var("SHELL") {
        path
    } else {
        bail!("SHELL environment variable not found; either set it or use --shell")
    };

    const EMOJI_POOL: [&str; 10] = ["🎨", "🎨", "🎨", "🎨", "🎨", "🎨", "🎨", "🎨", "🎨", "🤔"];
    let random_emoji = *EMOJI_POOL.choose(&mut rand::thread_rng()).unwrap();
    println!(
        "{}{}{}{} {}",
        "Launching ".cyan(),
        PathBuf::from(&shell_path)
            .file_name()
            .expect("get file name")
            .to_string_lossy()
            .magenta(),
        " in Escape Artist v".cyan(),
        env!("CARGO_PKG_VERSION").cyan(),
        random_emoji,
    );

    let _clean_up = CleanUp;
    let (tx, _) = broadcast::channel::<VteEventDto>(10000); // capacity arbitrarily chosen
    let state = AppState {
        all_dtos: Arc::new(Mutex::new(vec![])),
        tx,
    };

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

    let mut stdin = std::io::stdin();

    let mut command = CommandBuilder::new(shell_path);
    if let Ok(cwd) = std::env::current_dir() {
        command.cwd(cwd);
    }

    // Spawn the child process (shell usually), wired up to the PTY
    let child = pair.slave.spawn_command(command)?;
    // This reads output (stderr and stdout multiplexed into 1 stream) from child
    let mut reader = pair.master.try_clone_reader()?;

    let (action_sender, mut action_receiver) =
        tokio::sync::mpsc::channel::<(Action, Vec<u8>)>(10000);

    // Watch the child's output, pump it into the VTE parser/performer, and forward it to the terminal
    // We use a thread here because reading from the pty is blocking
    thread::spawn(move || -> Result<()> {
        let mut parser = Parser::new();
        let mut recording = if cli.log_to_file {
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
                if let Some((action, _)) = parser.parse_first(&[*byte]) {
                    let cmd_bytes = take(&mut curr_cmd_bytes);
                    // this may fail if the receiver has been dropped because we're exiting
                    let _ = action_sender.blocking_send((action, cmd_bytes));
                }
            }

            stdout().write_all(&bytes)?;
            stdout().flush()?;

            if let Some(recording) = &mut recording {
                recording.write_all(&bytes)?;
            }
        }
    });
    let runtime = tokio::runtime::Runtime::new()?;

    let cloned_state = state.clone();
    runtime.spawn(async move {
        let mut fg_color = ColorSpec::Default;
        let mut bg_color = ColorSpec::Default;

        while let Some((action, raw_bytes)) = action_receiver.recv().await {
            // optimization: if the last DTO was a print and this action is a print, concatenate them
            // this greatly cuts down on the number of events sent to the front-end

            if let Some(VteEventDto::Print {
                string: last_string,
                ..
            }) = cloned_state.all_dtos.lock().await.last_mut()
            {
                if let Action::Print(c) = &action {
                    last_string.push(*c);
                    let tuple = (action, raw_bytes);
                    let dto = VteEventDto::from(&tuple);
                    let _ = cloned_state.tx.send(dto);
                    continue;
                }
            }

            // otherwise, carry on; update global colours if needed and add the event to the list

            update_global_colors(&action, &mut fg_color, &mut bg_color);
            let tuple = (action, raw_bytes);
            let mut dto = VteEventDto::from(&tuple);
            update_dto_color(&mut dto, fg_color, bg_color);

            {
                let mut dtos = cloned_state.all_dtos.lock().await;
                dtos.push(dto.clone());
            }

            let _ = cloned_state.tx.send(dto);
        }
    });

    // start web server and attempt to open it in browser
    let cloned_state = state.clone();
    let _webserver = runtime.spawn(async move {
        let app = Router::new()
            .route("/", get(root))
            .route("/events", get(events_websocket))
            .route("/*file", get(static_handler))
            .with_state(cloned_state);

        let url = format!("http://localhost:{}", cli.port);
        let _ = open::that(url);

        let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));

        let listener = TcpListener::bind(addr).await.expect(
            "Failed to bind to socket. Maybe another service is already using the same port",
        );
        axum::serve(listener, app)
            .await
            .expect("Failed to start HTTP server.");
    });

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
            child.clone_killer().kill()?;
            drop(_clean_up);
            let event_count = &state.all_dtos.blocking_lock().len();
            println!(
                "{}{}",
                "Exited. Viewed ".cyan(),
                format!("{} escape codes", event_count).magenta()
            );
            // print_all_events(&state.all_events.blocking_lock());
            return Ok(());
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

fn update_dto_color(dto: &mut VteEventDto, fg_color: ColorSpec, bg_color: ColorSpec) {
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
    if let Action::CSI(csi) = action {
        if let CSI::Sgr(sgr) = csi {
            match sgr {
                Sgr::Foreground(color) => {
                    *fg_color = color.clone();
                }
                Sgr::Background(color) => {
                    *bg_color = color.clone();
                }
                Sgr::Reset => {
                    *fg_color = ColorSpec::Default;
                    *bg_color = ColorSpec::Default;
                }
                _ => {}
            }
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
                title: Some(format!("DCM {dcm:?}")),
                icon_svg: None,
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            Action::OperatingSystemCommand(osc) => VteEventDto::GenericEscape {
                title: Some(format!("OSC {osc:?}")),
                icon_svg: None,
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            Action::CSI(csi) => csi_to_dto(csi, sanitize_raw_bytes(raw_bytes)),
            Action::Esc(e) => esc_to_dto(e, raw_bytes),
            Action::Sixel(_) => todo!("sixel not implemented yet"),
            Action::XtGetTcap(_) => todo!("xt get tcap not implemented yet"),
            Action::KittyImage(_) => todo!("kitty image not implemented yet"),
        }
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
            _ => VteEventDto::GenericEscape {
                title: Some(format!("ESC {code:?}")),
                icon_svg: None,
                tooltip: None,
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
                Some("SGR (Select Graphic Rendition) Reset".into()),
                Some(iconify::svg!("carbon:reset").into()),
            ),
            Sgr::Foreground(color) => (
                Some("FG".into()),
                Some(format!("Set foreground color to: {color:?}")),
                None,
            ),
            Sgr::Background(color) => (
                Some("BG".into()),
                Some(format!("Set background color to: {color:?}")),
                None,
            ),

            _ => (Some("SGR".into()), Some(format!("Set {sgr:?}")), None),
        },
        CSI::Cursor(cursor) => (
            None,
            Some(format!("Move cursor: {cursor:?}")),
            Some(iconify::svg!("ph:cursor-text-fill").into()),
        ),
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
    let ret = ret.replace("", r"\x1b");
    ret
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
