https://asciinema.org/docs/how-it-works

> A pseudo terminal is a pair of pseudo-devices, one of which, the slave, emulates a real text terminal device, the other of which, the master, provides the means by which a terminal emulator process controls the slave. 

> The role of the terminal emulator process is to interact with the user; to feed text input to the master pseudo-device for use by the shell (which is connected to the slave pseudo-device) and to read text output from the master pseudo-device and show it to the user.

We need to:

1. spin up a pty
2. launch Nu and hook it up to the pty
   1. base this on: https://github.com/rgwood/pty-driver/blob/master/src/main.rs
3. handle output from Nu and input from the keyboard

Useful reading:
https://poor.dev/blog/terminal-anatomy/

# TO DO

- [ ] use a nerd font
- [ ] handle tab/spaces better
  - should show \t inline or turn it into multiple spaces
  - leading spaces should indent lines
- [ ] handle left arrow cursor movement better, currently reported as "Execute "
- [ ] show raw bytes for line breaks
- [x] embed all js libs so it works offline
- [x] add an option to log to a file
- [x] coalesce multiple chars into a single string event, cut down on data transfer and work for the front-end
- [x] add a favicon
- [x] use open-rs to launch to page in a browser
- [x] add help page
- [x] use rust-embed to serve files https://github.com/pyrossh/rust-embed
- [x] show raw bytes for other
- [x] display raw bytes better. replace control codes
- [x] hide tooltip description when not exists
- [x] use floating-ui for a custom tooltip
- [x] display raw bytes in tooltip
- [x] wrap <span> items, don't overflow horizontally
- [x] Use clap or argh or similar
- [x] Debounce/chunk streaming events to cut down on #renders
- [x] send a "disconnected" message to web browser when exiting
- [x] Move more logic to Rust. Send to-be-displayed info as a new type
- [x] Start using VTE to parse output stream: https://github.com/alacritty/vte
- [x] Spin up web UI (Axum?)
- [x] Start logging all events
- [x] Send tokens to web UI
- [x] Why doesn't it render prompt?
  - Maybe because the cursor is on the prompt line. Do I need to disable cursor? 
  - Was because I was forgetting to flush stdin
- [x] Figure out why update is so infrequent (and only prompted by several keypresses)
- [x] Reset raw mode etc. when child Nu exits
- [x] Can/should I detect ctrl+D/SIGQUIT? Or when the child dies more generally?
  - if we get an EOF from nu stdout...