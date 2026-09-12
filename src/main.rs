mod commands;
mod completions;
mod daemon;
mod env;
mod ipc;
mod label;
mod logger;
mod socket;
mod term_state;
mod util;

use crate::ipc::HistoryFormat;

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Smart {
        program: String,
        args: Vec<String>,
        force_new: bool,
    },
    Attach {
        name: String,
        detached: bool,
        cmd: Vec<String>,
        labels: Vec<String>,
    },
    List {
        short: bool,
        verbose: bool,
        where_pair: Option<String>,
    },
    LabelGet {
        name: String,
        key: Option<String>,
    },
    LabelSet {
        name: String,
        pairs: Vec<String>,
    },
    LabelUnset {
        name: String,
        keys: Vec<String>,
    },
    LabelClear {
        name: String,
    },
    Run {
        name: String,
        cmd: Vec<String>,
        detached: bool,
        fish: bool,
    },
    Send {
        name: String,
        text: Vec<String>,
    },
    Tail {
        names: Vec<String>,
    },
    Kill {
        names: Vec<String>,
        force: bool,
    },
    Print {
        name: String,
        text: Vec<String>,
    },
    Write {
        name: String,
        path: String,
    },
    Detach {
        name: String,
    },
    History {
        name: String,
        format: HistoryFormat,
    },
    Rename {
        name: String,
        new_name: String,
    },
    Wait {
        names: Vec<String>,
    },
    Completions {
        shell: String,
    },
    Logs {
        name: String,
        extra: Vec<String>,
    },
    PrintEnv {
        name: String,
        key: Option<String>,
        shell: bool,
    },
    Last,
    Pick,
    Version,
    Help,
}

/// Parsed result of a `<flags> <session> <command...>` invocation.
#[derive(Debug, PartialEq, Eq)]
struct SessionCommand {
    name: String,
    cmd: Vec<String>,
    detached: bool,
    fish: bool,
    labels: Vec<String>,
}

fn parse_session_command(
    args: &[String],
    allow_detached: bool,
    allow_fish: bool,
    allow_labels: bool,
) -> Result<SessionCommand, String> {
    let mut detached = false;
    let mut fish = false;
    let mut labels: Vec<String> = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--" => {
                index += 1;
                break;
            }
            "-d" | "--detached" if allow_detached => detached = true,
            "--fish" if allow_fish => fish = true,
            "--labels" if allow_labels => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--labels requires a value (e.g. \"k=v k2=v2\")".to_string());
                };
                labels.push(value.clone());
                index += 1;
            }
            _ if allow_labels && arg.starts_with("--labels=") => {
                labels.push(arg["--labels=".len()..].to_string());
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option '{}'", option));
            }
            _ => break,
        }
        index += 1;
    }

    let Some(name) = args.get(index) else {
        return Err("session name required".to_string());
    };
    Ok(SessionCommand {
        name: name.clone(),
        cmd: args[index + 1..].to_vec(),
        detached,
        fish,
        labels,
    })
}

fn parse_args() -> Command {
    parse_args_from(std::env::args().skip(1).collect())
}

