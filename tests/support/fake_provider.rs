//! A deterministic stand-in for an agent provider binary, so the WebSocket terminal can be
//! tested without `claude` or `codex` — or any network — being present.
//!
//! `IdeManager` will only launch an executable literally named `claude` or `codex` that it finds
//! on `PATH`, so a test puts this binary on a throwaway `PATH` under one of those names. It is
//! declared as a `[[bin]]` because that is the only target kind `cargo test` both builds and
//! hands the test a path to, through `CARGO_BIN_EXE_pidmesh-fake-provider` -- the same mechanism
//! `tests/mcp.rs` uses for `pidmesh-mcp`. An example would not do: `cargo test --all-targets`
//! compiles examples as test harnesses, which cannot be run as the program itself.
//!
//! It is driven by a line protocol, and every command is available from two places:
//!
//! * **At launch** — any line of `argv` that begins with `@@` runs, in order, before the provider
//!   starts reading input. That is the only way to script a provider that exits before a client
//!   can type at it, and it arrives through the launch `prompt`, which `managed_prompt` passes
//!   through verbatim.
//! * **At runtime** — one command per line on stdin, which is the PTY slave, so it is exactly
//!   what the terminal WebSocket and `POST .../input` write.
//!
//! | command | effect |
//! | --- | --- |
//! | `say TEXT` | writes `say:TEXT` |
//! | `size` | writes `size:ROWSxCOLS` as the *kernel* reports the PTY's window, via `stty` |
//! | `count N` | writes `n:1` … `n:N`, one per line, for ordering and loss assertions |
//! | `flood KIB` | writes `KIB` kibibytes, then `flood:done` |
//! | `binary` | writes bytes that are not valid UTF-8, then `binary:done` |
//! | `write PATH` | creates `PATH` under the worktree, then writes `write:PATH` |
//! | `spawn SECS` | starts a `sleep` grandchild and writes `spawn:PID` |
//! | `ignore-term` | installs a no-op SIGTERM/SIGINT handler, then writes `ignore-term:on` |
//! | `exit CODE` | exits with `CODE` |
//! | anything else | writes `got:LINE` |
//!
//! Every reply carries a prefix because a PTY echoes its own input: without one, a test could not
//! tell the command it typed from the answer it was looking for.

use std::env;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn main() {
    let script = boot_script(env::args().skip(1));
    let mut out = std::io::stdout();
    emit(&mut out, "provider:ready");
    for command in script {
        run(&command, &mut out);
    }
    read_commands(&mut out);
}

/// The `@@` lines of `argv`, in order. `managed_prompt` wraps the caller's prompt in fixed
/// instruction text, and none of that text starts with the marker.
fn boot_script(arguments: impl Iterator<Item = String>) -> Vec<String> {
    arguments
        .flat_map(|argument| {
            argument
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix("@@")
                        .map(|rest| rest.trim().to_owned())
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Read stdin a byte at a time and run one command per line.
///
/// Byte-wise rather than `read_line` because a client may send bytes that are not valid UTF-8:
/// `read_line` would fail the whole stream, which would end the session and hide whatever the
/// test was actually asserting. Commands are short, so the cost is irrelevant.
fn read_commands(out: &mut impl Write) {
    let mut input = std::io::stdin().lock();
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if byte[0] == b'\n' || byte[0] == b'\r' {
            let command = String::from_utf8_lossy(&line).into_owned();
            line.clear();
            run(command.trim(), out);
        } else {
            line.push(byte[0]);
        }
    }
}

fn run(line: &str, out: &mut impl Write) {
    let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
    match command {
        "" => {}
        "say" => emit(out, &format!("say:{argument}")),
        "size" => emit(out, &format!("size:{}", window_size())),
        "count" => {
            for index in 1..=argument.trim().parse::<u64>().unwrap_or(0) {
                emit(out, &format!("n:{index}"));
            }
        }
        "flood" => flood(out, argument.trim().parse::<usize>().unwrap_or(0)),
        "binary" => {
            let _ = out.write_all(&[0xff, 0xfe, 0x80, b'\n']);
            emit(out, "binary:done");
        }
        "write" => emit(out, &write_file(argument.trim())),
        "spawn" => emit(out, &spawn_grandchild(argument.trim())),
        "ignore-term" => {
            let _ = ctrlc::set_handler(|| {});
            emit(out, "ignore-term:on");
        }
        "exit" => {
            let _ = out.flush();
            std::process::exit(argument.trim().parse::<i32>().unwrap_or(0));
        }
        _ => emit(out, &format!("got:{line}")),
    }
}

fn emit(out: &mut impl Write, text: &str) {
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn flood(out: &mut impl Write, kibibytes: usize) {
    let mut block = vec![b'x'; 1023];
    block.push(b'\n');
    for _ in 0..kibibytes {
        if out.write_all(&block).is_err() {
            break;
        }
    }
    emit(out, "flood:done");
}

/// Ask the kernel for the PTY's window, rather than remembering what anyone claimed it was.
///
/// `stty` reads `TIOCGWINSZ` on stdin, which is the PTY slave, so this is the one fact a resize
/// is supposed to change and the only one worth asserting on. Shelling out keeps this helper free
/// of `unsafe`, which the crate denies.
fn window_size() -> String {
    Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()
        .map_or_else(
            || "unknown".to_owned(),
            |result| {
                String::from_utf8_lossy(&result.stdout)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("x")
            },
        )
}

/// Change a file inside the managed worktree, which is this process's working directory.
///
/// That is how a real provider earns a review, and the only way a test can give the scope rules
/// something to disagree about.
fn write_file(relative: &str) -> String {
    let path = std::path::Path::new(relative);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, "written by the fake provider\n").map_or_else(
        |error| format!("write-error:{error}"),
        |()| format!("write:{relative}"),
    )
}

/// A process in the session's group that is not the provider itself, so a test can tell
/// "the provider was signalled" from "the process group was".
fn spawn_grandchild(seconds: &str) -> String {
    let seconds = if seconds.is_empty() { "60" } else { seconds };
    Command::new("sleep")
        .arg(seconds)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_or_else(
            |error| format!("spawn-error:{error}"),
            |child| format!("spawn:{}", child.id()),
        )
}
