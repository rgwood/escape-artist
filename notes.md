## Asciinema

https://asciinema.org/docs/how-it-works

> A pseudo terminal is a pair of pseudo-devices, one of which, the slave, emulates a real text terminal device, the other of which, the master, provides the means by which a terminal emulator process controls the slave. 

> The role of the terminal emulator process is to interact with the user; to feed text input to the master pseudo-device for use by the shell (which is connected to the slave pseudo-device) and to read text output from the master pseudo-device and show it to the user.

Start of all the asciinema logic is in `recorder.record()`:
https://github.com/asciinema/asciinema/blob/b86f7cb529bb06327bda574bea95161dfadc1016/asciinema/recorder.py#L12


Q: why does it start a new Nu instance? I guess that's necessary to record but I don't have a great understanding yet
A: Launches Nu because its path is in $env.SHELL.


proceeds to `pty.record()`:
https://github.com/asciinema/asciinema/blob/b86f7cb529bb06327bda574bea95161dfadc1016/asciinema/pty_.py#L23

calls `pty.fork()` from Python stdlib
> Fork and make the child a session leader with a controlling terminal.

```python
if pid == pty.CHILD:
        os.execvpe(command[0], command, env)
```


`sig_fd` is a file descriptor that gets exit signals I think?


`copy()` is where the magic happens: https://github.com/asciinema/asciinema/blob/b86f7cb529bb06327bda574bea95161dfadc1016/asciinema/pty_.py#L91

loop over events from pty_fd, tty_stdin_fd, signal_fd

pty_fd is the master
    handle_master_read()
    I predict that this function is reading stdout data, recording it, and echoing it back out to stdout
    yep: https://github.com/asciinema/asciinema/blob/b86f7cb529bb06327bda574bea95161dfadc1016/asciinema/pty_.py#L46

tty_stdin_fd
    This is hooked up to the keyboard / main stdin
    on hearing from this, I think we write to the pty/master
    yep that's what it does


OK so I think I know how this works.

We need to:

1. spin up a pty
2. launch Nu and hook it up to the pty
   1. base this on: https://github.com/rgwood/pty-driver/blob/master/src/main.rs
3. handle output from Nu and input from the keyboard


OK so, where this gets tricky is that we essentially need async I/O
or something like epoll where we wait for either stdin or Nu's stdout

💡 Could we do the ol' loops and a channel thing? 1 thread reading from stdin in a loop, 1 thread reading from master in a loop, both pumping data to a channel

It is sort of working! But why does it only update infrequently?
Update: was forgetting to flush


Useful reading:
https://poor.dev/blog/terminal-anatomy/


## April 4 update
Cleaned up dependencies. Use crossterm, it's easy and it should work on Windows
Use alternate screen, it's nice
Looks like I was forgetting to flush stdin. Nu and Bash work nicely now
Next steps: get the recording streaming to a web UI, identify ANSI control chars

TO DO

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