fn parse_args_from(args: Vec<String>) -> Command {
    if args.is_empty() {
        return Command::Pick;
    }

    if is_subcommand(&args[0]) && args.get(1).is_some_and(|arg| is_help_flag(arg)) {
        return Command::Help;
    }

    if args[0] == "--new" {
        let Some(program) = args.get(1) else {
            eprintln!("error: --new requires a command");
            std::process::exit(1);
        };
        return Command::Smart {
            program: program.clone(),
            args: args[2..].to_vec(),
            force_new: true,
        };
    }

    let first = args[0].as_str();
    match first {
        "--help" | "-h" | "help" | "h" => Command::Help,
        "--version" | "-V" | "-v" | "version" | "v" => Command::Version,
        "list" | "ls" | "l" => {
            let short = args.iter().any(|a| a == "-s" || a == "--short");
            let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
            let where_pair = args
                .iter()
                .position(|arg| arg == "--where")
                .and_then(|index| args.get(index + 1))
                .cloned();
            if args.iter().any(|arg| arg == "--where") && where_pair.is_none() {
                eprintln!("error: list --where requires key=value");
                std::process::exit(1);
            }
            Command::List {
                short,
                verbose,
                where_pair,
            }
        }
        "get" | "g" => {
            let Some(name) = args.get(1) else {
                eprintln!("error: get requires a session name");
                std::process::exit(1);
            };
            Command::LabelGet {
                name: name.clone(),
                key: args.get(2).cloned(),
            }
        }
        "set" => {
            let Some(name) = args.get(1) else {
                eprintln!("error: set requires a session name");
                std::process::exit(1);
            };
            Command::LabelSet {
                name: name.clone(),
                pairs: args[2..].to_vec(),
            }
        }
        "unset" | "un" => {
            let Some(name) = args.get(1) else {
                eprintln!("error: unset requires a session name");
                std::process::exit(1);
            };
            Command::LabelUnset {
                name: name.clone(),
                keys: args[2..].to_vec(),
            }
        }
        "clear" | "cl" => {
            let Some(name) = args.get(1) else {
                eprintln!("error: clear requires a session name");
                std::process::exit(1);
            };
            Command::LabelClear { name: name.clone() }
        }
        "kill" | "k" => {
            if args.len() < 2 {
                eprintln!("error: kill requires a session name");
                std::process::exit(1);
            }
            let force = args.iter().any(|a| a == "-f" || a == "--force");
            let names: Vec<String> = args[1..]
                .iter()
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
            let parsed =
                parse_session_command(&args[1..], true, true, false).unwrap_or_else(|error| {
                    eprintln!("error: run {}", error);
                    std::process::exit(1);
                });
            Command::Run {
                name: parsed.name,
                cmd: parsed.cmd,
                detached: parsed.detached,
                fish: parsed.fish,
            }
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
            Command::Write {
                name: args[1].clone(),
                path: args[2].clone(),
            }
        }
        "tail" | "t" => {
            if args.len() < 2 {
                eprintln!("error: tail requires a session name");
                std::process::exit(1);
            }
            Command::Tail {
                names: args[1..].to_vec(),
            }
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
            let name = session_name.unwrap_or_else(socket::session_name_from_env);
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
            Command::Completions {
                shell: args[1].clone(),
            }
        }
        "logs" | "lg" => {
            if args.len() < 2 {
                eprintln!("error: logs requires a session name");
                std::process::exit(1);
            }
            let name = args[1].clone();
            let extra = args[2..].to_vec();
            Command::Logs { name, extra }
        }
        "last" | "la" => Command::Last,
        "print-env" | "pe" => {
            let mut name: Option<String> = None;
            let mut key: Option<String> = None;
            let mut shell = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "-s" | "--shell" => shell = true,
                    _ if name.is_none() => name = Some(arg.clone()),
                    _ if key.is_none() => key = Some(arg.clone()),
                    _ => {}
                }
            }
            let name = name.unwrap_or_else(socket::session_name_from_env);
            if name.is_empty() {
                eprintln!("error: print-env requires a session name");
                std::process::exit(1);
            }
            Command::PrintEnv { name, key, shell }
        }
        "new" | "n" => {
            let parsed =
                parse_session_command(&args[1..], false, false, true).unwrap_or_else(|error| {
                    eprintln!("error: new {}", error);
                    std::process::exit(1);
                });
            Command::Attach {
                name: parsed.name,
                detached: true,
                cmd: parsed.cmd,
                labels: parsed.labels,
            }
        }
        "attach" | "a" => {
            let parsed =
                parse_session_command(&args[1..], true, false, true).unwrap_or_else(|error| {
                    eprintln!("error: attach {}", error);
                    std::process::exit(1);
                });
            Command::Attach {
                name: parsed.name,
                detached: parsed.detached,
                cmd: parsed.cmd,
                labels: parsed.labels,
            }
        }
        program => {
            if program.starts_with('-') {
                eprintln!("error: unknown option '{}'", program);
                std::process::exit(1);
            }
            Command::Smart {
                program: program.to_string(),
                args: args[1..].to_vec(),
                force_new: false,
            }
        }
    }
}

fn is_help_flag(arg: &str) -> bool {
    matches!(arg, "--help" | "-h")
}

