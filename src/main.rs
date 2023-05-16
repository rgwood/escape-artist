use std::{
    io::Read,
    io::{stdout, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Result};
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
use rand::seq::SliceRandom;
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
    let all_events: Arc<Mutex<Vec<VteEvent>>> = Arc::new(Mutex::new(vec![]));
    let (tx, _) = broadcast::channel::<VteEventDto>(10000); // capacity arbitrarily chosen
    let state = AppState { all_events, tx };

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

    let child = pair.slave.spawn_command(command)?;
    // This reads output (stderr and stdout multiplexed into 1 stream) from child
    let mut reader = pair.master.try_clone_reader()?;

    // Watch the child's output, pump it into the VTE parser/performer, and forward it to the terminal
    let cloned_state = state.clone();
    thread::spawn(move || -> Result<()> {
        let mut performer = Performer {
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
        let _ = open::that(url);

        let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .expect(
                "Failed to bind to socket. Maybe another service is already using the same port",
            );
    });

    // let mc = pair.master.

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

fn _print_all_events(vte_rx: &Vec<VteEvent>) {
    let mut last_was_char = true;
    for event in vte_rx {
        match event {
            VteEvent::Print(c) => {
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
                if let Some(VteEventDto::Print {
                    string: last_string,
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

struct CleanUp;

impl Drop for CleanUp {
    fn drop(&mut self) {
        terminal::disable_raw_mode().expect("Could not disable raw mode");
        execute!(stdout(), cursor::SetCursorStyle::DefaultUserShape).unwrap();
    }
}

struct Performer {
    curr_cmd_bytes: Vec<u8>,
    state: AppState,
}

impl Performer {
    fn log(&mut self, event: VteEvent) {
        let _ = self.state.tx.send(VteEventDto::from(&event)); // this will fail if there's nobody listening yet, and that's OK
        self.state.all_events.blocking_lock().push(event);
        self.curr_cmd_bytes.clear();
    }
}

impl Perform for Performer {
    fn print(&mut self, c: char) {
        self.log(VteEvent::Print(c));
    }

    fn execute(&mut self, byte: u8) {
        self.log(VteEvent::Execute(byte));
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
enum VteEvent {
    Print(char),
    Execute(u8),
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
            VteEvent::Print(c) => VteEventDto::Print {
                string: String::from(*c),
            },
            VteEvent::Execute(byte) => match byte {
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
            VteEvent::EscDispatch {
                byte, raw_bytes, ..
            } => other_escape_front_end(byte, raw_bytes),
        }
    }
}

fn other_escape_front_end(byte: &u8, raw_bytes: &Vec<u8>) -> VteEventDto {
    let c = char::from(*byte);
    let (title, tooltip) = match c {
        'c' => ("RIS".into(), Some("Reset to initial state".into())),
        '7' => ("DECSC".into(), Some("Save Cursor Position".into())),
        '8' => ("DECRC".into(), Some("Restore Cursor Position".into())),
        _ => ("Other".into(), None),
    };

    VteEventDto::GenericEscape {
        title,
        tooltip,
        raw_bytes: sanitize_raw_bytes(raw_bytes),
    }
}

fn csi_front_end(
    params: &[Vec<u16>],
    intermediates: &[u8],
    _ignore: &bool,
    c: &char,
    raw_bytes: &Vec<u8>,
) -> VteEventDto {
    let intermediates = intermediates.iter().map(|i| char::from(*i)).collect_vec();
    let mut ascii = intermediates.iter().collect::<String>();
    ascii += &params
        .iter()
        .map(|p| p.iter().map(|s| s.to_string()).join(","))
        .join(";");
    ascii.push(*c);

    let params_without_subparams: Vec<u16> =
        params.iter().filter_map(|p| p.first().copied()).collect();

    let tooltip = match *c {
        'A' => move_cursor('A', params_without_subparams),
        'B' => move_cursor('B', params_without_subparams),
        'C' => move_cursor('C', params_without_subparams),
        'D' => move_cursor('D', params_without_subparams),
        'E' => move_cursor('E', params_without_subparams),
        'F' => move_cursor('F', params_without_subparams),
        'h' => csi_h(params_without_subparams, intermediates),
        'H' => csi_H(params_without_subparams),
        'J' => csi_J(params_without_subparams),
        'K' => csi_K(params_without_subparams),
        'l' => csi_l(params_without_subparams, intermediates),
        'm' => sgr(params),
        'n' => csi_n(params_without_subparams),
        _ => None,
    };

    VteEventDto::GenericEscape {
        title: format!("CSI {ascii}"),
        tooltip,
        raw_bytes: sanitize_raw_bytes(raw_bytes),
    }
}

fn move_cursor(c: char, params: Vec<u16>) -> Option<String> {
    if let [n] = params.as_slice() {
        return match c {
            'A' => Some(format!("Move cursor {n} rows up")),
            'B' => Some(format!("Move cursor {n} rows down")),
            'C' => Some(format!("Move cursor {n} columns right")),
            'D' => Some(format!("Move cursor {n} columns left")),
            'E' => Some(format!("Move cursor {n} rows down, to column 1")),
            'F' => Some(format!("Move cursor {n} rows up, to column 1")),
            _ => None,
        };
    }

    None
}

fn csi_h(params: Vec<u16>, intermediates: Vec<char>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();

    // private use sequences https://unix.stackexchange.com/a/289055/
    if intermediates.contains(&'?') {
        if params.contains(&1) {
            actions.push("Application Cursor Keys".into());
        }
        if params.contains(&25) {
            actions.push("Show cursor".into());
        }
        if params.contains(&47) {
            actions.push("Save screen".into());
        }
        if params.contains(&1049) {
            actions.push("Enable the alternative buffer".into());
        }
        if params.contains(&2004) {
            actions.push("Enable bracketed paste mode".into());
        }
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

#[allow(non_snake_case)]
fn csi_H(params: Vec<u16>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();

    match params.as_slice() {
        [] => actions.push("Move cursor to top left corner (0,0)".into()),
        // I believe a single param is just the row, but I'm not sure
        [row] => actions.push(format!("Move cursor to ({row},0)")),
        [row, column] => actions.push(format!("Move cursor to ({}, {})", row, column)),
        _ => {}
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

fn csi_l(params: Vec<u16>, intermediates: Vec<char>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();
    if intermediates.contains(&'?') {
        if params.contains(&1) {
            actions.push("Reset Cursor Keys".into());
        }
        if params.contains(&12) {
            actions.push("Stop blinking cursor".into());
        }
        if params.contains(&25) {
            actions.push("Hide cursor".into());
        }
        if params.contains(&47) {
            actions.push("Restore screen".into());
        }
        if params.contains(&1049) {
            actions.push("Disable the alternative buffer".into());
        }
        if params.contains(&2004) {
            actions.push("Disable bracketed paste mode".into());
        }
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

fn csi_n(params: Vec<u16>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();
    if params.contains(&6) {
        actions.push("Query cursor position".into());
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

#[allow(non_snake_case)]
fn csi_J(params: Vec<u16>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();

    if params.is_empty() || params.contains(&0) {
        actions.push("Clear from cursor to end of screen".into());
    }
    if params.contains(&1) {
        actions.push("Clear from cursor to start of screen".into());
    }
    if params.contains(&2) {
        actions.push("Clear entire screen".into());
    }
    if params.contains(&3) {
        actions.push("Clear saved lines".into());
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

#[allow(non_snake_case)]
fn csi_K(params: Vec<u16>) -> Option<String> {
    let mut actions: Vec<String> = Vec::new();

    if params.is_empty() || params.contains(&0) {
        actions.push("Clear from cursor to end of line".into());
    }
    if params.contains(&1) {
        actions.push("Clear from start of line to cursor".into());
    }
    if params.contains(&2) {
        actions.push("Clear entire line".into());
    }

    if actions.is_empty() {
        return None;
    }

    Some(actions.join(". "))
}

// Select Graphic Rendition https://stackoverflow.com/a/33206814/
fn sgr(params: &[Vec<u16>]) -> Option<String> {
    let mut foreground_colors: Vec<String> = vec![];
    let mut background_colors: Vec<String> = vec![];
    let mut attributes: Vec<String> = vec![];
    let mut reset_attributes: Vec<String> = vec![];

    // this iterator returns items of Vec<u16>
    // the first item in the vec is the parameter, later items are subparameters
    // however, I believe that the VTE crate only attaches subparameters if they are separated with a colon...
    // and in practice most applications separate subparameters with semicolons
    let mut iter = params.iter().peekable();

    loop {
        //
        let Some (param) = iter.next() else {
            break;
        };

        match param[0] {
            0 => return Some("Reset all modes (styles and colours)".into()),
            1 => attributes.push("bold".into()),
            2 => attributes.push("dim".into()),
            3 => attributes.push("italic".into()),
            4 => attributes.push("underline".into()),
            5 => attributes.push("blink".into()),
            7 => attributes.push("reverse".into()),
            8 => attributes.push("hidden".into()),
            9 => attributes.push("strikethrough".into()),
            22 => reset_attributes.push("bold/dim".into()),
            23 => reset_attributes.push("italic".into()),
            24 => reset_attributes.push("underline".into()),
            25 => reset_attributes.push("blink".into()),
            27 => reset_attributes.push("reverse".into()),
            28 => reset_attributes.push("hidden".into()),
            29 => reset_attributes.push("strikethrough".into()),
            30 => foreground_colors.push("black".into()),
            31 => foreground_colors.push("red".into()),
            32 => foreground_colors.push("green".into()),
            33 => foreground_colors.push("yellow".into()),
            34 => foreground_colors.push("blue".into()),
            35 => foreground_colors.push("magenta".into()),
            36 => foreground_colors.push("cyan".into()),
            37 => foreground_colors.push("white".into()),
            38 => {
                // Set foreground colour to a custom value
                // This is a little tricky because, as I understand it, the subparameters following 38
                // can be separated by either ; or : and that will affect whether VTE parses them as parameters or subparameters
                // https://wezfurlong.org/wezterm/escape-sequences.html#graphic-rendition-sgr
                // TODO: handle : subparameters

                foreground_colors.push("custom value".into());

                if let Some(next) = iter.peek() {
                    if next[0] == 5 {
                        // Next arguments are `5;<n>`
                        iter.next();
                        iter.next();
                    } else if next[0] == 2 {
                        // Next arguments are `2;<r>;<g>;<b>`
                        iter.next();
                        iter.next();
                        iter.next();
                        iter.next();
                    }
                }
            }
            39 => foreground_colors.push("default".into()),
            40 => background_colors.push("black".into()),
            41 => background_colors.push("red".into()),
            42 => background_colors.push("green".into()),
            43 => background_colors.push("yellow".into()),
            44 => background_colors.push("blue".into()),
            45 => background_colors.push("magenta".into()),
            46 => background_colors.push("cyan".into()),
            47 => background_colors.push("white".into()),
            48 => {
                // Set background colour to a custom value
                // This is a little tricky because, as I understand it, the subparameters following 48
                // can be separated by either ; or : and that will affect whether VTE parses them as parameters or subparameters
                // https://wezfurlong.org/wezterm/escape-sequences.html#graphic-rendition-sgr
                // TODO: handle : subparameters

                background_colors.push("custom value".into());

                if let Some(next) = iter.peek() {
                    if next[0] == 5 {
                        // Next arguments are `5;<n>`
                        iter.next();
                        iter.next();
                    } else if next[0] == 2 {
                        // Next arguments are `2;<r>;<g>;<b>`
                        iter.next();
                        iter.next();
                        iter.next();
                        iter.next();
                    }
                }
            }
            49 => background_colors.push("default".into()),
            90 => foreground_colors.push("bright black".into()),
            91 => foreground_colors.push("bright red".into()),
            92 => foreground_colors.push("bright green".into()),
            93 => foreground_colors.push("bright yellow".into()),
            94 => foreground_colors.push("bright blue".into()),
            95 => foreground_colors.push("bright magenta".into()),
            96 => foreground_colors.push("bright cyan".into()),
            97 => foreground_colors.push("bright white".into()),
            100 => background_colors.push("bright black".into()),
            101 => background_colors.push("bright red".into()),
            102 => background_colors.push("bright green".into()),
            103 => background_colors.push("bright yellow".into()),
            104 => background_colors.push("bright blue".into()),
            105 => background_colors.push("bright magenta".into()),
            106 => background_colors.push("bright cyan".into()),
            107 => background_colors.push("bright white".into()),
            _ => continue,
        }
    }

    let mut tooltip = String::new();

    if !attributes.is_empty() {
        tooltip.push_str(&format!("Set text to {}. ", attributes.join(", ")));
    }

    if !reset_attributes.is_empty() {
        tooltip.push_str(&format!(
            "Reset {} text attributes. ",
            reset_attributes.join("/")
        ));
    }

    if !foreground_colors.is_empty() {
        tooltip.push_str(&format!(
            "Set foreground color to {}. ",
            foreground_colors.join(", ")
        ));
    }

    if !background_colors.is_empty() {
        tooltip.push_str(&format!(
            "Set background color to {}. ",
            background_colors.join(", ")
        ));
    }

    if tooltip.is_empty() {
        None
    } else {
        Some(tooltip)
    }
}

fn osc_front_end(
    params: &Vec<Vec<u8>>,
    _bell_terminated: &bool,
    raw_bytes: &Vec<u8>,
) -> VteEventDto {
    // TODO handle things like OSC 133;B. Show more than first param
    // ]133;B

    if params.is_empty() {
        return VteEventDto::GenericEscape {
            title: "OSC".into(),
            tooltip: None,
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    let params = params
        .iter()
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect::<Vec<String>>();
    let first = &params[0];

    // set title https://tldp.org/HOWTO/Xterm-Title-3.html
    if first == "0" && params.len() > 1 {
        return VteEventDto::GenericEscape {
            title: format!("OSC {first}"),
            tooltip: Some(format!("Set icon name and window title to '{}'", params[1])),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    if first == "1" && params.len() > 1 {
        return VteEventDto::GenericEscape {
            title: format!("OSC {first}"),
            tooltip: Some(format!("Set icon name to '{}'", params[1])),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    if first == "2" && params.len() > 1 {
        return VteEventDto::GenericEscape {
            title: format!("OSC {first}"),
            tooltip: Some(format!("Set window title to '{}'", params[1])),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    if first == "133" && params.len() > 1 {
        let (title, tooltip) = match params[1].as_str() {
            "A" => ("Pre-prompt", Some("OSC 133 pre-prompt marker")),
            "B" => ("Post-prompt", Some("OSC 133 post-prompt marker")),
            "C" => ("Pre-input", Some("OSC 133 pre-input marker")),
            "D" => ("Post-input", Some("OSC 133 post-input marker")),
            _ => ("OSC 133", None),
        };

        return VteEventDto::GenericEscape {
            title: title.into(),
            tooltip: tooltip.map(|s| s.into()),
            raw_bytes: sanitize_raw_bytes(raw_bytes),
        };
    }

    VteEventDto::GenericEscape {
        title: format!("OSC {first}"),
        tooltip: None,
        raw_bytes: sanitize_raw_bytes(raw_bytes),
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

// Some snapshot tests that are mostly useful for seeing what VTE is doing
#[test]
fn set_bold() -> Result<()> {
    let esc = [b'\x1b'];
    let input = "[1m";
    let combined = esc.iter().chain(input.as_bytes()).copied();

    let events = parse_bytes(combined);
    insta::assert_yaml_snapshot!(events);

    Ok(())
}

#[test]
fn nu_prompt() -> Result<()> {
    let input = include_bytes!("snapshots/escape_artist__nu_prompt.input");

    let events = parse_bytes(input.iter().copied());
    insta::assert_yaml_snapshot!(events);

    Ok(())
}

#[test]
fn bash_starship_prompt() -> Result<()> {
    let input = include_bytes!("snapshots/bash_starship_prompt.input");

    let events = parse_bytes(input.iter().copied());
    insta::assert_yaml_snapshot!(events);

    Ok(())
}

#[cfg(test)]
fn parse_bytes(combined: impl Iterator<Item = u8>) -> Vec<VteEvent> {
    // capacity arbitrarily chosen
    let (tx, _) = broadcast::channel::<VteEventDto>(10000);
    let state = AppState {
        all_events: Arc::new(Mutex::new(vec![])),
        tx,
    };

    let mut performer = Performer {
        curr_cmd_bytes: Vec::new(),
        state: state.clone(),
    };

    let mut statemachine = vte::Parser::new();

    for byte in combined {
        performer.curr_cmd_bytes.push(byte);
        statemachine.advance(&mut performer, byte);
    }
    let events = state.all_events.blocking_lock();
    events.clone()
}
