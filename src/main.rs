use std::{
    io::Read,
    io::{stdout, Write},
    net::SocketAddr,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::Result;
use axum::{
    body::{boxed, BoxBody, Full},
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::{header, Response, StatusCode, Uri},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use clap::Parser;
use crossterm::{cursor, execute, style::Stylize, terminal};
use itertools::Itertools;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rust_embed::RustEmbed;
use serde::Serialize;
use tokio::{
    sync::{broadcast, Mutex},
    time::{timeout_at, Instant},
};
use vte::{Params, Perform};

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
    let cli = Cli::parse();
    let shell_path = match cli.shell {
        Some(s) => s,
        None => std::env::var("SHELL")?,
    };

    let _clean_up = CleanUp;
    let all_events: Arc<Mutex<Vec<VteEvent>>> = Arc::new(Mutex::new(vec![]));
    let (tx, _) = broadcast::channel::<VteEventDto>(10000); // capacity arbitrarily chosen
    let state = AppState { all_events, tx };

    let pty_system = native_pty_system();

    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        // TODO: handle SIGWINCH
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    println!(
        "{} {} {}",
        "Connect to".cyan(),
        format!("http://localhost:{}", &cli.port).magenta(),
        "to view terminal escape codes!".cyan()
    );
    terminal::enable_raw_mode()?;

    let mut stdin = std::io::stdin();

    let mut command = CommandBuilder::new(shell_path);
    if let Ok(cwd) = std::env::current_dir() {
        command.cwd(cwd);
    }

    let child = pair.slave.spawn_command(command)?;
    // This reads output (stderr and stdout multiplexed into 1 stream) from child
    let mut reader = pair.master.try_clone_reader()?;

    // Watch the child's output, responding to escape codes and writing all output to disk
    let cloned_state = state.clone();
    thread::spawn(move || -> Result<()> {
        let mut performer = VteLog {
            curr_cmd_bytes: Vec::new(),
            state: cloned_state,
        };
        let mut statemachine = vte::Parser::new();
        let mut recording = if cli.log_to_file {
            Some(std::fs::File::create("stdout.txt")?)
        } else {
            None
        };
        let mut buf = [0u8; 8192];

        loop {
            let size = reader.read(&mut buf)?;
            let bytes = buf[0..size].to_vec();

            for byte in &bytes {
                performer.curr_cmd_bytes.push(*byte);
                statemachine.advance(&mut performer, *byte);
            }

            stdout().write_all(&bytes)?;
            stdout().flush()?;

            if let Some(recording) = &mut recording {
                recording.write_all(&bytes)?;
            }
        }
    });

    // start web server and attempt to open it in browser
    let cloned_state = state;
    let rt = tokio::runtime::Runtime::new()?;
    let _webserver = rt.spawn(async move {
        let app = Router::new()
            .route("/", get(root))
            .route("/events", get(events_websocket))
            .route("/*file", get(static_handler))
            .with_state(cloned_state);

        let url = format!("http://localhost:{}", cli.port);
        if is_wsl::is_wsl() {
            // workaround for open-rs bug where it tries to open in `gio` even on wsl
            let _ = open::with(url, "wslview");
        } else {
            let _ = open::that(url);
        }

        let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    let mut child_stdin = pair.master.take_writer()?;
    // forward all input from this process to the child
    loop {
        let mut buffer = [0; 1024];
        let n = stdin.read(&mut buffer[..])?;

        let bytes = buffer[..n].to_vec();
        child_stdin.write_all(&bytes)?;

        if bytes.iter().any(|b| *b == 0x4) {
            // EOF
            child.clone_killer().kill()?;
            drop(_clean_up);
            // print_all_events(&state.all_events.blocking_lock());
            return Ok(());
        }
    }
}

fn _print_all_events(vte_rx: &Vec<VteEvent>) {
    let mut last_was_char = true;
    for event in vte_rx {
        match event {
            VteEvent::Print { c } => {
                print!("{c}");
                last_was_char = true;
            }
            _ => {
                if last_was_char {
                    println!();
                }
                println!("{:?}", event);
                last_was_char = false;
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    all_events: Arc<Mutex<Vec<VteEvent>>>,
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

// send all the already-logged events over the socket right away, then stream them as they occur
async fn stream_events(app_state: AppState, mut ws: WebSocket) {
    let events = app_state.all_events.lock().await;
    for chunk in events.chunks(100) {
        let dtos = chunk
            .iter()
            .map(VteEventDto::from)
            // optimization: coalesce adjacent characters before sending to front-end
            // TODO: figure out a more efficient way to do this?
            .coalesce(|x, y| match (&x, &y) {
                (VteEventDto::Print { string: x }, VteEventDto::Print { string: y }) => {
                    Ok(VteEventDto::Print {
                        string: format!("{x}{y}"),
                    })
                }
                _ => Err((x, y)),
            })
            .collect::<Vec<_>>();
        ws.send(Message::Text(serde_json::to_string(&dtos).unwrap()))
            .await
            .unwrap();
    }
    drop(events);

    let mut rx = app_state.tx.subscribe();
    // throttle event sending so we can cut down on renders
    const THROTTLE_DURATION: Duration = Duration::from_millis(10);
    let mut batch = vec![];
    let mut next_send = Instant::now() + THROTTLE_DURATION;

    loop {
        if let Ok(Ok(e)) = timeout_at(next_send, rx.recv()).await {
            // optimization: if this is a string and the last item in the batch is also a string, concatenate them
            // this greatly cuts down on the number of events sent to the front-end
            if let VteEventDto::Print { string } = &e {
                if let Some(last) = batch.last_mut() {
                    if let VteEventDto::Print {
                        string: last_string,
                    } = last
                    {
                        last_string.push_str(string);
                        continue;
                    }
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

struct CleanUp;

impl Drop for CleanUp {
    fn drop(&mut self) {
        terminal::disable_raw_mode().expect("Could not disable raw mode");
        execute!(stdout(), cursor::SetCursorStyle::DefaultUserShape).unwrap();
    }
}

struct VteLog {
    curr_cmd_bytes: Vec<u8>,
    state: AppState,
}

impl VteLog {
    fn log(&mut self, event: VteEvent) {
        let _ = self.state.tx.send(VteEventDto::from(&event)); // this will fail if there's nobody listening yet, and that's OK
        self.state.all_events.blocking_lock().push(event);
        self.curr_cmd_bytes.clear();
    }
}

impl Perform for VteLog {
    fn print(&mut self, c: char) {
        self.log(VteEvent::Print { c });
    }

    fn execute(&mut self, byte: u8) {
        self.log(VteEvent::Execute { byte });
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        let p = params.iter().map(|p| p.to_vec()).collect();

        self.log(VteEvent::Hook {
            params: p,
            intermediates: intermediates.to_vec(),
            ignore,
            c,
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }

    fn put(&mut self, byte: u8) {
        self.log(VteEvent::Put {
            byte,
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }

    fn unhook(&mut self) {
        self.log(VteEvent::Unhook {
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.log(VteEvent::OscDispatch {
            params: params.iter().map(|p| p.to_vec()).collect(),
            bell_terminated,
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        self.log(VteEvent::CsiDispatch {
            params: params.iter().map(|p| p.to_vec()).collect(),
            intermediates: intermediates.to_vec(),
            ignore,
            c,
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.log(VteEvent::EscDispatch {
            intermediates: intermediates.to_vec(),
            ignore,
            byte,
            raw_bytes: self.curr_cmd_bytes.clone(),
        });
    }
}

// These all map directly to their equivalent events in the `vte` library
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")] // give each JSON record a "type" field indicating the enum type, easier to consume from JS
enum VteEvent {
    Print {
        c: char,
    },
    Execute {
        byte: u8,
    },
    Hook {
        params: Vec<Vec<u16>>,
        intermediates: Vec<u8>,
        ignore: bool,
        c: char,
        raw_bytes: Vec<u8>,
    },
    Put {
        byte: u8,
        raw_bytes: Vec<u8>,
    },
    Unhook {
        raw_bytes: Vec<u8>,
    },
    OscDispatch {
        params: Vec<Vec<u8>>,
        bell_terminated: bool,
        raw_bytes: Vec<u8>,
    },
    CsiDispatch {
        params: Vec<Vec<u16>>,
        intermediates: Vec<u8>,
        ignore: bool,
        c: char,
        raw_bytes: Vec<u8>,
    },
    EscDispatch {
        intermediates: Vec<u8>,
        ignore: bool,
        byte: u8,
        raw_bytes: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")] // give each JSON record a "type" field indicating the enum type, easier to consume from JS
enum VteEventDto {
    Print {
        string: String,
    },
    GenericEscape {
        title: String,
        tooltip: Option<String>,
        raw_bytes: String,
    },
    LineBreak {
        title: String,
    },
}

impl From<&VteEvent> for VteEventDto {
    fn from(value: &VteEvent) -> Self {
        match value {
            VteEvent::Print { c } => VteEventDto::Print {
                string: String::from(*c),
            },
            VteEvent::Execute { byte } => match byte {
                10 => VteEventDto::LineBreak { title: "CR".into() },
                13 => VteEventDto::LineBreak { title: "LF".into() },
                _ => {
                    let bytes = [*byte];
                    let str = sanitize_raw_bytes(&bytes.to_vec());
                    Self::GenericEscape {
                        title: format!("Execute {str}"),
                        tooltip: None,
                        raw_bytes: str,
                    }
                }
            },
            VteEvent::Hook { raw_bytes, .. } => VteEventDto::GenericEscape {
                title: "Hook".into(),
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            VteEvent::Put { raw_bytes, .. } => VteEventDto::GenericEscape {
                title: "Put".into(),
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            VteEvent::Unhook { raw_bytes } => VteEventDto::GenericEscape {
                title: "Unhook".into(),
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
            VteEvent::OscDispatch {
                params,
                bell_terminated,
                raw_bytes,
            } => osc_front_end(params, bell_terminated, raw_bytes),
            VteEvent::CsiDispatch {
                params,
                intermediates,
                ignore,
                c,
                raw_bytes,
            } => csi_front_end(params, intermediates, ignore, c, raw_bytes),
            VteEvent::EscDispatch { raw_bytes, .. } => VteEventDto::GenericEscape {
                title: "Escape".into(),
                tooltip: None,
                raw_bytes: sanitize_raw_bytes(raw_bytes),
            },
        }
    }
}

fn csi_front_end(
    params: &[Vec<u16>],
    _intermediates: &[u8],
    _ignore: &bool,
    c: &char,
    raw_bytes: &Vec<u8>,
) -> VteEventDto {
    let mut ascii = params
        .iter()
        .map(|p| p.iter().map(|s| s.to_string()).join(","))
        .join(";");
    ascii.push(*c);

    // TODO: need to do more sophisticated matching on individual params, not the final string
    // ex: should support both 1;31m and 31;1m for bold red text
    #[allow(unused_assignments)]
    let mut tooltip = None;
    // Select Graphic Rendition https://stackoverflow.com/a/33206814/
    // if *c == 'm' {
    // }
    // 🙈🤠
    tooltip = match ascii.as_str() {
        "30m" => Some("Set foreground color to black".into()),
        "31m" => Some("Set foreground color to red".into()),
        "1;31m" => Some("Set foreground color to bold red".into()),
        "32m" => Some("Set foreground color to green".into()),
        "1;32m" => Some("Set foreground color to bold green".into()),
        "33m" => Some("Set foreground color to yellow".into()),
        "1;33m" => Some("Set foreground color to bold yellow".into()),
        "34m" => Some("Set foreground color to blue".into()),
        "1;34m" => Some("Set foreground color to bold blue".into()),
        "35m" => Some("Set foreground color to magenta".into()),
        "1;35m" => Some("Set foreground color to bold magenta".into()),
        "36m" => Some("Set foreground color to cyan".into()),
        "1;36m" => Some("Set foreground color to bold cyan".into()),
        "37m" => Some("Set foreground color to white".into()),
        "1;37m" => Some("Set foreground color to bold white".into()),
        "39m" => Some("Set foreground color to default".into()),
        "0m" => Some("Reset all modes (styles and colours)".into()),
        "2004h" => Some("Enable bracketed paste mode".into()),
        _ => None,
    };

    VteEventDto::GenericEscape {
        title: format!("CSI {ascii}"),
        tooltip,
        raw_bytes: sanitize_raw_bytes(raw_bytes),
    }
}

fn osc_front_end(
    params: &Vec<Vec<u8>>,
    _bell_terminated: &bool,
    raw_bytes: &Vec<u8>,
) -> VteEventDto {
    // TODO handle things like OSC 133;B. Show more than first param
    // ]133;B
    let first = params.first().unwrap();
    let ascii = String::from_utf8_lossy(first);

    // set title https://tldp.org/HOWTO/Xterm-Title-3.html
    if ascii == "0" && params.len() > 1 {
        let param = String::from_utf8_lossy(&params[1]);

        return VteEventDto::GenericEscape {
            title: format!("OSC {ascii}"),
            tooltip: Some(format!("Set title to '{param}'")),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    let more = if params.len() > 1 { "..." } else { "" };
    VteEventDto::GenericEscape {
        title: format!("OSC {ascii}{more}"),
        tooltip: None,
        raw_bytes: sanitize_raw_bytes(raw_bytes),
    }
}

/// Convert escape code bytes into a user-facing string,
/// replacing control codes with their \0x hex representations
fn sanitize_raw_bytes(raw_bytes: &[u8]) -> String {
    let ret = String::from_utf8_lossy(raw_bytes);
    // TODO: there's gotta be a better way to do this than a line for every interesting control char
    let ret = ret.replace("", r"\0x1b");
    ret
}

pub struct StaticFile<T>(pub T);

impl<T> IntoResponse for StaticFile<T>
where
    T: Into<String>,
{
    fn into_response(self) -> Response<BoxBody> {
        let path = self.0.into();

        match Asset::get(path.as_str()) {
            Some(content) => {
                let body = boxed(Full::from(content.data));
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                Response::builder()
                    .header(header::CONTENT_TYPE, mime.as_ref())
                    .body(body)
                    .unwrap()
            }
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(boxed(Full::from("404")))
                .unwrap(),
        }
    }
}