fn is_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "--new"
            | "attach"
            | "a"
            | "new"
            | "n"
            | "list"
            | "ls"
            | "l"
            | "get"
            | "g"
            | "set"
            | "unset"
            | "un"
            | "clear"
            | "cl"
            | "kill"
            | "k"
            | "detach"
            | "d"
            | "run"
            | "r"
            | "send"
            | "s"
            | "print"
            | "p"
            | "write"
            | "wr"
            | "tail"
            | "t"
            | "history"
            | "hi"
            | "wait"
            | "w"
            | "rename"
            | "rn"
            | "completions"
            | "c"
            | "logs"
            | "lg"
            | "print-env"
            | "pe"
            | "last"
            | "la"
            | "version"
            | "v"
            | "help"
            | "h"
    )
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cmd = parse_args();
    let code = match cmd {
        Command::Help => {
            print_help();
            0
        }
        Command::Smart {
            program,
            args,
            force_new,
        } => commands::cmd_smart(&program, &args, force_new),
        Command::Version => {
            let dir = socket::socket_dir();
            println!("rift {}", env!("CARGO_PKG_VERSION"));
            println!("socket dir: {}", dir.display());
            println!("log dir:    {}", dir.join("logs").display());
            0
        }
        Command::List {
            short,
            verbose,
            where_pair,
        } => commands::cmd_list(short, verbose, where_pair.as_deref()),
        Command::LabelGet { name, key } => commands::cmd_label_get(&name, key.as_deref()),
        Command::LabelSet { name, pairs } => commands::cmd_label_set(&name, &pairs),
        Command::LabelUnset { name, keys } => commands::cmd_label_unset(&name, &keys),
        Command::LabelClear { name } => commands::cmd_label_clear(&name),
        Command::Kill { names, force } => commands::cmd_kill(&names, force),
        Command::Detach { name } => commands::cmd_detach(&name),
        Command::Run {
            name,
            cmd,
            detached,
            fish,
        } => commands::cmd_run(&name, &cmd, detached, fish),
        Command::Send { name, text } => commands::cmd_send(&name, &text),
        Command::Print { name, text } => commands::cmd_print(&name, &text),
        Command::Write { name, path } => commands::cmd_write(&name, &path),
        Command::Tail { names } => commands::cmd_tail(&names),
        Command::History { name, format } => commands::cmd_history(&name, format),
        Command::Wait { names } => commands::cmd_wait(&names),
        Command::Rename { name, new_name } => commands::cmd_rename(&name, &new_name),
        Command::Completions { shell } => {
            completions::print_completions(&shell);
            0
        }
        Command::Logs { name, extra } => commands::cmd_logs(&name, &extra),
        Command::PrintEnv { name, key, shell } => {
            commands::cmd_print_env(&name, key.as_deref(), shell)
        }
        Command::Last => commands::cmd_last(),
        Command::Pick => commands::cmd_pick(),
        Command::Attach {
            name,
            detached,
            cmd,
            labels,
        } => commands::cmd_attach(&name, detached, &cmd, &labels),
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "\
rift — terminal session daemon

Usage:
  rift                          Pick a session interactively ($RIFT_PICKER or builtin)
  rift <name-or-command> [...]  Attach existing; otherwise run a PATH command or named shell
  rift --new <command> [...]    Run command in next free basename session (name, name.1, ...)
  rift attach|a <session>       Explicit session attach/create (optional <cmd> instead of shell)
                                Run from inside a session to switch to <session>
  rift attach -d <session>      Create session without attaching
  rift attach --labels \"k=v ...\" <session>  Attach/create with labels set atomically
  rift new|n <session>          Same as attach -d
  rift list|ls|l [-s|-v] [--where k=v]
                                List sessions, optionally filtered by label
  rift get|g <session> [key]    Get all labels or one label value
  rift set <session> k=v...     Set labels (empty value removes a label)
  rift unset|un <session> key... Remove labels
  rift clear|cl <session>       Clear all labels
  rift run|r <session> <cmd...> Run a command in a session (-d, --fish)
  rift send|s <session> <text>  Send keystrokes to a session
  rift print|p <session> <text> Inject text into session display
  rift write|wr <session> <path> Write stdin to a file in the session
  rift tail|t <name>...         Follow session output in real-time
  rift history|hi <session>     Print session output (--vt, --html)
  rift logs|lg <session> [...]  Tail -f the session log file (extra args pass to tail)
  rift print-env|pe [-s] <session> [key]  Print the leader client's tracked env vars
  rift last|la                  Attach to the most recently attached session
  rift detach|d [<session>]     Detach all clients from a session
  rift rename|rn [<old_name>] <new_name> Rename a session (defaults to $RIFT_SESSION)
  rift kill|k <name>...         Kill sessions (-f to force)
  rift wait|w <name>...         Wait for sessions to complete
  rift completions|c <shell>    Print shell completions (bash, zsh, fish, nu)
  rift version|v                Print version
  rift help|h                   Print this help

Detach key: Ctrl+\\"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn run_options_are_parsed_before_the_session() {
        assert_eq!(
            parse_session_command(
                &strings(&["-d", "--fish", "build", "cargo", "test"]),
                true,
                true,
                false,
            ),
            Ok(SessionCommand {
                name: "build".to_string(),
                cmd: strings(&["cargo", "test"]),
                detached: true,
                fish: true,
                labels: Vec::new(),
            })
        );
    }

    #[test]
    fn command_flags_are_preserved_after_the_session() {
        assert_eq!(
            parse_session_command(
                &strings(&["build", "cargo", "test", "--release", "-p", "rift"]),
                true,
                true,
                false,
            ),
            Ok(SessionCommand {
                name: "build".to_string(),
                cmd: strings(&["cargo", "test", "--release", "-p", "rift"]),
                detached: false,
                fish: false,
                labels: Vec::new(),
            })
        );
    }

    #[test]
    fn option_separator_allows_dash_prefixed_session_name_to_reach_validation() {
        assert_eq!(
            parse_session_command(&strings(&["--", "-session", "command"]), true, false, false),
            Ok(SessionCommand {
                name: "-session".to_string(),
                cmd: strings(&["command"]),
                detached: false,
                fish: false,
                labels: Vec::new(),
            })
        );
    }

    #[test]
    fn attach_labels_flag_is_collected_before_the_session() {
        assert_eq!(
            parse_session_command(
                &strings(&["--labels", "project=rift env=dev", "dev"]),
                true,
                false,
                true,
            ),
            Ok(SessionCommand {
                name: "dev".to_string(),
                cmd: Vec::new(),
                detached: false,
                fish: false,
                labels: strings(&["project=rift env=dev"]),
            })
        );
        // --labels=<value> form.
        assert_eq!(
            parse_session_command(&strings(&["--labels=team=core", "dev"]), true, false, true),
            Ok(SessionCommand {
                name: "dev".to_string(),
                cmd: Vec::new(),
                detached: false,
                fish: false,
                labels: strings(&["team=core"]),
            })
        );
        // Missing value is an error.
        assert!(parse_session_command(&strings(&["--labels"]), true, false, true).is_err());
        // Not allowed for `run`.
        assert!(
            parse_session_command(&strings(&["--labels", "a=b", "s"]), true, true, false).is_err()
        );
    }

    #[test]
    fn bare_command_preserves_arguments_for_smart_resolution() {
        assert_eq!(
            parse_args_from(strings(&["codex", "--model", "gpt-5"])),
            Command::Smart {
                program: "codex".to_string(),
                args: strings(&["--model", "gpt-5"]),
                force_new: false,
            }
        );
    }

    #[test]
    fn new_flag_requests_an_allocated_command_session() {
        assert_eq!(
            parse_args_from(strings(&["--new", "codex", "--model", "gpt-5"])),
            Command::Smart {
                program: "codex".to_string(),
                args: strings(&["--model", "gpt-5"]),
                force_new: true,
            }
        );
    }

    #[test]
    fn lowercase_v_is_a_version_alias() {
        assert_eq!(parse_args_from(strings(&["-v"])), Command::Version);
    }

    #[test]
    fn subcommand_help_is_detected_before_operands_are_parsed() {
        for command in [
            "attach",
            "run",
            "send",
            "list",
            "history",
            "kill",
            "set",
            "completions",
            "--new",
        ] {
            assert_eq!(
                parse_args_from(strings(&[command, "--help"])),
                Command::Help,
                "command={command}"
            );
            assert_eq!(
                parse_args_from(strings(&[command, "-h"])),
                Command::Help,
                "command={command}"
            );
        }
    }
}
