pub fn print_completions(shell: &str) {
    match shell {
        "bash" => print!("{}", BASH),
        "zsh" => print!("{}", ZSH),
        "fish" => print!("{}", FISH),
        _ => {
            eprintln!("error: unsupported shell '{}' (bash, zsh, fish)", shell);
            std::process::exit(1);
        }
    }
}

const BASH: &str = r#"_rift_completions() {
  local cur prev words cword
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  local commands="attach new run send print write tail detach list completions kill history wait version help rename rn logs lg last la"

  if [[ $COMP_CWORD -eq 1 ]]; then
    COMPREPLY=($(compgen -W "$commands" -- "$cur"))
    return 0
  fi

  case "$prev" in
    attach|a|new|n|run|r|send|s|print|p|write|wr|tail|t|kill|k|history|hi|detach|d|wait|w|rename|rn|logs|lg)
      local sessions=$(rift list --short 2>/dev/null | tr '\n' ' ')
      COMPREPLY=($(compgen -W "$sessions" -- "$cur"))
      ;;
    completions)
      COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur"))
      ;;
    list|ls)
      COMPREPLY=($(compgen -W "--short --verbose" -- "$cur"))
      ;;
    *)
      ;;
  esac
}

complete -o bashdefault -o default -F _rift_completions rift
"#;

const ZSH: &str = r#"_rift() {
  local context state state_descr line
  typeset -A opt_args

  _arguments -C \
    '1: :->commands' \
    '2: :->args' \
    '*: :->trailing' \
    && return 0

  case $state in
    commands)
      local -a commands
      commands=(
        'attach:Attach to session, creating if needed'
        'new:Create session without attaching'
        'run:Run a command in a session'
        'send:Send keystrokes to a session'
        'print:Inject text into session display'
        'write:Write stdin to a file in the session'
        'tail:Follow session output in real-time'
        'detach:Detach all clients from a session'
        'rename:Rename a session'
        'list:List active sessions'
        'completions:Print shell completion script'
        'kill:Kill a session'
        'history:Print session output'
        'logs:Tail -f the session log file'
        'last:Attach to the most recently attached session'
        'wait:Wait for sessions to complete'
        'version:Print version'
        'help:Print help'
      )
      _describe 'command' commands
      ;;
    args)
      case $words[2] in
        attach|a|new|n|kill|k|run|r|send|s|print|p|write|wr|tail|t|detach|d|history|hi|wait|w|rename|rn|logs|lg)
          _rift_sessions
          ;;
        completions)
          _values 'shell' 'bash' 'zsh' 'fish'
          ;;
        list|ls)
          _values 'options' '--short' '--verbose'
          ;;
      esac
      ;;
    trailing)
      ;;
  esac
}

_rift_sessions() {
  local -a sessions

  local local_sessions=$(rift list --short 2>/dev/null)
  if [[ -n "$local_sessions" ]]; then
    sessions+=(${(f)local_sessions})
  fi

  _describe 'local session' sessions
}

compdef _rift rift
"#;

const FISH: &str = r#"complete -c rift -f

complete -c rift -n "__fish_is_nth_token 1" -a 'a attach' -d 'Attach to session, creating if needed'
complete -c rift -n "__fish_is_nth_token 1" -a 'n new' -d 'Create session without attaching'
complete -c rift -n "__fish_is_nth_token 1" -a 'r run' -d 'Run a command in a session'
complete -c rift -n "__fish_is_nth_token 1" -a 's send' -d 'Send keystrokes to a session'
complete -c rift -n "__fish_is_nth_token 1" -a 'p print' -d 'Inject text into session display'
complete -c rift -n "__fish_is_nth_token 1" -a 'wr write' -d 'Write stdin to a file in the session'
complete -c rift -n "__fish_is_nth_token 1" -a 't tail' -d 'Follow session output in real-time'
complete -c rift -n "__fish_is_nth_token 1" -a 'd detach' -d 'Detach all clients from a session'
complete -c rift -n "__fish_is_nth_token 1" -a 'rn rename' -d 'Rename a session'
complete -c rift -n "__fish_is_nth_token 1" -a 'l ls list' -d 'List active sessions'
complete -c rift -n "__fish_is_nth_token 1" -a 'c completions' -d 'Print shell completion script'
complete -c rift -n "__fish_is_nth_token 1" -a 'k kill' -d 'Kill a session'
complete -c rift -n "__fish_is_nth_token 1" -a 'hi history' -d 'Print session output'
complete -c rift -n "__fish_is_nth_token 1" -a 'lg logs' -d 'Tail -f the session log file'
complete -c rift -n "__fish_is_nth_token 1" -a 'la last' -d 'Attach to the most recently attached session'
complete -c rift -n "__fish_is_nth_token 1" -a 'w wait' -d 'Wait for sessions to complete'
complete -c rift -n "__fish_is_nth_token 1" -a 'v version' -d 'Print version'
complete -c rift -s V -l version -d 'Print version'
complete -c rift -n "__fish_is_nth_token 1" -a 'h help' -d 'Print help'
complete -c rift -s h -d 'Print help'

complete -c rift -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from attach a new n run r send s print p write wr tail t kill k detach d history hi wait w rename rn logs lg" -a '(rift list --short 2>/dev/null)' -d 'Session name'

complete -c rift -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from completions c" -a 'bash zsh fish' -d Shell

complete -c rift -n "__fish_seen_subcommand_from list ls l" -l short -s s -d 'Short output'
complete -c rift -n "__fish_seen_subcommand_from list ls l" -l verbose -s v -d 'Verbose output (uptime, log path)'
complete -c rift -n "__fish_seen_subcommand_from history hi" -l vt -d 'VT escape sequence format'
complete -c rift -n "__fish_seen_subcommand_from history hi" -l html -d 'HTML format'
complete -c rift -n "__fish_seen_subcommand_from attach a" -s d -l detached -d 'Create without attaching'
complete -c rift -n "__fish_seen_subcommand_from run r" -s d -l detached -d 'Run detached (background)'
complete -c rift -n "__fish_seen_subcommand_from run r" -l fish -d 'Use fish shell completion detection'
complete -c rift -n "__fish_seen_subcommand_from kill k" -s f -l force -d 'Force kill (SIGKILL)'
"#;
