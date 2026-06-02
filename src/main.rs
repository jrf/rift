mod commands;
mod completions;
mod daemon;
mod ipc;
mod logger;
mod socket;
mod util;

use crate::util::HistoryFormat;

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

enum Command {
    Attach { name: String, detached: bool, cmd: Vec<String> },
    List { short: bool },
    Run { name: String, cmd: Vec<String>, detached: bool, fish: bool },
    Send { name: String, text: Vec<String> },
    Tail { names: Vec<String> },
    Kill { names: Vec<String>, force: bool },
    Print { name: String, text: Vec<String> },
    Write { name: String, path: String },
    Detach { name: String },
    History { name: String, format: HistoryFormat },
    Rename { name: String, new_name: String },
    Wait { names: Vec<String> },
    Completions { shell: String },
    Version,
    Help,
}

fn parse_args() -> Command {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        return Command::Help;
    }

    let first = args[0].as_str();
    match first {
        "--help" | "-h" | "help" | "h" => Command::Help,
        "--version" | "-V" | "version" | "v" => Command::Version,
        "list" | "ls" | "l" => {
            let short = args.iter().any(|a| a == "-s" || a == "--short");
            Command::List { short }
        }
        "kill" | "k" => {
            if args.len() < 2 {
                eprintln!("error: kill requires a session name");
                std::process::exit(1);
            }
            let force = args.iter().any(|a| a == "-f" || a == "--force");
            let names: Vec<String> = args[1..].iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if names.is_empty() {
                eprintln!("error: kill requires a session name");
                std::process::exit(1);
            }
            Command::Kill { names, force }
        }
        "detach" | "d" => {
            let name = if args.len() >= 2 {
                args[1].clone()
            } else {
                let env = socket::session_name_from_env();
                if env.is_empty() {
                    eprintln!("error: detach requires a session name");
                    std::process::exit(1);
                }
                env
            };
            Command::Detach { name }
        }
        "run" | "r" => {
            if args.len() < 2 {
                eprintln!("error: run requires a session name");
                std::process::exit(1);
            }
            let detached = args.iter().any(|a| a == "-d" || a == "--detached");
            let fish = args.iter().any(|a| a == "--fish");
            let positional: Vec<String> = args[1..].iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if positional.is_empty() {
                eprintln!("error: run requires a session name");
                std::process::exit(1);
            }
            let name = positional[0].clone();
            let cmd = positional[1..].to_vec();
            Command::Run { name, cmd, detached, fish }
        }
        "send" | "s" => {
            if args.len() < 2 {
                eprintln!("error: send requires a session name");
                std::process::exit(1);
            }
            let name = args[1].clone();
            let text = args[2..].to_vec();
            Command::Send { name, text }
        }
        "print" | "p" => {
            if args.len() < 2 {
                eprintln!("error: print requires a session name");
                std::process::exit(1);
            }
            let name = args[1].clone();
            let text = args[2..].to_vec();
            Command::Print { name, text }
        }
        "write" | "wr" => {
            if args.len() < 3 {
                eprintln!("error: write requires a session name and file path");
                std::process::exit(1);
            }
            Command::Write { name: args[1].clone(), path: args[2].clone() }
        }
        "tail" | "t" => {
            if args.len() < 2 {
                eprintln!("error: tail requires a session name");
                std::process::exit(1);
            }
            Command::Tail { names: args[1..].to_vec() }
        }
        "history" | "hi" => {
            let mut session_name: Option<String> = None;
            let mut format = HistoryFormat::Plain;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--vt" => format = HistoryFormat::Vt,
                    "--html" => format = HistoryFormat::Html,
                    _ if session_name.is_none() => session_name = Some(arg.clone()),
                    _ => {}
                }
            }
            let name = session_name.unwrap_or_else(|| socket::session_name_from_env());
            if name.is_empty() {
                eprintln!("error: history requires a session name");
                std::process::exit(1);
            }
            Command::History { name, format }
        }
        "wait" | "w" => {
            let names: Vec<String> = args[1..].to_vec();
            Command::Wait { names }
        }
        "rename" | "rn" => {
            if args.len() < 2 {
                eprintln!("error: rename requires a new name");
                std::process::exit(1);
            }
            let (name, new_name) = if args.len() == 2 {
                let env = socket::session_name_from_env();
                if env.is_empty() {
                    eprintln!("error: rename outside a session requires current_name and new_name");
                    std::process::exit(1);
                }
                (env, args[1].clone())
            } else {
                (args[1].clone(), args[2].clone())
            };
            Command::Rename { name, new_name }
        }
        "completions" | "c" => {
            if args.len() < 2 {
                eprintln!("error: completions requires a shell name (bash, zsh, fish)");
                std::process::exit(1);
            }
            Command::Completions { shell: args[1].clone() }
        }
        "new" | "n" => {
            if args.len() < 2 {
                eprintln!("error: new requires a session name");
                std::process::exit(1);
            }
            let positional: Vec<String> = args[1..].iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            let name = positional[0].clone();
            let cmd = positional[1..].to_vec();
            Command::Attach { name, detached: true, cmd }
        }
        "attach" | "a" => {
            if args.len() < 2 {
                eprintln!("error: attach requires a session name");
                std::process::exit(1);
            }
            let detached = args.iter().any(|a| a == "-d" || a == "--detached");
            let positional: Vec<String> = args[1..].iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if positional.is_empty() {
                eprintln!("error: attach requires a session name");
                std::process::exit(1);
            }
            let name = positional[0].clone();
            let cmd = positional[1..].to_vec();
            Command::Attach { name, detached, cmd }
        }
        name => {
            if name.starts_with('-') {
                eprintln!("error: unknown option '{}'", name);
                std::process::exit(1);
            }
            Command::Attach { name: name.to_string(), detached: false, cmd: vec![] }
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cmd = parse_args();
    let code = match cmd {
        Command::Help => { print_help(); 0 }
        Command::Version => { println!("rift {}", env!("CARGO_PKG_VERSION")); 0 }
        Command::List { short } => commands::cmd_list(short),
        Command::Kill { names, force } => commands::cmd_kill(&names, force),
        Command::Detach { name } => commands::cmd_detach(&name),
        Command::Run { name, cmd, detached, fish } => commands::cmd_run(&name, &cmd, detached, fish),
        Command::Send { name, text } => commands::cmd_send(&name, &text),
        Command::Print { name, text } => commands::cmd_print(&name, &text),
        Command::Write { name, path } => commands::cmd_write(&name, &path),
        Command::Tail { names } => commands::cmd_tail(&names),
        Command::History { name, format } => commands::cmd_history(&name, format),
        Command::Wait { names } => commands::cmd_wait(&names),
        Command::Rename { name, new_name } => commands::cmd_rename(&name, &new_name),
        Command::Completions { shell } => { completions::print_completions(&shell); 0 }
        Command::Attach { name, detached, cmd } => commands::cmd_attach(&name, detached, &cmd),
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "\
rift — terminal session daemon

Usage:
  rift <session>                Attach to (or create) a session
  rift attach|a <session>       Same as above (optional <cmd> to run instead of shell)
  rift attach -d <session>      Create session without attaching
  rift new|n <session>          Same as attach -d
  rift list|ls|l [-s]           List sessions (-s for short format)
  rift run|r <session> <cmd...> Run a command in a session (-d, --fish)
  rift send|s <session> <text>  Send keystrokes to a session
  rift print|p <session> <text> Inject text into session display
  rift write|wr <session> <path> Write stdin to a file in the session
  rift tail|t <name>...         Follow session output in real-time
  rift history|hi <session>     Print session output (--vt, --html)
  rift detach|d [<session>]     Detach all clients from a session
  rift rename|rn [<old_name>] <new_name> Rename a session (defaults to $RIFT_SESSION)
  rift kill|k <name>...         Kill sessions (-f to force)
  rift wait|w <name>...         Wait for sessions to complete
  rift completions|c <shell>    Print shell completions (bash, zsh, fish)
  rift version|v                Print version
  rift help|h                   Print this help

Detach key: Ctrl+\\"
    );
}
