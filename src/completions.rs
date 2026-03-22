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

const BASH: &str = r#"_ryx_completions() {
  local cur prev words cword
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  local commands="attach new run detach list completions kill history wait version help"

  if [[ $COMP_CWORD -eq 1 ]]; then
    COMPREPLY=($(compgen -W "$commands" -- "$cur"))
    return 0
  fi

  case "$prev" in
    attach|new|run|kill|history|hi|detach|d|wait|w)
      local sessions=$(ryx list --short 2>/dev/null | tr '\n' ' ')
      COMPREPLY=($(compgen -W "$sessions" -- "$cur"))
      ;;
    completions)
      COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur"))
      ;;
    list|ls)
      COMPREPLY=($(compgen -W "--short" -- "$cur"))
      ;;
    *)
      ;;
  esac
}

complete -o bashdefault -o default -F _ryx_completions ryx
"#;

const ZSH: &str = r#"_ryx() {
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
        'detach:Detach all clients from a session'
        'list:List active sessions'
        'completions:Print shell completion script'
        'kill:Kill a session'
        'history:Print session output'
        'wait:Wait for sessions to complete'
        'version:Print version'
        'help:Print help'
      )
      _describe 'command' commands
      ;;
    args)
      case $words[2] in
        attach|new|kill|run|detach|d|history|hi|wait|w)
          _ryx_sessions
          ;;
        completions)
          _values 'shell' 'bash' 'zsh' 'fish'
          ;;
        list|ls)
          _values 'options' '--short'
          ;;
      esac
      ;;
    trailing)
      ;;
  esac
}

_ryx_sessions() {
  local -a sessions

  local local_sessions=$(ryx list --short 2>/dev/null)
  if [[ -n "$local_sessions" ]]; then
    sessions+=(${(f)local_sessions})
  fi

  _describe 'local session' sessions
}

compdef _ryx ryx
"#;

const FISH: &str = r#"complete -c ryx -f

complete -c ryx -n "__fish_is_nth_token 1" -a 'attach' -d 'Attach to session, creating if needed'
complete -c ryx -n "__fish_is_nth_token 1" -a 'new' -d 'Create session without attaching'
complete -c ryx -n "__fish_is_nth_token 1" -a 'run' -d 'Run a command in a session'
complete -c ryx -n "__fish_is_nth_token 1" -a 'd detach' -d 'Detach all clients from a session'
complete -c ryx -n "__fish_is_nth_token 1" -a 'ls list' -d 'List active sessions'
complete -c ryx -n "__fish_is_nth_token 1" -a 'completions' -d 'Print shell completion script'
complete -c ryx -n "__fish_is_nth_token 1" -a 'kill' -d 'Kill a session'
complete -c ryx -n "__fish_is_nth_token 1" -a 'hi history' -d 'Print session output'
complete -c ryx -n "__fish_is_nth_token 1" -a 'w wait' -d 'Wait for sessions to complete'
complete -c ryx -n "__fish_is_nth_token 1" -a 'version' -d 'Print version'
complete -c ryx -s V -l version -d 'Print version'
complete -c ryx -n "__fish_is_nth_token 1" -a 'help' -d 'Print help'
complete -c ryx -s h -d 'Print help'

complete -c ryx -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from attach new run kill detach d history hi wait w" -a '(ryx list --short 2>/dev/null)' -d 'Session name'

complete -c ryx -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from completions" -a 'bash zsh fish' -d Shell

complete -c ryx -n "__fish_seen_subcommand_from list ls" -l short -s s -d 'Short output'
complete -c ryx -n "__fish_seen_subcommand_from history hi" -l vt -d 'VT escape sequence format'
complete -c ryx -n "__fish_seen_subcommand_from history hi" -l html -d 'HTML format'
complete -c ryx -n "__fish_seen_subcommand_from attach" -s d -l detached -d 'Create without attaching'
"#;
